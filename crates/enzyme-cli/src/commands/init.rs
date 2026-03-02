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

    // Step 1: Indexing
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

    // Step 2: Embeddings
    match get_embedding_service(&paths.home) {
        Ok(service) => {
            println!("Generating embeddings...");

            let emb_pipeline = EmbeddingPipeline::new(&db, service, paths.embeddings_dir.clone());
            let emb_stats = emb_pipeline.run(|done, total| {
                if total > 0 {
                    println!("  Embedded {}/{} documents", done, total);
                }
            })?;

            println!(
                "  Embedded: {} documents, {} chunks",
                emb_stats.documents_embedded,
                emb_stats.chunks_embedded,
            );

            // Step 3: Catalysts (if API key available)
            match resolve_api_config() {
                Ok((api_key, base_url)) => {
                    println!("Generating catalysts...");
                    let llm = LlmClient::with_base_url(&api_key, &base_url);
                    let cat_pipeline = CatalystPipeline::new(&db, &llm);
                    let cat_stats = cat_pipeline.run(30, 7, |entity, i, total| {
                        println!("  [{}/{}] {}", i, total, entity);
                    }).await?;

                    println!(
                        "  Catalysts: {} entities processed, {} catalysts generated",
                        cat_stats.entities_processed,
                        cat_stats.catalysts_generated,
                    );

                    if !cat_stats.errors.is_empty() {
                        println!("  Errors: {}", cat_stats.errors.len());
                    }

                    // Step 4: Similarity computation
                    println!("Computing similarities...");
                    let sim_pipeline = SimilarityPipeline::new(&db, service, paths.embeddings_dir.clone());
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
                Err(_) => {
                    println!("No OpenRouter API key found — skipping catalyst generation.");
                    println!("Set OPENROUTER_API_KEY or create ~/.enzyme/auth.json to enable.");
                }
            }
        }
        Err(e) => {
            println!("Embedding model not found — skipping embeddings.");
            println!("  {}", e);
        }
    }

    println!("Done! ({}ms)", idx_stats.duration_ms);
    Ok(())
}
