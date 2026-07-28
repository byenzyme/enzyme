use libsql::{Connection, Value};
use tracing::info;

use crate::document::strip_frontmatter;
use crate::embedding::quantize;
use crate::models::{EMBEDDING_DIMENSION, INLINE_CONTEXT_MAX_WORDS, representative_excerpt};

use super::errors::DbError;
use super::schema;

/// Config key guarding the one-time `entity_occurrences` context slimming
/// migration (see [`slim_oversized_occurrence_contexts`]).
const CONTEXT_SLIM_MIGRATION_KEY: &str = "migration.context_slim_v1";

/// Config key guarding the one-time backfill that reconstructs bounded
/// excerpts for long-note contexts that an earlier slim migration nulled out
/// (see [`backfill_excerpts_for_slimmed_contexts`]).
const CONTEXT_EXCERPT_BACKFILL_KEY: &str = "migration.context_excerpt_backfill_v1";

/// Config key guarding the one-time embedding int8 quantization migration
/// (see [`migrate_embeddings_to_int8`]).
const EMBEDDING_INT8_MIGRATION_KEY: &str = "migration.embedding_int8_v1";

/// Only bother with the one-time post-migration VACUUM when it actually
/// freed enough to matter; below this the I/O outweighs the reclaim.
const VACUUM_RECLAIM_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Number of rows to batch per transaction when rewriting embedding blobs.
const EMBEDDING_MIGRATION_BATCH_SIZE: usize = 500;

/// Apply all pending migrations on a connection.
///
/// Called during `Database::open` after pragmas are set.
pub async fn run_all_on_conn(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(schema::SCHEMA_SQL)
        .await
        .map_err(DbError::Libsql)?;
    slim_oversized_occurrence_contexts(conn).await?;
    backfill_excerpts_for_slimmed_contexts(conn).await?;
    migrate_embeddings_to_int8(conn).await?;
    Ok(())
}

/// One-time data migration: older indexing runs stored full file bodies as
/// `entity_occurrences.context` for frontmatter tags/links, folders, and the
/// flat-vault root-folder entity — even when the body was large enough that
/// the catalyst pipeline's inline-context filter (`INLINE_CONTEXT_MAX_WORDS`)
/// would always discard it at read time. This walks existing rows once and
/// replaces each oversized body with a bounded [`representative_excerpt`]
/// (first + last words, kept under the filter) — matching what current
/// indexing stores — then records completion in `config` so subsequent opens
/// skip the scan entirely. Replacing (rather than nulling) keeps the note in
/// the entity query centroid while still reclaiming most of the bytes.
async fn slim_oversized_occurrence_contexts(conn: &Connection) -> Result<(), DbError> {
    // Cheap short-circuit: skip the scan entirely once already applied.
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(CONTEXT_SLIM_MIGRATION_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(());
    }

    let mut rows = conn
        .query(
            "SELECT id, context FROM entity_occurrences WHERE context IS NOT NULL",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;

    // (id, bounded excerpt) for each oversized context.
    let mut updates: Vec<(i64, String)> = Vec::new();
    let mut bytes_freed: u64 = 0;
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let context: Option<String> = row.get(1).map_err(DbError::Libsql)?;
        if let Some(context) = context {
            if context.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS {
                let excerpt = representative_excerpt(&context);
                // Only the difference is reclaimed; the excerpt stays behind.
                bytes_freed += (context.len() as u64).saturating_sub(excerpt.len() as u64);
                updates.push((id, excerpt));
            }
        }
    }
    drop(rows);

    if !updates.is_empty() {
        // Single transaction so the whole slim is atomic: a crash before commit
        // leaves the original bodies intact, and a crash after commit but before
        // the flag write simply finds no oversized rows on the next scan (the
        // excerpts are already under the threshold).
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        for (id, excerpt) in &updates {
            tx.execute(
                "UPDATE entity_occurrences SET context = ?1 WHERE id = ?2",
                vec![Value::Text(excerpt.clone()), Value::Integer(*id)],
            )
            .await
            .map_err(DbError::Libsql)?;
        }
        tx.commit().await.map_err(DbError::Libsql)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES (?1, '1')",
        vec![Value::Text(CONTEXT_SLIM_MIGRATION_KEY.to_string())],
    )
    .await
    .map_err(DbError::Libsql)?;

    if updates.is_empty() {
        info!("context-slim migration: no oversized entity_occurrences contexts found");
    } else {
        info!(
            rows_slimmed = updates.len(),
            approx_bytes_freed = bytes_freed,
            "context-slim migration: replaced oversized entity_occurrences contexts with excerpts"
        );
    }

    // One-time reclaim, tied to the same one-time migration: replacing bodies
    // with excerpts frees pages for reuse but never shrinks the file, and
    // nothing else in the lifecycle ever vacuums. Keyed on the actual freelist
    // rather than this run's freed bytes so an earlier interrupted run (UPDATE
    // committed, flag not yet written → rescan finds nothing oversized) still
    // gets its reclaim.
    reclaim_freelist_space(conn, "context-slim").await
}

/// One-time data migration for vaults that ran the *original* context-slim
/// migration, which nulled long-note contexts outright. A null context drops
/// the note from the entity query centroid entirely (the catalyst reader treats
/// null as "no inline context"), so those high-frequency / long-note entities
/// stopped contributing to their own query vectors. This walks null-context
/// occurrences whose source doc is still available, and backfills the same
/// bounded [`representative_excerpt`] that current indexing and the updated
/// slim migration produce.
///
/// Crash-idempotent: it only ever fills currently-null contexts, so a run
/// interrupted after some updates committed but before the completion flag is
/// written simply skips the already-filled rows on the next open.
async fn backfill_excerpts_for_slimmed_contexts(conn: &Connection) -> Result<(), DbError> {
    // Cheap short-circuit: skip the scan entirely once already applied.
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(CONTEXT_EXCERPT_BACKFILL_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(());
    }
    drop(check);

    // Null contexts joined to their source doc body. Multiple occurrences can
    // share a file_path (several frontmatter tags/links + folder + root); each
    // gets the same excerpt for that note.
    let mut rows = conn
        .query(
            "SELECT eo.id, d.content
             FROM entity_occurrences eo
             JOIN docs d ON d.source_ref = eo.file_path
             WHERE eo.context IS NULL AND d.content IS NOT NULL",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;

    let mut updates: Vec<(i64, String)> = Vec::new();
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let content: String = row.get(1).map_err(DbError::Libsql)?;
        let body = strip_frontmatter(&content);
        // Defensive: only long notes were ever nulled; skip anything that would
        // have been stored whole (its null came from something else).
        if body.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS {
            updates.push((id, representative_excerpt(body)));
        }
    }
    drop(rows);

    if !updates.is_empty() {
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        for (id, excerpt) in &updates {
            tx.execute(
                "UPDATE entity_occurrences SET context = ?1 WHERE id = ?2 AND context IS NULL",
                vec![Value::Text(excerpt.clone()), Value::Integer(*id)],
            )
            .await
            .map_err(DbError::Libsql)?;
        }
        tx.commit().await.map_err(DbError::Libsql)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES (?1, '1')",
        vec![Value::Text(CONTEXT_EXCERPT_BACKFILL_KEY.to_string())],
    )
    .await
    .map_err(DbError::Libsql)?;

    if updates.is_empty() {
        info!("context-excerpt backfill: no null long-note contexts to restore");
    } else {
        info!(
            rows_backfilled = updates.len(),
            "context-excerpt backfill: restored bounded excerpts for slimmed long notes"
        );
    }

    Ok(())
}

/// One-time data migration: rewrite `chunk_embeddings.vector` and the single
/// `catalyst_embeddings_packed` row from raw little-endian f32 to the
/// symmetric int8 `int8_v1` format (see [`crate::embedding::quantize`]),
/// shrinking on-disk embedding storage ~4x. Consumers keep operating on
/// `Vec<f32>`; only the on-disk representation changes.
///
/// Crash-idempotent: each row is only rewritten if its blob length doesn't
/// already match the `int8_v1` shape for the vault's expected embedding
/// dimension, so a run interrupted after some UPDATEs committed but before
/// the completion flag is written will simply skip the already-converted
/// rows on the next open and finish the rest.
async fn migrate_embeddings_to_int8(conn: &Connection) -> Result<(), DbError> {
    // Cheap short-circuit: skip the scan entirely once already applied.
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(EMBEDDING_INT8_MIGRATION_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(());
    }
    drop(check);

    let expected_dim = EMBEDDING_DIMENSION;
    let mut bytes_before: u64 = 0;
    let mut bytes_after: u64 = 0;

    // --- chunk_embeddings ----------------------------------------------------
    let mut rows = conn
        .query(
            "SELECT file_path, chunk_index, vector FROM chunk_embeddings",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;
    let mut chunk_updates: Vec<(String, i64, Vec<u8>)> = Vec::new();
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let file_path: String = row.get(0).map_err(DbError::Libsql)?;
        let chunk_index: i64 = row.get(1).map_err(DbError::Libsql)?;
        let blob: Vec<u8> = row.get(2).map_err(DbError::Libsql)?;
        if quantize::is_int8_v1_len(blob.len(), expected_dim) {
            continue; // already migrated in an earlier interrupted run
        }
        let vector = quantize::decode_vector(&blob, expected_dim);
        let new_blob = quantize::quantize_vector(&vector);
        bytes_before += blob.len() as u64;
        bytes_after += new_blob.len() as u64;
        chunk_updates.push((file_path, chunk_index, new_blob));
    }
    drop(rows);

    for batch in chunk_updates.chunks(EMBEDDING_MIGRATION_BATCH_SIZE) {
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        for (file_path, chunk_index, blob) in batch {
            tx.execute(
                "UPDATE chunk_embeddings SET vector = ?1 WHERE file_path = ?2 AND chunk_index = ?3",
                vec![
                    Value::Blob(blob.clone()),
                    Value::Text(file_path.clone()),
                    Value::Integer(*chunk_index),
                ],
            )
            .await
            .map_err(DbError::Libsql)?;
        }
        tx.commit().await.map_err(DbError::Libsql)?;
    }
    let chunk_rows_migrated = chunk_updates.len();

    // --- catalyst_embeddings_packed (single row, id = 1) ----------------------
    let mut packed_rows = conn
        .query(
            "SELECT count, vectors FROM catalyst_embeddings_packed WHERE id = 1",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;
    let packed_update = if let Some(row) = packed_rows.next().await.map_err(DbError::Libsql)? {
        let count: i64 = row.get(0).map_err(DbError::Libsql)?;
        let vectors_blob: Vec<u8> = row.get(1).map_err(DbError::Libsql)?;
        if count > 0 && !quantize::is_int8_v1_packed_len(vectors_blob.len(), count as usize, expected_dim)
        {
            let decoded = quantize::decode_packed(&vectors_blob, count as usize, expected_dim);
            let refs: Vec<&[f32]> = decoded.iter().map(|v| v.as_slice()).collect();
            let new_blob = quantize::quantize_packed(refs.into_iter());
            bytes_before += vectors_blob.len() as u64;
            bytes_after += new_blob.len() as u64;
            Some(new_blob)
        } else {
            None
        }
    } else {
        None
    };
    drop(packed_rows);

    let packed_row_migrated = packed_update.is_some();
    if let Some(new_blob) = packed_update {
        conn.execute(
            "UPDATE catalyst_embeddings_packed SET vectors = ?1 WHERE id = 1",
            vec![Value::Blob(new_blob)],
        )
        .await
        .map_err(DbError::Libsql)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES (?1, '1')",
        vec![Value::Text(EMBEDDING_INT8_MIGRATION_KEY.to_string())],
    )
    .await
    .map_err(DbError::Libsql)?;
    conn.execute(
        "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        vec![
            Value::Text(quantize::VECTOR_FORMAT_CONFIG_KEY.to_string()),
            Value::Text(quantize::VECTOR_FORMAT_INT8_V1.to_string()),
        ],
    )
    .await
    .map_err(DbError::Libsql)?;

    if chunk_rows_migrated == 0 && !packed_row_migrated {
        info!("embedding-int8 migration: no legacy f32 embedding blobs found");
    } else {
        info!(
            chunk_rows_migrated,
            packed_row_migrated,
            approx_bytes_freed = bytes_before.saturating_sub(bytes_after),
            "embedding-int8 migration: rewrote embedding blobs to int8_v1"
        );
    }

    reclaim_freelist_space(conn, "embedding-int8").await
}

/// Best-effort reclaim of freed pages via `VACUUM`, shared by one-time
/// migrations that null/shrink data in place (which frees pages for reuse
/// but never shrinks the file on its own). Keyed on the actual freelist
/// rather than the caller's own freed-byte estimate, so an earlier
/// interrupted migration run (data committed, completion flag not yet
/// written → rescan finds nothing to do) still gets its reclaim. Failure is
/// swallowed — a failed VACUUM (e.g. low disk for the transient copy) must
/// not fail `Database::open`.
async fn reclaim_freelist_space(conn: &Connection, migration_name: &str) -> Result<(), DbError> {
    let freelist_bytes = {
        let count = query_pragma_u64(conn, "PRAGMA freelist_count").await?;
        let page_size = query_pragma_u64(conn, "PRAGMA page_size").await?;
        count * page_size
    };
    if freelist_bytes >= VACUUM_RECLAIM_THRESHOLD_BYTES {
        match conn.execute("VACUUM", ()).await {
            Ok(_) => info!(
                freelist_bytes,
                migration = migration_name,
                "vacuumed database to reclaim freed space"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                migration = migration_name,
                "VACUUM failed; freed pages will be reused by future writes but the file will not shrink"
            ),
        }
    }
    Ok(())
}

async fn query_pragma_u64(conn: &Connection, pragma: &str) -> Result<u64, DbError> {
    let mut rows = conn.query(pragma, ()).await.map_err(DbError::Libsql)?;
    let row = rows
        .next()
        .await
        .map_err(DbError::Libsql)?
        .ok_or_else(|| DbError::Migration(format!("{pragma} returned no rows")))?;
    let value: i64 = row.get(0).map_err(DbError::Libsql)?;
    Ok(value.max(0) as u64)
}
