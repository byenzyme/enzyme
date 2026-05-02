use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::{Database, EntityRefreshStats};

/// Number of new document mentions for an entity before it's considered stale.
const NEW_MENTIONS_THRESHOLD: usize = 3;

/// If this many or more entities are stale, do a full refresh instead of partial.
pub const FULL_REFRESH_ENTITY_THRESHOLD: usize = 3;

/// Config DB key for the catalyst config hash.
const CONFIG_HASH_KEY: &str = "catalyst_config_hash";

/// Config DB key for the entity doc count snapshot (JSON).
const ENTITY_DOC_COUNTS_KEY: &str = "catalyst_entity_doc_counts";

/// Minimum meaningful movement in effective document date before refresh treats
/// the entity's catalysts as stale. This avoids tiny timestamp noise while still
/// catching backfilled daily notes, imported highlights, and fresh SDK entries.
const EFFECTIVE_DATE_STALENESS_MS: i64 = 60 * 60 * 1000;

/// An entity that has new content since last catalyst generation.
#[derive(Debug, Clone)]
pub struct StaleEntity {
    pub name: String,
    pub delta: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EntityStalenessSnapshot {
    doc_count: usize,
    latest_effective_date: Option<i64>,
}

/// Why the catalysts are stale.
#[derive(Debug)]
pub enum StaleReason {
    /// The enzyme-config.yaml changed (entities or profile).
    ConfigChanged,
    /// One or more entities have significantly more content since last generation.
    EntitiesStale { stale_entities: Vec<StaleEntity> },
    /// No catalysts exist yet.
    NoCatalysts,
}

impl StaleReason {
    /// Whether this staleness requires a full refresh (all entities).
    ///
    /// True for config changes, no catalysts, or when enough entities are stale
    /// that a full refresh is more efficient than partial.
    pub fn needs_full_refresh(&self) -> bool {
        match self {
            StaleReason::ConfigChanged | StaleReason::NoCatalysts => true,
            StaleReason::EntitiesStale { stale_entities } => {
                stale_entities.len() >= FULL_REFRESH_ENTITY_THRESHOLD
            }
        }
    }

    /// Entity names to regenerate for a partial refresh.
    ///
    /// Returns `Some(names)` when only a subset of entities need regeneration,
    /// `None` when a full refresh is needed.
    pub fn stale_entity_names(&self) -> Option<Vec<String>> {
        match self {
            StaleReason::EntitiesStale { stale_entities }
                if stale_entities.len() < FULL_REFRESH_ENTITY_THRESHOLD =>
            {
                Some(stale_entities.iter().map(|e| e.name.clone()).collect())
            }
            _ => None,
        }
    }
}

/// Result of a staleness check.
#[derive(Debug)]
pub enum StalenessResult {
    /// Catalysts are up to date.
    Fresh,
    /// Catalysts need regeneration.
    Stale(StaleReason),
}

impl StalenessResult {
    pub fn is_stale(&self) -> bool {
        matches!(self, StalenessResult::Stale(_))
    }

    pub fn is_fresh(&self) -> bool {
        matches!(self, StalenessResult::Fresh)
    }
}

/// Check whether the vault's catalysts are stale and need regeneration.
///
/// Two independent checks (any one triggers stale):
/// 1. **Config hash** — SHA256 of `catalyzedEntities` + `catalystProfile` changed.
/// 2. **New content** — entities with 3+ new document mentions since last generation.
///
/// All stale entities are collected (not short-circuited) so callers can decide
/// between a partial refresh (few entities) and a full refresh (many entities).
pub fn check_catalyst_staleness(db: &Database, vault_path: &Path) -> Result<StalenessResult> {
    check_catalyst_staleness_for_entities(db, vault_path, None)
}

/// Check catalyst staleness against an optional current entity set.
///
/// Supplying current entity names lets refresh catch newly selected entities
/// that are not present in the previous bookkeeping snapshot yet.
pub fn check_catalyst_staleness_for_entities(
    db: &Database,
    vault_path: &Path,
    current_entity_names: Option<&HashSet<String>>,
) -> Result<StalenessResult> {
    // --- Check 0: Do catalysts exist at all? ---
    let latest_ts = db.get_latest_catalyst_timestamp()?;
    if latest_ts.is_none() {
        info!("no catalysts found — stale");
        return Ok(StalenessResult::Stale(StaleReason::NoCatalysts));
    }

    // --- Check 1: Config hash ---
    let current_hash = compute_config_hash(vault_path)?;
    let stored_hash = db.get_config(CONFIG_HASH_KEY)?;

    if stored_hash.as_deref() != Some(&current_hash) {
        info!(
            stored = stored_hash.as_deref().unwrap_or("<none>"),
            current = current_hash.as_str(),
            "config hash changed — stale"
        );
        return Ok(StalenessResult::Stale(StaleReason::ConfigChanged));
    }

    // --- Check 2: New content (collect ALL stale entities) ---
    let stored_snapshots = load_stored_entity_snapshots(db)?;
    let catalyzed_entities = db.get_catalyzed_entity_names()?;
    let mut stale_entities = Vec::new();
    if !stored_snapshots.is_empty() {
        for (entity_name, snapshot) in &stored_snapshots {
            let current = db.get_entity_refresh_stats(entity_name)?;
            let delta = current.doc_count.saturating_sub(snapshot.doc_count);
            let count_stale = delta >= NEW_MENTIONS_THRESHOLD;
            let effective_date_stale = is_newer(
                current.latest_effective_date,
                snapshot.latest_effective_date,
                EFFECTIVE_DATE_STALENESS_MS,
            );
            let missing_catalysts = !catalyzed_entities.contains(entity_name);

            if count_stale || effective_date_stale || missing_catalysts {
                info!(
                    entity = entity_name.as_str(),
                    old_count = snapshot.doc_count,
                    current_count = current.doc_count,
                    delta,
                    effective_date_stale,
                    missing_catalysts,
                    "entity has stale catalyst evidence"
                );
                stale_entities.push(StaleEntity {
                    name: entity_name.clone(),
                    delta: delta.max(usize::from(effective_date_stale || missing_catalysts)),
                });
            }
        }
    }

    if let Some(current_names) = current_entity_names {
        for entity_name in current_names {
            if stored_snapshots.contains_key(entity_name) {
                continue;
            }
            let missing_catalysts = !catalyzed_entities.contains(entity_name);
            if missing_catalysts {
                info!(
                    entity = entity_name.as_str(),
                    "selected entity is missing from catalyst staleness snapshot"
                );
                stale_entities.push(StaleEntity {
                    name: entity_name.clone(),
                    delta: 1,
                });
            }
        }
    }

    if !stale_entities.is_empty() {
        return Ok(StalenessResult::Stale(StaleReason::EntitiesStale {
            stale_entities,
        }));
    }

    debug!("catalysts are fresh");
    Ok(StalenessResult::Fresh)
}

/// Record bookkeeping state after a successful catalyst generation.
///
/// Stores the current config hash and per-entity document counts so the next
/// `check_catalyst_staleness` call can detect drift.
pub fn store_staleness_bookkeeping(
    db: &Database,
    vault_path: &Path,
    entity_names: &[String],
) -> Result<()> {
    // Store config hash
    let hash = compute_config_hash(vault_path)?;
    db.set_config(CONFIG_HASH_KEY, &hash)?;

    // Store entity doc counts
    let mut counts: HashMap<String, EntityStalenessSnapshot> = HashMap::new();
    for name in entity_names {
        counts.insert(
            name.clone(),
            snapshot_from_stats(db.get_entity_refresh_stats(name)?),
        );
    }
    let json = serde_json::to_string(&counts)?;
    db.set_config(ENTITY_DOC_COUNTS_KEY, &json)?;

    debug!(
        config_hash = hash.as_str(),
        entity_count = entity_names.len(),
        "stored staleness bookkeeping"
    );
    Ok(())
}

/// Record bookkeeping for a partial refresh — updates doc counts only for the
/// specified entities, preserving counts for all others.
pub fn store_staleness_bookkeeping_partial(db: &Database, entity_names: &[String]) -> Result<()> {
    // Load existing counts and merge in the refreshed entities
    let mut counts = load_stored_entity_snapshots(db)?;
    for name in entity_names {
        counts.insert(
            name.clone(),
            snapshot_from_stats(db.get_entity_refresh_stats(name)?),
        );
    }
    let json = serde_json::to_string(&counts)?;
    db.set_config(ENTITY_DOC_COUNTS_KEY, &json)?;

    debug!(
        refreshed = entity_names.len(),
        total = counts.len(),
        "stored partial staleness bookkeeping"
    );
    Ok(())
}

/// Compute a SHA256 hash of the config fields that affect catalyst generation.
///
/// Hashes the sorted list of parsed entity names/types.
/// This is deterministic regardless of YAML formatting or ordering.
fn compute_config_hash(vault_path: &Path) -> Result<String> {
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let prefs = config.parse_preferences();

    let mut hasher = Sha256::new();

    // Hash entities: sort by (name, type) for determinism
    let mut entity_keys: Vec<String> = prefs
        .entities
        .iter()
        .map(|e| format!("{}:{}", e.entity_type, e.name))
        .collect();
    entity_keys.sort();
    for key in &entity_keys {
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }

    let hash = format!("{:x}", hasher.finalize());
    Ok(hash)
}

/// Load entity names from the last staleness snapshot.
pub fn load_stored_entity_names(db: &Database) -> Result<HashSet<String>> {
    let counts = load_stored_entity_snapshots(db)?;
    Ok(counts.into_keys().collect())
}

/// Load stored entity snapshots from the config table.
///
/// Older databases stored this key as `{ "entity": count }`; keep accepting that
/// shape so existing vaults get upgraded naturally on the next successful
/// catalyst generation.
fn load_stored_entity_snapshots(db: &Database) -> Result<HashMap<String, EntityStalenessSnapshot>> {
    match db.get_config(ENTITY_DOC_COUNTS_KEY)? {
        Some(json) => {
            let raw: serde_json::Value = serde_json::from_str(&json)?;
            let mut snapshots = HashMap::new();
            if let Some(map) = raw.as_object() {
                for (name, value) in map {
                    let snapshot = if let Some(count) = value.as_u64() {
                        EntityStalenessSnapshot {
                            doc_count: count as usize,
                            ..Default::default()
                        }
                    } else {
                        serde_json::from_value(value.clone())?
                    };
                    snapshots.insert(name.clone(), snapshot);
                }
            }
            Ok(snapshots)
        }
        None => Ok(HashMap::new()),
    }
}

fn snapshot_from_stats(stats: EntityRefreshStats) -> EntityStalenessSnapshot {
    EntityStalenessSnapshot {
        doc_count: stats.doc_count,
        latest_effective_date: stats.latest_effective_date,
    }
}

fn is_newer(current: Option<i64>, stored: Option<i64>, tolerance_ms: i64) -> bool {
    match (current, stored) {
        (Some(current), Some(stored)) => current > stored.saturating_add(tolerance_ms),
        (Some(_), None) => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Catalyst;
    use chrono::Utc;
    use libsql::Value;

    #[test]
    fn test_config_hash_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path();

        let h1 = compute_config_hash(vault_path).unwrap();
        let h2 = compute_config_hash(vault_path).unwrap();
        assert_eq!(h1, h2, "same config should produce same hash");
    }

    #[test]
    fn test_staleness_no_catalysts() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();

        let result = check_catalyst_staleness(&db, dir.path()).unwrap();
        assert!(result.is_stale());
        assert!(matches!(
            result,
            StalenessResult::Stale(StaleReason::NoCatalysts)
        ));
    }

    #[test]
    fn test_staleness_respects_new_doc_threshold() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();

        db.execute_write(
            "INSERT INTO entities (id, name, type, frequency, first_seen, last_seen)
             VALUES (1, 'alpha', 'tag', 1, 1000, 1000)",
            vec![],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO entity_occurrences
             (entity_id, file_path, context, created_at, effective_date, date_source)
             VALUES (1, 'note.md', 'old', 1000, 1000, 'filesystem')",
            vec![],
        )
        .unwrap();
        db.insert_catalysts(&[Catalyst {
            id: "cat-alpha".to_string(),
            text: "alpha catalyst".to_string(),
            entity: "alpha".to_string(),
            context: "default".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
        store_staleness_bookkeeping(&db, dir.path(), &["alpha".to_string()]).unwrap();

        db.execute_write(
            "INSERT INTO entity_occurrences
             (entity_id, file_path, context, created_at, effective_date, date_source)
             VALUES (1, 'second.md', 'new doc', ?1, 1000, 'filesystem')",
            vec![Value::Integer(2000)],
        )
        .unwrap();

        let result = check_catalyst_staleness(&db, dir.path()).unwrap();
        assert!(matches!(result, StalenessResult::Fresh));
    }

    #[test]
    fn stale_entity_names_switches_to_full_refresh_at_threshold() {
        let one = StaleReason::EntitiesStale {
            stale_entities: vec![StaleEntity {
                name: "alpha".to_string(),
                delta: 3,
            }],
        };
        assert_eq!(one.stale_entity_names(), Some(vec!["alpha".to_string()]));

        let threshold = StaleReason::EntitiesStale {
            stale_entities: vec![
                StaleEntity {
                    name: "alpha".to_string(),
                    delta: 3,
                },
                StaleEntity {
                    name: "beta".to_string(),
                    delta: 3,
                },
                StaleEntity {
                    name: "gamma".to_string(),
                    delta: 3,
                },
            ],
        };
        assert!(threshold.stale_entity_names().is_none());
        assert!(threshold.needs_full_refresh());
    }

    #[test]
    fn selected_entity_missing_from_snapshot_is_stale() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();

        db.execute_write(
            "INSERT INTO entities (id, name, type, frequency, first_seen, last_seen)
             VALUES (1, 'alpha', 'tag', 1, 1000, 1000),
                    (2, 'beta', 'tag', 1, 1000, 1000)",
            vec![],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO entity_occurrences
             (entity_id, file_path, context, created_at, effective_date, date_source)
             VALUES (1, 'alpha.md', 'old', 1000, 1000, 'filesystem'),
                    (2, 'beta.md', 'new selected entity', 1000, 1000, 'filesystem')",
            vec![],
        )
        .unwrap();
        db.insert_catalysts(&[Catalyst {
            id: "cat-alpha".to_string(),
            text: "alpha catalyst".to_string(),
            entity: "alpha".to_string(),
            context: "default".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
        store_staleness_bookkeeping(&db, dir.path(), &["alpha".to_string()]).unwrap();

        let current = HashSet::from(["alpha".to_string(), "beta".to_string()]);
        let result =
            check_catalyst_staleness_for_entities(&db, dir.path(), Some(&current)).unwrap();

        match result {
            StalenessResult::Stale(StaleReason::EntitiesStale { stale_entities }) => {
                assert!(
                    stale_entities.iter().any(|entity| entity.name == "beta"),
                    "new selected entity should be stale: {:?}",
                    stale_entities
                );
            }
            other => panic!("expected beta to be stale, got {:?}", other),
        }
    }
}
