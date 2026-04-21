use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{debug, info};

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient, resolve_api_config};
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystEvent, CatalystPipeline, calculate_allocations,
};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::pipeline::staleness::{
    StalenessResult, check_catalyst_staleness, load_stored_entity_names,
    store_staleness_bookkeeping, store_staleness_bookkeeping_partial,
};

use super::lock::{acquire_refresh_lock, is_refresh_locked};
use super::vault_summary;
use crate::display::{DisplayEvent, InitDisplay};

/// Stats collected during an init run, used for JSON output in quiet mode.
pub struct InitResult {
    pub files_discovered: usize,
    pub files_new: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub catalysts_generated: usize,
    pub embeddings_documents: Option<usize>,
    pub embeddings_pruned: Option<usize>,
    pub semantic_search: bool,
    pub catalysts_available: bool,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

impl InitResult {
    /// Build the JSON output for quiet mode, including petri data.
    pub fn to_json(&self, vault_path: &Path) -> Result<serde_json::Value> {
        let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
        let mut output = json!({
            "status": "completed",
            "fresh": false,
            "indexing": {
                "files_discovered": self.files_discovered,
                "files_new": self.files_new,
                "files_updated": self.files_updated,
                "files_deleted": self.files_deleted,
            },
            "catalysts_generated": self.catalysts_generated,
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
/// Desktop app uses quiet mode (JSON output) to populate its guide text box.
pub async fn run_init_index_only(vault_path: &Path, full: bool, quiet: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
    }

    let db = Database::open(&paths.vault_db)?;

    // Run indexing
    let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
    let idx_stats = indexing.run(full, |_| {})?;

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

pub async fn run_init(
    vault_path: &Path,
    full: bool,
    _detached: bool,
    quiet: bool,
    json_progress: bool,
    vault_guide: Option<&str>,
    min_top_catalysts: Option<usize>,
) -> Result<Option<InitResult>> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;
    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
    }

    // Open database
    let db = Database::open(&paths.vault_db)?;

    // Resolve API config early (needed for both LLM selection and catalyst generation)
    let api_config = match resolve_api_config().await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("API config resolution failed: {}", e);
            None
        }
    };

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

        if let Some(ref cfg) = api_config {
            if matches!(cfg.provider, enzyme_core::llm::ConfigProvider::OpenAiEnv) {
                tx.send(DisplayEvent::ApiConfig {
                    base_url: cfg.base_url.clone(),
                    model: cfg.model.clone(),
                    masked_key: cfg.masked_api_key(),
                })
                .ok();
            }
        }

        tx.send(DisplayEvent::IndexingStart).ok();
        let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
        let tx_idx = tx.clone();
        let stats = indexing.run(full, |msg| {
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

    let vault_guide = vault_guide.map(|s| s.to_string());
    let vault_guide = vault_guide.as_deref();

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
    // Write only base entities to TOML — exclude children that get auto-expanded
    // from expandable folders at runtime.
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
        .map(|entity| match entity.entity_type {
            enzyme_core::models::EntityType::Tag => format!("#{}", entity.name),
            enzyme_core::models::EntityType::Link => format!("[[{}]]", entity.name),
            enzyme_core::models::EntityType::Folder => format!("folder:{}", entity.name),
        })
        .collect();
    enzyme_config.set_auto_selected_entities(vault_path, selected_entity_refs)?;

    tx.send(DisplayEvent::SelectionComplete {
        count: sel_result.entities.len(),
        source: sel_result.source.clone(),
    })
    .ok();

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
        tx.send(DisplayEvent::EmbeddingStart).ok();
        Some(tokio::task::spawn_blocking(move || {
            let bg_db = Database::open(&db_path)?;
            let pipeline = EmbeddingPipeline::new(&bg_db, svc);
            pipeline.run(|done, total| {
                tx_emb
                    .send(DisplayEvent::EmbeddingProgress { done, total })
                    .ok();
            })
        }))
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

    let cat_stats = if let Some(resolved) = api_config {
        let total = sel_result.entities.len();
        tx.send(DisplayEvent::DiscoveryStart { total }).ok();

        let llm = LlmClient::from_config(&resolved);
        let cat_pipeline = CatalystPipeline::new(&db, &llm, service);
        let tx_cat = tx.clone();

        // Merge user-provided vault guide with DB-built context
        let effective_vault_guide = match (vault_guide, vault_context.as_deref()) {
            (Some(guide), Some(context)) => {
                // User guide takes priority, append DB context as supplementary
                Some(format!(
                    "{guide}\n\n<vault_context>\n{context}\n</vault_context>"
                ))
            }
            (Some(guide), None) => Some(guide.to_string()),
            (None, Some(context)) => Some(context.to_string()),
            (None, None) => None,
        };
        let effective_vault_guide = effective_vault_guide.as_deref();

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
            default_profile: enzyme_config
                .catalyst_profile
                .as_deref()
                .unwrap_or(DEFAULT_PROFILE_KEY),
            format_key: enzyme_config
                .catalyst_format
                .as_deref()
                .unwrap_or(DEFAULT_FORMAT_KEY),
            vault_guide: effective_vault_guide,
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

    // ── Step 6: Wait for background embedding ─────────────────────────────
    let emb_stats = if let Some(handle) = embedding_handle {
        match handle.await.context("embedding task panicked")? {
            Ok(stats) => {
                tx.send(DisplayEvent::EmbeddingComplete {
                    documents: stats.documents_embedded,
                    pruned: stats.documents_pruned,
                    duration_ms: stats.duration_ms,
                })
                .ok();
                Some((stats.documents_embedded, stats.documents_pruned))
            }
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
    if emb_completed
        && cat_stats
            .as_ref()
            .map_or(false, |s| s.catalysts_generated > 0)
    {
        tx.send(DisplayEvent::SimilarityStart).ok();
        let svc = service.unwrap(); // safe: emb_completed implies service was Some
        let sim_pipeline = SimilarityPipeline::new(&db, svc);
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
    }

    // ── Step 8: Re-apply targets ──────────────────────────────────────────
    if cat_stats
        .as_ref()
        .map_or(false, |s| s.catalysts_generated > 0)
    {
        reapply_targets(vault_path).await;
    }

    // ── Step 9: Generate CLAUDE.md and hook (interactive mode only) ──────
    if !quiet {
        if let Err(e) = super::claude_md::generate_claude_md(vault_path) {
            tracing::warn!("failed to update CLAUDE.md: {}", e);
        }
        // Set up petri hook for Claude Code users (only if .claude/ already exists)
        if vault_path.join(".claude").exists() {
            if let Err(e) = setup_petri_hook(vault_path) {
                tracing::warn!("failed to set up petri hook: {}", e);
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    tx.send(DisplayEvent::Done { duration_ms }).ok();

    // Wait for display thread to finish rendering
    display.finish();

    // Show starter prompt and bonsai CTA on interactive init
    if !quiet {
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
            "  {}A bonsai guide is shaped to your vault — sharper catalysts, better questions.{}",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Grey),
            crossterm::style::ResetColor,
        );
        eprintln!(
            "  {}enzyme.garden/bonsai{} {}— join the waitlist{}",
            crossterm::style::SetForegroundColor(crate::display::colors::ENZYME_PURPLE),
            crossterm::style::ResetColor,
            crossterm::style::SetForegroundColor(crossterm::style::Color::Grey),
            crossterm::style::ResetColor,
        );
        eprintln!();
    }

    if quiet {
        Ok(Some(InitResult {
            files_discovered: idx_stats.files_discovered,
            files_new: idx_stats.files_new,
            files_updated: idx_stats.files_updated,
            files_deleted: idx_stats.files_deleted,
            catalysts_generated: cat_stats.as_ref().map_or(0, |s| s.catalysts_generated),
            embeddings_documents: emb_stats.map(|(docs, _)| docs),
            embeddings_pruned: emb_stats.map(|(_, pruned)| pruned),
            semantic_search: emb_completed,
            catalysts_available: cat_stats.is_some(),
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
    let staleness = check_catalyst_staleness(&db, vault_path)?;

    if matches!(staleness, StalenessResult::Fresh) {
        return Ok(());
    }

    drop(db);

    // Run init in quiet mode (suppresses all output)
    let _ = run_init(vault_path, false, false, true, false, None, None).await?;

    // Store bookkeeping for next staleness check
    let db = Database::open(&paths.vault_db)?;
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names: Vec<String> = sel_result.entities.iter().map(|e| e.name.clone()).collect();
    store_staleness_bookkeeping(&db, vault_path, &entity_names)?;

    Ok(())
}

/// Lightweight refresh: dispatches to the appropriate path based on flags.
///
/// - `--detached` → `run_refresh_detached` (background lockfile-guarded catalyst run)
/// - `--quiet` without `--full` → `run_refresh_fast` (sync hot path for hooks)
/// - Otherwise → interactive full pipeline
pub async fn run_refresh(
    vault_path: &Path,
    full: bool,
    detached: bool,
    quiet: bool,
    vault_guide: Option<&str>,
    min_top_catalysts: Option<usize>,
) -> Result<()> {
    if detached {
        return run_refresh_detached(vault_path).await;
    }
    if quiet && !full {
        return run_refresh_fast(vault_path).await;
    }
    run_refresh_interactive(vault_path, full, quiet, vault_guide, min_top_catalysts).await
}

/// Fast sync path — the hook hot path for `enzyme refresh --quiet`.
///
/// Runs only cheap local work (indexing, embedding, similarity) synchronously,
/// then spawns a background process for expensive LLM catalyst generation if
/// the vault is stale.
async fn run_refresh_fast(vault_path: &Path) -> Result<()> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    // If the vault was never initialized, don't create .enzyme — just bail.
    if !paths.vault_db.exists() {
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;

    // ── Step 1: Index (filesystem scan, fast) ────────────────────────────
    // Capture timestamp before indexing so we can scope embedding to only
    // the docs processed in this run.
    let index_start_ms = chrono::Utc::now().timestamp_millis();
    let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
    let idx_stats = indexing.run(false, |_msg| {})?;
    debug!(
        new = idx_stats.files_new,
        updated = idx_stats.files_updated,
        deleted = idx_stats.files_deleted,
        "fast refresh: indexing complete"
    );

    // ── Step 2: Warm the embedding daemon & embed changed docs ──────────
    // Always call get_embedding_service so the daemon stays warm across
    // invocations (avoids ~650ms cold-start on the first `catalyze` call).
    let svc = get_embedding_service(&paths.home).ok();

    let has_changes =
        idx_stats.files_new > 0 || idx_stats.files_updated > 0 || idx_stats.files_deleted > 0;
    let emb_produced = if has_changes {
        match svc {
            Some(svc) => {
                let pipeline = EmbeddingPipeline::new(&db, svc);
                match pipeline.run_since(index_start_ms, |_, _| {}) {
                    Ok(stats) => {
                        debug!(
                            docs = stats.documents_embedded,
                            "fast refresh: embedding complete"
                        );
                        stats.documents_embedded > 0
                    }
                    Err(e) => {
                        debug!("fast refresh: embedding failed: {}", e);
                        false
                    }
                }
            }
            None => false,
        }
    } else {
        false
    };

    // ── Step 3: Similarity (only if new embeddings and catalysts exist) ──
    if emb_produced {
        let has_catalysts = db
            .query_read("SELECT COUNT(*) FROM catalysts", vec![], |row| {
                row.get::<i64>(0)
            })
            .map(|rows| rows.first().copied().unwrap_or(0) > 0)
            .unwrap_or(false);

        if has_catalysts {
            if let Ok(svc) = get_embedding_service(&paths.home) {
                let sim_pipeline = SimilarityPipeline::new(&db, svc);
                if let Err(e) = sim_pipeline.run(|_, _| {}) {
                    debug!("fast refresh: similarity failed: {}", e);
                }
            }
        }
    }

    // ── Step 4: Check staleness → spawn background if needed ────────────
    let staleness = check_catalyst_staleness(&db, vault_path)?;
    let background_spawned = match &staleness {
        StalenessResult::Fresh => false,
        StalenessResult::Stale(reason) => {
            debug!(?reason, "fast refresh: stale, spawning background");
            spawn_background_refresh(vault_path, &paths)
        }
    };

    // ── Step 5: Output JSON ─────────────────────────────────────────────
    let duration_ms = start.elapsed().as_millis() as u64;
    let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
    let output = json!({
        "status": "completed",
        "fresh": staleness.is_fresh(),
        "background_spawned": background_spawned,
        "indexing": {
            "files_discovered": idx_stats.files_discovered,
            "files_new": idx_stats.files_new,
            "files_updated": idx_stats.files_updated,
            "files_deleted": idx_stats.files_deleted,
        },
        "duration_ms": duration_ms,
        "petri": petri,
    });
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}

/// Detached background path — runs the expensive LLM catalyst pipeline.
///
/// Called by `spawn_background_refresh` via `enzyme refresh --detached --quiet`.
/// Acquires a lockfile to prevent concurrent runs.
async fn run_refresh_detached(vault_path: &Path) -> Result<()> {
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

    let db = Database::open(&paths.vault_db)?;

    // ── Step 2: Re-check staleness (may have been resolved since spawn) ─
    let staleness = check_catalyst_staleness(&db, vault_path)?;
    let reason = match staleness {
        StalenessResult::Fresh => {
            info!("detached refresh: already fresh, exiting");
            return Ok(());
        }
        StalenessResult::Stale(reason) => reason,
    };

    // ── Step 3: Select entities ─────────────────────────────────────────
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;

    // Determine which entities to process based on staleness reason
    let entities_to_process = match reason.stale_entity_names() {
        Some(stale_names) => {
            // Partial refresh: only regenerate stale entities
            info!(
                count = stale_names.len(),
                "detached refresh: partial regeneration"
            );
            sel_result
                .entities
                .iter()
                .filter(|e| stale_names.contains(&e.name))
                .cloned()
                .collect::<Vec<_>>()
        }
        None => {
            // Full refresh
            info!(
                count = sel_result.entities.len(),
                "detached refresh: full regeneration"
            );
            sel_result.entities.clone()
        }
    };

    if entities_to_process.is_empty() {
        info!("detached refresh: no entities to process");
        return Ok(());
    }

    // ── Step 4: Run catalyst pipeline ───────────────────────────────────
    let api_config = resolve_api_config().await?;
    let llm = LlmClient::from_config(&api_config);
    let svc = get_embedding_service(&paths.home).ok();
    let cat_pipeline = CatalystPipeline::new(&db, &llm, svc);

    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();

    let vault_context = {
        let ctx_builder = ContextBuilder::new(&db);
        ctx_builder.build(&entities_to_process, 5, 300).ok()
    };

    let detached_min_top = enzyme_config
        .trending
        .as_ref()
        .and_then(|t| t.min_top_catalysts);
    let allocations = calculate_allocations(&entities_to_process, detached_min_top);

    let cat_config = CatalystConfig {
        entity_profiles: &sel_result.entity_profiles,
        catalysts_per_entity: 7,
        default_profile: enzyme_config
            .catalyst_profile
            .as_deref()
            .unwrap_or(DEFAULT_PROFILE_KEY),
        format_key: enzyme_config
            .catalyst_format
            .as_deref()
            .unwrap_or(DEFAULT_FORMAT_KEY),
        vault_guide: vault_context.as_deref(),
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
    };

    let stats = cat_pipeline
        .run(&entities_to_process, &cat_config, |_event| {})
        .await?;

    info!(
        entities = stats.entities_processed,
        catalysts = stats.catalysts_generated,
        "detached refresh: catalyst generation complete"
    );

    // ── Step 5: Store bookkeeping ───────────────────────────────────────
    let is_partial = reason.stale_entity_names().is_some();
    if is_partial {
        let names: Vec<String> = entities_to_process.iter().map(|e| e.name.clone()).collect();
        store_staleness_bookkeeping_partial(&db, &names)?;
    } else {
        let entity_names: Vec<String> =
            sel_result.entities.iter().map(|e| e.name.clone()).collect();
        store_staleness_bookkeeping(&db, vault_path, &entity_names)?;
    }

    // ── Step 6: Recompute similarity ────────────────────────────────────
    if stats.catalysts_generated > 0 {
        if let Ok(svc) = get_embedding_service(&paths.home) {
            let sim_pipeline = SimilarityPipeline::new(&db, svc);
            if let Err(e) = sim_pipeline.run(|_, _| {}) {
                info!("detached refresh: similarity failed: {}", e);
            }
        }
    }

    // ── Step 7: Re-apply targets ───────────────────────────────────────
    if stats.catalysts_generated > 0 {
        reapply_targets(vault_path).await;
    }

    // Lock released on drop
    Ok(())
}

/// Re-apply all previously applied targets using the vault's current catalysts.
///
/// Merges targets from config.toml (declarative, survives DB resets) and the
/// `applied_targets` DB table (runtime record). Config is the source of truth;
/// DB targets that aren't in config are also re-applied for backward compat.
///
/// Called after catalyst regeneration so that target similarity mappings stay
/// in sync. Errors are logged but don't fail the parent operation.
async fn reapply_targets(vault_path: &Path) {
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
            info!(path = %target, "skipping re-apply: target path no longer exists");
            continue;
        }
        info!(path = %target, "re-applying target after catalyst regeneration");
        if let Err(e) = super::apply::run_apply(vault_path, &target_path, true).await {
            info!(path = %target, err = %e, "re-apply failed for target");
        }
    }
}

/// Spawn a background refresh process (detached child).
///
/// Returns `true` if a background process was spawned, `false` if skipped
/// (e.g., another refresh is already running).
fn spawn_background_refresh(vault_path: &Path, paths: &EnzymePaths) -> bool {
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
        .arg(vault_path)
        .stdin(std::process::Stdio::null())
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

/// Interactive refresh path — the original full pipeline with display output.
///
/// Used for `enzyme refresh` (no --quiet) and `enzyme refresh --full --quiet`.
async fn run_refresh_interactive(
    vault_path: &Path,
    full: bool,
    quiet: bool,
    vault_guide: Option<&str>,
    min_top_catalysts: Option<usize>,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Capture old entity names before init for diffing.
    let old_entity_names = if paths.vault_db.exists() {
        let db = Database::open(&paths.vault_db)?;
        let staleness = check_catalyst_staleness(&db, vault_path)?;

        match staleness {
            StalenessResult::Fresh => {
                debug!("catalysts are fresh — nothing to do");
                if quiet {
                    let petri = super::petri::build_petri_json(vault_path, 10, None, None)?;
                    let output = json!({
                        "status": "completed",
                        "fresh": true,
                        "petri": petri,
                    });
                    println!("{}", serde_json::to_string(&output)?);
                }
                return Ok(());
            }
            StalenessResult::Stale(reason) => {
                debug!(?reason, "catalysts are stale — running full init");
            }
        }

        load_stored_entity_names(&db).ok()
    } else {
        None
    };

    // Run the full pipeline.
    let init_result = run_init(
        vault_path,
        full,
        false,
        quiet,
        false,
        vault_guide,
        min_top_catalysts,
    )
    .await?;

    // Store bookkeeping for next staleness check.
    let db = Database::open(&paths.vault_db)?;
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names: Vec<String> = sel_result.entities.iter().map(|e| e.name.clone()).collect();
    store_staleness_bookkeeping(&db, vault_path, &entity_names)?;

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

    if quiet {
        if let Some(result) = init_result {
            let mut output = result.to_json(vault_path)?;

            if let Some((added, removed)) = entity_changes {
                output["entity_changes"] = json!({ "added": added, "removed": removed });
            }

            println!("{}", serde_json::to_string(&output)?);
        }
    } else {
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
    }

    Ok(())
}

/// Set up a UserPromptSubmit hook that auto-injects query-aware petri context.
///
/// Creates `.claude/hooks/enzyme-petri.sh` and merges the hook config into
/// `.claude/settings.json`. Idempotent — skips if already configured.
fn setup_petri_hook(vault_path: &Path) -> Result<()> {
    let claude_dir = vault_path.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;

    // Write hook script (always overwrite — it's generated)
    let hook_path = hooks_dir.join("enzyme-petri.sh");
    let script = format!(
        r#"#!/bin/bash
INPUT=$(cat)
PROMPT=$(echo "$INPUT" | jq -r '.prompt')
RESULTS=$(enzyme petri -p "{vault}" --query "$PROMPT" 2>/dev/null)
if [ $? -eq 0 ] && [ -n "$RESULTS" ]; then
  ESCAPED=$(echo "$RESULTS" | jq -Rs .)
  echo "{{\"additionalContext\": $ESCAPED}}"
fi
"#,
        vault = vault_path.display()
    );
    fs::write(&hook_path, &script)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
    }

    // Merge hook config into settings.json
    let settings_path = claude_dir.join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    let hook_command = ".claude/hooks/enzyme-petri.sh";
    let already_configured = settings
        .get("hooks")
        .and_then(|h| h.get("UserPromptSubmit"))
        .and_then(|ups| ups.as_array())
        .map(|arr| {
            arr.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command").and_then(|c| c.as_str()) == Some(hook_command)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if !already_configured {
        let hook_entry = json!({
            "hooks": [{
                "type": "command",
                "command": hook_command
            }]
        });

        let hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert(json!({}));
        let ups = hooks
            .as_object_mut()
            .unwrap()
            .entry("UserPromptSubmit")
            .or_insert(json!([]));
        ups.as_array_mut().unwrap().push(hook_entry);

        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
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
