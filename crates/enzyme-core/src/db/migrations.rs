use libsql::{Connection, Value};
use tracing::info;

use crate::models::INLINE_CONTEXT_MAX_WORDS;

use super::errors::DbError;
use super::schema;

/// Config key guarding the one-time `entity_occurrences` context slimming
/// migration (see [`slim_oversized_occurrence_contexts`]).
const CONTEXT_SLIM_MIGRATION_KEY: &str = "migration.context_slim_v1";

/// Only bother with the one-time post-slim VACUUM when the migration actually
/// freed enough to matter; below this the I/O outweighs the reclaim.
const VACUUM_RECLAIM_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Apply all pending migrations on a connection.
///
/// Called during `Database::open` after pragmas are set.
pub async fn run_all_on_conn(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(schema::SCHEMA_SQL)
        .await
        .map_err(DbError::Libsql)?;
    slim_oversized_occurrence_contexts(conn).await?;
    Ok(())
}

/// One-time data migration: older indexing runs stored full file bodies as
/// `entity_occurrences.context` for frontmatter tags/links, folders, and the
/// flat-vault root-folder entity — even when the body was large enough that
/// the catalyst pipeline's inline-context filter (`INLINE_CONTEXT_MAX_WORDS`)
/// would always discard it at read time. This walks existing rows once,
/// nulls out contexts that are and always were unused, and records
/// completion in `config` so subsequent opens skip the scan entirely.
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

    // One-time reclaim, tied to the same one-time migration: nulling contexts
    // frees pages for reuse but never shrinks the file, and nothing else in
    // the lifecycle ever vacuums. Best-effort — a failed VACUUM (e.g. low
    // disk for the transient copy) must not fail Database::open, and the
    // migration flag is already set so it won't rescan either way.
    if bytes_freed >= VACUUM_RECLAIM_THRESHOLD_BYTES {
        match conn.execute("VACUUM", ()).await {
            Ok(_) => info!(
                approx_bytes_freed = bytes_freed,
                "context-slim migration: vacuumed database to reclaim freed space"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "context-slim migration: VACUUM failed; freed pages will be \
                 reused by future writes but the file will not shrink"
            ),
        }
    }

    Ok(())
}
