use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tracing::info;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::models::AppliedTarget;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;

use crate::display::{DisplayEvent, InitDisplay};

pub async fn run_apply(source_path: &Path, target_path: &Path, _detached: bool) -> Result<()> {
    let start = Instant::now();
    info!(?source_path, ?target_path, "enzyme apply");

    // ── Phase 1: Setup ─────────────────────────────────────────────────────
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
    let catalysts = source_db.get_all_catalysts()?;
    if catalysts.is_empty() {
        bail!(
            "source vault has no catalysts — run `enzyme init` on {} first",
            source_path.display()
        );
    }

    // Print header before starting display (not animated)
    println!("enzyme apply");
    println!("  source: {} ({} catalysts)", source_path.display(), catalysts.len());
    println!("  target: {}", target_path.display());

    // Start animated display
    let display = InitDisplay::start();
    let tx = display.sender();

    // ── Phase 2: Index target ──────────────────────────────────────────────
    if let Some(parent) = target_paths.vault_db.parent() {
        fs::create_dir_all(parent)
            .context("failed to create target .enzyme directory")?;
    }

    tx.send(DisplayEvent::IndexingStart).ok();
    let target_db = Database::open(&target_paths.vault_db)?;
    let indexing = IndexingPipeline::new(&target_db, target_path.to_path_buf());
    let tx_idx = tx.clone();
    let idx_stats = indexing.run(false, |msg| {
        tx_idx.send(DisplayEvent::IndexingProgress(msg.to_string())).ok();
    })?;

    tx.send(DisplayEvent::IndexingComplete {
        files_discovered: idx_stats.files_discovered,
        files_new: idx_stats.files_new,
        files_updated: idx_stats.files_updated,
        files_deleted: idx_stats.files_deleted,
    }).ok();

    // ── Phase 3: Embed target (spawn_blocking — CPU-bound ONNX) ────────────
    let target_db_path = target_paths.vault_db.clone();
    let target_embeddings_dir = target_paths.embeddings_dir.clone();

    tx.send(DisplayEvent::EmbeddingStart).ok();
    let emb_stats = tokio::task::spawn_blocking({
        let target_db_path = target_db_path.clone();
        let target_embeddings_dir = target_embeddings_dir.clone();
        let tx_emb = tx.clone();
        move || {
            let db = Database::open(&target_db_path)?;
            let pipeline = EmbeddingPipeline::new(&db, service, target_embeddings_dir);
            pipeline.run_all(|done, total| {
                tx_emb.send(DisplayEvent::EmbeddingProgress { done, total }).ok();
            })
        }
    })
    .await
    .context("embedding task panicked")??;

    tx.send(DisplayEvent::EmbeddingComplete {
        documents: emb_stats.documents_embedded,
        pruned: emb_stats.documents_pruned,
        duration_ms: emb_stats.duration_ms,
    }).ok();

    // ── Phase 4: Similarity (spawn_blocking — CPU-bound vector math) ───────
    tx.send(DisplayEvent::SimilarityStart).ok();
    let catalyst_count = catalysts.len();
    let sim_stats = tokio::task::spawn_blocking({
        let tx_sim = tx.clone();
        move || {
            let db = Database::open(&target_db_path)?;
            let pipeline = SimilarityPipeline::new(&db, service, target_embeddings_dir);
            pipeline.run_with_catalysts(&catalysts, true, |done, total| {
                tx_sim.send(DisplayEvent::SimilarityProgress { done, total }).ok();
            })
        }
    })
    .await
    .context("similarity task panicked")??;

    tx.send(DisplayEvent::SimilarityComplete {
        catalysts: sim_stats.catalysts_processed,
        documents: sim_stats.documents_compared,
    }).ok();

    // ── Record applied target in source vault ─────────────────────────────
    let canonical_target = target_path
        .canonicalize()
        .unwrap_or_else(|_| target_path.to_path_buf());
    let source_db = Database::open(&source_paths.vault_db)?;
    source_db.upsert_applied_target(&AppliedTarget {
        id: None,
        target_path: canonical_target.to_string_lossy().to_string(),
        doc_count: idx_stats.files_discovered as i64,
        catalyst_count: catalyst_count as i64,
        applied_at: chrono::Utc::now(),
    })?;

    let duration_ms = start.elapsed().as_millis() as u64;
    tx.send(DisplayEvent::Done { duration_ms }).ok();

    display.finish();

    Ok(())
}
