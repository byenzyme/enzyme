use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use libsql::Value;
use serde::Serialize;
use serde_json::json;

use enzyme_core::config::enzyme_config::{CatalystMode, EnzymeConfig, SqliteSourceConfig};
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::db::update_sentinel::{self, UpdateStatus};
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient, model_registry};
use enzyme_core::pipeline::catalysts::{CatalystConfig, CatalystPipeline, SkipReason};
use enzyme_core::pipeline::epoch::GenerationContract;
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::sqlite_source::{check_sqlite_source_reachability, source_refresh_staleness};

use super::init::{BINARY_VERSION, EvidenceLimits, read_collection_cache_epoch};
use super::refresh_outcome::{BackgroundRefreshOutcome, OutcomeStatus, load_outcome};
use super::update::{
    BinaryUpdateOutcome, BinaryUpdateStatus, load_outcome as load_binary_update, update_available,
};

#[derive(Debug, Serialize)]
struct SourceStatus {
    name: String,
    database: String,
    documents: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    row_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refresh_ms: Option<i64>,
    stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_reason: Option<String>,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reachability_error: Option<String>,
}

/// The index as `status` sees it: never created, migrated, or written.
enum StatusIndex {
    Missing,
    /// Built by an older binary; reading it would need a migration.
    Outdated(String),
    Ready(Database),
}

fn open_status_index(paths: &EnzymePaths) -> Result<StatusIndex> {
    if !paths.vault_db.exists() {
        return Ok(StatusIndex::Missing);
    }
    let db = Database::open_read_only(&paths.vault_db)?;
    Ok(match db.validate_expected_schema() {
        Ok(()) => StatusIndex::Ready(db),
        Err(error) => StatusIndex::Outdated(error.to_string()),
    })
}

/// Build structured JSON with vault status info.
/// Returns None if the vault is not initialized.
pub async fn build_status_json(vault_path: &Path) -> Result<Option<serde_json::Value>> {
    Ok(build_status(vault_path)?.map(|(status, _)| status))
}

/// Status JSON plus the read-only index it was read from (`None` when the
/// index schema is outdated and only the identity fields are reported).
fn build_status(vault_path: &Path) -> Result<Option<(serde_json::Value, Option<Database>)>> {
    let paths = EnzymePaths::new(vault_path)?;
    let workspace = EnzymePaths::bound_workspace(vault_path)?;

    let db = match open_status_index(&paths)? {
        StatusIndex::Missing => return Ok(None),
        StatusIndex::Outdated(reason) => {
            return Ok(Some((
                json!({
                    "workspace": workspace,
                    "vault": vault_path.display().to_string(),
                    "database": paths.vault_db.display().to_string(),
                    "schema_outdated": true,
                    "update": {
                        "status": "schema_outdated",
                        "regen_required": true,
                        "reason": reason,
                        "binary": { "current": BINARY_VERSION, "last_outcome": null },
                    },
                }),
                None,
            )));
        }
        StatusIndex::Ready(db) => db,
    };

    // Status must stay read-only and offline: report provider intent from the
    // persisted mode rather than resolving hosted credentials over the network.
    let llm = EnzymeConfig::catalyst_llm_config()?;
    let local_model = model_registry::selected_name();
    let local_model_installed = model_registry::model_path(&local_model)?.exists();
    let catalyst_mode = match llm.mode {
        CatalystMode::Auto if local_model_installed => "local",
        CatalystMode::Auto => "hosted",
        CatalystMode::Local if local_model_installed => "local",
        CatalystMode::Local => "local-missing",
        CatalystMode::Hosted => "hosted",
    };
    let api_key_configured = std::env::var("OPENAI_API_KEY").is_ok_and(|value| !value.is_empty());
    let api_access = if api_key_configured {
        "environment"
    } else {
        "enzyme-managed"
    };

    let model_path = enzyme_core::embedding::model_description(&paths.home);

    let doc_count = db.get_document_count()?;
    let entities = db.get_all_entities()?;
    let entity_count = entities.len();
    let catalysts = db.get_all_catalysts()?;
    let catalyst_count = catalysts.len();

    let docs_needing = db.get_documents_needing_embedding(None, None)?;
    let awaiting_embedding = docs_needing.len() as i64;
    let embedded_count = doc_count - awaiting_embedding;
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let sources = build_source_statuses(&db, &config.sources)?;
    let markdown = build_markdown_freshness(&db, vault_path, &config)?;

    // Terminal outcome of the most recent detached background epoch build
    // (see `refresh_outcome`). `null` when no background build has finished
    // yet (or the vault predates outcome recording).
    let last_background_refresh = load_outcome(&db);

    // Status stays read-only: peek at the update sentinel without advancing
    // it, so agents can see whether a regen is pending before it happens.
    let binary_update = load_binary_update(&db);
    let mut update = match update_sentinel::peek(&db, BINARY_VERSION)? {
        UpdateStatus::UpToDate => json!({
            "status": "up_to_date", "regen_required": false, "reason": null
        }),
        UpdateStatus::SentinelRefreshed { previous_version } => json!({
            "status": "sentinel_stale", "regen_required": false,
            "reason": format!(
                "binary changed since {}; schema compatible, sentinel will advance on next refresh",
                previous_version.as_deref().unwrap_or("unknown")
            ),
        }),
        UpdateStatus::RegenRequired { reason } => json!({
            "status": "regen_required", "regen_required": true, "reason": reason
        }),
    };
    update["binary"] = json!({
        "current": BINARY_VERSION,
        "last_outcome": binary_update,
    });

    let status = json!({
        "workspace": workspace,
        "vault": vault_path.display().to_string(),
        "database": paths.vault_db.display().to_string(),
        "schema_outdated": false,
        "documents": doc_count,
        "embedded": embedded_count,
        "awaiting_embedding": awaiting_embedding,
        "entities": entity_count,
        "catalysts": catalyst_count,
        "model": model_path,
        "catalyst_mode_configured": llm.mode.to_string(),
        "catalyst_mode": catalyst_mode,
        "catalyst_model": local_model,
        "catalyst_model_installed": local_model_installed,
        "api_key_configured": api_key_configured,
        "api_access": api_access,
        "last_background_refresh": last_background_refresh,
        "sources": sources,
        "markdown": markdown,
        "update": update,
    });
    Ok(Some((status, Some(db))))
}

/// Markdown freshness for targets that read Markdown (`null` for a named
/// workspace without a Markdown source).
fn build_markdown_freshness(
    db: &Database,
    vault_path: &Path,
    config: &EnzymeConfig,
) -> Result<serde_json::Value> {
    if EnzymePaths::bound_workspace(vault_path)?.is_some()
        && config.notes_roots.is_empty()
        && config.notes_path.is_none()
    {
        return Ok(serde_json::Value::Null);
    }
    let freshness = super::init::configured_indexing_pipeline(db, vault_path)?.freshness()?;
    Ok(serde_json::to_value(freshness)?)
}

fn build_source_statuses(
    db: &Database,
    sources: &HashMap<String, SqliteSourceConfig>,
) -> Result<Vec<SourceStatus>> {
    let mut names: Vec<_> = sources.keys().collect();
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let source = sources
                .get(name)
                .expect("source name was collected from the same map");
            let pattern = format!("sqlite:{}/%", escape_like_pattern(name));
            let documents = db
                .query_read(
                    "SELECT COUNT(*) FROM docs WHERE source_ref LIKE ?1 ESCAPE '\\'",
                    vec![Value::Text(pattern)],
                    |row| row.get::<i64>(0),
                )?
                .first()
                .copied()
                .unwrap_or(0);
            let (reachable, reachability_error) = match check_sqlite_source_reachability(&source.db)
            {
                Ok(()) => (true, None),
                Err(error) => (false, Some(format!("{error:#}"))),
            };
            let refresh = db.get_source_refresh_status(name)?;
            let staleness = source_refresh_staleness(db, name, &source.db)?;

            Ok(SourceStatus {
                name: name.clone(),
                database: source.db.display().to_string(),
                documents,
                row_count: refresh.map(|status| status.0),
                bucket_count: refresh.map(|status| status.1),
                last_refresh_ms: refresh.map(|status| status.2),
                stale: staleness.stale,
                stale_reason: staleness.reason,
                reachable,
                reachability_error,
            })
        })
        .collect()
}

pub const STATUS_SCHEMA: &str = "enzyme.status.v1";

/// Most entities without catalysts that `status --json` builds a generation
/// job for to explain readiness; the rest report `unchecked`.
pub const STATUS_PLAN_LIMIT: usize = 16;

/// Generation readiness of one selected entity.
#[derive(Debug, Serialize)]
struct EntityReadiness {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    /// `ready` (has catalysts), `skipped` (no generation job; see
    /// `skip_reason`), `pending` (has a job but no catalysts yet), or
    /// `unchecked` (no catalysts; beyond `STATUS_PLAN_LIMIT`, not planned).
    state: &'static str,
    catalysts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_reason: Option<SkipReason>,
}

/// Selected entities with their catalyst counts and, for entities without
/// catalysts, why generation would not produce any. Read-only: runs the same
/// selection and job construction init runs, without a model.
fn build_selection_readiness(db: &Database, vault_path: &Path) -> Result<serde_json::Value> {
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let selection = SelectionPipeline::new(db, vault_path).run_with_config(Some(&config))?;
    let counts = db.get_catalyst_counts_by_entity()?;
    let catalysts_for = |name: &str, entity_type: &str| -> usize {
        counts
            .iter()
            .filter(|((entity, kind), _)| {
                entity == name && (kind == entity_type || kind.is_empty())
            })
            .map(|(_, count)| *count)
            .sum()
    };

    // Job construction gathers evidence and builds a prompt per entity, so
    // status plans at most `STATUS_PLAN_LIMIT` entities without catalysts.
    // Ready entities need no plan; on a fresh or `--llm none` index every
    // selected entity is waiting, and the cap bounds that worst case.
    let waiting_all: Vec<_> = selection
        .entities
        .iter()
        .filter(|entity| catalysts_for(&entity.name, entity.entity_type.as_str()) == 0)
        .cloned()
        .collect();
    let planned: std::collections::HashSet<(String, String)> = waiting_all
        .iter()
        .take(STATUS_PLAN_LIMIT)
        .map(|entity| (entity.name.clone(), entity.entity_type.as_str().to_string()))
        .collect();
    let waiting: Vec<_> = waiting_all.into_iter().take(STATUS_PLAN_LIMIT).collect();
    let mut skips = HashMap::new();
    if !waiting.is_empty() {
        let limits = EvidenceLimits::from_env();
        let cat_config = CatalystConfig {
            reading_spec: config.reading_spec.as_ref(),
            entity_profiles: &selection.entity_profiles,
            catalysts_per_entity: 7,
            default_profile: DEFAULT_PROFILE_KEY,
            format_key: config
                .catalyst_format
                .as_deref()
                .unwrap_or(DEFAULT_FORMAT_KEY),
            prompt_context: None,
            model: "status",
            per_entity_allocations: None,
            max_context_chars: limits.max_context_chars,
            max_samples: limits.max_samples,
            concurrency: None,
            stream_partial_catalysts: false,
            force_regenerate: false,
            min_context_documents: limits.min_context_documents,
            min_context_tokens: limits.min_context_tokens,
            epoch_id: None,
        };
        // Never contacted: job construction only builds prompts.
        let llm = LlmClient::with_base_url("status", "https://status.invalid/v1");
        let plan = CatalystPipeline::new(db, &llm, None).plan_jobs(&waiting, &cat_config)?;
        for skip in plan.skipped {
            skips.insert((skip.entity_name, skip.entity_type), skip.reason);
        }
    }

    let entities: Vec<EntityReadiness> = selection
        .entities
        .iter()
        .map(|entity| {
            let entity_type = entity.entity_type.as_str().to_string();
            let catalysts = catalysts_for(&entity.name, &entity_type);
            let skip_reason = skips.remove(&(entity.name.clone(), entity_type.clone()));
            let state = match (catalysts, &skip_reason) {
                (n, _) if n > 0 => "ready",
                (_, Some(_)) => "skipped",
                _ if !planned.contains(&(entity.name.clone(), entity_type.clone())) => {
                    "unchecked"
                }
                _ => "pending",
            };
            EntityReadiness {
                name: entity.name.clone(),
                entity_type,
                state,
                catalysts,
                skip_reason,
            }
        })
        .collect();
    let count = |state: &str| entities.iter().filter(|e| e.state == state).count();
    Ok(json!({
        "selected": entities.len(),
        "ready": count("ready"),
        "pending": count("pending"),
        "skipped": count("skipped"),
        "unchecked": count("unchecked"),
        "entities": entities,
    }))
}

/// What built the index and which catalyst epoch is active.
fn build_index_manifest(db: &Database) -> Result<serde_json::Value> {
    let epoch = db.get_epoch_state()?;
    let contract_fingerprint = epoch
        .active_generation_contract
        .as_deref()
        .and_then(|json| serde_json::from_str::<GenerationContract>(json).ok())
        .and_then(|contract| contract.fingerprint().ok());
    Ok(json!({
        "built_by": db.get_config(update_sentinel::BINARY_VERSION_KEY)?,
        "schema_fingerprint": db.get_config(update_sentinel::SCHEMA_FINGERPRINT_KEY)?,
        "cache_epoch": read_collection_cache_epoch(db)?,
        "epoch": {
            "active_epoch_id": epoch.active_epoch_id,
            "active_epoch_completed_at_ms": epoch.active_epoch_completed_at_ms,
            "active_epoch_corpus_generation": epoch.active_epoch_corpus_generation,
            "corpus_generation": epoch.corpus_generation,
            "corpus_changed_since_epoch":
                epoch.active_epoch_corpus_generation != Some(epoch.corpus_generation),
            "build_epoch_id": epoch.build_epoch_id,
            "contract_fingerprint": contract_fingerprint,
        },
    }))
}

/// `status --json`: [`build_status_json`] plus selection readiness and the
/// index manifest, under a versioned schema. An uninitialized vault still
/// answers, with `initialized: false`.
pub async fn build_status_json_v1(vault_path: &Path) -> Result<serde_json::Value> {
    let Some((mut status, db)) = build_status(vault_path)? else {
        let paths = EnzymePaths::new(vault_path)?;
        return Ok(json!({
            "schema": STATUS_SCHEMA,
            "initialized": false,
            "workspace": EnzymePaths::bound_workspace(vault_path)?,
            "vault": vault_path.display().to_string(),
            "database": paths.vault_db.display().to_string(),
        }));
    };
    let object = status.as_object_mut().expect("status JSON is an object");
    object.insert("schema".into(), json!(STATUS_SCHEMA));
    object.insert("initialized".into(), json!(true));
    if let Some(db) = db {
        object.insert(
            "selection".into(),
            build_selection_readiness(&db, vault_path)?,
        );
        object.insert("index".into(), build_index_manifest(&db)?);
    }
    Ok(status)
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Render `finished_at_ms` for humans, falling back to the raw value if it is
/// not a representable timestamp.
fn format_finished_at(finished_at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(finished_at_ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| format!("{finished_at_ms} ms"))
}

pub async fn run_status(vault_path: &Path, json_output: bool) -> Result<()> {
    if json_output {
        let status = build_status_json_v1(vault_path).await?;
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    match build_status_json(vault_path).await? {
        None => {
            if let Some(workspace) = EnzymePaths::bound_workspace(vault_path)? {
                println!("Workspace: {workspace}");
                println!("Store: {}", vault_path.display());
            } else {
                println!("Vault: {}", vault_path.display());
            }
            println!("Status: not initialized");
            println!();
            println!("Run 'enzyme init' to initialize.");
        }
        Some(json) => {
            let outcome: Option<BackgroundRefreshOutcome> =
                serde_json::from_value(json["last_background_refresh"].clone()).ok();
            let binary_update: Option<BinaryUpdateOutcome> =
                serde_json::from_value(json["update"]["binary"]["last_outcome"].clone()).ok();

            if let Some(workspace) = json["workspace"].as_str() {
                println!("Workspace: {workspace}");
                println!("Store: {}", json["vault"].as_str().unwrap_or(""));
            } else {
                println!("Vault: {}", json["vault"].as_str().unwrap_or(""));
            }
            println!("Database: {}", json["database"].as_str().unwrap_or(""));
            println!();
            if json["schema_outdated"].as_bool().unwrap_or(false) {
                println!(
                    "⚠ Index schema is outdated ({}). The next `enzyme init` or agent refresh will rebuild it.",
                    json["update"]["reason"].as_str().unwrap_or("built by an older binary")
                );
                return Ok(());
            }
            if let Some(outcome) = &outcome {
                if outcome.status == OutcomeStatus::Succeeded {
                    println!("Refreshed:  {}", format_finished_at(outcome.finished_at_ms));
                }
            }
            println!(
                "API access: {}",
                match json["api_access"].as_str() {
                    Some("environment") => "environment key",
                    _ => "Enzyme-managed (resolved on demand)",
                }
            );
            if let Some(sources) = json["sources"].as_array()
                && !sources.is_empty()
            {
                println!();
                println!("Sources:");
                for source in sources {
                    println!(
                        "  {}: {} documents",
                        source["name"].as_str().unwrap_or("unknown"),
                        source["documents"]
                    );
                    if source["reachable"].as_bool().unwrap_or(false) {
                        println!(
                            "    Database: reachable ({})",
                            source["database"].as_str().unwrap_or("unknown")
                        );
                    } else {
                        println!(
                            "    Database: unreachable ({}) — {}",
                            source["database"].as_str().unwrap_or("unknown"),
                            source["reachability_error"]
                                .as_str()
                                .unwrap_or("unknown error")
                        );
                    }
                    if let Some(row_count) = source["row_count"].as_u64() {
                        println!(
                            "    Rows: {row_count}; buckets: {}",
                            source["bucket_count"].as_u64().unwrap_or(0)
                        );
                    }
                    if let Some(last_refresh_ms) = source["last_refresh_ms"].as_i64() {
                        println!("    Refreshed: {}", format_finished_at(last_refresh_ms));
                    }
                    if source["stale"].as_bool().unwrap_or(true) {
                        println!(
                            "    Stale: yes ({})",
                            source["stale_reason"].as_str().unwrap_or("unknown")
                        );
                    } else {
                        println!("    Stale: no");
                    }
                }
            }
            if json["markdown"]["stale"].as_bool().unwrap_or(false) {
                println!();
                println!(
                    "Markdown: stale ({} new, {} modified, {} deleted since the last index)",
                    json["markdown"]["files_new"],
                    json["markdown"]["files_modified"],
                    json["markdown"]["files_deleted"]
                );
            }
            if json["update"]["regen_required"].as_bool().unwrap_or(false) {
                println!();
                println!(
                    "⚠ Update: regen required — {}. The next `enzyme init` or agent refresh will rebuild the index automatically (auth and config are preserved).",
                    json["update"]["reason"].as_str().unwrap_or("schema drift"),
                );
            }
            if let Some(binary) = &binary_update {
                let staged = binary.status == BinaryUpdateStatus::Updated
                    && binary.to.as_deref() != Some(BINARY_VERSION);
                if staged {
                    println!();
                    println!(
                        "⚠ Binary update to {} is staged and takes effect on the next invocation.",
                        binary.to.as_deref().unwrap_or("the latest version")
                    );
                } else if !binary.channel.is_self_swappable()
                    && binary.to.as_deref().is_some_and(|latest| {
                        update_available(BINARY_VERSION, latest).unwrap_or(false)
                    })
                {
                    println!();
                    println!(
                        "⚠ Binary update available: {}.",
                        binary.channel.advisory_action()
                    );
                }
            }
            if let Some(outcome) = &outcome {
                if outcome.status == OutcomeStatus::Failed {
                    println!();
                    println!(
                        "⚠ Last background refresh failed ({}): {} — previous catalysts carried forward; see .enzyme/background_refresh.log",
                        format_finished_at(outcome.finished_at_ms),
                        outcome.error.as_deref().unwrap_or("unknown error"),
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_core::config::enzyme_config::{
        SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit, SqliteWhoRole,
    };
    use tempfile::TempDir;

    fn source_config(path: &Path) -> SqliteSourceConfig {
        SqliteSourceConfig {
            db: path.to_path_buf(),
            query: "SELECT id, created_at, body FROM entries".to_string(),
            roles: SqliteSourceRoles {
                id: vec!["id".to_string()],
                document_ref: None,
                who: SqliteWhoRole::default(),
                when: "created_at".to_string(),
                what: vec!["body".to_string()],
                r#where: Vec::new(),
                weight: None,
            },
            timestamp: SqliteTimestampConfig {
                unit: SqliteTimestampUnit::Ms,
                epoch: None,
            },
            filter: None,
        }
    }

    #[test]
    fn source_status_reports_documents_refresh_orientation_and_reachability() {
        let temp = TempDir::new().unwrap();
        let source_path = temp.path().join("source.db");
        Database::open(&source_path).unwrap();
        let db = Database::open_in_memory().unwrap();
        for source_ref in [
            "sqlite:entry_log/integer:1",
            "sqlite:entry_log/integer:2",
            "sqlite:entryXlog/not-this-source",
            "notes/entry.md",
        ] {
            db.execute_write(
                "INSERT INTO docs
                    (source_ref, content_hash, created_at, modified_at, indexed_at)
                 VALUES (?1, 'hash', 1, 1, 1)",
                vec![Value::Text(source_ref.to_string())],
            )
            .unwrap();
        }
        db.set_source_refresh_status("entry_log", 3000, 2, 1_700_000_000_000)
            .unwrap();
        let sources = HashMap::from([("entry_log".to_string(), source_config(&source_path))]);

        let statuses = build_source_statuses(&db, &sources).unwrap();
        let value = serde_json::to_value(&statuses[0]).unwrap();

        assert_eq!(value["name"], "entry_log");
        assert_eq!(value["documents"], 2);
        assert_eq!(value["reachable"], true);
        assert_eq!(value["row_count"], 3000);
        assert_eq!(value["bucket_count"], 2);
        assert_eq!(value["last_refresh_ms"], 1_700_000_000_000_i64);
    }
}
