use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{debug, info};

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::document::FileDiscovery;
use enzyme_core::embedding::{EmbeddingService, get_embedding_service};
use enzyme_core::llm::{
    DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmCredentialMode, resolve_llm_client_for_mode,
};
use enzyme_core::models::Entity;
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystEvent, CatalystPipeline, CatalystStats, calculate_allocations,
};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::embedding::{EmbeddingPipeline, EmbeddingStats};
use enzyme_core::pipeline::epoch::{EPOCH_CADENCE_MS, EpochSchedule, GenerationContract, now_ms};
use enzyme_core::pipeline::indexing::{IndexingPipeline, IndexingStats};
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::pipeline::similarity::{
    IncrementalSimilarityStats, SimilarityPipeline, catalysts_fingerprint,
};
use enzyme_core::pipeline::staleness::store_staleness_bookkeeping;

use super::lock::{acquire_projection_lock, acquire_refresh_lock, is_refresh_locked};
use super::vault_summary;
use crate::display::{CompletionStatus, DisplayEvent, InitDisplay};
use enzyme_core::config::auth::AuthToken;

async fn preflight_env_llm(use_env_llm: bool) -> Result<()> {
    if use_env_llm {
        let _ = resolve_llm_client_for_mode(LlmCredentialMode::Env)
            .await
            .context("--use-env-llm was set but no env LLM config could be resolved")?;
    }
    Ok(())
}

fn loginless_hosted_init_allowed_for_keys(current_key: &str, configured_keys: &[String]) -> bool {
    configured_keys.is_empty()
        || (configured_keys.len() == 1 && configured_keys.first().is_some_and(|k| k == current_key))
}

/// Enzyme's first hosted init should be frictionless: a user can scan/configure
/// one vault and run `enzyme init` without signing in. Once this machine has
/// global config for multiple vaults, or the user is using refresh, we require a
/// real login so hosted usage is tied to an account.
fn require_login_for_hosted_init_if_needed(vault_path: &Path, paths: &EnzymePaths) -> Result<()> {
    if AuthToken::load(&paths.auth_token()).is_ok() {
        return Ok(());
    }

    let current_key = EnzymeConfig::vault_key(vault_path);
    let configured_keys = EnzymeConfig::configured_vault_keys()?;
    if loginless_hosted_init_allowed_for_keys(&current_key, &configured_keys) {
        return Ok(());
    }

    anyhow::bail!(
        "Login required before initializing another vault on this machine. First init can run without login, but this Enzyme home already has {} configured vault(s). Run `enzyme login` and retry.",
        configured_keys.len()
    )
}

/// Returns true when the user has set a non-empty OPENAI_API_KEY (BYOK mode).
///
/// BYOK users pay their own inference costs and do not need an Enzyme account,
/// so the login gate is skipped for them.
fn byok_active() -> bool {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
}

fn catalyst_generation_planned(entities: &[Entity]) -> bool {
    !entities.is_empty()
}

fn reset_vault_database(paths: &EnzymePaths) -> Result<()> {
    if paths.vault_db.exists() {
        fs::remove_file(&paths.vault_db)
            .with_context(|| format!("failed to reset {}", paths.vault_db.display()))?;
        debug!(path = %paths.vault_db.display(), "reset vault database for init");
    }
    Ok(())
}

fn should_preserve_db_only_corpus(paths: &EnzymePaths, vault_path: &Path) -> Result<bool> {
    if !paths.vault_db.exists() {
        return Ok(false);
    }

    // Honor `exclude` here too: "indexable files present" must be computed the
    // same way IndexingPipeline will index, or a vault whose only files live in
    // excluded folders would be judged non-empty and its DB-only corpus wiped.
    let discovered_files = FileDiscovery::new(vault_path.to_path_buf())
        .with_excluded_folders(EnzymeConfig::excluded_folders_at(vault_path))
        .discover()?;
    if !discovered_files.is_empty() {
        return Ok(false);
    }

    let db = Database::open(&paths.vault_db)?;
    Ok(db.get_document_count().unwrap_or(0) > 0)
}

struct LocalRefreshResult {
    indexing: IndexingStats,
    embedding: EmbeddingStats,
    similarity: IncrementalSimilarityStats,
}

impl LocalRefreshResult {
    fn collection_changed(&self) -> bool {
        has_index_changes(&self.indexing)
            || self.embedding.documents_embedded > 0
            || self.embedding.documents_pruned > 0
            || self.similarity.rows_upserted > 0
    }

    fn change_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if has_index_changes(&self.indexing) {
            reasons.push("documents_changed");
        }
        if self.embedding.documents_embedded > 0 || self.embedding.documents_pruned > 0 {
            reasons.push("embeddings_changed");
        }
        if self.similarity.rows_upserted > 0 || self.similarity.obsolete_rows_removed > 0 {
            reasons.push("similarities_incrementally_maintained");
        }
        reasons
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalystQuality {
    Passed,
    Degraded,
    Failed,
}

struct CatalystQualityGate {
    quality: CatalystQuality,
    message: Option<String>,
}

fn evaluate_catalyst_quality(stats: &CatalystStats, total_entities: usize) -> CatalystQualityGate {
    let failed = stats.errors.len();
    let succeeded = stats.entities_processed;
    let attempted = succeeded + failed;

    if stats.catalysts_generated == 0 {
        return CatalystQualityGate {
            quality: CatalystQuality::Failed,
            message: Some(format!(
                "Catalyst generation produced 0 catalysts for {attempted}/{total_entities} attempted entities"
            )),
        };
    }

    if total_entities > 0 && succeeded * 2 < total_entities {
        return CatalystQualityGate {
            quality: CatalystQuality::Failed,
            message: Some(format!(
                "Catalyst generation succeeded for {succeeded}/{total_entities} entities; refusing to mark catalysts fresh"
            )),
        };
    }

    if failed > 0 {
        return CatalystQualityGate {
            quality: CatalystQuality::Degraded,
            message: Some(format!(
                "Catalyst generation completed with {failed} failed entities and {} generated catalysts",
                stats.catalysts_generated
            )),
        };
    }

    CatalystQualityGate {
        quality: CatalystQuality::Passed,
        message: None,
    }
}

// Persisted under the historical key so existing databases remain readable.
// Semantically this is a cache epoch: it bumps when collection-derived caches
// should be invalidated, not when user-visible collection content is versioned.
const COLLECTION_CACHE_EPOCH_CONFIG_KEY: &str = "collection.index_generation";

fn read_collection_cache_epoch(db: &Database) -> Result<u64> {
    Ok(db
        .get_config(COLLECTION_CACHE_EPOCH_CONFIG_KEY)?
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0))
}

fn bump_collection_cache_epoch(db: &Database) -> Result<u64> {
    let next = read_collection_cache_epoch(db)?.saturating_add(1);
    db.set_config(COLLECTION_CACHE_EPOCH_CONFIG_KEY, &next.to_string())?;
    Ok(next)
}

fn maybe_bump_collection_cache_epoch(db: &Database, changed: bool) -> Result<u64> {
    if changed {
        bump_collection_cache_epoch(db)
    } else {
        read_collection_cache_epoch(db)
    }
}

/// Stats collected during an init run, used for JSON output in quiet mode.
pub struct InitResult {
    pub status: CompletionStatus,
    pub files_discovered: usize,
    pub files_new: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub catalysts_generated: usize,
    pub embeddings_documents: Option<usize>,
    pub embeddings_pruned: Option<usize>,
    pub semantic_search: bool,
    pub catalysts_available: bool,
    pub cache_epoch: u64,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

impl InitResult {
    /// Build the JSON output for quiet mode, including petri data.
    pub fn to_json(&self, vault_path: &Path) -> Result<serde_json::Value> {
        let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
        let mut output = json!({
            "status": match self.status {
                CompletionStatus::Done => "completed",
                CompletionStatus::Degraded => "completed_with_warnings",
                CompletionStatus::Failed => "failed",
            },
            "fresh": false,
            "indexing": {
                "files_discovered": self.files_discovered,
                "files_new": self.files_new,
                "files_updated": self.files_updated,
                "files_deleted": self.files_deleted,
            },
            "catalysts_generated": self.catalysts_generated,
            "cache_epoch": self.cache_epoch,
            "capabilities": {
                "semantic_search": self.semantic_search,
                "catalysts": self.catalysts_available,
            },
            "warnings": &self.warnings,
            "duration_ms": self.duration_ms,
            "petri": petri,
        });

        if let (Some(docs), Some(pruned)) = (self.embeddings_documents, self.embeddings_pruned) {
            output["embeddings"] = json!({ "documents": docs, "pruned": pruned });
        }

        Ok(output)
    }
}

/// Run only the indexing stage, print a vault summary, and exit.
///
/// Used by `enzyme init --stage index` to populate the DB and show
/// the user what enzyme found before committing to LLM + catalyst work.
/// Desktop app uses quiet mode (JSON output) to populate its setup text box.
pub async fn run_init_index_only(vault_path: &Path, quiet: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    // Index-only runs do zero LLM/credit work (no catalysts, no Enzyme server
    // calls), so no login is required. Mirrors `catalyze --target` behaviour.

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
    }

    if !should_preserve_db_only_corpus(&paths, vault_path)? {
        reset_vault_database(&paths)?;
    }
    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();

    // Run indexing
    let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
    let idx_stats = indexing.run(true, |_| {})?;

    debug!(
        new = idx_stats.files_new,
        updated = idx_stats.files_updated,
        deleted = idx_stats.files_deleted,
        "index-only: indexing complete"
    );

    // Build and print summary
    let summary = vault_summary::build_summary(&db, vault_path)?;

    if quiet {
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        eprintln!("{}", vault_summary::format_summary_human(&summary));
    }

    Ok(())
}

/// Start a detached full init and return when enough petri context is visible.
///
/// This preserves the normal `enzyme init` contract while adding an earlier
/// readiness contract for voice agents. The detached worker owns the real init
/// pipeline, so LLM work is not duplicated; this foreground process only waits
/// until streamed catalyst records have made a seed petri readable.
pub async fn run_init_voice_ready(
    vault_path: &Path,
    seed_entities: usize,
    seed_min_catalysts: usize,
    timeout: Duration,
    min_top_catalysts: Option<usize>,
    use_env_llm: bool,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    let seed_entities = seed_entities.max(1);
    let seed_min_catalysts = seed_min_catalysts.max(1);
    let paths = EnzymePaths::new(vault_path)?;
    if !use_env_llm {
        require_login_for_hosted_init_if_needed(vault_path, &paths)?;
    }
    preflight_env_llm(use_env_llm).await?;

    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
    }

    let background_spawned =
        spawn_background_init(vault_path, &paths, min_top_catalysts, true, use_env_llm);
    let log_path = paths
        .vault_db
        .parent()
        .map(|dir| dir.join("background_init.log"));

    let mut last_error = "no petri available yet".to_string();
    loop {
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "voice-ready init timed out after {:.1}s waiting for seed petri ({})",
                timeout.as_secs_f64(),
                last_error
            );
        }

        match super::petri::build_petri_json(vault_path, 20, None, None) {
            Ok(petri) => {
                let ready_entities = count_seed_petri_entities(&petri, seed_min_catalysts);
                if ready_entities >= seed_entities {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    return Ok(json!({
                        "status": "partial_ready",
                        "fresh": false,
                        "duration_ms": duration_ms,
                        "background_spawned": background_spawned,
                        "background_log": log_path.as_ref().map(|p| p.display().to_string()),
                        "readiness": {
                            "index": "ready",
                            "petri": "seed_ready",
                            "seed_entities_ready": ready_entities,
                            "seed_entities_required": seed_entities,
                            "seed_min_catalysts_required": seed_min_catalysts,
                            "embeddings": "running",
                            "similarities": "pending",
                            "semantic_search": false,
                            "background": if background_spawned { "spawned" } else { "already_running_or_unavailable" },
                        },
                        "capabilities": {
                            "semantic_search": false,
                            "catalysts": true,
                        },
                        "petri": petri,
                    }));
                }
                last_error = format!(
                    "seed petri has {ready_entities}/{seed_entities} entities with at least {seed_min_catalysts} catalysts"
                );
            }
            Err(e) => {
                last_error = e.to_string();
            }
        }

        if let Some(reason) = background_init_terminal_reason(log_path.as_deref()) {
            anyhow::bail!(
                "voice-ready init failed before seed petri became ready: {} ({})",
                reason,
                last_error
            );
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn count_seed_petri_entities(petri: &serde_json::Value, min_catalysts_per_entity: usize) -> usize {
    petri
        .get("entities")
        .and_then(|v| v.as_array())
        .map(|entities| {
            entities
                .iter()
                .filter(|entity| {
                    entity
                        .get("catalysts")
                        .and_then(|v| v.as_array())
                        .map_or(false, |catalysts| {
                            catalysts.len() >= min_catalysts_per_entity
                        })
                })
                .count()
        })
        .unwrap_or(0)
}

fn background_init_terminal_reason(log_path: Option<&Path>) -> Option<String> {
    let log_path = log_path?;
    let contents = fs::read_to_string(log_path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(line) = trimmed.lines().find(|line| line.starts_with("Error:")) {
        return Some(line.to_string());
    }

    let last_line = trimmed.lines().last()?;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(last_line) {
        if value.get("status").and_then(|v| v.as_str()) == Some("completed") {
            let generated = value
                .get("catalysts_generated")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if generated == 0 {
                return Some("background init completed with 0 generated catalysts".to_string());
            }
        }
    }

    None
}

pub async fn run_init(
    vault_path: &Path,
    detached: bool,
    quiet: bool,
    json_progress: bool,
    min_top_catalysts: Option<usize>,
    stream_partial_catalysts: bool,
    use_env_llm: bool,
) -> Result<Option<InitResult>> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;
    // Local env-var check only — fail an explicit --use-env-llm misconfiguration
    // before the vault database is reset below.
    preflight_env_llm(use_env_llm).await?;

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
    }

    let _detached_lock = if detached {
        let enzyme_dir = paths
            .vault_db
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| vault_path.join(".enzyme"));
        Some(acquire_refresh_lock(&enzyme_dir)?)
    } else {
        None
    };

    if !should_preserve_db_only_corpus(&paths, vault_path)? {
        reset_vault_database(&paths)?;
    }

    // Open database
    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();

    // ── Step 1: Indexing ───────────────────────────────────────────────────
    // Run with animated display, then finish it before the interactive prompt.
    let idx_stats = {
        let idx_display = if json_progress {
            InitDisplay::start_json()
        } else if quiet {
            InitDisplay::start_quiet()
        } else {
            InitDisplay::start()
        };
        let tx = idx_display.sender();

        tx.send(DisplayEvent::IndexingStart).ok();
        let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
        let tx_idx = tx.clone();
        let stats = indexing.run(true, |msg| {
            tx_idx
                .send(DisplayEvent::IndexingProgress(msg.to_string()))
                .ok();
        })?;

        tx.send(DisplayEvent::IndexingComplete {
            files_discovered: stats.files_discovered,
            files_new: stats.files_new,
            files_updated: stats.files_updated,
            files_deleted: stats.files_deleted,
        })
        .ok();
        // Drop all sender clones so the display thread can exit
        drop(tx);
        drop(tx_idx);
        idx_display.finish();
        stats
    };

    // ── Start display for remaining pipeline ────────────────────────────
    let display = if json_progress {
        InitDisplay::start_json()
    } else if quiet {
        InitDisplay::start_quiet()
    } else {
        InitDisplay::start()
    };
    let tx = display.sender();
    let mut warnings: Vec<String> = Vec::new();
    let mut enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();

    // ── Step 2: Entity selection ──────────────────────────────────────────
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    // Only write entities to TOML on first init (shortlist). If the config
    // already has entities, those take precedence and are not overwritten.
    if matches!(
        sel_result.source,
        enzyme_core::pipeline::selection::SelectionSource::Coverage
    ) {
        let expanded_children: std::collections::HashSet<String> = sel_result
            .expandable_folders
            .values()
            .flatten()
            .cloned()
            .collect();
        let selected_entity_refs: Vec<String> = sel_result
            .entities
            .iter()
            .filter(|entity| !expanded_children.contains(&entity.name))
            .map(selected_entity_ref)
            .collect();
        enzyme_config.set_auto_selected_entities(vault_path, selected_entity_refs)?;
    }

    tx.send(DisplayEvent::SelectionComplete {
        count: sel_result.entities.len(),
        source: sel_result.source.clone(),
    })
    .ok();

    // Kept when hosted credential resolution fails, so the skip diagnostic can
    // distinguish a reachable server's structured 402 (credits exhausted) from
    // a genuine network/parse failure.
    let mut hosted_resolve_error: Option<anyhow::Error> = None;
    let api_config = if catalyst_generation_planned(&sel_result.entities) {
        if !use_env_llm && !byok_active() {
            require_login_for_hosted_init_if_needed(vault_path, &paths)?;
        }

        let llm_mode = if use_env_llm {
            LlmCredentialMode::Env
        } else {
            LlmCredentialMode::Hosted
        };
        let resolved = match resolve_llm_client_for_mode(llm_mode).await {
            Ok(resolved) => Some(resolved),
            Err(e) => {
                if use_env_llm {
                    return Err(e)
                        .context("--use-env-llm was set but no env LLM config could be resolved");
                }
                tracing::warn!("API config resolution failed: {}", e);
                hosted_resolve_error = Some(e);
                None
            }
        };

        if let Some((ref cfg, _)) = resolved {
            if matches!(cfg.provider, enzyme_core::llm::ConfigProvider::Env) {
                tx.send(DisplayEvent::ApiConfig {
                    base_url: cfg.base_url.clone(),
                    model: cfg.model.clone(),
                    masked_key: cfg.masked_api_key(),
                })
                .ok();
            }
        }

        // The 3-credit hosted deduction happens AFTER generation succeeds (see
        // below): a run that produces no catalysts must net zero credits, and
        // no refund endpoint exists to compensate an up-front charge.
        resolved
    } else {
        debug!("init selected no entities; skipping LLM credential resolution");
        None
    };

    // ── Step 2b: Build XML context from DB ───────────────────────────────
    let vault_context = {
        let ctx_builder = ContextBuilder::new(&db);
        match ctx_builder.build(&sel_result.entities, 5, 300) {
            Ok(xml) => {
                debug!("built vault context XML ({} bytes)", xml.len());
                Some(xml)
            }
            Err(e) => {
                tracing::warn!("failed to build vault context: {}", e);
                None
            }
        }
    };

    // ── Step 3: Resolve resources upfront ─────────────────────────────────
    let service = match get_embedding_service(&paths.home) {
        Ok(svc) => Some(svc),
        Err(e) => {
            let reason = format!("model not found: {}", e);
            tx.send(DisplayEvent::EmbeddingSkipped(reason.clone())).ok();
            warnings.push(format!("Embedding skipped: {}", reason));
            None
        }
    };

    // ── Step 4: Launch doc embedding in background ────────────────────────
    let embedding_handle = if let Some(svc) = service {
        let db_path = paths.vault_db.clone();
        let tx_emb = tx.clone();
        let max_embed = enzyme_config.effective_max_embedding_files();
        tx.send(DisplayEvent::EmbeddingStart).ok();
        Some(tokio::task::spawn_blocking(
            move || -> anyhow::Result<EmbeddingStats> {
                let bg_db = Database::open(&db_path)?;
                let pipeline = EmbeddingPipeline::new(&bg_db, svc);
                let stats = pipeline.run_with_limit(max_embed, |p| {
                    tx_emb
                        .send(DisplayEvent::EmbeddingProgress {
                            done: p.done,
                            total: p.total,
                            current_date_ms: p.current_date_ms,
                        })
                        .ok();
                })?;
                // Notify display immediately so the finished state renders
                // above the entity tree while discovery is still running.
                tx_emb
                    .send(DisplayEvent::EmbeddingComplete {
                        chunks: stats.chunks_embedded,
                        pruned: stats.documents_pruned,
                        oldest_date_ms: stats.oldest_embedded_ms,
                        encode_ms: stats.encode_ms,
                    })
                    .ok();
                Ok(stats)
            },
        ))
    } else {
        None
    };

    // ── Step 5: Catalyst generation in foreground ─────────────────────────
    // Persist --min-top-catalysts to config so refreshes respect it
    if let Some(mtc) = min_top_catalysts {
        if let Err(e) = enzyme_config.persist_min_top_catalysts(vault_path, mtc) {
            tracing::warn!("failed to persist min_top_catalysts: {}", e);
        }
    }

    let cat_stats = if let Some((resolved, llm)) = api_config {
        let total = sel_result.entities.len();
        tx.send(DisplayEvent::DiscoveryStart { total }).ok();

        let cat_pipeline = CatalystPipeline::new(&db, &llm, service);
        let tx_cat = tx.clone();

        let effective_min_top = min_top_catalysts.or_else(|| {
            enzyme_config
                .trending
                .as_ref()
                .and_then(|t| t.min_top_catalysts)
        });
        let allocations = calculate_allocations(&sel_result.entities, effective_min_top);

        let cat_config = CatalystConfig {
            entity_profiles: &sel_result.entity_profiles,
            catalysts_per_entity: 7,
            default_profile: DEFAULT_PROFILE_KEY,
            format_key: enzyme_config
                .catalyst_format
                .as_deref()
                .unwrap_or(DEFAULT_FORMAT_KEY),
            prompt_context: vault_context.as_deref(),
            model: &resolved.model,
            per_entity_allocations: Some(&allocations),
            max_context_chars: std::env::var("ENZYME_MAX_CONTEXT_CHARS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            max_samples: std::env::var("ENZYME_MAX_CONTEXT_SAMPLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            concurrency: std::env::var("ENZYME_LLM_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse().ok()),
            stream_partial_catalysts,
        };

        let stats = cat_pipeline
            .run(
                &sel_result.entities,
                &cat_config,
                move |event| match event {
                    CatalystEvent::EntityCompleted {
                        entity_name,
                        entity_type,
                        catalyst_texts,
                        topic_names,
                        completed,
                        total,
                    } => {
                        tx_cat
                            .send(DisplayEvent::EntityDiscovered {
                                entity_name,
                                entity_type: parse_entity_type(&entity_type),
                                catalyst_texts,
                                topic_names,
                                completed,
                                total,
                            })
                            .ok();
                    }
                    CatalystEvent::EntitySkipped {
                        entity_name,
                        completed,
                        total,
                    } => {
                        tx_cat
                            .send(DisplayEvent::EntitySkipped {
                                entity_name,
                                completed,
                                total,
                            })
                            .ok();
                    }
                    CatalystEvent::EntityFailed {
                        entity_name,
                        entity_type,
                        error,
                        completed,
                        total,
                    } => {
                        tx_cat
                            .send(DisplayEvent::EntityFailed {
                                entity_name,
                                entity_type: parse_entity_type(&entity_type),
                                error,
                                completed,
                                total,
                            })
                            .ok();
                    }
                    CatalystEvent::EntityStarted {
                        entity_name,
                        entity_type,
                    } => {
                        tx_cat
                            .send(DisplayEvent::EntityStarted {
                                entity_name,
                                entity_type: parse_entity_type(&entity_type),
                            })
                            .ok();
                    }
                },
            )
            .await?;

        tx.send(DisplayEvent::DiscoveryComplete {
            entities_processed: stats.entities_processed,
            catalysts_generated: stats.catalysts_generated,
            errors_count: stats.errors.len(),
            rate_limited: stats.rate_limited,
        })
        .ok();

        // Post-success deduction: hosted runs are billed 3 credits only after
        // generation actually produced catalysts (defer-until-success — no
        // refund endpoint exists, so nothing may be charged up front). BYOK
        // (`ConfigProvider::Env`) pays its own inference and is never billed.
        if stats.catalysts_generated > 0
            && matches!(
                resolved.provider,
                enzyme_core::llm::ConfigProvider::ServerFallback
            )
        {
            let auth_path = paths.auth_token();
            if let Ok(token) = enzyme_core::config::auth::AuthToken::load(&auth_path) {
                let remaining =
                    enzyme_core::config::credits::deduct_credits(&token.api_key, 3.0, "init")
                        .await?;
                if remaining <= 3.0 && !quiet {
                    eprintln!(
                        "  {}{:.1} credits remaining — visit app.enzyme.garden/plans{}",
                        crossterm::style::SetForegroundColor(crossterm::style::Color::Yellow),
                        remaining,
                        crossterm::style::ResetColor,
                    );
                }
            } else {
                debug!("hosted init has no login token; skipping account credit deduction");
            }
        }

        Some(stats)
    } else {
        // A structured 402 from the hosted free-config fetch means the server
        // was reachable and refused for credits — say so (this is the one
        // place top-up guidance belongs). Anything else keeps the catch-all.
        let msg = match hosted_resolve_error
            .as_ref()
            .and_then(|e| e.downcast_ref::<enzyme_core::llm::EnzymeCreditsExhausted>())
        {
            Some(credits_err) => format!("{credits_err}. Skipping catalyst generation."),
            None => "No API key found and Enzyme server unreachable — skipping catalyst generation. Set OPENAI_API_KEY or ensure network access.".to_string(),
        };
        tx.send(DisplayEvent::ApiSkipped(msg.clone())).ok();
        warnings.push(msg);
        None
    };
    let catalyst_gate = cat_stats
        .as_ref()
        .map(|stats| evaluate_catalyst_quality(stats, sel_result.entities.len()));
    if let Some(gate) = &catalyst_gate {
        match gate.quality {
            CatalystQuality::Passed => {
                info!(
                    catalysts = cat_stats.as_ref().map_or(0, |s| s.catalysts_generated),
                    "init stage=catalysts status=ok"
                );
            }
            CatalystQuality::Degraded => {
                if let Some(message) = &gate.message {
                    warnings.push(message.clone());
                    tracing::warn!(message = %message, "init stage=catalysts status=degraded");
                }
            }
            CatalystQuality::Failed => {
                if let Some(message) = &gate.message {
                    warnings.push(message.clone());
                    tracing::warn!(message = %message, "init stage=catalysts status=failed");
                }
            }
        }
    }

    // ── Step 6: Wait for background embedding ─────────────────────────────
    // EmbeddingComplete was already sent from inside the spawn_blocking task,
    // so we just collect the stats here.
    let emb_stats = if let Some(handle) = embedding_handle {
        match handle.await.context("embedding task panicked")? {
            Ok(stats) => Some((stats.documents_embedded, stats.documents_pruned)),
            Err(e) => {
                let msg = format!("Embedding failed: {}", e);
                tx.send(DisplayEvent::EmbeddingFailed(msg.clone())).ok();
                warnings.push(msg);
                None
            }
        }
    } else {
        None
    };
    let emb_completed = emb_stats.is_some();

    // ── Step 7: Similarity computation ────────────────────────────────────
    let mut similarities_recomputed = false;
    if emb_completed
        && cat_stats
            .as_ref()
            .map_or(false, |s| s.catalysts_generated > 0)
        && catalyst_gate
            .as_ref()
            .map_or(false, |g| g.quality != CatalystQuality::Failed)
    {
        tx.send(DisplayEvent::SimilarityStart).ok();
        let svc = service.unwrap(); // safe: emb_completed implies service was Some
        // Reopen after the background embedding connection commits so similarity
        // reads the complete current embedding set.
        let sim_db = Database::open(&paths.vault_db)?;
        let sim_pipeline = SimilarityPipeline::new(&sim_db, svc);
        let tx_sim = tx.clone();
        let sim_stats = sim_pipeline.run(|done, total| {
            tx_sim
                .send(DisplayEvent::SimilarityProgress { done, total })
                .ok();
        })?;

        tx.send(DisplayEvent::SimilarityComplete {
            catalysts: sim_stats.catalysts_processed,
            documents: sim_stats.documents_compared,
        })
        .ok();
        similarities_recomputed = true;
    }

    if catalyst_gate
        .as_ref()
        .map_or(false, |g| g.quality == CatalystQuality::Passed)
    {
        let entity_names: Vec<String> =
            sel_result.entities.iter().map(|e| e.name.clone()).collect();
        if let Err(e) = store_staleness_bookkeeping(&db, vault_path, &entity_names) {
            tracing::warn!("failed to store catalyst staleness bookkeeping: {}", e);
        }
    } else if cat_stats.is_some() {
        tracing::warn!("init stage=staleness status=skipped reason=catalyst_quality_not_clean");
    }

    // A successful initial build becomes the first durable catalyst epoch.
    // Legacy catalyst tables remain the active materialized view.
    if similarities_recomputed
        && catalyst_gate
            .as_ref()
            .is_some_and(|gate| gate.quality == CatalystQuality::Passed)
    {
        db.reconcile_corpus_generation(now_ms())?;
        let contract = generation_contract(vault_path, &paths, use_env_llm).await?;
        db.bootstrap_legacy_epoch(&contract, now_ms(), true)?;
    }

    // ── Step 8: Refresh prepared targets ──────────────────────────────────
    if cat_stats
        .as_ref()
        .map_or(false, |s| s.catalysts_generated > 0)
        && catalyst_gate
            .as_ref()
            .map_or(false, |g| g.quality != CatalystQuality::Failed)
    {
        refresh_targets(vault_path).await;
    }

    let retrieval_changed = has_index_changes(&idx_stats)
        || emb_completed
        || similarities_recomputed
        || catalyst_gate
            .as_ref()
            .map_or(false, |g| g.quality != CatalystQuality::Failed);
    let cache_epoch = maybe_bump_collection_cache_epoch(&db, retrieval_changed)?;

    let duration_ms = start.elapsed().as_millis() as u64;
    let completion_status = match catalyst_gate.as_ref().map(|g| g.quality) {
        Some(CatalystQuality::Failed) => CompletionStatus::Failed,
        Some(CatalystQuality::Degraded) => CompletionStatus::Degraded,
        _ if !warnings.is_empty() => CompletionStatus::Degraded,
        _ => CompletionStatus::Done,
    };
    let completion_message = match completion_status {
        CompletionStatus::Failed | CompletionStatus::Degraded => warnings.last().cloned(),
        CompletionStatus::Done => None,
    };
    tracing::info!(
        status = ?completion_status,
        warnings = warnings.len(),
        catalysts = cat_stats.as_ref().map_or(0, |s| s.catalysts_generated),
        catalyst_errors = cat_stats.as_ref().map_or(0, |s| s.errors.len()),
        duration_ms,
        "init complete"
    );
    tx.send(DisplayEvent::Done {
        duration_ms,
        status: completion_status,
        message: completion_message.clone(),
    })
    .ok();

    // Wait for display thread to finish rendering
    display.finish();

    if completion_status == CompletionStatus::Failed {
        anyhow::bail!(
            "{}",
            completion_message.unwrap_or_else(|| "init failed quality checks".to_string())
        );
    }

    // Show starter prompt on interactive init
    if !quiet && !json_progress {
        if let Ok(config_path) = EnzymeConfig::config_path() {
            eprintln!("  Config saved to {}", config_path.display());
            eprintln!();
        }
        eprintln!();
        eprintln!(
            "  {}Try:{} claude {}\"explore with enzyme and tell me what i'm not seeing\"{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::White),
            crossterm::style::ResetColor,
            crossterm::style::SetForegroundColor(crate::display::colors::ENZYME_PURPLE),
            crossterm::style::ResetColor,
        );
        eprintln!();
    }

    if quiet || json_progress {
        Ok(Some(InitResult {
            status: completion_status,
            files_discovered: idx_stats.files_discovered,
            files_new: idx_stats.files_new,
            files_updated: idx_stats.files_updated,
            files_deleted: idx_stats.files_deleted,
            catalysts_generated: cat_stats.as_ref().map_or(0, |s| s.catalysts_generated),
            embeddings_documents: emb_stats.map(|(docs, _)| docs),
            embeddings_pruned: emb_stats.map(|(_, pruned)| pruned),
            semantic_search: emb_completed,
            catalysts_available: cat_stats.is_some(),
            cache_epoch,
            warnings,
            duration_ms,
        }))
    } else {
        Ok(None)
    }
}

/// Ensure the vault is fresh — check staleness, run init if needed.
/// Does not print anything to stdout (safe for MCP/programmatic use).
pub async fn ensure_fresh_vault(vault_path: &Path) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;
    let local = run_local_refresh_work(&db, vault_path, &paths)?;
    db.reconcile_corpus_generation(now_ms())?;
    let contract = generation_contract(vault_path, &paths, false).await?;
    db.bootstrap_legacy_epoch(&contract, now_ms(), false)?;
    recover_abandoned_epoch_worker(&db, &paths)?;
    let schedule = db.schedule_catalyst_epoch(now_ms(), &contract, false)?;
    if schedule.launch_needed {
        let _ = spawn_background_refresh(vault_path, &paths, false);
    }
    let _ = maybe_bump_collection_cache_epoch(&db, local.collection_changed())?;
    Ok(())
}

/// Lightweight refresh: dispatches to the appropriate path based on flags.
///
/// - `--detached` → `run_refresh_detached` (background lockfile-guarded catalyst run)
/// - `--quiet` → `run_refresh_fast` (sync hot path for hooks)
/// - Otherwise → interactive incremental refresh
pub async fn run_refresh(
    vault_path: &Path,
    detached: bool,
    quiet: bool,
    min_top_catalysts: Option<usize>,
    use_env_llm: bool,
    force_full: bool,
) -> Result<()> {
    // Local env-var check only — a misconfigured --use-env-llm should fail
    // here rather than inside a detached worker whose errors only reach a log.
    preflight_env_llm(use_env_llm).await?;

    if detached {
        return run_refresh_detached(vault_path, use_env_llm).await;
    }
    if quiet {
        return run_refresh_fast(vault_path, use_env_llm, force_full).await;
    }
    run_refresh_interactive(vault_path, min_top_catalysts, use_env_llm, force_full).await
}

/// Fast sync path — the hook hot path for `enzyme refresh --quiet`.
///
/// Runs only cheap local work (indexing, embedding, similarity) synchronously,
/// then spawns a background process for expensive LLM catalyst generation if
/// the vault is stale.
async fn run_refresh_fast(vault_path: &Path, use_env_llm: bool, force_full: bool) -> Result<()> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    // If the vault was never initialized, don't create .enzyme — just bail.
    if !paths.vault_db.exists() {
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();
    let local = run_local_refresh_work(&db, vault_path, &paths)?;
    let collection_changed = local.collection_changed();
    let collection_change_reasons = local.change_reasons();
    db.reconcile_corpus_generation(now_ms())?;
    let contract = generation_contract(vault_path, &paths, use_env_llm).await?;
    db.bootstrap_legacy_epoch(&contract, now_ms(), false)?;
    recover_abandoned_epoch_worker(&db, &paths)?;
    let schedule = db.schedule_catalyst_epoch(now_ms(), &contract, force_full)?;
    let background_spawned =
        schedule.launch_needed && spawn_background_refresh(vault_path, &paths, use_env_llm);
    let cache_epoch = maybe_bump_collection_cache_epoch(&db, collection_changed)?;

    // ── Step 5: Output JSON ─────────────────────────────────────────────
    let duration_ms = start.elapsed().as_millis() as u64;
    let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
    let output = json!({
        "status": "completed",
        "fresh": true,
        "background_spawned": background_spawned,
        "embedding_changed": local.embedding.documents_embedded > 0 || local.embedding.documents_pruned > 0,
        "similarities_recomputed": false,
        "embedding": {
            "documents_embedded": local.embedding.documents_embedded,
            "chunks_embedded": local.embedding.chunks_embedded,
            "documents_pruned": local.embedding.documents_pruned,
            "documents_skipped": local.embedding.documents_skipped,
            "duration_ms": local.embedding.duration_ms,
            "encode_ms": local.embedding.encode_ms,
        },
        "similarity": local.similarity,
        "epoch": epoch_metrics(&db, &schedule, now_ms())?,
        "cache_epoch": cache_epoch,
        "collection_changed": collection_changed,
        "collection_change_reasons": collection_change_reasons,
        "indexing": {
            "files_discovered": local.indexing.files_discovered,
            "files_new": local.indexing.files_new,
            "files_updated": local.indexing.files_updated,
            "files_deleted": local.indexing.files_deleted,
        },
        "duration_ms": duration_ms,
        "petri": petri,
    });
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}

fn run_local_refresh_work(
    db: &Database,
    vault_path: &Path,
    paths: &EnzymePaths,
) -> Result<LocalRefreshResult> {
    let _projection_lock =
        acquire_projection_lock(paths.vault_db.parent().context("vault DB has no parent")?)?;
    let recovery_required = db.continuous_projection_fence()?.dirty;
    // This marker is committed before any document/embedding mutation. If the
    // process exits between the independently committed pipeline phases, the
    // next invocation repairs the complete active top-K materialization.
    let projection_token = db.begin_continuous_projection()?;
    let before_embeddings = db.get_all_chunk_embeddings()?;
    let index_start_ms = chrono::Utc::now().timestamp_millis();
    let indexing = IndexingPipeline::new(db, vault_path.to_path_buf());
    let idx_stats = indexing.run(false, |_msg| {})?;
    debug!(
        new = idx_stats.files_new,
        updated = idx_stats.files_updated,
        deleted = idx_stats.files_deleted,
        "fast refresh: indexing complete"
    );

    let service = get_embedding_service(&paths.home).ok();
    let embedding =
        refresh_changed_doc_embeddings(db, service, index_start_ms, has_index_changes(&idx_stats))?;

    let after_embeddings = db.get_all_chunk_embeddings()?;
    let similarity = if recovery_required && database_has_catalysts(db) {
        let service = service.context("embedding service unavailable for similarity recovery")?;
        let repaired = SimilarityPipeline::new(db, service).run(|_, _| {})?;
        IncrementalSimilarityStats {
            candidate_scores: repaired.similarities_computed,
            rows_upserted: repaired.similarities_computed,
            full_repair: true,
            duration_ms: repaired.duration_ms,
            ..Default::default()
        }
    } else if before_embeddings != after_embeddings && database_has_catalysts(db) {
        let service =
            service.context("embedding service unavailable for similarity maintenance")?;
        SimilarityPipeline::new(db, service)
            .maintain_incremental(&before_embeddings, &after_embeddings)?
    } else {
        IncrementalSimilarityStats::default()
    };

    db.finish_continuous_projection(&projection_token)?;
    Ok(LocalRefreshResult {
        indexing: idx_stats,
        embedding,
        similarity,
    })
}

fn has_index_changes(stats: &IndexingStats) -> bool {
    stats.files_new > 0 || stats.files_updated > 0 || stats.files_deleted > 0
}

pub(super) async fn generation_contract(
    vault_path: &Path,
    paths: &EnzymePaths,
    use_env_llm: bool,
) -> Result<GenerationContract> {
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let service = get_embedding_service(&paths.home)?;
    let mode = if use_env_llm {
        LlmCredentialMode::Env
    } else {
        LlmCredentialMode::Hosted
    };
    let (resolved_llm, _) = resolve_llm_client_for_mode(mode).await?;
    let model = resolved_llm.model;
    let llm_mode = if use_env_llm { "env" } else { "hosted" }.to_string();
    let llm_provider = match resolved_llm.provider {
        enzyme_core::llm::ConfigProvider::Env => "env",
        enzyme_core::llm::ConfigProvider::ServerFallback => "server-fallback",
    }
    .to_string();
    let llm_base_url = resolved_llm.base_url;
    let contract_db = paths
        .vault_db
        .exists()
        .then(|| Database::open(&paths.vault_db))
        .transpose()?;
    let targets = epoch_targets(&config, contract_db.as_ref())?;
    let mut catalyst_config = config.clone();
    // Embedding batching is a continuous-projection concern, not an input to
    // catalyst interpretation, so it is deliberately excluded from the epoch
    // contract fingerprint.
    catalyst_config.max_embedding_files = None;
    let selection_config_json = serde_json::to_string(&catalyst_config)?;
    Ok(GenerationContract {
        schema_version: "catalyst-epoch.v1".to_string(),
        prompt_version: "catalyst-prompts.2026-07".to_string(),
        catalyst_format: config
            .catalyst_format
            .clone()
            .unwrap_or_else(|| DEFAULT_FORMAT_KEY.to_string()),
        catalyst_profile: DEFAULT_PROFILE_KEY.to_string(),
        model,
        llm_mode,
        llm_provider,
        llm_base_url,
        allocation_semantics: "volume-floor-and-top-boost.v1".to_string(),
        catalysts_per_entity: 7,
        min_top_catalysts: config.trending.as_ref().and_then(|t| t.min_top_catalysts),
        embedding_backend: service.backend_name().to_string(),
        embedding_dimension: service.dimension(),
        targets,
        selection_config_json,
        max_context_chars: std::env::var("ENZYME_MAX_CONTEXT_CHARS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        max_context_samples: std::env::var("ENZYME_MAX_CONTEXT_SAMPLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    })
}

fn epoch_targets(config: &EnzymeConfig, db: Option<&Database>) -> Result<Vec<String>> {
    let mut targets = config.targets.clone();
    if let Some(db) = db {
        targets.extend(
            db.get_all_applied_targets()?
                .into_iter()
                .map(|target| target.target_path)
                .filter(|target| std::path::Path::new(target).exists()),
        );
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub(super) fn recover_abandoned_epoch_worker(db: &Database, paths: &EnzymePaths) -> Result<()> {
    let enzyme_dir = paths.vault_db.parent().context("vault DB has no parent")?;
    if !is_refresh_locked(enzyme_dir) {
        db.recover_abandoned_epoch_build()?;
    }
    Ok(())
}

fn epoch_metrics(db: &Database, schedule: &EpochSchedule, now: i64) -> Result<serde_json::Value> {
    let state = db.get_epoch_state()?;
    let age_ms = state
        .active_epoch_completed_at_ms
        .map(|completed| now.saturating_sub(completed));
    Ok(json!({
        "active_epoch_id": state.active_epoch_id,
        "age_ms": age_ms,
        "cadence_ms": EPOCH_CADENCE_MS,
        "corpus_generation": state.corpus_generation,
        "active_epoch_corpus_generation": state.active_epoch_corpus_generation,
        "corpus_changed_since_epoch": state.active_epoch_corpus_generation != Some(state.corpus_generation),
        "due": schedule.due,
        "scheduled": schedule.scheduled,
        "worker_launch_needed": schedule.launch_needed,
        "coalesced": schedule.coalesced,
        "build_epoch_id": schedule.epoch_id,
        "due_reason": schedule.reason.map(|reason| format!("{reason:?}").to_lowercase()),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingRefreshPlan {
    Backlog,
    SinceIndexStart,
}

fn embedding_refresh_plan(has_index_changes: bool) -> EmbeddingRefreshPlan {
    if has_index_changes {
        EmbeddingRefreshPlan::SinceIndexStart
    } else {
        EmbeddingRefreshPlan::Backlog
    }
}

fn refresh_changed_doc_embeddings(
    db: &Database,
    service: Option<&EmbeddingService>,
    index_start_ms: i64,
    has_index_changes: bool,
) -> Result<EmbeddingStats> {
    let Some(service) = service else {
        anyhow::bail!(
            "embedding service unavailable; cannot provide continuous projection freshness"
        );
    };

    let pipeline = EmbeddingPipeline::new(db, service);
    let result = match embedding_refresh_plan(has_index_changes) {
        EmbeddingRefreshPlan::Backlog => pipeline.run_with_limit(None, |_| {}),
        EmbeddingRefreshPlan::SinceIndexStart => {
            // Continuous projection may not defer part of a changed batch.
            pipeline.run_since_with_limit(index_start_ms, None, |_| {})
        }
    };
    match result {
        Ok(stats) => {
            let changed = stats.documents_embedded > 0 || stats.documents_pruned > 0;
            debug!(
                docs = stats.documents_embedded,
                pruned = stats.documents_pruned,
                changed,
                "fast refresh: embedding complete"
            );
            Ok(stats)
        }
        Err(e) => Err(e).context("continuous document embedding refresh failed"),
    }
}

fn database_has_catalysts(db: &Database) -> bool {
    db.query_read("SELECT COUNT(*) FROM catalysts", vec![], |row| {
        row.get::<i64>(0)
    })
    .map(|rows| rows.first().copied().unwrap_or(0) > 0)
    .unwrap_or(false)
}

/// Detached background path — runs the expensive LLM catalyst pipeline.
///
/// Called by `spawn_background_refresh` via `enzyme refresh --detached --quiet`.
/// Acquires a lockfile to prevent concurrent runs.
async fn run_refresh_detached(vault_path: &Path, use_env_llm: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    let enzyme_dir = paths
        .vault_db
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| vault_path.join(".enzyme"));

    // ── Step 1: Acquire lockfile ────────────────────────────────────────
    let _lock = match acquire_refresh_lock(&enzyme_dir) {
        Ok(lock) => lock,
        Err(e) => {
            info!("detached refresh: lock held, exiting: {}", e);
            return Ok(());
        }
    };

    // The parent refresh that spawned this worker may still hold its own
    // connection while it finishes reporting; opening reruns the schema DDL
    // batch, which can lose that lock race and die before claiming the
    // epoch. Retry briefly instead of giving up the whole build.
    let db = open_vault_db_with_retry(&paths.vault_db)?;
    let Some(claim) = db.claim_scheduled_epoch(now_ms())? else {
        info!("detached refresh: no scheduled catalyst epoch");
        return Ok(());
    };
    let epoch_id = claim.epoch_id.clone();
    if let Err(error) = build_and_activate_epoch(vault_path, &paths, &db, claim, use_env_llm).await
    {
        db.mark_epoch_failed(&epoch_id, now_ms(), &format!("{error:#}"))?;
        return Err(error);
    }
    // Lock released on drop
    Ok(())
}

async fn build_and_activate_epoch(
    vault_path: &Path,
    paths: &EnzymePaths,
    active_db: &Database,
    mut claim: enzyme_core::pipeline::epoch::EpochClaim,
    _use_env_llm: bool,
) -> Result<()> {
    let contract: GenerationContract = serde_json::from_str(&claim.generation_contract_json)
        .context("decode frozen generation contract")?;
    let epoch_dir = paths.vault_db.parent().unwrap_or(vault_path).join("epochs");
    fs::create_dir_all(&epoch_dir)?;
    let snapshot_path = claim
        .snapshot_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| epoch_dir.join(format!("{}-source.db", claim.epoch_id)));
    {
        let _snapshot_lock =
            acquire_projection_lock(paths.vault_db.parent().context("vault DB has no parent")?)?;
        anyhow::ensure!(
            !active_db.continuous_projection_fence()?.dirty,
            "continuous projection needs recovery before catalyst snapshot"
        );
        if claim.snapshot_path.is_none() {
            let latest = active_db.reconcile_corpus_generation(now_ms())?;
            active_db.update_unsnapshotted_epoch_corpus(
                &claim.epoch_id,
                latest.corpus_generation,
                &latest.corpus_fingerprint,
            )?;
            claim.corpus_generation = latest.corpus_generation;
            claim.corpus_fingerprint = latest.corpus_fingerprint;
            if snapshot_path.exists() {
                let orphan = Database::open(&snapshot_path)?;
                if enzyme_core::pipeline::epoch::corpus_fingerprint(&orphan)?
                    != claim.corpus_fingerprint
                {
                    drop(orphan);
                    fs::remove_file(&snapshot_path)?;
                }
            }
            if !snapshot_path.exists() {
                active_db.snapshot_to(&snapshot_path)?;
            }
        }
        anyhow::ensure!(snapshot_path.exists(), "recorded epoch snapshot is missing");
        let snapshot = Database::open(&snapshot_path)?;
        anyhow::ensure!(
            enzyme_core::pipeline::epoch::corpus_fingerprint(&snapshot)?
                == claim.corpus_fingerprint,
            "epoch snapshot corpus does not match its durable generation"
        );
        if claim.snapshot_path.is_none() {
            active_db.record_epoch_snapshot(&claim.epoch_id, &snapshot_path.to_string_lossy())?;
            claim.snapshot_path = Some(snapshot_path.to_string_lossy().to_string());
        }
    }
    let frozen_db = Database::open(&snapshot_path)?;
    let shadow_path = epoch_dir.join(format!("{}-shadow.db", claim.epoch_id));
    frozen_db.snapshot_to(&shadow_path)?;
    let db = Database::open(&shadow_path)?;

    // The snapshot freezes docs, chunks, entities, embeddings, and config
    // contract. Remove only the old active catalyst view before constructing a
    // complete replacement.
    db.execute_write("DELETE FROM catalyst_similarities", vec![])?;
    db.execute_write("DELETE FROM catalyst_embeddings_packed", vec![])?;
    db.execute_write("DELETE FROM catalysts", vec![])?;

    let selection = SelectionPipeline::new(&db, vault_path);
    let frozen_config: EnzymeConfig = serde_json::from_str(&contract.selection_config_json)
        .context("decode frozen vault selection config")?;
    let sel_result = selection.run_with_config(Some(&frozen_config))?;
    if sel_result.entities.is_empty() {
        anyhow::bail!("catalyst epoch has no selected entities");
    }

    let llm_mode = if contract.llm_mode == "env" {
        LlmCredentialMode::Env
    } else {
        LlmCredentialMode::Hosted
    };
    let (api_config, llm) = resolve_llm_client_for_mode(llm_mode).await?;
    let resolved_provider = match &api_config.provider {
        enzyme_core::llm::ConfigProvider::Env => "env",
        enzyme_core::llm::ConfigProvider::ServerFallback => "server-fallback",
    };
    anyhow::ensure!(
        api_config.base_url == contract.llm_base_url
            && api_config.model == contract.model
            && resolved_provider == contract.llm_provider,
        "resolved LLM configuration changed after epoch scheduling"
    );

    let svc = get_embedding_service(&paths.home)?;
    anyhow::ensure!(
        svc.backend_name() == contract.embedding_backend
            && svc.dimension() == contract.embedding_dimension,
        "embedding backend changed after epoch scheduling"
    );

    let cat_pipeline = CatalystPipeline::new(&db, &llm, Some(svc));

    let vault_context = {
        let ctx_builder = ContextBuilder::new(&db);
        ctx_builder.build(&sel_result.entities, 5, 300).ok()
    };
    let allocations = calculate_allocations(&sel_result.entities, contract.min_top_catalysts);

    let cat_config = CatalystConfig {
        entity_profiles: &sel_result.entity_profiles,
        catalysts_per_entity: contract.catalysts_per_entity,
        default_profile: &contract.catalyst_profile,
        format_key: &contract.catalyst_format,
        prompt_context: vault_context.as_deref(),
        model: &contract.model,
        per_entity_allocations: Some(&allocations),
        max_context_chars: contract.max_context_chars,
        max_samples: contract.max_context_samples,
        concurrency: std::env::var("ENZYME_LLM_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok()),
        stream_partial_catalysts: false,
    };

    let stats = cat_pipeline
        .run(&sel_result.entities, &cat_config, |_event| {})
        .await?;
    let catalyst_gate = evaluate_catalyst_quality(&stats, sel_result.entities.len());

    info!(
        entities = stats.entities_processed,
        catalysts = stats.catalysts_generated,
        errors = stats.errors.len(),
        quality = ?catalyst_gate.quality,
        "catalyst refresh: generation complete"
    );

    // A total or majority-failed run keeps the active epoch untouched: don't
    // paper over a broken generation by carrying everything forward and stamping
    // a fresh epoch (which would falsely reset staleness). Only Passed/Degraded
    // runs proceed to per-entity gap-filling below.
    if catalyst_gate.quality == CatalystQuality::Failed {
        let message = catalyst_gate
            .message
            .unwrap_or_else(|| "catalyst refresh failed quality checks".to_string());
        tracing::warn!(message = %message, "refresh stage=catalysts status=failed");
        anyhow::bail!("{message}");
    }

    // Post-success deduction: bill the 1-credit hosted refresh only after
    // generation produced catalysts (defer-until-success; a refresh that
    // generated nothing must net zero credits). BYOK refreshes skip billing.
    if stats.catalysts_generated > 0
        && matches!(
            &api_config.provider,
            enzyme_core::llm::ConfigProvider::ServerFallback
        )
    {
        let auth_home = EnzymePaths::enzyme_home()?;
        let auth_path = auth_home.join("auth.json");
        if let Ok(token) = enzyme_core::config::auth::AuthToken::load(&auth_path) {
            let remaining =
                enzyme_core::config::credits::deduct_credits(&token.api_key, 1.0, "refresh")
                    .await?;
            if remaining <= 3.0 {
                eprintln!(
                    "  {}{:.1} credits remaining — visit app.enzyme.garden/plans{}",
                    crossterm::style::SetForegroundColor(crossterm::style::Color::Yellow),
                    remaining,
                    crossterm::style::ResetColor,
                );
            }
        } else {
            // Loginless bootstrap configs are gated server-side; mirror the
            // init credit handling instead of hard-failing here.
            debug!("hosted refresh has no login token; skipping account credit deduction");
        }
    }

    // Carry forward prior catalysts for any selected entity that generated none
    // this epoch. The shadow began from the frozen snapshot with catalysts wiped,
    // so we re-populate only the per-entity gaps from the currently-active epoch.
    // This keeps one flaky entity from either blocking the whole refresh or
    // dropping that entity to zero coverage. Similarity re-embeds every shadow
    // catalyst below, so carried-forward rows become first-class members of the
    // new epoch (their stale `generated_at` marks the provenance).
    let mut covered: std::collections::HashSet<String> = db
        .get_all_catalysts()?
        .into_iter()
        .map(|c| c.entity)
        .collect();
    let mut carried_entities = 0usize;
    let mut carried_catalysts = 0usize;
    for entity in &sel_result.entities {
        if covered.contains(&entity.name) {
            continue;
        }
        let prior = active_db.get_catalysts_for_entity(&entity.name)?;
        if prior.is_empty() {
            continue;
        }
        carried_entities += 1;
        carried_catalysts += prior.len();
        db.insert_catalysts(&prior)?;
        covered.insert(entity.name.clone());
        tracing::warn!(
            entity = %entity.name,
            count = prior.len(),
            "refresh stage=catalysts carried forward prior catalysts for entity that generated none"
        );
    }

    // After carry-forward, every selected entity must have catalysts. An entity
    // still uncovered generated nothing and had none to reuse (e.g. newly added
    // to config), so the shadow is genuinely incomplete and the active epoch is
    // kept.
    let uncovered: Vec<&str> = sel_result
        .entities
        .iter()
        .map(|e| e.name.as_str())
        .filter(|name| !covered.contains(*name))
        .collect();
    if !uncovered.is_empty() {
        let message = format!(
            "catalyst refresh incomplete: {} entit{} produced no catalysts and had none to carry forward ({})",
            uncovered.len(),
            if uncovered.len() == 1 { "y" } else { "ies" },
            uncovered.join(", ")
        );
        tracing::warn!(message = %message, "refresh stage=catalysts status=failed");
        anyhow::bail!("{message}");
    }
    if carried_entities > 0 {
        tracing::warn!(
            carried_entities,
            carried_catalysts,
            "refresh stage=catalysts status=degraded: activating with carried-forward catalysts"
        );
    }

    let sim_pipeline = SimilarityPipeline::new(&db, svc);
    let sim_stats = sim_pipeline.run(|_, _| {})?;
    let shadow_catalyst_count = db.get_all_catalysts()?.len();
    anyhow::ensure!(
        sim_stats.catalysts_processed == shadow_catalyst_count,
        "shadow epoch similarity set is incomplete"
    );

    // Target epoch projections are built from frozen target DB snapshots before
    // source activation. The active source remains untouched if any target
    // component fails.
    let catalysts = db.get_all_catalysts()?;
    let source_old_epoch = active_db.get_epoch_state()?.active_epoch_id;
    let old_catalysts = active_db.get_all_catalysts()?;
    let mut target_shadows = Vec::new();
    for (target_index, target) in contract.targets.iter().enumerate() {
        let target_path = std::path::PathBuf::from(target);
        if !target_path.exists() {
            anyhow::bail!("configured epoch target does not exist: {target}");
        }
        let target_db_path = paths.target_db(&target_path);
        if !target_db_path.exists() {
            anyhow::bail!("configured target is not prepared: {target}");
        }
        let target_shadow_path =
            epoch_dir.join(format!("{}-target-{}.db", claim.epoch_id, target_index));
        {
            let _target_snapshot_lock = acquire_projection_lock(
                target_db_path
                    .parent()
                    .context("target DB has no parent directory")?,
            )?;
            let target_active = Database::open(&target_db_path)?;
            if let Some(old_epoch) = &source_old_epoch {
                if target_active
                    .get_epoch_catalyst_embeddings(old_epoch)?
                    .is_empty()
                {
                    target_active.stage_epoch_projection_from_shadow(&target_active, old_epoch)?;
                }
            }
            target_active.snapshot_to(&target_shadow_path)?;
        }
        let target_shadow = Database::open(&target_shadow_path)?;
        IndexingPipeline::new(&target_shadow, target_path.clone()).run(false, |_| {})?;
        EmbeddingPipeline::new(&target_shadow, svc).run_all(|_| {})?;
        SimilarityPipeline::new(&target_shadow, svc).run_with_catalysts(&catalysts, |_, _| {})?;
        target_shadows.push((target_db_path, target_shadow_path));
    }

    // Close the snapshot-to-activation race. Target preparation and source
    // continuous projection use these same locks. Once held, refresh each live
    // target against the completed catalyst set and stage that exact live view;
    // source activation remains the single logical commit point across DBs.
    let _target_projection_lock = if target_shadows.is_empty() {
        None
    } else {
        Some(acquire_projection_lock(
            paths
                .vault_db
                .parent()
                .unwrap_or(vault_path)
                .join("targets")
                .as_path(),
        )?)
    };
    let _source_projection_lock =
        acquire_projection_lock(paths.vault_db.parent().context("vault DB has no parent")?)?;
    anyhow::ensure!(
        !active_db.continuous_projection_fence()?.dirty,
        "continuous projection needs recovery before catalyst activation"
    );
    for ((target_db_path, shadow_path), target) in
        target_shadows.iter().zip(contract.targets.iter())
    {
        let target_db = Database::open(target_db_path)?;
        let target_projection_token = target_db.begin_continuous_projection()?;
        IndexingPipeline::new(&target_db, std::path::PathBuf::from(target)).run(false, |_| {})?;
        EmbeddingPipeline::new(&target_db, svc).run_all(|_| {})?;
        if let Some(old_epoch) = &source_old_epoch {
            // Keep the still-active target epoch exact against any target docs
            // caught up here. This also leaves a fully usable rollback view if
            // a later target or source activation fails.
            SimilarityPipeline::new(&target_db, svc)
                .run_with_catalysts(&old_catalysts, |_, _| {})?;
            target_db.stage_epoch_projection_from_shadow(&target_db, old_epoch)?;
            target_db.set_config(
                "catalyst_fingerprint",
                &catalysts_fingerprint(&old_catalysts),
            )?;
        }
        // Compute the not-yet-active catalyst projection in a shadow copy.
        // Never replace the target's unkeyed old-epoch catalyst cache before
        // source activation: a later target failure must leave it retryable.
        target_db.snapshot_to(shadow_path)?;
        let target_shadow = Database::open(shadow_path)?;
        SimilarityPipeline::new(&target_shadow, svc).run_with_catalysts(&catalysts, |_, _| {})?;
        target_db.stage_epoch_projection_from_shadow(&target_shadow, &claim.epoch_id)?;
        target_db.finish_continuous_projection(&target_projection_token)?;
    }
    active_db.activate_epoch_from_shadow(&db, &claim.epoch_id, now_ms())?;
    let _ = bump_collection_cache_epoch(active_db)?;
    info!(epoch_id = %claim.epoch_id, "catalyst shadow epoch activated");
    let _ = api_config; // credential resolution is also the hosted preflight.
    Ok(())
}

/// Refresh all previously prepared targets using the vault's current catalysts.
///
/// Merges targets from config.toml (declarative, survives DB resets) and the
/// `applied_targets` DB table (runtime record). Config is the source of truth;
/// DB targets that aren't in config are also refreshed for backward compat.
///
/// Called after catalyst regeneration so that target similarity mappings stay
/// in sync. Errors are logged but don't fail the parent operation.
async fn refresh_targets(vault_path: &Path) {
    let paths = match EnzymePaths::new(vault_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Collect target paths from both sources, deduplicated
    let mut target_paths = std::collections::HashSet::new();

    // Config targets (source of truth)
    if let Ok(Some(config)) = EnzymeConfig::load(vault_path) {
        for t in &config.targets {
            target_paths.insert(t.clone());
        }
    }

    // DB targets (backward compat — catches targets added before config persistence)
    if let Ok(db) = Database::open(&paths.vault_db) {
        if let Ok(db_targets) = db.get_all_applied_targets() {
            for t in &db_targets {
                target_paths.insert(t.target_path.clone());
            }
        }
    }

    for target in &target_paths {
        let target_path = std::path::PathBuf::from(target);
        if !target_path.exists() {
            info!(path = %target, "skipping target refresh: target path no longer exists");
            continue;
        }
        info!(path = %target, "refreshing target after catalyst regeneration");
        if let Err(e) = super::target::prepare_target(vault_path, &target_path).await {
            info!(path = %target, err = %e, "target refresh failed");
        }
    }
}

/// Spawn a background refresh process (detached child).
///
/// Returns `true` if a background process was spawned, `false` if skipped
/// (e.g., another refresh is already running).
/// Open the vault DB, retrying briefly on lock contention.
///
/// `Database::open` reruns the idempotent schema batch, which can collide
/// with another process's open connection (notably the parent refresh that
/// just spawned this detached worker). The contention window is the tail of
/// that process's run, so a short bounded retry is enough.
fn open_vault_db_with_retry(vault_db: &Path) -> Result<Database> {
    const ATTEMPTS: u32 = 10;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);
    let mut last_err = None;
    for attempt in 0..ATTEMPTS {
        match Database::open(vault_db) {
            Ok(db) => return Ok(db),
            Err(e) if format!("{e:#}").contains("locked") => {
                debug!(attempt, "vault DB locked, retrying: {e}");
                last_err = Some(e);
                std::thread::sleep(BACKOFF);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_err.expect("at least one locked attempt").into())
}

pub(super) fn spawn_background_refresh(
    vault_path: &Path,
    paths: &EnzymePaths,
    use_env_llm: bool,
) -> bool {
    let enzyme_dir = match paths.vault_db.parent() {
        Some(p) => p,
        None => return false,
    };

    // Skip if another background refresh is already running
    if is_refresh_locked(enzyme_dir) {
        debug!("background refresh already running, skipping spawn");
        return false;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            debug!("failed to get current exe: {}", e);
            return false;
        }
    };

    let log_path = enzyme_dir.join("background_refresh.log");

    // Open log file and /dev/null for stdin
    let log_file = match fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to create background refresh log: {}", e);
            return false;
        }
    };
    let log_stderr = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to clone log file handle: {}", e);
            return false;
        }
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["refresh", "--detached", "--quiet", "-p"])
        .arg(vault_path);
    if use_env_llm {
        cmd.arg("--use-env-llm");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr));

    // On Unix, double-fork to fully detach: the intermediate child exits
    // immediately, the grandchild (the real worker) is adopted by init.
    // This prevents zombies regardless of parent/child exit ordering.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()),         // grandchild: continue to exec
                    _ => libc::_exit(0), // intermediate: exit immediately
                }
            });
        }
    }

    match cmd.spawn() {
        Ok(mut child) => {
            // Reap intermediate process (exited immediately after double-fork)
            let _ = child.wait();
            debug!("spawned background refresh (double-forked)");
            true
        }
        Err(e) => {
            debug!("failed to spawn background refresh: {}", e);
            false
        }
    }
}

/// Spawn a detached full init worker for the voice-ready foreground path.
fn spawn_background_init(
    vault_path: &Path,
    paths: &EnzymePaths,
    min_top_catalysts: Option<usize>,
    stream_partial_catalysts: bool,
    use_env_llm: bool,
) -> bool {
    let enzyme_dir = match paths.vault_db.parent() {
        Some(p) => p,
        None => return false,
    };

    if is_refresh_locked(enzyme_dir) {
        debug!("background init or refresh already running, skipping spawn");
        return false;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            debug!("failed to get current exe: {}", e);
            return false;
        }
    };

    let log_path = enzyme_dir.join("background_init.log");
    let log_file = match fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to create background init log: {}", e);
            return false;
        }
    };
    let log_stderr = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to clone background init log file handle: {}", e);
            return false;
        }
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["init", "--detached", "--quiet", "-p"])
        .arg(vault_path);
    if let Some(value) = min_top_catalysts {
        cmd.arg("--min-top-catalysts").arg(value.to_string());
    }
    if stream_partial_catalysts {
        cmd.arg("--stream-partial-catalysts");
    }
    if use_env_llm {
        cmd.arg("--use-env-llm");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()),
                    _ => libc::_exit(0),
                }
            });
        }
    }

    match cmd.spawn() {
        Ok(mut child) => {
            let _ = child.wait();
            debug!("spawned background init (double-forked)");
            true
        }
        Err(e) => {
            debug!("failed to spawn background init: {}", e);
            false
        }
    }
}

/// Interactive refresh path.
///
/// Runs the same local incremental work as quiet refresh, then runs the
/// catalyst pipeline only when catalyst staleness says it is needed.
async fn run_refresh_interactive(
    vault_path: &Path,
    min_top_catalysts: Option<usize>,
    use_env_llm: bool,
    force_full: bool,
) -> Result<()> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    if let Some(value) = min_top_catalysts {
        let mut config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
        config.persist_min_top_catalysts(vault_path, value)?;
    }

    if !paths.vault_db.exists() {
        let _ = run_init(
            vault_path,
            false,
            false,
            false,
            min_top_catalysts,
            false,
            use_env_llm,
        )
        .await?;
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();
    let local = run_local_refresh_work(&db, vault_path, &paths)?;
    db.reconcile_corpus_generation(now_ms())?;
    let contract = generation_contract(vault_path, &paths, use_env_llm).await?;
    db.bootstrap_legacy_epoch(&contract, now_ms(), false)?;
    recover_abandoned_epoch_worker(&db, &paths)?;
    let schedule = db.schedule_catalyst_epoch(now_ms(), &contract, force_full)?;
    let spawned =
        schedule.launch_needed && spawn_background_refresh(vault_path, &paths, use_env_llm);
    let _ = maybe_bump_collection_cache_epoch(&db, local.collection_changed())?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "status": "completed",
            "background_spawned": spawned,
            "indexing": {
                "files_discovered": local.indexing.files_discovered,
                "files_new": local.indexing.files_new,
                "files_updated": local.indexing.files_updated,
                "files_deleted": local.indexing.files_deleted,
                "duration_ms": local.indexing.duration_ms,
            },
            "embedding": {
                "documents_embedded": local.embedding.documents_embedded,
                "chunks_embedded": local.embedding.chunks_embedded,
                "documents_pruned": local.embedding.documents_pruned,
                "duration_ms": local.embedding.duration_ms,
            },
            "similarity": local.similarity,
            "epoch": epoch_metrics(&db, &schedule, now_ms())?,
            "duration_ms": start.elapsed().as_millis() as u64,
        }))?
    );
    Ok(())
}

fn parse_entity_type(s: &str) -> enzyme_core::models::EntityType {
    match s {
        "tag" => enzyme_core::models::EntityType::Tag,
        "link" => enzyme_core::models::EntityType::Link,
        "folder" => enzyme_core::models::EntityType::Folder,
        _ => enzyme_core::models::EntityType::Tag,
    }
}

fn selected_entity_ref(entity: &Entity) -> String {
    match entity.entity_type {
        enzyme_core::models::EntityType::Tag => format!("#{}", entity.name),
        enzyme_core::models::EntityType::Link => format!("[[{}]]", entity.name),
        enzyme_core::models::EntityType::Folder
            if entity.name == enzyme_core::models::ROOT_FOLDER_ENTITY =>
        {
            "folder:.".to_string()
        }
        enzyme_core::models::EntityType::Folder => format!("folder:{}", entity.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_core::models::{EntityTrends, EntityType};

    fn entity(name: &str) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type: EntityType::Tag,
            frequency: 1,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends::default(),
            metadata: serde_json::json!({}),
        }
    }

    fn folder_entity(name: &str) -> Entity {
        Entity {
            entity_type: EntityType::Folder,
            ..entity(name)
        }
    }

    fn catalyst_stats(succeeded: usize, generated: usize, failed: usize) -> CatalystStats {
        CatalystStats {
            entities_processed: succeeded,
            catalysts_generated: generated,
            errors: (0..failed)
                .map(|i| (format!("entity-{i}"), "failed".to_string()))
                .collect(),
            duration_ms: 0,
            rate_limited: 0,
        }
    }

    #[test]
    fn catalyst_quality_fails_zero_generated() {
        let gate = evaluate_catalyst_quality(&catalyst_stats(0, 0, 3), 3);
        assert_eq!(gate.quality, CatalystQuality::Failed);
    }

    #[test]
    fn catalyst_quality_degrades_partial_success_above_threshold() {
        let gate = evaluate_catalyst_quality(&catalyst_stats(3, 21, 1), 4);
        assert_eq!(gate.quality, CatalystQuality::Degraded);
    }

    #[test]
    fn catalyst_quality_fails_majority_entity_failure() {
        let gate = evaluate_catalyst_quality(&catalyst_stats(1, 7, 3), 4);
        assert_eq!(gate.quality, CatalystQuality::Failed);
    }

    #[test]
    fn embedding_refresh_checks_backlog_when_indexing_has_no_changes() {
        assert_eq!(embedding_refresh_plan(false), EmbeddingRefreshPlan::Backlog);
        assert_eq!(
            embedding_refresh_plan(true),
            EmbeddingRefreshPlan::SinceIndexStart
        );
    }

    #[test]
    fn catalyst_credentials_are_needed_only_when_entities_exist() {
        assert!(!catalyst_generation_planned(&[]));
        assert!(catalyst_generation_planned(&[entity("alpha")]));
    }

    #[test]
    fn loginless_hosted_init_is_limited_to_first_configured_vault() {
        assert!(loginless_hosted_init_allowed_for_keys("/vault/a", &[]));
        assert!(loginless_hosted_init_allowed_for_keys(
            "/vault/a",
            &["/vault/a".to_string()]
        ));
        assert!(!loginless_hosted_init_allowed_for_keys(
            "/vault/b",
            &["/vault/a".to_string()]
        ));
        assert!(!loginless_hosted_init_allowed_for_keys(
            "/vault/a",
            &["/vault/a".to_string(), "/vault/b".to_string()]
        ));
    }

    #[test]
    fn selected_entity_ref_formats_root_folder_for_config() {
        assert_eq!(
            selected_entity_ref(&folder_entity(enzyme_core::models::ROOT_FOLDER_ENTITY)),
            "folder:."
        );
        assert_eq!(
            selected_entity_ref(&folder_entity("projects")),
            "folder:projects"
        );
    }

    #[test]
    fn epoch_targets_include_legacy_applied_target_without_existing_config() {
        let target = tempfile::TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();
        db.upsert_applied_target(&enzyme_core::models::AppliedTarget {
            id: None,
            target_path: target.path().to_string_lossy().to_string(),
            doc_count: 0,
            catalyst_count: 0,
            applied_at: chrono::Utc::now(),
        })
        .unwrap();

        assert_eq!(
            epoch_targets(&EnzymeConfig::default(), Some(&db)).unwrap(),
            vec![target.path().to_string_lossy().to_string()]
        );
    }

    #[test]
    fn seed_petri_count_requires_nonempty_top_level_catalysts() {
        let petri = serde_json::json!({
            "entities": [
                {"name": "ready", "catalysts": [{"text": "one"}]},
                {"name": "empty", "catalysts": []},
                {"name": "missing"}
            ]
        });

        assert_eq!(count_seed_petri_entities(&petri, 1), 1);
        assert_eq!(count_seed_petri_entities(&petri, 2), 0);
    }

    #[test]
    fn background_init_terminal_reason_detects_worker_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("background_init.log");
        std::fs::write(&log, "Error: insufficient credits\n").unwrap();

        assert_eq!(
            background_init_terminal_reason(Some(&log)).as_deref(),
            Some("Error: insufficient credits")
        );
    }

    #[test]
    fn background_init_terminal_reason_detects_zero_catalyst_completion() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("background_init.log");
        std::fs::write(
            &log,
            serde_json::json!({
                "status": "completed",
                "catalysts_generated": 0
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            background_init_terminal_reason(Some(&log)).as_deref(),
            Some("background init completed with 0 generated catalysts")
        );
    }
}
