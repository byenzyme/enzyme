use libsql::{Connection, Value};
use tracing::info;

use crate::embedding::quantize;
use crate::models::{EMBEDDING_DIMENSION, INLINE_CONTEXT_MAX_WORDS};

use super::errors::DbError;
use super::schema;

/// Config key guarding the one-time `entity_occurrences` context slimming
/// migration (see [`slim_oversized_occurrence_contexts`]).
const CONTEXT_SLIM_MIGRATION_KEY: &str = "migration.context_slim_v1";

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
    let slim_hint = slim_oversized_occurrence_contexts(conn).await?;
    let int8_hint = migrate_embeddings_to_int8(conn).await?;
    // Reclaim once, after all migrations: each VACUUM rewrites the whole
    // file, so per-migration reclaims would multiply the one-time I/O cost.
    // `None` means a migration short-circuited on its completion flag; if
    // every migration short-circuited there is nothing new to reclaim.
    if slim_hint.is_some() || int8_hint.is_some() {
        reclaim_freelist_space(
            conn,
            "post-migration",
            slim_hint.unwrap_or(0) + int8_hint.unwrap_or(0),
        )
        .await?;
    }
    Ok(())
}

/// One-time data migration: older indexing runs stored full file bodies as
/// `entity_occurrences.context` for frontmatter tags/links, folders, and the
/// flat-vault root-folder entity — even when the body was large enough that
/// the catalyst pipeline's inline-context filter (`INLINE_CONTEXT_MAX_WORDS`)
/// would always discard it at read time. This walks existing rows once,
/// nulls out contexts that are and always were unused, and records
/// completion in `config` so subsequent opens skip the scan entirely.
async fn slim_oversized_occurrence_contexts(conn: &Connection) -> Result<Option<u64>, DbError> {
    // Cheap short-circuit: skip the scan entirely once already applied.
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(CONTEXT_SLIM_MIGRATION_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(None);
    }

    let mut rows = conn
        .query(
            "SELECT id, context FROM entity_occurrences WHERE context IS NOT NULL",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;

    let mut oversized_ids: Vec<i64> = Vec::new();
    let mut bytes_freed: u64 = 0;
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let context: Option<String> = row.get(1).map_err(DbError::Libsql)?;
        if let Some(context) = context {
            if context.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS {
                bytes_freed += context.len() as u64;
                oversized_ids.push(id);
            }
        }
    }
    drop(rows);

    if !oversized_ids.is_empty() {
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        // Batch updates to keep individual statements small; SQLite has no
        // hard bind-parameter limit concern here since we chunk anyway.
        for chunk in oversized_ids.chunks(500) {
            let placeholders = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE entity_occurrences SET context = NULL WHERE id IN ({placeholders})"
            );
            let params: Vec<Value> = chunk.iter().map(|id| Value::Integer(*id)).collect();
            tx.execute(&sql, params).await.map_err(DbError::Libsql)?;
        }
        tx.commit().await.map_err(DbError::Libsql)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES (?1, '1')",
        vec![Value::Text(CONTEXT_SLIM_MIGRATION_KEY.to_string())],
    )
    .await
    .map_err(DbError::Libsql)?;

    if oversized_ids.is_empty() {
        info!("context-slim migration: no oversized entity_occurrences contexts found");
    } else {
        info!(
            rows_slimmed = oversized_ids.len(),
            approx_bytes_freed = bytes_freed,
            "context-slim migration: nulled oversized entity_occurrences contexts"
        );
    }

    // Freed context text spans whole overflow pages, so it lands on the
    // freelist where the shared reclaim pass will see it — no intra-page
    // hint needed.
    Ok(Some(0))
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
async fn migrate_embeddings_to_int8(conn: &Connection) -> Result<Option<u64>, DbError> {
    // Cheap short-circuit: skip the scan entirely once already applied.
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(EMBEDDING_INT8_MIGRATION_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(None);
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

    // Shrinking a blob in place frees space *inside* its page, which never
    // shows up on the freelist — report the known payload reduction so the
    // shared reclaim pass sees it.
    Ok(Some(bytes_before.saturating_sub(bytes_after)))
}

/// Best-effort reclaim of freed pages via `VACUUM`, shared by one-time
/// migrations that null/shrink data in place (which frees pages for reuse
/// but never shrinks the file on its own). Keyed on the actual freelist
/// rather than the caller's own freed-byte estimate, so an earlier
/// interrupted migration run (data committed, completion flag not yet
/// written → rescan finds nothing to do) still gets its reclaim. Failure is
/// swallowed — a failed VACUUM (e.g. low disk for the transient copy) must
/// not fail `Database::open`.
async fn reclaim_freelist_space(
    conn: &Connection,
    migration_name: &str,
    intra_page_bytes_freed: u64,
) -> Result<(), DbError> {
    // The freelist only counts *fully empty* pages. A migration that shrinks
    // blobs in place (f32 → int8) frees space inside still-occupied pages,
    // which VACUUM can repack but the freelist can't see — the caller passes
    // that known reduction as `intra_page_bytes_freed`.
    let freelist_bytes = {
        let count = query_pragma_u64(conn, "PRAGMA freelist_count").await?;
        let page_size = query_pragma_u64(conn, "PRAGMA page_size").await?;
        count * page_size
    };
    if freelist_bytes + intra_page_bytes_freed >= VACUUM_RECLAIM_THRESHOLD_BYTES {
        match conn.execute("VACUUM", ()).await {
            Ok(_) => info!(
                freelist_bytes,
                intra_page_bytes_freed,
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
