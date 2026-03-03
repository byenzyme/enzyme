use std::path::Path;
use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::debug;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{LlmClient, resolve_api_config, DEFAULT_PROFILE_KEY, DEFAULT_FORMAT_KEY};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::catalysts::{CatalystPipeline, CatalystConfig, CatalystEvent};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::pipeline::staleness::{check_catalyst_staleness, store_staleness_bookkeeping, StalenessResult};

use crate::display::{DisplayEvent, InitDisplay};

pub async fn run_init(vault_path: &Path, full: bool, _detached: bool, quiet: bool, vault_guide: Option<&str>) -> Result<()> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent)
            .context("failed to create .enzyme directory")?;
    }

    // Open database
    let db = Database::open(&paths.vault_db)?;

    // Start display (quiet mode suppresses all output)
    let display = if quiet { InitDisplay::start_quiet() } else { InitDisplay::start() };
    let tx = display.sender();

    // ── Step 1: Indexing ───────────────────────────────────────────────────
    tx.send(DisplayEvent::IndexingStart).ok();
    let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
    let tx_idx = tx.clone();
    let idx_stats = indexing.run(full, |msg| {
        tx_idx.send(DisplayEvent::IndexingProgress(msg.to_string())).ok();
    })?;

    tx.send(DisplayEvent::IndexingComplete {
        files_discovered: idx_stats.files_discovered,
        files_new: idx_stats.files_new,
        files_updated: idx_stats.files_updated,
        files_deleted: idx_stats.files_deleted,
    }).ok();

    // ── Step 2: Entity selection ──────────────────────────────────────────
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;

    tx.send(DisplayEvent::SelectionComplete {
        count: sel_result.entities.len(),
        source: sel_result.source.clone(),
    }).ok();

    // ── Step 2b: Build XML context from DB ───────────────────────────────
    let vault_context = {
        let ctx_builder = ContextBuilder::new(&db);
        match ctx_builder.build(&sel_result.entities, 5, 300) {
            Ok(ctx) => {
                debug!("built vault context XML ({} bytes, {} entities)",
                    ctx.xml.len(), ctx.entity_contexts.len());
                Some(ctx)
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
            tx.send(DisplayEvent::EmbeddingSkipped(format!("model not found: {}", e))).ok();
            None
        }
    };

    let api_config = match resolve_api_config().await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("API config resolution failed: {}", e);
            None
        }
    };

    // ── Step 4: Launch doc embedding in background ────────────────────────
    let embedding_handle = if let Some(svc) = service {
        let db_path = paths.vault_db.clone();
        let embeddings_dir = paths.embeddings_dir.clone();
        let tx_emb = tx.clone();
        tx.send(DisplayEvent::EmbeddingStart).ok();
        Some(tokio::task::spawn_blocking(move || {
            let bg_db = Database::open(&db_path)?;
            let pipeline = EmbeddingPipeline::new(&bg_db, svc, embeddings_dir);
            pipeline.run(|done, total| {
                tx_emb.send(DisplayEvent::EmbeddingProgress { done, total }).ok();
            })
        }))
    } else {
        None
    };

    // ── Step 5: Catalyst generation in foreground ─────────────────────────
    // Load enzyme config for catalyst profile/format settings
    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();

    let cat_stats = if let Some(resolved) = api_config {
        let total = sel_result.entities.len();
        tx.send(DisplayEvent::DiscoveryStart { total }).ok();

        let llm = LlmClient::with_base_url(&resolved.api_key, &resolved.base_url);
        let cat_pipeline = CatalystPipeline::new(&db, &llm);
        let tx_cat = tx.clone();

        // Prefer built context XML; fall back to externally provided vault_guide
        let effective_vault_guide = vault_context
            .as_ref()
            .map(|ctx| ctx.xml.as_str())
            .or(vault_guide);

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
            custom_guidance: enzyme_config.custom_profile_guidance.as_ref(),
            guide_signal: enzyme_config.guide_signal.as_deref(),
            vault_guide: effective_vault_guide,
            model: &resolved.model,
        };

        let stats = cat_pipeline.run(
            &sel_result.entities,
            &cat_config,
            move |event| {
                match event {
                    CatalystEvent::EntityCompleted {
                        entity_name,
                        entity_type,
                        catalyst_texts,
                        completed,
                        total,
                    } => {
                        tx_cat.send(DisplayEvent::EntityDiscovered {
                            entity_name,
                            entity_type: parse_entity_type(&entity_type),
                            catalyst_texts,
                            completed,
                            total,
                        }).ok();
                    }
                    CatalystEvent::EntitySkipped {
                        entity_name,
                        completed,
                        total,
                    } => {
                        tx_cat.send(DisplayEvent::EntitySkipped {
                            entity_name,
                            completed,
                            total,
                        }).ok();
                    }
                    CatalystEvent::EntityFailed {
                        entity_name,
                        entity_type,
                        error,
                        completed,
                        total,
                    } => {
                        tx_cat.send(DisplayEvent::EntityFailed {
                            entity_name,
                            entity_type: parse_entity_type(&entity_type),
                            error,
                            completed,
                            total,
                        }).ok();
                    }
                    CatalystEvent::EntityStarted { entity_name, entity_type } => {
                        tx_cat.send(DisplayEvent::EntityStarted {
                            entity_name,
                            entity_type: parse_entity_type(&entity_type),
                        }).ok();
                    }
                    CatalystEvent::CatalystPartial {
                        entity_name,
                        entity_type: _,
                        catalyst_text,
                        catalyst_index,
                    } => {
                        tx_cat.send(DisplayEvent::CatalystPartial {
                            entity_name,
                            catalyst_text,
                            catalyst_index,
                        }).ok();
                    }
                }
            },
        ).await?;

        tx.send(DisplayEvent::DiscoveryComplete {
            entities_processed: stats.entities_processed,
            catalysts_generated: stats.catalysts_generated,
        }).ok();

        if let Some(ref prompt) = stats.starter_prompt {
            tx.send(DisplayEvent::StarterPrompt(prompt.clone())).ok();
        }

        Some(stats)
    } else {
        tx.send(DisplayEvent::ApiSkipped(
            "No API key found and Enzyme server unreachable — skipping catalyst generation. Set OPENAI_API_KEY or ensure network access.".to_string()
        )).ok();
        None
    };

    // ── Step 6: Wait for background embedding ─────────────────────────────
    let emb_completed = if let Some(handle) = embedding_handle {
        match handle.await.context("embedding task panicked")? {
            Ok(stats) => {
                tx.send(DisplayEvent::EmbeddingComplete {
                    documents: stats.documents_embedded,
                    pruned: stats.documents_pruned,
                    duration_ms: stats.duration_ms,
                }).ok();
                true
            }
            Err(e) => {
                tx.send(DisplayEvent::EmbeddingFailed(format!("{}", e))).ok();
                false
            }
        }
    } else {
        false
    };

    // ── Step 7: Similarity computation ────────────────────────────────────
    if emb_completed && cat_stats.as_ref().map_or(false, |s| s.catalysts_generated > 0) {
        tx.send(DisplayEvent::SimilarityStart).ok();
        let svc = service.unwrap(); // safe: emb_completed implies service was Some
        let sim_pipeline = SimilarityPipeline::new(&db, svc, paths.embeddings_dir.clone());
        let tx_sim = tx.clone();
        let sim_stats = sim_pipeline.run(|done, total| {
            tx_sim.send(DisplayEvent::SimilarityProgress { done, total }).ok();
        })?;

        tx.send(DisplayEvent::SimilarityComplete {
            catalysts: sim_stats.catalysts_processed,
            documents: sim_stats.documents_compared,
        }).ok();
    }

    // ── Step 8: Generate CLAUDE.md ───────────────────────────────────────
    if let Err(e) = super::claude_md::generate_claude_md(vault_path) {
        tracing::warn!("failed to update CLAUDE.md: {}", e);
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    tx.send(DisplayEvent::Done { duration_ms }).ok();

    // Wait for display thread to finish rendering
    display.finish();

    Ok(())
}

/// Lightweight refresh: check staleness first, only run full init if stale.
///
/// After a successful init, stores bookkeeping (config hash + entity doc counts)
/// so the next refresh can detect whether anything changed.
pub async fn run_refresh(vault_path: &Path, full: bool, detached: bool, quiet: bool, vault_guide: Option<&str>) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // If the DB doesn't exist yet, fall through to full init.
    if paths.vault_db.exists() {
        let db = Database::open(&paths.vault_db)?;
        let staleness = check_catalyst_staleness(&db, vault_path)?;

        match staleness {
            StalenessResult::Fresh => {
                debug!("catalysts are fresh — nothing to do");
                return Ok(());
            }
            StalenessResult::Stale(reason) => {
                debug!(?reason, "catalysts are stale — running full init");
            }
        }
    }

    // Run the full pipeline.
    run_init(vault_path, full, detached, quiet, vault_guide).await?;

    // Store bookkeeping for next staleness check.
    let db = Database::open(&paths.vault_db)?;
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names: Vec<String> = sel_result.entities.iter().map(|e| e.name.clone()).collect();
    store_staleness_bookkeeping(&db, vault_path, &entity_names)?;

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
