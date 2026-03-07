use std::io::Write;
use std::path::Path;
use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::debug;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{LlmClient, resolve_api_config, DEFAULT_PROFILE_KEY, DEFAULT_FORMAT_KEY};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::catalysts::{CatalystPipeline, CatalystConfig, CatalystEvent, calculate_allocations};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::llm_selection::LlmSelectionPipeline;
use enzyme_core::pipeline::selection::{SelectionPipeline, SelectionSource};
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::pipeline::staleness::{check_catalyst_staleness, load_stored_entity_names, store_staleness_bookkeeping, StalenessResult};

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
        let petri = super::petri::build_petri_json(vault_path, 10)?;
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

pub async fn run_init(vault_path: &Path, full: bool, _detached: bool, quiet: bool, vault_guide: Option<&str>) -> Result<Option<InitResult>> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    // Check for existing enzyme data and offer hard reset
    if !quiet && atty::is(atty::Stream::Stdin) {
        let enzyme_dir = paths.vault_db.parent().map(|p| p.to_path_buf());
        let embeddings_dir = &paths.embeddings_dir;

        let enzyme_exists = enzyme_dir.as_ref().map_or(false, |d| d.exists());
        let embeddings_exists = embeddings_dir.exists();

        if enzyme_exists || embeddings_exists {
            let mut dirs_found = Vec::new();
            if enzyme_exists {
                dirs_found.push(enzyme_dir.as_ref().unwrap().display().to_string());
            }
            if embeddings_exists {
                dirs_found.push(embeddings_dir.display().to_string());
            }

            eprintln!("Found existing enzyme data:");
            for d in &dirs_found {
                eprintln!("  {}", d);
            }
            eprint!("Clear and hard reset? [y/N] ");
            std::io::stderr().flush().ok();

            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();

            if input.trim().eq_ignore_ascii_case("y") {
                if let Some(ref dir) = enzyme_dir {
                    if dir.exists() {
                        fs::remove_dir_all(dir).context("failed to remove enzyme data directory")?;
                        eprintln!("Cleared {}", dir.display());
                    }
                }
                if embeddings_dir.exists() {
                    fs::remove_dir_all(embeddings_dir).context("failed to remove embeddings directory")?;
                    eprintln!("Cleared {}", embeddings_dir.display());
                }
            }
        }
    }

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent)
            .context("failed to create .enzyme directory")?;
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

    // Start display (quiet mode suppresses all output)
    let display = if quiet { InitDisplay::start_quiet() } else { InitDisplay::start() };
    let tx = display.sender();
    let mut warnings: Vec<String> = Vec::new();

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
    let mut sel_result = selection.run()?;

    tx.send(DisplayEvent::SelectionComplete {
        count: sel_result.entities.len(),
        source: sel_result.source.clone(),
    }).ok();

    // ── Step 2a: LLM-assisted selection (first init only) ────────────────
    if matches!(sel_result.source, SelectionSource::Trending) {
        if let Some(ref resolved) = api_config {
            tx.send(DisplayEvent::LlmSelectionStart).ok();
            let llm = LlmClient::with_base_url(&resolved.api_key, &resolved.base_url);
            let llm_sel = LlmSelectionPipeline::new(&db, &llm);
            match llm_sel.run(&sel_result.entities, vault_path, &resolved.model).await {
                Ok(result) => {
                    tx.send(DisplayEvent::LlmSelectionComplete {
                        entities_selected: result.entities.len(),
                    }).ok();
                    sel_result = result;
                }
                Err(e) => {
                    let msg = format!("LLM selection failed: {e}");
                    tx.send(DisplayEvent::LlmSelectionSkipped(msg.clone())).ok();
                    warnings.push(msg);
                }
            }
        }
    }

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
            .as_deref()
            .or(vault_guide);

        let allocations = calculate_allocations(&sel_result.entities);

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
            per_entity_allocations: Some(&allocations),
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
                }).ok();
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

/// Lightweight refresh: check staleness first, only run full init if stale.
///
/// After a successful init, stores bookkeeping (config hash + entity doc counts)
/// so the next refresh can detect whether anything changed.
pub async fn run_refresh(vault_path: &Path, full: bool, detached: bool, quiet: bool, vault_guide: Option<&str>) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Capture old entity names before init for diffing.
    let old_entity_names = if paths.vault_db.exists() {
        let db = Database::open(&paths.vault_db)?;
        let staleness = check_catalyst_staleness(&db, vault_path)?;

        match staleness {
            StalenessResult::Fresh => {
                debug!("catalysts are fresh — nothing to do");
                if quiet {
                    let petri = super::petri::build_petri_json(vault_path, 10)?;
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
    let init_result = run_init(vault_path, full, detached, quiet, vault_guide).await?;

    // Store bookkeeping for next staleness check.
    let db = Database::open(&paths.vault_db)?;
    let selection = SelectionPipeline::new(&db, vault_path);
    let sel_result = selection.run()?;
    let entity_names: Vec<String> = sel_result.entities.iter().map(|e| e.name.clone()).collect();
    store_staleness_bookkeeping(&db, vault_path, &entity_names)?;

    // Compute entity set diff if we had a previous snapshot.
    let new_names: std::collections::HashSet<String> =
        entity_names.iter().cloned().collect();
    let entity_changes = if let Some(ref old_names) = old_entity_names {
        if !old_names.is_empty() {
            let mut added: Vec<&String> = new_names.difference(old_names).collect();
            let mut removed: Vec<&String> = old_names.difference(&new_names).collect();
            added.sort();
            removed.sort();

            if !added.is_empty() || !removed.is_empty() {
                Some((added.into_iter().cloned().collect::<Vec<_>>(), removed.into_iter().cloned().collect::<Vec<_>>()))
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

fn parse_entity_type(s: &str) -> enzyme_core::models::EntityType {
    match s {
        "tag" => enzyme_core::models::EntityType::Tag,
        "link" => enzyme_core::models::EntityType::Link,
        "folder" => enzyme_core::models::EntityType::Folder,
        _ => enzyme_core::models::EntityType::Tag,
    }
}
