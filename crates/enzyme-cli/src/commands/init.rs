use std::path::Path;
use std::fs;

use anyhow::{Context, Result};

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{LlmClient, resolve_api_config};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::catalysts::CatalystPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;

pub async fn run_init(vault_path: &Path, full: bool, _detached: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Ensure .enzyme directory exists
    if let Some(parent) = paths.vault_db.parent() {
        fs::create_dir_all(parent)
            .context("failed to create .enzyme directory")?;
    }

    // Open database
    let db = Database::open(&paths.vault_db)?;

    // ── Step 1: Indexing ───────────────────────────────────────────────────
    println!("Indexing vault...");
    let indexing = IndexingPipeline::new(&db, vault_path.to_path_buf());
    let idx_stats = indexing.run(full, |msg| {
        println!("  {}", msg);
    })?;

    println!(
        "  Indexed: {} discovered, {} new, {} updated, {} deleted, {} entities",
        idx_stats.files_discovered,
        idx_stats.files_new,
        idx_stats.files_updated,
        idx_stats.files_deleted,
        idx_stats.entities_extracted,
    );

    // ── Step 2: Resolve resources upfront ──────────────────────────────────
    // Check both embedding model and API config before launching either task
    // so we can run them concurrently.
    let service = match get_embedding_service(&paths.home) {
        Ok(svc) => Some(svc),
        Err(e) => {
            println!("Embedding model not found — skipping embeddings.");
            println!("  {}", e);
            None
        }
    };

    let api_config = resolve_api_config().ok();

    // ── Step 3: Launch doc embedding in background ─────────────────────────
    // Embedding is CPU/GPU-bound (ONNX inference). It runs in spawn_blocking
    // with its own Database connection (SQLite WAL allows concurrent writers
    // with busy_timeout serialization).
    let embedding_handle = if let Some(svc) = service {
        let db_path = paths.vault_db.clone();
        let embeddings_dir = paths.embeddings_dir.clone();
        println!("Generating embeddings...");
        Some(tokio::task::spawn_blocking(move || {
            let bg_db = Database::open(&db_path)?;
            let pipeline = EmbeddingPipeline::new(&bg_db, svc, embeddings_dir);
            pipeline.run(|done, total| {
                if total > 0 {
                    println!("  [embed] {}/{} documents", done, total);
                }
            })
        }))
    } else {
        None
    };

    // ── Step 4: Catalyst generation in foreground ──────────────────────────
    // Catalysts are IO-bound (HTTP API calls) and already use tokio JoinSet
    // internally for per-entity concurrency. Running concurrently with
    // embedding hides the embedding latency behind LLM wait time.
    let cat_stats = if let Some((api_key, base_url)) = api_config {
        println!("Generating catalysts...");
        let llm = LlmClient::with_base_url(&api_key, &base_url);
        let cat_pipeline = CatalystPipeline::new(&db, &llm);
        let stats = cat_pipeline.run(30, 7, |entity, i, total| {
            println!("  [catalyst] [{}/{}] {}", i, total, entity);
        }).await?;

        println!(
            "  Catalysts: {} entities processed, {} catalysts generated",
            stats.entities_processed,
            stats.catalysts_generated,
        );

        if !stats.errors.is_empty() {
            println!("  Catalyst errors: {}", stats.errors.len());
        }

        Some(stats)
    } else {
        println!("No OpenRouter API key found — skipping catalyst generation.");
        println!("Set OPENROUTER_API_KEY or create ~/.enzyme/auth.json to enable.");
        None
    };

    // ── Step 5: Wait for background embedding ──────────────────────────────
    let emb_completed = if let Some(handle) = embedding_handle {
        match handle.await.context("embedding task panicked")? {
            Ok(stats) => {
                println!(
                    "  Embedded: {} documents, {} chunks",
                    stats.documents_embedded,
                    stats.chunks_embedded,
                );
                true
            }
            Err(e) => {
                println!("  Embedding failed: {}", e);
                false
            }
        }
    } else {
        false
    };

    // ── Step 6: Similarity computation ─────────────────────────────────────
    // Needs both embeddings on disk and catalysts in DB.
    // Reuses the same &'static EmbeddingService for catalyst text embedding.
    if emb_completed && cat_stats.as_ref().map_or(false, |s| s.catalysts_generated > 0) {
        println!("Computing similarities...");
        let svc = service.unwrap(); // safe: emb_completed implies service was Some
        let sim_pipeline = SimilarityPipeline::new(&db, svc, paths.embeddings_dir.clone());
        let sim_stats = sim_pipeline.run(|done, total| {
            if total > 0 && done % 10 == 0 {
                println!("  [{}/{}] catalysts processed", done, total);
            }
        })?;

        println!(
            "  Similarities: {} catalysts, {} computed",
            sim_stats.catalysts_processed,
            sim_stats.similarities_computed,
        );
    }

    println!("Done! ({}ms)", idx_stats.duration_ms);
    Ok(())
}
