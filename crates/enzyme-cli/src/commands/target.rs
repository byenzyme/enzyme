use std::fs;
use std::path::{Path, PathBuf};
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
use enzyme_core::pipeline::similarity::{SimilarityPipeline, catalysts_fingerprint};

use crate::display::{CompletionStatus, DisplayEvent, InitDisplay};

/// Resolves a `--target` argument. A relative path that is not a directory from
/// the current directory is tried against the vault root, so the vault-relative
/// paths that `petri` lists work from anywhere.
pub fn resolve_target_path(vault_path: &Path, target_path: &Path) -> PathBuf {
    if target_path.is_relative() && !target_path.is_dir() {
        let in_vault = vault_path.join(target_path);
        if in_vault.is_dir() {
            return in_vault;
        }
    }
    target_path.to_path_buf()
}

/// How `petri` shows a prepared target: relative to the vault when it lies
/// inside it, absolute otherwise, and not at all once its folder is gone.
pub fn display_target_path(vault_path: &Path, stored_path: &str) -> Option<String> {
    let target = Path::new(stored_path);
    if !target.is_dir() {
        return None;
    }
    let vault = vault_path
        .canonicalize()
        .unwrap_or_else(|_| vault_path.to_path_buf());
    match target.strip_prefix(&vault) {
        Ok(rel) if rel.as_os_str().is_empty() => Some(".".to_string()),
        Ok(rel) => Some(rel.to_string_lossy().to_string()),
        Err(_) => Some(stored_path.to_string()),
    }
}

pub async fn prepare_target(source_path: &Path, target_path: &Path) -> Result<()> {
    for _ in 0..4 {
        if prepare_target_once(source_path, target_path).await? {
            return Ok(());
        }
        info!("source epoch changed during target preparation; restaging latest epoch");
    }
    bail!("source catalyst epoch changed repeatedly during target preparation")
}

async fn prepare_target_once(source_path: &Path, target_path: &Path) -> Result<bool> {
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
    let source_epoch = source_db
        .get_epoch_state()?
        .active_epoch_id
        .context("source has no active catalyst epoch")?;
    let catalysts = source_db.get_all_catalysts()?;
    if catalysts.is_empty() {
        bail!(
            "source vault has no catalysts — run `enzyme init` on {} first",
            source_path.display()
        );
    }

    // Target DB lives in source vault's .enzyme/targets/ directory
    let target_db_path = source_paths.target_db(target_path);
    let _projection_lock = super::lock::acquire_projection_lock(
        target_db_path
            .parent()
            .context("target database has no parent directory")?,
    )?;

    let display = InitDisplay::start_quiet();
    let tx = display.sender();

    // ── Phase 2: Index target ──────────────────────────────────────────────
    if let Some(parent) = target_db_path.parent() {
        fs::create_dir_all(parent).context("failed to create targets directory")?;
    }

    tx.send(DisplayEvent::IndexingStart).ok();
    let target_db = Database::open(&target_db_path)?;
    let recovery_required = target_db.continuous_projection_fence()?.dirty;
    let projection_token = target_db.begin_continuous_projection()?;
    let before_embeddings = target_db.get_all_chunk_embeddings()?;
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
    //
    // Warm-path skip: if nothing changed in the target (indexing + embedding
    // both reported no mutations) AND the catalyst set is identical to the last
    // run (fingerprint stored in the target DB matches), skip the expensive
    // delete + recompute cycle entirely.
    const CATALYST_FINGERPRINT_KEY: &str = "catalyst_fingerprint";

    let idx_unchanged =
        idx_stats.files_new == 0 && idx_stats.files_updated == 0 && idx_stats.files_deleted == 0;
    let emb_unchanged = emb_stats.chunks_embedded == 0 && emb_stats.documents_pruned == 0;

    let current_fingerprint = catalysts_fingerprint(&catalysts);
    let stored_fingerprint = {
        let tdb = Database::open(&target_db_path)?;
        tdb.get_config(CATALYST_FINGERPRINT_KEY)?
    };
    let fingerprint_matches = stored_fingerprint.as_deref() == Some(current_fingerprint.as_str());

    let skip_similarity =
        !recovery_required && idx_unchanged && emb_unchanged && fingerprint_matches;

    let catalyst_count = catalysts.len();

    let _sim_stats = if skip_similarity {
        info!("catalyst similarities up to date; skipping recompute");
        // Still emit display events so callers see consistent output.
        tx.send(DisplayEvent::SimilarityStart).ok();
        tx.send(DisplayEvent::SimilarityComplete {
            catalysts: catalyst_count,
            documents: 0,
        })
        .ok();
        // Return a zero-work stats struct.
        enzyme_core::pipeline::similarity::SimilarityStats {
            catalysts_processed: catalyst_count,
            similarities_computed: 0,
            documents_compared: 0,
            duration_ms: 0,
        }
    } else if fingerprint_matches && !recovery_required {
        tx.send(DisplayEvent::SimilarityStart).ok();
        let after_embeddings = target_db.get_all_chunk_embeddings()?;
        let incremental = SimilarityPipeline::new(&target_db, service)
            .maintain_incremental(&before_embeddings, &after_embeddings)?;
        tx.send(DisplayEvent::SimilarityComplete {
            catalysts: catalyst_count,
            documents: idx_stats.files_discovered,
        })
        .ok();
        enzyme_core::pipeline::similarity::SimilarityStats {
            catalysts_processed: catalyst_count,
            similarities_computed: incremental.rows_upserted,
            documents_compared: idx_stats.files_discovered,
            duration_ms: incremental.duration_ms,
        }
    } else {
        tx.send(DisplayEvent::SimilarityStart).ok();
        let sim_stats = tokio::task::spawn_blocking({
            let tx_sim = tx.clone();
            let target_db_path = target_db_path.clone();
            move || {
                let db = Database::open(&target_db_path)?;
                let pipeline = SimilarityPipeline::new(&db, service);
                pipeline.run_with_catalysts(&catalysts, |done, total| {
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

        // Persist fingerprint so the next warm run can skip.
        let tdb = Database::open(&target_db_path)?;
        tdb.set_config(CATALYST_FINGERPRINT_KEY, &current_fingerprint)?;

        sim_stats
    };

    // ── Record target in source vault for future refreshes ────────────────
    let canonical_target = target_path
        .canonicalize()
        .unwrap_or_else(|_| target_path.to_path_buf());
    let source_db = Database::open(&source_paths.vault_db)?;
    target_db.stage_epoch_projection_from_shadow(&target_db, &source_epoch)?;
    target_db.finish_continuous_projection(&projection_token)?;
    source_db.upsert_applied_target(&AppliedTarget {
        id: None,
        target_path: canonical_target.to_string_lossy().to_string(),
        doc_count: idx_stats.files_discovered as i64,
        catalyst_count: catalyst_count as i64,
        applied_at: chrono::Utc::now(),
    })?;

    // Persist target to config.toml so it survives DB resets
    let mut config = EnzymeConfig::load(source_path)?.unwrap_or_default();
    if let Err(e) = config.add_target(source_path, target_path) {
        tracing::warn!("failed to persist target to config: {}", e);
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    tx.send(DisplayEvent::Done {
        duration_ms,
        status: CompletionStatus::Done,
        message: None,
    })
    .ok();

    display.finish();

    Ok(source_db.get_epoch_state()?.active_epoch_id.as_deref() == Some(source_epoch.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn petri_shows_targets_inside_the_vault_relative_to_it() {
        let vault = tempfile::tempdir().unwrap();
        let inside = vault.path().join("Readwise");
        fs::create_dir(&inside).unwrap();
        let stored = inside.canonicalize().unwrap();
        assert_eq!(
            display_target_path(vault.path(), &stored.to_string_lossy()).as_deref(),
            Some("Readwise")
        );
    }

    #[test]
    fn petri_keeps_outside_targets_absolute_and_drops_missing_ones() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let stored = outside.path().canonicalize().unwrap();
        let stored = stored.to_string_lossy();
        assert_eq!(
            display_target_path(vault.path(), &stored).as_deref(),
            Some(stored.as_ref())
        );
        let gone = stored.to_string() + "/missing";
        assert_eq!(display_target_path(vault.path(), &gone), None);
    }

    #[test]
    fn target_arguments_resolve_against_the_vault_when_not_found_locally() {
        let vault = tempfile::tempdir().unwrap();
        fs::create_dir(vault.path().join("Readwise-test-target")).unwrap();
        assert_eq!(
            resolve_target_path(vault.path(), Path::new("Readwise-test-target")),
            vault.path().join("Readwise-test-target")
        );
        let absolute = vault.path().to_path_buf();
        assert_eq!(resolve_target_path(vault.path(), &absolute), absolute);
    }
}
