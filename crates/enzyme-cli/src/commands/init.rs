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
use enzyme_core::pipeline::indexing::{IndexingPipeline, IndexingStats};
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::pipeline::staleness::{
    StaleReason, StalenessResult, check_catalyst_staleness_for_entities, load_stored_entity_names,
    store_staleness_bookkeeping, store_staleness_bookkeeping_partial,
};

use super::lock::{acquire_refresh_lock, is_refresh_locked};
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

    let discovered_files = FileDiscovery::new(vault_path.to_path_buf()).discover()?;
    if !discovered_files.is_empty() {
        return Ok(false);
    }

    let db = Database::open(&paths.vault_db)?;
    Ok(db.get_document_count().unwrap_or(0) > 0)
}

struct LocalRefreshResult {
    indexing: IndexingStats,
    embedding_changed: bool,
    similarities_recomputed: bool,
}

impl LocalRefreshResult {
    fn collection_changed(&self) -> bool {
        has_index_changes(&self.indexing) || self.embedding_changed || self.similarities_recomputed
    }

    fn change_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if has_index_changes(&self.indexing) {
            reasons.push("documents_changed");
        }
        if self.embedding_changed {
            reasons.push("embeddings_changed");
        }
        if self.similarities_recomputed {
            reasons.push("similarities_recomputed");
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
        enzyme_core::pipeline::selection::SelectionSource::Shortlist
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

        // Free-tier preflight: deduct 3 credits only when catalyst generation
        // will run. BYOK users pay their own inference and skip this gate.
        if resolved.as_ref().map_or(false, |(cfg, _)| {
            matches!(
                cfg.provider,
                enzyme_core::llm::ConfigProvider::ServerFallback
            )
        }) {
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
                debug!("hosted init has no login token; skipping account credit preflight");
            }
        }

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

        Some(stats)
    } else {
        let msg = "No API key found and Enzyme server unreachable — skipping catalyst generation. Set OPENAI_API_KEY or ensure network access.".to_string();
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
        eprintln!(
            "  {}Publish this vault to access it from Claude anywhere:{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::DarkGrey),
            crossterm::style::ResetColor,
        );
        eprintln!("    enzyme publish",);
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
    let _local = run_local_refresh_work(&db, vault_path, &paths)?;
    let _ = maybe_bump_collection_cache_epoch(&db, _local.collection_changed())?;
    drop(db);

    // Local index/embedding work already succeeded; a catalyst credential or
    // quota failure should not take down MCP search over existing catalysts.
    if let Err(e) = run_stale_catalyst_refresh(vault_path, None, false).await {
        tracing::warn!("catalyst refresh failed; serving existing catalysts: {e:#}");
    }
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
) -> Result<()> {
    // Local env-var check only — a misconfigured --use-env-llm should fail
    // here rather than inside a detached worker whose errors only reach a log.
    preflight_env_llm(use_env_llm).await?;

    if detached {
        return run_refresh_detached(vault_path, use_env_llm).await;
    }
    if quiet {
        return run_refresh_fast(vault_path, use_env_llm).await;
    }
    run_refresh_interactive(vault_path, min_top_catalysts, use_env_llm).await
}

/// Fast sync path — the hook hot path for `enzyme refresh --quiet`.
///
/// Runs only cheap local work (indexing, embedding, similarity) synchronously,
/// then spawns a background process for expensive LLM catalyst generation if
/// the vault is stale.
async fn run_refresh_fast(vault_path: &Path, use_env_llm: bool) -> Result<()> {
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
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names = selected_entity_names(&sel_result.entities);

    // Catalyst staleness controls only catalyst regeneration, not doc embedding.
    let staleness = check_catalyst_staleness_for_entities(&db, vault_path, Some(&entity_names))?;
    let background_spawned = match &staleness {
        StalenessResult::Fresh => false,
        StalenessResult::Stale(reason) => {
            debug!(?reason, "fast refresh: stale, spawning background");
            spawn_background_refresh(vault_path, &paths, use_env_llm)
        }
    };
    let cache_epoch = maybe_bump_collection_cache_epoch(&db, collection_changed)?;

    // ── Step 5: Output JSON ─────────────────────────────────────────────
    let duration_ms = start.elapsed().as_millis() as u64;
    let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
    let output = json!({
        "status": "completed",
        "fresh": staleness.is_fresh(),
        "background_spawned": background_spawned,
        "embedding_changed": local.embedding_changed,
        "similarities_recomputed": local.similarities_recomputed,
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
    let max_embed = EnzymeConfig::load(vault_path)
        .ok()
        .flatten()
        .unwrap_or_default()
        .effective_max_embedding_files();
    let embedding_changed = refresh_changed_doc_embeddings(
        db,
        service,
        index_start_ms,
        max_embed,
        has_index_changes(&idx_stats),
    );

    let similarities_recomputed = if embedding_changed {
        recompute_current_similarities(db, service)
    } else {
        false
    };

    Ok(LocalRefreshResult {
        indexing: idx_stats,
        embedding_changed,
        similarities_recomputed,
    })
}

fn has_index_changes(stats: &IndexingStats) -> bool {
    stats.files_new > 0 || stats.files_updated > 0 || stats.files_deleted > 0
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
    max_embed: Option<usize>,
    has_index_changes: bool,
) -> bool {
    let Some(service) = service else {
        debug!("fast refresh: embedding unavailable");
        return false;
    };

    let pipeline = EmbeddingPipeline::new(db, service);
    let result = match embedding_refresh_plan(has_index_changes) {
        EmbeddingRefreshPlan::Backlog => pipeline.run_with_limit(max_embed, |_| {}),
        EmbeddingRefreshPlan::SinceIndexStart => {
            pipeline.run_since_with_limit(index_start_ms, max_embed, |_| {})
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
            changed
        }
        Err(e) => {
            debug!("fast refresh: embedding failed: {}", e);
            false
        }
    }
}

fn selected_entity_names(entities: &[Entity]) -> std::collections::HashSet<String> {
    entities.iter().map(|entity| entity.name.clone()).collect()
}

fn entities_for_catalyst_refresh(selected: &[Entity], reason: &StaleReason) -> Vec<Entity> {
    match reason.stale_entity_names() {
        Some(stale_names) => selected
            .iter()
            .filter(|entity| stale_names.contains(&entity.name))
            .cloned()
            .collect(),
        None => selected.to_vec(),
    }
}

fn recompute_current_similarities(db: &Database, service: Option<&EmbeddingService>) -> bool {
    let Some(service) = service else {
        return false;
    };
    if !database_has_catalysts(db) {
        return false;
    }

    let sim_pipeline = SimilarityPipeline::new(db, service);
    match sim_pipeline.run(|_, _| {}) {
        Ok(_) => true,
        Err(e) => {
            debug!("fast refresh: similarity failed: {}", e);
            false
        }
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

    run_stale_catalyst_refresh(vault_path, None, use_env_llm).await?;
    // Lock released on drop
    Ok(())
}

async fn run_stale_catalyst_refresh(
    vault_path: &Path,
    min_top_catalysts: Option<usize>,
    use_env_llm: bool,
) -> Result<bool> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();

    if let Some(mtc) = min_top_catalysts {
        let mut config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
        if let Err(e) = config.persist_min_top_catalysts(vault_path, mtc) {
            tracing::warn!("failed to persist min_top_catalysts: {}", e);
        }
    }

    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names = selected_entity_names(&sel_result.entities);
    let staleness = check_catalyst_staleness_for_entities(&db, vault_path, Some(&entity_names))?;
    let reason = match staleness {
        StalenessResult::Fresh => {
            info!("catalyst refresh: already fresh, exiting");
            return Ok(false);
        }
        StalenessResult::Stale(reason) => reason,
    };

    let entities_to_process = match reason.stale_entity_names() {
        Some(stale_names) => {
            info!(
                count = stale_names.len(),
                "catalyst refresh: partial regeneration"
            );
            entities_for_catalyst_refresh(&sel_result.entities, &reason)
        }
        None => {
            info!(
                count = sel_result.entities.len(),
                "catalyst refresh: full regeneration"
            );
            entities_for_catalyst_refresh(&sel_result.entities, &reason)
        }
    };

    if entities_to_process.is_empty() {
        info!("catalyst refresh: no entities to process");
        return Ok(false);
    }

    let llm_mode = if use_env_llm {
        LlmCredentialMode::Env
    } else {
        LlmCredentialMode::Hosted
    };
    let (api_config, llm) = resolve_llm_client_for_mode(llm_mode).await?;

    // Free-tier preflight: deduct 1 credit up front. Bail before LLM work
    // if balance is insufficient. BYOK users skip this gate.
    if matches!(
        api_config.provider,
        enzyme_core::llm::ConfigProvider::ServerFallback
    ) {
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
            // init credit preflight instead of hard-failing here.
            debug!("hosted refresh has no login token; skipping account credit preflight");
        }
    }

    let svc = get_embedding_service(&paths.home).ok();
    let cat_pipeline = CatalystPipeline::new(&db, &llm, svc);

    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();

    let vault_context = {
        let ctx_builder = ContextBuilder::new(&db);
        ctx_builder.build(&entities_to_process, 5, 300).ok()
    };
    let effective_min_top = min_top_catalysts.or_else(|| {
        enzyme_config
            .trending
            .as_ref()
            .and_then(|t| t.min_top_catalysts)
    });
    let allocations = calculate_allocations(&entities_to_process, effective_min_top);

    let cat_config = CatalystConfig {
        entity_profiles: &sel_result.entity_profiles,
        catalysts_per_entity: 7,
        default_profile: DEFAULT_PROFILE_KEY,
        format_key: enzyme_config
            .catalyst_format
            .as_deref()
            .unwrap_or(DEFAULT_FORMAT_KEY),
        prompt_context: vault_context.as_deref(),
        model: &api_config.model,
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
        stream_partial_catalysts: false,
    };

    let stats = cat_pipeline
        .run(&entities_to_process, &cat_config, |_event| {})
        .await?;
    let catalyst_gate = evaluate_catalyst_quality(&stats, entities_to_process.len());

    info!(
        entities = stats.entities_processed,
        catalysts = stats.catalysts_generated,
        errors = stats.errors.len(),
        quality = ?catalyst_gate.quality,
        "catalyst refresh: generation complete"
    );

    if catalyst_gate.quality == CatalystQuality::Failed {
        let message = catalyst_gate
            .message
            .unwrap_or_else(|| "catalyst refresh failed quality checks".to_string());
        tracing::warn!(message = %message, "refresh stage=catalysts status=failed");
        anyhow::bail!("{message}");
    }

    let is_partial = reason.stale_entity_names().is_some();
    if catalyst_gate.quality == CatalystQuality::Passed && is_partial {
        let names: Vec<String> = entities_to_process.iter().map(|e| e.name.clone()).collect();
        store_staleness_bookkeeping_partial(&db, &names)?;
    } else if catalyst_gate.quality == CatalystQuality::Passed {
        let entity_names: Vec<String> =
            sel_result.entities.iter().map(|e| e.name.clone()).collect();
        store_staleness_bookkeeping(&db, vault_path, &entity_names)?;
    } else {
        tracing::warn!("refresh stage=staleness status=skipped reason=catalyst_quality_degraded");
    }

    if stats.catalysts_generated > 0 {
        if let Ok(svc) = get_embedding_service(&paths.home) {
            let sim_pipeline = SimilarityPipeline::new(&db, svc);
            if let Err(e) = sim_pipeline.run(|_, _| {}) {
                info!("catalyst refresh: similarity failed: {}", e);
            }
        }
        refresh_targets(vault_path).await;
        let _ = bump_collection_cache_epoch(&db)?;
    }

    Ok(stats.catalysts_generated > 0)
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
fn spawn_background_refresh(vault_path: &Path, paths: &EnzymePaths, use_env_llm: bool) -> bool {
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
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

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
    let old_entity_names = load_stored_entity_names(&db).ok();
    let _local = run_local_refresh_work(&db, vault_path, &paths)?;
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names = selected_entity_names(&sel_result.entities);

    let staleness = check_catalyst_staleness_for_entities(&db, vault_path, Some(&entity_names))?;
    match staleness {
        StalenessResult::Fresh => {
            debug!("catalysts are fresh after local refresh");
            let _ = maybe_bump_collection_cache_epoch(&db, _local.collection_changed())?;
            return Ok(());
        }
        StalenessResult::Stale(reason) => {
            debug!(?reason, "catalysts are stale — running catalyst pipeline");
        }
    }
    drop(db);

    run_stale_catalyst_refresh(vault_path, min_top_catalysts, use_env_llm).await?;

    let db = Database::open(&paths.vault_db)?;
    let entity_names = load_stored_entity_names(&db)?;

    // Compute entity set diff if we had a previous snapshot.
    let new_names: std::collections::HashSet<String> = entity_names.iter().cloned().collect();
    let entity_changes = if let Some(ref old_names) = old_entity_names {
        if !old_names.is_empty() {
            let mut added: Vec<&String> = new_names.difference(old_names).collect();
            let mut removed: Vec<&String> = old_names.difference(&new_names).collect();
            added.sort();
            removed.sort();

            if !added.is_empty() || !removed.is_empty() {
                Some((
                    added.into_iter().cloned().collect::<Vec<_>>(),
                    removed.into_iter().cloned().collect::<Vec<_>>(),
                ))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Surface entity set diff to stderr for interactive use.
    if let Some((added, removed)) = entity_changes {
        eprintln!("\n  Entity changes:");
        for name in &added {
            eprintln!("    + {}", name);
        }
        for name in &removed {
            eprintln!("    - {}", name);
        }
        if let Some(ref old_names) = old_entity_names {
            eprintln!(
                "    {} unchanged",
                old_names.intersection(&new_names).count()
            );
        }
    }

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
    fn catalyst_refresh_processes_only_stale_entities_below_threshold() {
        let selected = vec![entity("alpha"), entity("beta"), entity("gamma")];
        let reason = StaleReason::EntitiesStale {
            stale_entities: vec![enzyme_core::pipeline::staleness::StaleEntity {
                name: "beta".to_string(),
                delta: 3,
            }],
        };

        let names: Vec<String> = entities_for_catalyst_refresh(&selected, &reason)
            .into_iter()
            .map(|entity| entity.name)
            .collect();

        assert_eq!(names, vec!["beta"]);
    }

    #[test]
    fn catalyst_refresh_processes_all_entities_at_full_refresh_threshold() {
        let selected = vec![entity("alpha"), entity("beta"), entity("gamma")];
        let reason = StaleReason::EntitiesStale {
            stale_entities: vec![
                enzyme_core::pipeline::staleness::StaleEntity {
                    name: "alpha".to_string(),
                    delta: 3,
                },
                enzyme_core::pipeline::staleness::StaleEntity {
                    name: "beta".to_string(),
                    delta: 3,
                },
                enzyme_core::pipeline::staleness::StaleEntity {
                    name: "gamma".to_string(),
                    delta: 3,
                },
            ],
        };

        let names: Vec<String> = entities_for_catalyst_refresh(&selected, &reason)
            .into_iter()
            .map(|entity| entity.name)
            .collect();

        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn catalyst_refresh_processes_all_entities_for_config_or_empty_state() {
        let selected = vec![entity("alpha"), entity("beta")];

        let config_names: Vec<String> =
            entities_for_catalyst_refresh(&selected, &StaleReason::ConfigChanged)
                .into_iter()
                .map(|entity| entity.name)
                .collect();
        assert_eq!(config_names, vec!["alpha", "beta"]);

        let no_catalyst_names: Vec<String> =
            entities_for_catalyst_refresh(&selected, &StaleReason::NoCatalysts)
                .into_iter()
                .map(|entity| entity.name)
                .collect();
        assert_eq!(no_catalyst_names, vec!["alpha", "beta"]);
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
