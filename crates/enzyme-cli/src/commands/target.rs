use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::info;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::models::AppliedTarget;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;

use crate::display::{DisplayEvent, InitDisplay};

pub async fn prepare_target(source_path: &Path, target_path: &Path) -> Result<()> {
    let start = Instant::now();
    info!(?source_path, ?target_path, "preparing catalyze target");

    if !target_path.is_dir() {
        bail!("target is not a directory: {}", target_path.display());
    }

    // ── Phase 1: Setup ─────────────────────────────────────────────────────
    let source_paths = EnzymePaths::new(source_path)?;
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

    // Target DB lives in source vault's .enzyme/targets/ directory
    let target_db_path = source_paths.target_db(target_path);

    let display = InitDisplay::start_quiet();
    let tx = display.sender();

    // ── Phase 2: Index target ──────────────────────────────────────────────
    if let Some(parent) = target_db_path.parent() {
        fs::create_dir_all(parent).context("failed to create targets directory")?;
    }

    tx.send(DisplayEvent::IndexingStart).ok();
    let target_db = Database::open(&target_db_path)?;
    let indexing = IndexingPipeline::new(&target_db, target_path.to_path_buf());
    let tx_idx = tx.clone();
    let idx_stats = indexing.run(false, |msg| {
        tx_idx
            .send(DisplayEvent::IndexingProgress(msg.to_string()))
            .ok();
    })?;

    tx.send(DisplayEvent::IndexingComplete {
        files_discovered: idx_stats.files_discovered,
        files_new: idx_stats.files_new,
        files_updated: idx_stats.files_updated,
        files_deleted: idx_stats.files_deleted,
    })
    .ok();

    // ── Phase 3: Embed target (spawn_blocking — CPU-bound ONNX) ────────────
    tx.send(DisplayEvent::EmbeddingStart).ok();
    let emb_stats = tokio::task::spawn_blocking({
        let target_db_path = target_db_path.clone();
        let tx_emb = tx.clone();
        move || {
            let db = Database::open(&target_db_path)?;
            let pipeline = EmbeddingPipeline::new(&db, service);
            pipeline.run_all(|p| {
                tx_emb
                    .send(DisplayEvent::EmbeddingProgress {
                        done: p.done,
                        total: p.total,
                        current_date_ms: p.current_date_ms,
                    })
                    .ok();
            })
        }
    })
    .await
    .context("embedding task panicked")??;

    tx.send(DisplayEvent::EmbeddingComplete {
        chunks: emb_stats.chunks_embedded,
        pruned: emb_stats.documents_pruned,
        oldest_date_ms: emb_stats.oldest_embedded_ms,
        encode_ms: emb_stats.encode_ms,
    })
    .ok();

    // ── Phase 4: Similarity (spawn_blocking — CPU-bound vector math) ───────
    tx.send(DisplayEvent::SimilarityStart).ok();
    let catalyst_count = catalysts.len();
    let sim_stats = tokio::task::spawn_blocking({
        let tx_sim = tx.clone();
        move || {
            let db = Database::open(&target_db_path)?;
            let pipeline = SimilarityPipeline::new(&db, service);
            pipeline.run_with_catalysts(&catalysts, true, |done, total| {
                tx_sim
                    .send(DisplayEvent::SimilarityProgress { done, total })
                    .ok();
            })
        }
    })
    .await
    .context("similarity task panicked")??;

    tx.send(DisplayEvent::SimilarityComplete {
        catalysts: sim_stats.catalysts_processed,
        documents: sim_stats.documents_compared,
    })
    .ok();

    // ── Record target in source vault for future refreshes ────────────────
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

    // Persist target to config.toml so it survives DB resets
    if let Ok(Some(mut config)) = EnzymeConfig::load(source_path) {
        if let Err(e) = config.add_target(source_path, target_path) {
            tracing::warn!("failed to persist target to config: {}", e);
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    tx.send(DisplayEvent::Done { duration_ms }).ok();

    display.finish();

    Ok(())
}
