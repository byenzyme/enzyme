use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::{Database, Repository};

/// Maximum age (in days) before catalysts are considered stale.
const MAX_CATALYST_AGE_DAYS: i64 = 3;

/// Number of new document mentions for an entity before it's considered stale.
const NEW_MENTIONS_THRESHOLD: usize = 3;

/// Config DB key for the catalyst config hash.
const CONFIG_HASH_KEY: &str = "catalyst_config_hash";

/// Config DB key for the entity doc count snapshot (JSON).
const ENTITY_DOC_COUNTS_KEY: &str = "catalyst_entity_doc_counts";

/// Why the catalysts are stale.
#[derive(Debug)]
pub enum StaleReason {
    /// The enzyme-config.yaml changed (entities or profile).
    ConfigChanged,
    /// Catalysts are older than the max age threshold.
    CatalystsExpired,
    /// One or more entities have significantly more content since last generation.
    NewContent { entity: String, delta: usize },
    /// No catalysts exist yet.
    NoCatalysts,
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
}

/// Check whether the vault's catalysts are stale and need regeneration.
///
/// Three independent checks (any one triggers stale):
/// 1. **Config hash** — SHA256 of `catalyzedEntities` + `catalystProfile` changed.
/// 2. **Catalyst age** — newest catalyst is older than `MAX_CATALYST_AGE_DAYS`.
/// 3. **New content** — any selected entity has 3+ new document mentions.
pub fn check_catalyst_staleness(
    db: &Database,
    vault_path: &Path,
) -> Result<StalenessResult> {
    let repo = Repository::new(db);

    // --- Check 0: Do catalysts exist at all? ---
    let latest_ts = repo.get_latest_catalyst_timestamp()?;
    if latest_ts.is_none() {
        info!("no catalysts found — stale");
        return Ok(StalenessResult::Stale(StaleReason::NoCatalysts));
    }

    // --- Check 1: Config hash ---
    let current_hash = compute_config_hash(vault_path)?;
    let stored_hash = repo.get_config(CONFIG_HASH_KEY)?;

    if stored_hash.as_deref() != Some(&current_hash) {
        info!(
            stored = stored_hash.as_deref().unwrap_or("<none>"),
            current = current_hash.as_str(),
            "config hash changed — stale"
        );
        return Ok(StalenessResult::Stale(StaleReason::ConfigChanged));
    }

    // --- Check 2: Catalyst age ---
    if let Some(ts) = latest_ts {
        let age = Utc::now().signed_duration_since(ts);
        if age.num_days() > MAX_CATALYST_AGE_DAYS {
            info!(age_days = age.num_days(), "catalysts expired — stale");
            return Ok(StalenessResult::Stale(StaleReason::CatalystsExpired));
        }
    }

    // --- Check 3: New content ---
    let stored_counts = load_stored_doc_counts(&repo)?;
    if !stored_counts.is_empty() {
        for (entity_name, old_count) in &stored_counts {
            let current_count = repo.get_entity_doc_count(entity_name)?;
            if current_count >= *old_count + NEW_MENTIONS_THRESHOLD {
                let delta = current_count - old_count;
                info!(
                    entity = entity_name.as_str(),
                    old_count,
                    current_count,
                    "entity has new content — stale"
                );
                return Ok(StalenessResult::Stale(StaleReason::NewContent {
                    entity: entity_name.clone(),
                    delta,
                }));
            }
        }
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
    let repo = Repository::new(db);

    // Store config hash
    let hash = compute_config_hash(vault_path)?;
    repo.set_config(CONFIG_HASH_KEY, &hash)?;

    // Store entity doc counts
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in entity_names {
        let count = repo.get_entity_doc_count(name)?;
        counts.insert(name.clone(), count);
    }
    let json = serde_json::to_string(&counts)?;
    repo.set_config(ENTITY_DOC_COUNTS_KEY, &json)?;

    debug!(
        config_hash = hash.as_str(),
        entity_count = entity_names.len(),
        "stored staleness bookkeeping"
    );
    Ok(())
}

/// Compute a SHA256 hash of the config fields that affect catalyst generation.
///
/// Hashes the sorted list of parsed entity names/types and the catalyst profile.
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

    // Hash catalystProfile
    if let Some(ref profile) = config.catalyst_profile {
        hasher.update(b"profile:");
        hasher.update(profile.as_bytes());
    }

    let hash = format!("{:x}", hasher.finalize());
    Ok(hash)
}

/// Load entity names from the last staleness snapshot.
pub fn load_stored_entity_names(db: &Database) -> Result<HashSet<String>> {
    let repo = Repository::new(db);
    let counts = load_stored_doc_counts(&repo)?;
    Ok(counts.into_keys().collect())
}

/// Load stored entity doc counts from the config table.
fn load_stored_doc_counts(repo: &Repository) -> Result<HashMap<String, usize>> {
    match repo.get_config(ENTITY_DOC_COUNTS_KEY)? {
        Some(json) => {
            let counts: HashMap<String, usize> = serde_json::from_str(&json)?;
            Ok(counts)
        }
        None => Ok(HashMap::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(result, StalenessResult::Stale(StaleReason::NoCatalysts)));
    }
}
