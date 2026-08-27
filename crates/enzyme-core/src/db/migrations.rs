use libsql::{Connection, Value};
use tracing::info;

use crate::document::{
    DEFAULT_CHUNK_SIZE_CHARS, LOG_AUTODETECT_MIN_WORDS, MIN_LOG_ENTRIES, chunk_append_log_dated,
    detect_append_log, entries_with_body, most_recent_entry, strip_frontmatter,
};
use crate::embedding::quantize;
use crate::models::{EMBEDDING_DIMENSION, INLINE_CONTEXT_MAX_WORDS, representative_excerpt};

/// A long note's bounded document-level context, mirroring
/// `pipeline::indexing`: append-only logs keep their most-recent entry; every
/// other long note stores nothing (Tier-1 chunk selection handles it, and a
/// body excerpt would only pollute the link centroid). `body` must already be
/// frontmatter-stripped.
///
/// Unlike the live pipeline — which threads the structure resolved once at
/// process time through `ProcessedDocument::append_log` — this migration only
/// has stored bodies, so it re-runs detection here by necessity. That also
/// means it cannot see `type:` frontmatter overrides; a forced-on log whose
/// dates the detector cannot parse simply keeps a `None` context, matching the
/// pipeline's degraded semantics for that case.
fn long_note_context(body: &str) -> Option<String> {
    detect_append_log(body).map(|log| representative_excerpt(&most_recent_entry(body, &log)))
}

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

/// Config key guarding the one-time `chunks.entry_date` backfill for
/// append-only logs (see [`backfill_chunk_entry_dates`]). v2: rewrites
/// legacy paragraph-chunked logs instead of skipping on count mismatch.
/// v3: re-run after weekday-prefixed date headers became parseable, so
/// transcript-style logs (`### Monday, February 16, 2026`) get entry dates.
const CHUNK_ENTRY_DATE_BACKFILL_KEY: &str = "migration.chunk_entry_dates_v3";

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
    // SQLite sources now reconcile complete per-document fingerprints. The
    // legacy high-water cursor is neither read nor retained as sync state.
    conn.execute("DROP TABLE IF EXISTS source_cursors", ())
        .await
        .map_err(DbError::Libsql)?;
    ensure_chunk_entry_date_column(conn).await?;
    ensure_occurrence_weight_column(conn).await?;
    backfill_chunk_entry_dates(conn).await?;
    let slim_hint = slim_oversized_occurrence_contexts(conn).await?;
    backfill_excerpts_for_slimmed_contexts(conn).await?;
    let int8_hint = migrate_embeddings_to_int8(conn).await?;
    // Reclaim once, after all migrations: each VACUUM rewrites the whole
    // file, so per-migration reclaims would multiply the one-time I/O cost.
    // `None` means a migration short-circuited on its completion flag; if
    // every migration short-circuited there is nothing new to reclaim.
    // (The excerpt backfill only fills NULLs — it frees nothing, so it
    // contributes no hint.)
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

async fn ensure_occurrence_weight_column(conn: &Connection) -> Result<(), DbError> {
    match conn
        .execute(
            "ALTER TABLE entity_occurrences ADD COLUMN weight REAL NOT NULL DEFAULT 1.0",
            (),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(DbError::Libsql(e)),
    }
}

/// Add `chunks.entry_date` to databases created before the column existed in
/// the schema. `CREATE TABLE IF NOT EXISTS` never alters an existing table,
/// so older vaults need an explicit ALTER; the duplicate-column error from
/// vaults that already have it is the cheap idempotence check.
async fn ensure_chunk_entry_date_column(conn: &Connection) -> Result<(), DbError> {
    match conn
        .execute("ALTER TABLE chunks ADD COLUMN entry_date INTEGER", ())
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(DbError::Libsql(e)),
    }
}

/// One-time backfill of `chunks.entry_date` for already-indexed append-only
/// logs. Unchanged documents never re-chunk (content-hash gate), so without
/// this pass a long-lived journal would keep NULL entry dates — and a
/// collapsed timeline in era selection — until its next edit.
///
/// Like [`long_note_context`], this only has stored bodies: it re-runs
/// detection with the same structural gate the processor uses for the
/// auto-flag (entries with bodies + word floor) and cannot see `type:`
/// frontmatter overrides. For each qualifying document it re-derives dated
/// chunks. When the derived count matches the stored count (same chunker,
/// same content ⇒ same boundaries) it stamps dates by index. When it does
/// not — the log was indexed before entry-aligned chunking existed — the
/// stored content is still the source of truth, so the chunks are rewritten
/// outright and the document's embeddings invalidated (embedding re-queues
/// off `embedded_content_hash`), rather than leaving the timeline collapsed
/// until the file next changes.
async fn backfill_chunk_entry_dates(conn: &Connection) -> Result<(), DbError> {
    let mut check = conn
        .query(
            "SELECT 1 FROM config WHERE key = ?1",
            vec![Value::Text(CHUNK_ENTRY_DATE_BACKFILL_KEY.to_string())],
        )
        .await
        .map_err(DbError::Libsql)?;
    if check.next().await.map_err(DbError::Libsql)?.is_some() {
        return Ok(());
    }

    let mut rows = conn
        .query(
            "SELECT d.id, d.source_ref, d.content, COUNT(c.id), MIN(c.created_at)
             FROM docs d JOIN chunks c ON c.doc_id = d.id
             WHERE d.content IS NOT NULL
             GROUP BY d.id",
            (),
        )
        .await
        .map_err(DbError::Libsql)?;

    // Docs whose derived chunking aligns with what is stored: stamp dates in
    // place. Docs where it doesn't: rewrite the chunk set.
    struct Stamp {
        doc_id: i64,
        dates: Vec<Option<i64>>,
    }
    struct Rewrite {
        doc_id: i64,
        source_ref: String,
        created_at: i64,
        chunks: Vec<crate::document::DatedChunk>,
    }
    let mut stamps: Vec<Stamp> = Vec::new();
    let mut rewrites: Vec<Rewrite> = Vec::new();
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let doc_id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let source_ref: String = row.get(1).map_err(DbError::Libsql)?;
        let content: String = row.get(2).map_err(DbError::Libsql)?;
        let stored_count: i64 = row.get(3).map_err(DbError::Libsql)?;
        let created_at: i64 = row.get(4).map_err(DbError::Libsql)?;

        let body = strip_frontmatter(&content);
        let Some(log) = detect_append_log(body) else {
            continue;
        };
        if entries_with_body(body, &log) < MIN_LOG_ENTRIES
            || (body.split_whitespace().count() as i64) < LOG_AUTODETECT_MIN_WORDS
        {
            continue;
        }
        let dated = chunk_append_log_dated(body, &log, DEFAULT_CHUNK_SIZE_CHARS);
        if dated.len() as i64 == stored_count {
            stamps.push(Stamp {
                doc_id,
                dates: dated.into_iter().map(|c| c.entry_date).collect(),
            });
        } else {
            rewrites.push(Rewrite {
                doc_id,
                source_ref,
                created_at,
                chunks: dated,
            });
        }
    }
    drop(rows);

    if !stamps.is_empty() || !rewrites.is_empty() {
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        for stamp in &stamps {
            for (index, entry_date) in stamp.dates.iter().enumerate() {
                let Some(entry_date) = entry_date else {
                    continue;
                };
                tx.execute(
                    "UPDATE chunks SET entry_date = ?1 WHERE doc_id = ?2 AND chunk_index = ?3",
                    vec![
                        Value::Integer(*entry_date),
                        Value::Integer(stamp.doc_id),
                        Value::Integer(index as i64),
                    ],
                )
                .await
                .map_err(DbError::Libsql)?;
            }
        }
        for rewrite in &rewrites {
            tx.execute(
                "DELETE FROM chunks WHERE doc_id = ?1",
                vec![Value::Integer(rewrite.doc_id)],
            )
            .await
            .map_err(DbError::Libsql)?;
            for (index, chunk) in rewrite.chunks.iter().enumerate() {
                let word_count = chunk.content.split_whitespace().count() as i64;
                tx.execute(
                    "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at, entry_date)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    vec![
                        Value::Integer(rewrite.doc_id),
                        Value::Integer(index as i64),
                        Value::Text(chunk.content.clone()),
                        Value::Integer(word_count),
                        Value::Integer(rewrite.created_at),
                        chunk.entry_date.map(Value::Integer).unwrap_or(Value::Null),
                    ],
                )
                .await
                .map_err(DbError::Libsql)?;
            }
            // Old per-chunk embeddings no longer line up with the new
            // boundaries; drop them and re-queue the doc for embedding.
            // `indexed_at` must move too: incremental refresh windows its
            // embedding pass on it, and a stale value would leave the doc
            // re-queued but never picked up.
            tx.execute(
                "DELETE FROM chunk_embeddings WHERE file_path = ?1",
                vec![Value::Text(rewrite.source_ref.clone())],
            )
            .await
            .map_err(DbError::Libsql)?;
            tx.execute(
                "UPDATE docs SET embedded_at = NULL, embedded_content_hash = NULL, indexed_at = ?1
                 WHERE id = ?2",
                vec![
                    Value::Integer(chrono::Utc::now().timestamp_millis()),
                    Value::Integer(rewrite.doc_id),
                ],
            )
            .await
            .map_err(DbError::Libsql)?;
        }
        tx.commit().await.map_err(DbError::Libsql)?;
        info!(
            stamped = stamps.len(),
            rechunked = rewrites.len(),
            "backfilled chunk entry dates for append-only logs"
        );
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES (?1, '1')",
        vec![Value::Text(CHUNK_ENTRY_DATE_BACKFILL_KEY.to_string())],
    )
    .await
    .map_err(DbError::Libsql)?;
    Ok(())
}

/// One-time data migration: older indexing runs stored full file bodies as
/// `entity_occurrences.context` for frontmatter tags/links, folders, and the
/// flat-vault root-folder entity — even when the body was large enough that
/// the catalyst pipeline's inline-context filter (`INLINE_CONTEXT_MAX_WORDS`)
/// would always discard it at read time. This walks existing rows once and
/// replaces each oversized body with its bounded [`long_note_context`] —
/// matching current indexing: append-only logs keep a recency excerpt, every
/// other long note is nulled — then records completion in `config` so
/// subsequent opens skip the scan entirely.
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

    // (id, replacement) for each oversized context. `None` → set NULL (coherent
    // long note); `Some(excerpt)` → recency excerpt (append-only log).
    let mut updates: Vec<(i64, Option<String>)> = Vec::new();
    let mut bytes_freed: u64 = 0;
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let context: Option<String> = row.get(1).map_err(DbError::Libsql)?;
        if let Some(context) = context {
            if context.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS {
                // The stored context is already the frontmatter-stripped body.
                let replacement = long_note_context(&context);
                let kept = replacement.as_ref().map(|s| s.len()).unwrap_or(0);
                bytes_freed += (context.len() as u64).saturating_sub(kept as u64);
                updates.push((id, replacement));
            }
        }
    }
    drop(rows);

    if !updates.is_empty() {
        // Single transaction so the whole slim is atomic: a crash before commit
        // leaves the original bodies intact, and a crash after commit but before
        // the flag write simply finds no oversized rows on the next scan (the
        // replacements are all under the threshold, NULL included).
        let tx = conn.transaction().await.map_err(DbError::Libsql)?;
        for (id, replacement) in &updates {
            let value = match replacement {
                Some(excerpt) => Value::Text(excerpt.clone()),
                None => Value::Null,
            };
            tx.execute(
                "UPDATE entity_occurrences SET context = ?1 WHERE id = ?2",
                vec![value, Value::Integer(*id)],
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
        let logs_kept = updates.iter().filter(|(_, r)| r.is_some()).count();
        info!(
            rows_slimmed = updates.len(),
            logs_kept,
            approx_bytes_freed = bytes_freed,
            "context-slim migration: nulled oversized contexts, kept recency excerpts for logs"
        );
    }

    // Freed context text spans whole overflow pages, so it lands on the
    // freelist where the shared reclaim pass will see it — no intra-page
    // hint needed.
    Ok(Some(0))
}

/// One-time data migration for vaults that ran an earlier context-slim
/// migration, which stored long-note contexts as either NULL or a symmetric
/// head+tail excerpt. Only **append-only logs** should carry a document-level
/// context (a recency excerpt); coherent long notes should stay NULL and rely
/// on Tier-1 chunk selection. This walks null-context occurrences whose source
/// doc is still available and, for the ones that are logs, backfills the same
/// recency excerpt [`long_note_context`] that current indexing produces. It
/// leaves coherent long notes NULL.
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

    // Only append-only logs get a context restored; coherent long notes stay
    // NULL by design (`long_note_context` returns None for them).
    let mut updates: Vec<(i64, String)> = Vec::new();
    while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
        let id: i64 = row.get(0).map_err(DbError::Libsql)?;
        let content: String = row.get(1).map_err(DbError::Libsql)?;
        let body = strip_frontmatter(&content);
        if let Some(excerpt) = long_note_context(body) {
            updates.push((id, excerpt));
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
        if count > 0
            && !quantize::is_int8_v1_packed_len(vectors_blob.len(), count as usize, expected_dim)
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
