use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tracing::info;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;

pub async fn run_apply(source_path: &Path, target_path: &Path, _detached: bool) -> Result<()> {
    let start = Instant::now();
    info!(?source_path, ?target_path, "enzyme apply");

    // ── Phase 1: Setup ─────────────────────────────────────────────────────
    // Load embedding model and source catalysts.
    let source_paths = EnzymePaths::new(source_path)?;
    let target_paths = EnzymePaths::new(target_path)?;

    let service = get_embedding_service(&source_paths.home)?;

    // Load catalysts from source vault
    if !source_paths.vault_db.exists() {
        bail!(
            "source vault has no enzyme database — run `enzyme init` on {} first",
            source_path.display()
        );
    }
    let source_db = Database::open(&source_paths.vault_db)?;
    let source_repo = Repository::new(&source_db);
    let catalysts = source_repo.get_all_catalysts()?;
    if catalysts.is_empty() {
        bail!(
            "source vault has no catalysts — run `enzyme init` on {} first",
            source_path.display()
        );
    }

    println!("enzyme apply");
    println!("  source: {} ({} catalysts)", source_path.display(), catalysts.len());
    println!("  target: {}", target_path.display());
    println!();

    // ── Phase 2: Index target ──────────────────────────────────────────────
    // Ensure target .enzyme directory exists
    if let Some(parent) = target_paths.vault_db.parent() {
        fs::create_dir_all(parent)
            .context("failed to create target .enzyme directory")?;
    }

    let target_db = Database::open(&target_paths.vault_db)?;
    let indexing = IndexingPipeline::new(&target_db, target_path.to_path_buf());
    let idx_stats = indexing.run(false, |_msg| {})?;
    println!(
        "  indexing target... {} files discovered",
        idx_stats.files_discovered
    );

    // ── Phase 3: Embed target (spawn_blocking — CPU-bound ONNX) ────────────
    let target_db_path = target_paths.vault_db.clone();
    let target_embeddings_dir = target_paths.embeddings_dir.clone();

    let emb_stats = tokio::task::spawn_blocking({
        let target_db_path = target_db_path.clone();
        let target_embeddings_dir = target_embeddings_dir.clone();
        move || {
            let db = Database::open(&target_db_path)?;
            let pipeline = EmbeddingPipeline::new(&db, service, target_embeddings_dir);
            pipeline.run(|done, total| {
                print!("\r  embedding... {}/{} documents", done, total);
            })
        }
    })
    .await
    .context("embedding task panicked")??;

    println!(
        "\r  embedding... {}/{} documents",
        emb_stats.documents_embedded, emb_stats.documents_embedded
    );

    // ── Phase 4: Similarity (spawn_blocking — CPU-bound vector math) ───────
    let catalyst_count = catalysts.len();
    let sim_stats = tokio::task::spawn_blocking(move || {
        let db = Database::open(&target_db_path)?;
        let pipeline = SimilarityPipeline::new(&db, service, target_embeddings_dir);
        pipeline.run_with_catalysts(&catalysts, true, |done, total| {
            print!("\r  computing similarities... {}/{} catalysts", done, total);
        })
    })
    .await
    .context("similarity task panicked")??;

    println!(
        "\r  computing similarities... {}/{} catalysts",
        catalyst_count, catalyst_count
    );

    let duration_secs = start.elapsed().as_secs_f64();
    println!();
    println!("  done in {:.1}s", duration_secs);
    println!(
        "  {} similarities stored in {}",
        sim_stats.similarities_computed,
        target_paths.vault_db.display()
    );

    Ok(())
}
