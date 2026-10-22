use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use libsql::Value;

use crate::document::ProcessedDocument;
use crate::embedding::quantize;
use crate::models::*;

use super::connection::{Database, block_in_place_sync};

// ---------------------------------------------------------------------------
// Context query types
// ---------------------------------------------------------------------------

/// Paths bound per `IN (...)` when clearing occurrences (SQLite allows 32,766).
const OCCURRENCE_PATH_BATCH: usize = 10_000;

/// Minimum link frequency for a folder page to count as an expandable child.
pub const PAGE_LINK_MIN_FREQUENCY: i64 = 3;

/// The folder-expansion eligibility gate: a page link mentioned at least
/// [`PAGE_LINK_MIN_FREQUENCY`] times with cached activity in the year before the
/// index-time reference instant (`trends.frequency_recent`).
pub fn is_active_page_link(entity: &Entity) -> bool {
    entity.entity_type == EntityType::Link
        && entity.frequency >= PAGE_LINK_MIN_FREQUENCY
        && entity.trends.frequency_recent > 0
}

/// Live statistics for linked-child selection. One source document counts once.
#[derive(Debug, Default)]
pub struct EntitySelectionStats {
    pub documents: usize,
    pub recency: f64,
    /// Repair legacy cached rankings only when a future timestamp affected them.
    pub corrected_legacy_recency: Option<f64>,
}

/// A child document within an expandable folder, ranked by external references.
#[derive(Debug, Clone)]
pub struct FolderChild {
    /// File stem, e.g. "Person Name".
    pub name: String,
    /// Relative vault path, e.g. "people/Person Name.md".
    pub source_ref: String,
    /// Number of documents *outside* the folder that wikilink this child.
    pub reference_count: i64,
    /// Most recent modification timestamp (millis).
    pub last_seen: Option<i64>,
}

/// Stats for a top-level vault folder derived from document paths.
#[derive(Debug, Clone)]
pub struct FolderStats {
    pub name: String,
    pub file_count: i64,
    pub most_recent: Option<i64>,
    pub subfolders: Vec<SubfolderStats>,
}

/// Stats for a subfolder within a top-level folder.
#[derive(Debug, Clone)]
pub struct SubfolderStats {
    pub name: String,
    pub file_count: i64,
}

/// A chunk joined to its document path for retrieval-time selection.
#[derive(Debug, Clone)]
pub struct FileChunk {
    pub file_path: String,
    pub chunk_index: u32,
    pub content: String,
    /// Log-entry date (millis) for chunks cut from a dated append-log entry.
    pub entry_date: Option<i64>,
}

/// Document and chunks that need embedding.
#[derive(Debug, Clone)]
pub struct DocumentForEmbedding {
    pub doc_id: i64,
    pub source_ref: String,
    pub content_hash: String,
    pub created_at: i64,
    pub chunks: Vec<String>,
}

/// Per-entity evidence used by catalyst refresh staleness checks.
#[derive(Debug, Clone)]
pub struct EntityRefreshStats {
    pub doc_count: usize,
    pub latest_effective_date: Option<i64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `DateSource` from its string representation stored in the database.
fn parse_date_source(s: &str) -> DateSource {
    match s {
        "frontmatter" => DateSource::Frontmatter,
        "filename" => DateSource::Filename,
        "content" => DateSource::Content,
        _ => DateSource::Filesystem,
    }
}

/// Parse an `EntityType` from its string representation stored in the database.
fn parse_entity_type(s: &str) -> EntityType {
    match s {
        "tag" => EntityType::Tag,
        "link" => EntityType::Link,
        "folder" => EntityType::Folder,
        "collection" => EntityType::Collection,
        "append_log" => EntityType::AppendLog,
        _ => EntityType::Tag,
    }
}

/// Parse a `DateTime<Utc>` from a SQLite TEXT column produced by `datetime('now')`.
fn parse_datetime_utc(s: &str) -> DateTime<Utc> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return ndt.and_utc();
    }
    DateTime::UNIX_EPOCH
}

/// Convert `Option<T>` to a libsql `Value`, using Null for None.
fn opt_val<T: Into<Value>>(opt: Option<T>) -> Value {
    match opt {
        Some(v) => v.into(),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// High-level query / CRUD methods on Database
// ---------------------------------------------------------------------------

impl Database {
    // -----------------------------------------------------------------------
    // Document operations
    // -----------------------------------------------------------------------

    /// Insert or replace documents and their associated tags, links, and chunks.
    ///
    /// All documents are written inside a **single transaction** so that SQLite
    /// only fsyncs once.
    pub fn upsert_documents_batch(&self, docs: &[ProcessedDocument]) -> Result<Vec<i64>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let docs = docs.to_vec();
        let now = chrono::Utc::now().timestamp_millis();

        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_documents_batch")?;

            let mut doc_ids = Vec::with_capacity(docs.len());

            for doc in &docs {
                let metadata = doc
                    .frontmatter
                    .as_ref()
                    .map(|fm| serde_json::to_string(&fm.extra).unwrap_or_else(|_| "{}".to_string()))
                    .unwrap_or_else(|| "{}".to_string());

                // Delete old children if the document already exists.
                let mut lookup_rows = tx
                    .query(
                        "SELECT id FROM docs WHERE source_ref = ?1",
                        vec![Value::Text(doc.source_ref.clone())],
                    )
                    .await?;
                let existing_id: Option<i64> = if let Some(row) = lookup_rows.next().await? {
                    Some(row.get::<i64>(0)?)
                } else {
                    None
                };

                if let Some(old_id) = existing_id {
                    tx.execute(
                        "DELETE FROM doc_tags WHERE doc_id = ?1",
                        vec![Value::Integer(old_id)],
                    )
                    .await?;
                    tx.execute(
                        "DELETE FROM doc_links WHERE doc_id = ?1",
                        vec![Value::Integer(old_id)],
                    )
                    .await?;
                    tx.execute(
                        "DELETE FROM chunks WHERE doc_id = ?1",
                        vec![Value::Integer(old_id)],
                    )
                    .await?;
                }

                let insert_params = vec![
                    Value::Text(doc.source_ref.clone()),
                    opt_val(doc.title.clone()),
                    Value::Text(doc.content.clone()),
                    Value::Text(doc.content_hash.clone()),
                    Value::Integer(doc.word_count),
                    Value::Integer(doc.char_count),
                    Value::Integer(doc.created_at),
                    Value::Text(doc.created_source.as_str().to_string()),
                    Value::Integer(doc.modified_at),
                    Value::Text(doc.modified_source.as_str().to_string()),
                    Value::Integer(now),
                    Value::Text(metadata),
                ];
                tx.execute(
                    "INSERT OR REPLACE INTO docs (
                        source_ref, title, content, content_hash,
                        word_count, char_count,
                        created_at, created_source,
                        modified_at, modified_source,
                        indexed_at, metadata
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    insert_params,
                )
                .await?;

                let mut id_rows = tx
                    .query(
                        "SELECT id FROM docs WHERE source_ref = ?1",
                        vec![Value::Text(doc.source_ref.clone())],
                    )
                    .await?;
                let doc_id: i64 = id_rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("doc not found after insert"))?
                    .get::<i64>(0)?;

                for tag in &doc.tags {
                    tx.execute(
                        "INSERT OR IGNORE INTO doc_tags (doc_id, tag) VALUES (?1, ?2)",
                        vec![Value::Integer(doc_id), Value::Text(tag.clone())],
                    )
                    .await?;
                }
                for link in &doc.links {
                    tx.execute(
                        "INSERT OR IGNORE INTO doc_links (doc_id, link) VALUES (?1, ?2)",
                        vec![Value::Integer(doc_id), Value::Text(link.clone())],
                    )
                    .await?;
                }
                let handle = lexical_handle(&doc.source_ref, doc.title.as_deref(), &doc.links);
                for (i, chunk_content) in doc.chunks.iter().enumerate() {
                    let word_count = chunk_content.split_whitespace().count() as i64;
                    let entry_date = doc
                        .chunk_entry_dates
                        .get(i)
                        .copied()
                        .flatten()
                        .map(Value::Integer)
                        .unwrap_or(Value::Null);
                    tx.execute(
                        "INSERT INTO chunks (doc_id, path, chunk_index, content, word_count, created_at, entry_date)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        vec![
                            Value::Integer(doc_id),
                            Value::Text(handle.clone()),
                            Value::Integer(i as i64),
                            Value::Text(chunk_content.clone()),
                            Value::Integer(word_count),
                            Value::Integer(now),
                            entry_date,
                        ],
                    )
                    .await?;
                }

                doc_ids.push(doc_id);
            }

            tx.commit()
                .await
                .context("failed to commit upsert_documents_batch")?;
            Ok(doc_ids)
        })
    }

    /// Fetch document bodies for the given vault paths, keyed by `source_ref`.
    ///
    /// Used to rehydrate occurrence contexts that are stored as NULL since the
    /// context-slim change (oversized bodies are no longer duplicated into
    /// `entity_occurrences.context`; the canonical copy lives in `docs`).
    pub fn get_doc_contents_for_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut contents = std::collections::HashMap::new();
        for chunk in paths.chunks(500) {
            let placeholders = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT source_ref, content FROM docs
                 WHERE content IS NOT NULL AND source_ref IN ({placeholders})"
            );
            let params: Vec<Value> = chunk.iter().map(|p| Value::Text(p.clone())).collect();
            let rows = self.query_read(&sql, params, |row| {
                Ok((row.get::<String>(0)?, row.get::<String>(1)?))
            })?;
            contents.extend(rows);
        }
        Ok(contents)
    }

    /// Stored title and the first `head_chars` characters of content for each
    /// path, keyed by `source_ref`. Missing documents are absent from the map.
    pub fn get_doc_titles_for_paths(
        &self,
        paths: &[String],
        head_chars: usize,
    ) -> Result<std::collections::HashMap<String, (Option<String>, Option<String>)>> {
        let mut titles = std::collections::HashMap::new();
        for chunk in paths.chunks(500) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT source_ref, title, substr(content, 1, ?1) FROM docs
                 WHERE source_ref IN ({placeholders})"
            );
            let mut params = vec![Value::Integer(head_chars as i64)];
            params.extend(chunk.iter().map(|p| Value::Text(p.clone())));
            let rows = self.query_read(&sql, params, |row| {
                Ok((
                    row.get::<String>(0)?,
                    (row.get::<Option<String>>(1)?, row.get::<Option<String>>(2)?),
                ))
            })?;
            titles.extend(rows);
        }
        Ok(titles)
    }

    /// Catalyst links for the given notes: `(catalyst_id, file_path, similarity)`
    /// from the precomputed catalyst-to-chunk similarities, best chunk per
    /// catalyst and note.
    pub fn get_catalyst_links_for_paths(
        &self,
        paths: &[String],
    ) -> Result<Vec<(String, String, f64)>> {
        let mut links = Vec::new();
        for chunk in paths.chunks(500) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT catalyst_id, file_path, MAX(similarity) FROM catalyst_similarities
                 WHERE file_path IN ({placeholders})
                 GROUP BY catalyst_id, file_path"
            );
            let params = chunk.iter().map(|p| Value::Text(p.clone())).collect();
            let rows = self.query_read(&sql, params, |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<f64>(2)?,
                ))
            })?;
            links.extend(rows);
        }
        Ok(links)
    }

    /// Retrieve a single document by its `source_ref` (relative vault path).
    pub fn get_document_by_path(&self, source_ref: &str) -> Result<Option<Document>> {
        let rows = self.query_read(
            "SELECT id, source_ref, title, content, content_hash,
                    word_count, char_count,
                    created_at, created_source,
                    modified_at, modified_source,
                    indexed_at, embedded_at, embedded_content_hash,
                    metadata
             FROM docs WHERE source_ref = ?1",
            vec![Value::Text(source_ref.to_string())],
            |row| {
                Ok(Document {
                    id: Some(row.get::<i64>(0)?),
                    source_ref: row.get::<String>(1)?,
                    title: row.get::<Option<String>>(2)?,
                    content: row.get::<Option<String>>(3)?,
                    content_hash: row.get::<String>(4)?,
                    word_count: row.get::<i64>(5)?,
                    char_count: row.get::<i64>(6)?,
                    created_at: row.get::<i64>(7)?,
                    created_source: parse_date_source(
                        &row.get::<Option<String>>(8)?.unwrap_or_default(),
                    ),
                    modified_at: row.get::<i64>(9)?,
                    modified_source: parse_date_source(
                        &row.get::<Option<String>>(10)?.unwrap_or_default(),
                    ),
                    indexed_at: row.get::<i64>(11)?,
                    embedded_at: row.get::<Option<i64>>(12)?,
                    embedded_content_hash: row.get::<Option<String>>(13)?,
                    metadata: row
                        .get::<Option<String>>(14)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                })
            },
        )?;

        Ok(rows.into_iter().next())
    }

    /// Return a map of `source_ref -> (content_hash, modified_at)` for every document.
    pub fn get_all_document_hashes(&self) -> Result<HashMap<String, (String, i64)>> {
        let rows = self.query_read(
            "SELECT source_ref, content_hash, modified_at FROM docs",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    (row.get::<String>(1)?, row.get::<i64>(2)?),
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// A per-document signature of every stored date: the document's created
    /// and modified dates and its chunks' log-entry dates. Comparing it with
    /// [`ProcessedDocument::date_signature`] finds documents whose dates a
    /// changed date rule reinterprets, so only those are re-upserted.
    pub fn get_document_date_signatures(&self) -> Result<HashMap<String, String>> {
        let rows = self.query_read(
            "SELECT d.source_ref, d.created_at, d.created_source, d.modified_at,
                    d.modified_source, COUNT(c.id), COUNT(c.entry_date),
                    COALESCE(SUM(c.entry_date), 0), COALESCE(MIN(c.entry_date), 0),
                    COALESCE(MAX(c.entry_date), 0)
             FROM docs d LEFT JOIN chunks c ON c.doc_id = d.id
             GROUP BY d.id",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    crate::document::date_signature(
                        row.get::<i64>(1)?,
                        &row.get::<String>(2)?,
                        row.get::<i64>(3)?,
                        &row.get::<String>(4)?,
                        [
                            row.get::<i64>(5)?,
                            row.get::<i64>(6)?,
                            row.get::<i64>(7)?,
                            row.get::<i64>(8)?,
                            row.get::<i64>(9)?,
                        ],
                    ),
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Rewrite only the stored created/modified dates of existing documents,
    /// leaving chunks and embedding receipts intact.
    pub fn update_document_dates(&self, docs: &[ProcessedDocument]) -> Result<()> {
        for doc in docs {
            self.execute_write(
                "UPDATE docs SET created_at = ?1, created_source = ?2,
                                 modified_at = ?3, modified_source = ?4
                 WHERE source_ref = ?5",
                vec![
                    Value::Integer(doc.created_at),
                    Value::Text(doc.created_source.as_str().to_string()),
                    Value::Integer(doc.modified_at),
                    Value::Text(doc.modified_source.as_str().to_string()),
                    Value::Text(doc.source_ref.clone()),
                ],
            )?;
        }
        Ok(())
    }

    /// Find documents whose embeddings are stale or missing.
    ///
    /// Returns `(doc_id, source_ref, created_at_ms, chunks)` sorted by
    /// `created_at DESC` (newest first).
    pub fn get_documents_needing_embedding(
        &self,
        max_count: Option<usize>,
        indexed_since: Option<i64>,
    ) -> Result<Vec<DocumentForEmbedding>> {
        let mut params: Vec<Value> = Vec::new();
        let mut sql = String::from(
            "SELECT id, source_ref, content_hash, created_at FROM docs
             WHERE (embedded_content_hash IS NULL
                OR embedded_content_hash != content_hash
                OR embedded_at IS NULL)",
        );

        if let Some(ts) = indexed_since {
            params.push(Value::Integer(ts));
            sql.push_str(&format!(" AND indexed_at >= ?{}", params.len()));
        }

        sql.push_str(" ORDER BY created_at DESC");

        if let Some(n) = max_count {
            params.push(Value::Integer(n as i64));
            sql.push_str(&format!(" LIMIT ?{}", params.len()));
        }

        let docs = self.query_read(&sql, params, |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<i64>(3)?,
            ))
        })?;

        let mut results = Vec::with_capacity(docs.len());
        for (doc_id, source_ref, content_hash, created_at) in docs {
            let chunks = self.query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                vec![Value::Integer(doc_id)],
                |row| row.get::<String>(0),
            )?;
            results.push(DocumentForEmbedding {
                doc_id,
                source_ref,
                content_hash,
                created_at,
                chunks,
            });
        }

        Ok(results)
    }

    /// Count documents whose embeddings are stale or missing.
    /// Documents dated by their own content or metadata (not filesystem time)
    /// within `[since_ms, now_ms]`.
    pub fn count_dated_documents_since(&self, since_ms: i64, now_ms: i64) -> Result<usize> {
        let rows = self.query_read(
            "SELECT COUNT(*) FROM docs
             WHERE created_source != 'filesystem' AND created_at >= ?1 AND created_at <= ?2",
            vec![Value::Integer(since_ms), Value::Integer(now_ms)],
            |row| row.get::<i64>(0),
        )?;
        Ok(rows.first().copied().unwrap_or(0) as usize)
    }

    pub fn count_documents_needing_embedding(&self, indexed_since: Option<i64>) -> Result<usize> {
        let sql = "SELECT COUNT(*) FROM docs
             WHERE (embedded_content_hash IS NULL
                OR embedded_content_hash != content_hash
                OR embedded_at IS NULL)";
        let mut sql = sql.to_string();
        let mut params = Vec::new();
        if let Some(ts) = indexed_since {
            params.push(Value::Integer(ts));
            sql.push_str(&format!(" AND indexed_at >= ?{}", params.len()));
        }
        let rows = self.query_read(&sql, params, |row| row.get::<i64>(0))?;
        Ok(rows.first().copied().unwrap_or(0) as usize)
    }

    /// Mark a document as having been embedded at `now` with the given `content_hash`.
    pub fn mark_document_embedded(
        &self,
        doc_id: i64,
        content_hash: &str,
        now: Timestamp,
    ) -> Result<()> {
        self.execute_write(
            "UPDATE docs SET embedded_at = ?1, embedded_content_hash = ?2 WHERE id = ?3",
            vec![
                Value::Integer(now),
                Value::Text(content_hash.to_string()),
                Value::Integer(doc_id),
            ],
        )?;
        Ok(())
    }

    /// Delete documents by their `source_ref` paths.
    ///
    /// Foreign-key cascades handle child rows (tags, links, chunks).
    /// Returns the number of deleted documents.
    pub fn delete_documents_by_paths(&self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }

        let placeholders: Vec<String> = (1..=paths.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM docs WHERE source_ref IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<Value> = paths.iter().map(|p| Value::Text(p.clone())).collect();
        let count = self.execute_write(&sql, params)?;
        // `entity_occurrences.file_path` is a plain path, not a foreign key, so
        // nothing cascades: without this the mentions of a deleted note keep
        // propping up its entities forever.
        self.remove_occurrences_for_paths(paths)?;
        Ok(count)
    }

    /// Forget every entity occurrence recorded for these paths and recompute
    /// the affected entities' frequency from what remains.
    ///
    /// Occurrences accumulate: reprocessing a document re-emits all of its
    /// mentions, and deleting one leaves them stranded. Both callers reconcile
    /// through here, so a document's contribution to an entity is always
    /// exactly what its current content says it is.
    pub fn remove_occurrences_for_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let paths = paths.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for remove_occurrences_for_paths")?;
            Database::remove_occurrences_for_paths_on_tx(&tx, &paths).await?;
            tx.commit()
                .await
                .context("failed to commit remove_occurrences_for_paths")?;
            Ok(())
        })
    }

    async fn remove_occurrences_for_paths_on_tx(
        tx: &libsql::Transaction,
        paths: &[String],
    ) -> Result<()> {
        // SQLite caps bound parameters at 32,766; a first index of a large vault
        // passes every path at once, so bind them in bounded batches.
        let mut affected = std::collections::BTreeSet::<i64>::new();
        for batch in paths.chunks(OCCURRENCE_PATH_BATCH) {
            let placeholders: Vec<String> = (1..=batch.len()).map(|i| format!("?{i}")).collect();
            let list = placeholders.join(", ");
            let params: Vec<Value> = batch.iter().map(|p| Value::Text(p.clone())).collect();

            let mut rows = tx
                .query(
                    &format!(
                        "SELECT DISTINCT entity_id FROM entity_occurrences WHERE file_path IN ({list})"
                    ),
                    params.clone(),
                )
                .await?;
            let mut batch_affected = Vec::new();
            while let Some(row) = rows.next().await? {
                batch_affected.push(row.get::<i64>(0)?);
            }
            drop(rows);
            if batch_affected.is_empty() {
                continue;
            }
            affected.extend(batch_affected);
            tx.execute(
                &format!("DELETE FROM entity_occurrences WHERE file_path IN ({list})"),
                params,
            )
            .await?;
        }
        if affected.is_empty() {
            return Ok(());
        }
        // Recompute rather than decrement: it is self-correcting for rows that
        // an older accumulate-only pass already double-counted.
        for entity_id in affected {
            tx.execute(
                "UPDATE entities
                 SET frequency = (SELECT COUNT(*) FROM entity_occurrences eo
                                  WHERE eo.entity_id = entities.id)
                 WHERE id = ?1",
                vec![Value::Integer(entity_id)],
            )
            .await?;
        }
        Ok(())
    }

    /// Delete entities that no longer have a single occurrence.
    ///
    /// An entity with no occurrences has no material to catalyze — the
    /// generation stage already skips it — and would otherwise linger as a
    /// ghost in selection and ranking after its last note is deleted.
    pub fn prune_entities_without_occurrences(&self) -> Result<usize> {
        self.execute_write(
            "DELETE FROM entities
             WHERE NOT EXISTS (SELECT 1 FROM entity_occurrences eo WHERE eo.entity_id = entities.id)",
            vec![],
        )
        .map_err(Into::into)
    }

    /// Return all `source_ref` values from the docs table.
    pub fn get_all_source_refs(&self) -> Result<Vec<String>> {
        Ok(
            self.query_read("SELECT source_ref FROM docs", vec![], |row| {
                row.get::<String>(0)
            })?,
        )
    }

    /// Return the total number of documents in the database.
    pub fn get_document_count(&self) -> Result<i64> {
        let rows = self.query_read("SELECT COUNT(*) FROM docs", vec![], |row| row.get::<i64>(0))?;
        Ok(rows.into_iter().next().unwrap_or(0))
    }

    // -----------------------------------------------------------------------
    // Entity operations
    // -----------------------------------------------------------------------

    /// Batch-insert or update entities and record occurrences.
    pub fn upsert_entities_batch(
        &self,
        entries: &[(&str, &EntityType, &EntityOccurrence)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Collect owned data before entering the async block.
        let entries: Vec<(String, String, EntityOccurrence)> = entries
            .iter()
            .map(|&(name, entity_type, occ)| {
                (
                    name.to_string(),
                    entity_type.as_str().to_string(),
                    occ.clone(),
                )
            })
            .collect();

        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_entities_batch")?;

            // Every mention in a reprocessed document is about to be
            // re-inserted, so drop what that document said last time first.
            // Otherwise a re-index appends a second copy of every occurrence
            // and inflates `frequency` — which drives catalyst allocation and
            // entity ranking — a little more on each run.
            let batch_paths: Vec<String> = entries
                .iter()
                .map(|(_, _, occ)| occ.file_path.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            Database::remove_occurrences_for_paths_on_tx(&tx, &batch_paths).await?;

            for (name, type_str, occurrence) in &entries {
                let effective_date = occurrence.effective_date;

                tx.execute(
                    "INSERT OR IGNORE INTO entities (name, type, frequency, first_seen, last_seen)
                     VALUES (?1, ?2, 0, ?3, ?4)",
                    vec![
                        Value::Text(name.clone()),
                        Value::Text(type_str.clone()),
                        opt_val(effective_date),
                        opt_val(effective_date),
                    ],
                )
                .await?;

                tx.execute(
                    "UPDATE entities
                     SET frequency = frequency + 1,
                         last_seen = MAX(COALESCE(last_seen, 0), COALESCE(?1, 0))
                     WHERE name = ?2 AND type = ?3",
                    vec![
                        opt_val(effective_date),
                        Value::Text(name.clone()),
                        Value::Text(type_str.clone()),
                    ],
                )
                .await?;

                let mut rows = tx
                    .query(
                        "SELECT id FROM entities WHERE name = ?1 AND type = ?2",
                        vec![Value::Text(name.clone()), Value::Text(type_str.clone())],
                    )
                    .await?;
                let entity_id: i64 = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("entity not found after insert"))?
                    .get::<i64>(0)?;

                tx.execute(
                    "INSERT INTO entity_occurrences
                     (entity_id, file_path, context, created_at, effective_date, date_source, weight)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    vec![
                        Value::Integer(entity_id),
                        Value::Text(occurrence.file_path.clone()),
                        opt_val(occurrence.context.clone()),
                        Value::Integer(occurrence.created_at),
                        opt_val(occurrence.effective_date),
                        Value::Text(occurrence.date_source.clone()),
                        Value::Real(occurrence.weight),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit upsert_entities_batch")?;
            Ok(())
        })
    }

    /// Upsert only the addressed `(entity, document)` occurrence pairs.
    ///
    /// Unlike [`Self::upsert_entities_batch`], this does not remove every
    /// occurrence associated with a reprocessed document. Remote collection
    /// adapters use it for bounded observations: seeing one block in one
    /// collection must not erase an older observed membership in another
    /// collection, and absence from a partial page is not a deletion signal.
    pub fn upsert_entity_occurrences_partial(
        &self,
        entries: &[(&str, &EntityType, &EntityOccurrence)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let entries: Vec<(String, String, EntityOccurrence)> = entries
            .iter()
            .map(|&(name, entity_type, occurrence)| {
                (
                    name.to_string(),
                    entity_type.as_str().to_string(),
                    occurrence.clone(),
                )
            })
            .collect();

        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin partial occurrence upsert")?;
            let mut affected = std::collections::BTreeSet::new();

            for (name, type_str, occurrence) in &entries {
                tx.execute(
                    "INSERT OR IGNORE INTO entities (name, type, frequency, first_seen, last_seen)
                     VALUES (?1, ?2, 0, ?3, ?4)",
                    vec![
                        Value::Text(name.clone()),
                        Value::Text(type_str.clone()),
                        opt_val(occurrence.effective_date),
                        opt_val(occurrence.effective_date),
                    ],
                )
                .await?;

                let mut rows = tx
                    .query(
                        "SELECT id FROM entities WHERE name = ?1 AND type = ?2",
                        vec![Value::Text(name.clone()), Value::Text(type_str.clone())],
                    )
                    .await?;
                let entity_id: i64 = rows
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("entity not found after insert"))?
                    .get::<i64>(0)?;
                drop(rows);
                affected.insert(entity_id);

                tx.execute(
                    "DELETE FROM entity_occurrences
                     WHERE entity_id = ?1 AND file_path = ?2",
                    vec![
                        Value::Integer(entity_id),
                        Value::Text(occurrence.file_path.clone()),
                    ],
                )
                .await?;
                tx.execute(
                    "INSERT INTO entity_occurrences
                     (entity_id, file_path, context, created_at, effective_date, date_source, weight)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    vec![
                        Value::Integer(entity_id),
                        Value::Text(occurrence.file_path.clone()),
                        opt_val(occurrence.context.clone()),
                        Value::Integer(occurrence.created_at),
                        opt_val(occurrence.effective_date),
                        Value::Text(occurrence.date_source.clone()),
                        Value::Real(occurrence.weight),
                    ],
                )
                .await?;
            }

            for entity_id in affected {
                tx.execute(
                    "UPDATE entities
                     SET frequency = (SELECT COUNT(*) FROM entity_occurrences eo
                                      WHERE eo.entity_id = entities.id),
                         first_seen = (SELECT MIN(effective_date) FROM entity_occurrences eo
                                       WHERE eo.entity_id = entities.id),
                         last_seen = (SELECT MAX(effective_date) FROM entity_occurrences eo
                                      WHERE eo.entity_id = entities.id)
                     WHERE id = ?1",
                    vec![Value::Integer(entity_id)],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit partial occurrence upsert")?;
            Ok(())
        })
    }

    /// Replace a config-declared append-log entity's occurrences wholesale.
    ///
    /// Logs need replace semantics, not the accumulate semantics of
    /// `upsert_entities_batch`: a log re-emits every entry each time its file
    /// is reprocessed, so accumulating would duplicate occurrences and
    /// inflate frequency on every edit. Frequency is set to the occurrence
    /// count (= entry count), and first/last_seen to the entry-date span, so
    /// the entity ranks by the log's real activity.
    pub fn replace_append_log_entity(
        &self,
        name: &str,
        occurrences: &[EntityOccurrence],
    ) -> Result<()> {
        let name = name.to_string();
        let occurrences = occurrences.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for replace_append_log_entity")?;

            let first_seen = occurrences.iter().filter_map(|o| o.effective_date).min();
            let last_seen = occurrences.iter().filter_map(|o| o.effective_date).max();

            tx.execute(
                "INSERT OR IGNORE INTO entities (name, type, frequency, first_seen, last_seen)
                 VALUES (?1, 'append_log', 0, ?2, ?3)",
                vec![
                    Value::Text(name.clone()),
                    opt_val(first_seen),
                    opt_val(last_seen),
                ],
            )
            .await?;
            let mut rows = tx
                .query(
                    "SELECT id FROM entities WHERE name = ?1 AND type = 'append_log'",
                    vec![Value::Text(name.clone())],
                )
                .await?;
            let entity_id: i64 = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("append_log entity not found after insert"))?
                .get::<i64>(0)?;
            drop(rows);

            tx.execute(
                "DELETE FROM entity_occurrences WHERE entity_id = ?1",
                vec![Value::Integer(entity_id)],
            )
            .await?;
            tx.execute(
                "UPDATE entities SET frequency = ?1, first_seen = ?2, last_seen = ?3
                 WHERE id = ?4",
                vec![
                    Value::Integer(occurrences.len() as i64),
                    opt_val(first_seen),
                    opt_val(last_seen),
                    Value::Integer(entity_id),
                ],
            )
            .await?;
            for occ in &occurrences {
                tx.execute(
                    "INSERT INTO entity_occurrences
                     (entity_id, file_path, context, created_at, effective_date, date_source, weight)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    vec![
                        Value::Integer(entity_id),
                        Value::Text(occ.file_path.clone()),
                        opt_val(occ.context.clone()),
                        Value::Integer(occ.created_at),
                        opt_val(occ.effective_date),
                        Value::Text(occ.date_source.clone()),
                        Value::Real(occ.weight),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit replace_append_log_entity")?;
            Ok(())
        })
    }

    /// Delete append-log entities (and their occurrences) whose names are not
    /// in the configured set. Called when a `log:` line is removed from
    /// config — the entity disappears on the next refresh.
    pub fn delete_append_log_entities_not_in(
        &self,
        keep: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let existing = self.query_read(
            "SELECT id, name FROM entities WHERE type = 'append_log'",
            vec![],
            |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
        )?;
        let mut removed = 0;
        for (id, name) in existing {
            if !keep.contains(&name) {
                self.execute_write(
                    "DELETE FROM entity_occurrences WHERE entity_id = ?1",
                    vec![Value::Integer(id)],
                )?;
                self.execute_write(
                    "DELETE FROM entities WHERE id = ?1",
                    vec![Value::Integer(id)],
                )?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Get all occurrences for a given entity.
    pub fn get_entity_occurrences(&self, entity_id: i64) -> Result<Vec<EntityOccurrence>> {
        let rows = self.query_read(
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source, weight
             FROM entity_occurrences WHERE entity_id = ?1",
            vec![Value::Integer(entity_id)],
            |row| {
                Ok(EntityOccurrence {
                    id: Some(row.get::<i64>(0)?),
                    entity_id: row.get::<i64>(1)?,
                    file_path: row.get::<String>(2)?,
                    context: row.get::<Option<String>>(3)?,
                    created_at: row.get::<Option<i64>>(4)?.unwrap_or(0),
                    effective_date: row.get::<Option<i64>>(5)?,
                    date_source: row.get::<Option<String>>(6)?.unwrap_or_default(),
                    weight: row.get::<f64>(7)?,
                })
            },
        )?;
        Ok(rows)
    }

    /// Snapshot collection occurrences for selected documents before re-indexing.
    ///
    /// Bounded remote imports use this to retain previously observed collection
    /// membership. A page that does not include an older connection is not
    /// evidence that the connection was removed upstream.
    pub fn get_collection_occurrences_for_paths_by_date_source(
        &self,
        file_paths: &[String],
        date_source: &str,
    ) -> Result<Vec<(String, EntityOccurrence)>> {
        if file_paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for batch in file_paths.chunks(500) {
            let placeholders = (0..batch.len())
                .map(|index| format!("?{}", index + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT e.name, eo.id, eo.entity_id, eo.file_path, eo.context,
                        eo.created_at, eo.effective_date, eo.date_source, eo.weight
                 FROM entity_occurrences eo
                 JOIN entities e ON e.id = eo.entity_id
                 WHERE e.type = 'collection' AND eo.date_source = ?1
                   AND eo.file_path IN ({placeholders})"
            );
            let mut params = vec![Value::Text(date_source.to_string())];
            params.extend(batch.iter().cloned().map(Value::Text));
            result.extend(self.query_read(&sql, params, |row| {
                Ok((
                    row.get::<String>(0)?,
                    EntityOccurrence {
                        id: Some(row.get::<i64>(1)?),
                        entity_id: row.get::<i64>(2)?,
                        file_path: row.get::<String>(3)?,
                        context: row.get::<Option<String>>(4)?,
                        created_at: row.get::<Option<i64>>(5)?.unwrap_or(0),
                        effective_date: row.get::<Option<i64>>(6)?,
                        date_source: row.get::<Option<String>>(7)?.unwrap_or_default(),
                        weight: row.get::<f64>(8)?,
                    },
                ))
            })?);
        }
        Ok(result)
    }

    /// Load all chunks for the given file paths.
    pub fn get_chunks_for_files(&self, file_paths: &[String]) -> Result<Vec<FileChunk>> {
        if file_paths.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = (1..=file_paths.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT d.source_ref, c.chunk_index, c.content, c.entry_date
             FROM chunks c
             JOIN docs d ON d.id = c.doc_id
             WHERE d.source_ref IN ({placeholders})
             ORDER BY d.source_ref, c.chunk_index"
        );
        let params = file_paths
            .iter()
            .cloned()
            .map(Value::Text)
            .collect::<Vec<_>>();

        Ok(self.query_read(&sql, params, |row| {
            Ok(FileChunk {
                file_path: row.get::<String>(0)?,
                chunk_index: row.get::<i64>(1)? as u32,
                content: row.get::<String>(2)?,
                entry_date: row.get::<Option<i64>>(3)?,
            })
        })?)
    }

    /// Get the top entities, ordered by the given column.
    pub fn get_top_entities(&self, limit: usize, order_by: &str) -> Result<Vec<Entity>> {
        if order_by == "recency_score" {
            let mut entities = self.get_all_entities()?;
            entities.sort_by(|a, b| {
                b.trends
                    .recency_score
                    .partial_cmp(&a.trends.recency_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            entities.truncate(limit);
            Ok(entities)
        } else {
            let rows = self.query_read(
                "SELECT e.id, e.name, e.type, e.frequency,
                        CAST(COALESCE((SELECT SUM(eo.weight) FROM entity_occurrences eo WHERE eo.entity_id = e.id), e.frequency) AS REAL),
                        e.first_seen, e.last_seen, e.trends, e.metadata
                 FROM entities e ORDER BY 5 DESC LIMIT ?1",
                vec![Value::Integer(limit as i64)],
                map_entity_row,
            )?;
            Ok(rows)
        }
    }

    /// Batch-update trends JSON blobs for multiple entities in a single transaction.
    pub fn update_entity_trends_batch(&self, updates: &[(i64, EntityTrends)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let updates: Vec<(i64, EntityTrends)> = updates.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for update_entity_trends_batch")?;

            for (entity_id, trends) in &updates {
                let trends_json =
                    serde_json::to_string(trends).context("failed to serialize EntityTrends")?;
                tx.execute(
                    "UPDATE entities SET trends = ?1 WHERE id = ?2",
                    vec![Value::Text(trends_json), Value::Integer(*entity_id)],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit update_entity_trends_batch")?;
            Ok(())
        })
    }

    /// Compute selection scores from distinct source documents, independent of
    /// occurrence repetitions and cached trends from older indexers.
    ///
    /// With an `as of` instant, documents dated after it are ignored and
    /// activity decays from that instant rather than the wall clock.
    pub fn get_entity_selection_stats(
        &self,
        entity_ids: &[i64],
        as_of_ms: Option<i64>,
    ) -> Result<HashMap<i64, EntitySelectionStats>> {
        self.get_entity_selection_stats_in_prefix(entity_ids, as_of_ms, None)
    }

    /// Selection scores limited to one SQLite source's document namespace.
    pub fn get_entity_selection_stats_in_prefix(
        &self,
        entity_ids: &[i64],
        as_of_ms: Option<i64>,
        source_prefix: Option<&str>,
    ) -> Result<HashMap<i64, EntitySelectionStats>> {
        let now = as_of_ms.unwrap_or_else(|| Utc::now().timestamp_millis());
        let ids: Vec<_> = entity_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut rows = Vec::new();
        for batch in ids.chunks(500) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let mut params: Vec<Value> = batch.iter().copied().map(Value::Integer).collect();
            let scope = if let Some(prefix) = source_prefix {
                params.push(Value::Integer(prefix.len() as i64));
                params.push(Value::Text(prefix.to_owned()));
                format!(
                    " AND substr(file_path, 1, ?{}) = ?{}",
                    params.len() - 1,
                    params.len()
                )
            } else {
                String::new()
            };
            rows.extend(self.query_read(
                &format!("SELECT entity_id, file_path, COALESCE(effective_date, created_at, 0), SUM(weight)
                 FROM entity_occurrences WHERE entity_id IN ({placeholders}){scope}
                 GROUP BY entity_id, file_path, COALESCE(effective_date, created_at, 0)
                 ORDER BY entity_id, file_path, COALESCE(effective_date, created_at, 0)"),
                params,
                |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?, row.get::<i64>(2)?, row.get::<f64>(3)?)),
            )?);
        }
        let mut docs = HashMap::<i64, std::collections::BTreeMap<String, i64>>::new();
        let mut legacy = HashMap::<i64, (f64, f64, bool)>::new();
        for (id, path, date, weight) in rows {
            if as_of_ms.is_some_and(|reference| date > reference) {
                continue;
            }
            docs.entry(id)
                .or_default()
                .entry(path)
                .and_modify(|d| *d = (*d).max(date))
                .or_insert(date);
            let acc = legacy.entry(id).or_default();
            acc.0 += activity_decay(now, date) * weight;
            acc.1 += weight;
            acc.2 |= date > now;
        }
        Ok(docs
            .into_iter()
            .map(|(id, dates)| {
                let (sum, weight, future) = legacy[&id];
                (
                    id,
                    EntitySelectionStats {
                        documents: dates.len(),
                        recency: dates.values().map(|date| activity_decay(now, *date)).sum(),
                        corrected_legacy_recency: future
                            .then(|| sum * (1.0 + (1.0 + weight.max(0.0)).ln())),
                    },
                )
            })
            .collect())
    }

    /// Link entities with at least one occurrence in the named SQLite source.
    /// SQLite ingestion creates link occurrences from its mapped `who` values.
    pub fn get_sqlite_who_link_candidates(&self, source_prefix: &str) -> Result<Vec<Entity>> {
        Ok(self.query_read(
            "SELECT DISTINCT e.id, e.name, e.type, e.frequency,
                    CAST(COALESCE((SELECT SUM(weight) FROM entity_occurrences WHERE entity_id = e.id), e.frequency) AS REAL),
                    e.first_seen, e.last_seen, e.trends, e.metadata
             FROM entities e JOIN entity_occurrences eo ON eo.entity_id = e.id
             WHERE e.type = 'link' AND substr(eo.file_path, 1, ?1) = ?2",
            vec![Value::Integer(source_prefix.len() as i64), Value::Text(source_prefix.to_owned())],
            map_entity_row,
        )?)
    }

    /// Newest occurrence date (effective date, else creation time) not after `limit_ms`.
    pub fn latest_evidence_ms(&self, limit_ms: i64) -> Result<Option<i64>> {
        let rows = self.query_read(
            "SELECT MAX(COALESCE(effective_date, created_at)) FROM entity_occurrences
             WHERE COALESCE(effective_date, created_at) <= ?1",
            vec![Value::Integer(limit_ms)],
            |row| row.get::<Option<i64>>(0),
        )?;
        Ok(rows.into_iter().next().flatten())
    }

    /// Fetch all occurrence timestamps in one query, grouped by entity_id.
    pub fn get_all_occurrence_dates(&self) -> Result<Vec<(i64, Option<i64>, i64, f64)>> {
        let rows = self.query_read(
            "SELECT entity_id, effective_date, created_at, weight
             FROM entity_occurrences
             ORDER BY entity_id",
            vec![],
            |row| {
                Ok((
                    row.get::<i64>(0)?,
                    row.get::<Option<i64>>(1)?,
                    row.get::<i64>(2)?,
                    row.get::<f64>(3)?,
                ))
            },
        )?;
        Ok(rows)
    }

    /// Fetch effective document dates for occurrences of the requested entities.
    ///
    /// Retrieval uses the occurrence's semantic date when present and falls back
    /// to the owning document's creation date. The entity id is retained because
    /// a path can belong to more than one anchored entity timeline.
    pub fn get_entity_occurrence_effective_dates(
        &self,
        entity_ids: &[i64],
    ) -> Result<Vec<(i64, String, i64)>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = (1..=entity_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT eo.entity_id, eo.file_path,
                    COALESCE(eo.effective_date, d.created_at)
             FROM entity_occurrences eo
             JOIN docs d ON d.source_ref = eo.file_path
             WHERE eo.entity_id IN ({placeholders})"
        );
        let params = entity_ids
            .iter()
            .copied()
            .map(Value::Integer)
            .collect::<Vec<_>>();

        Ok(self.query_read(&sql, params, |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
            ))
        })?)
    }

    /// Look up entities by their (name, type) pairs.
    pub fn get_entities_by_names(&self, names: &[(&str, EntityType)]) -> Result<Vec<Entity>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        for (name, entity_type) in names {
            let rows = self.query_read(
                "SELECT e.id, e.name, e.type, e.frequency,
                        CAST(COALESCE((SELECT SUM(eo.weight) FROM entity_occurrences eo WHERE eo.entity_id = e.id), e.frequency) AS REAL),
                        e.first_seen, e.last_seen, e.trends, e.metadata
                 FROM entities e WHERE LOWER(e.name) = ?1 AND e.type = ?2",
                vec![
                    Value::Text(name.to_lowercase()),
                    Value::Text(entity_type.as_str().to_string()),
                ],
                map_entity_row,
            )?;
            results.extend(rows);
        }

        Ok(results)
    }

    /// Every entity of one type, e.g. the candidates for a name-pattern reading.
    pub fn get_entities_by_type(&self, entity_type: &EntityType) -> Result<Vec<Entity>> {
        Ok(self.query_read(
            "SELECT e.id, e.name, e.type, e.frequency,
                    CAST(COALESCE((SELECT SUM(eo.weight) FROM entity_occurrences eo WHERE eo.entity_id = e.id), e.frequency) AS REAL),
                    e.first_seen, e.last_seen, e.trends, e.metadata
             FROM entities e WHERE e.type = ?1",
            vec![Value::Text(entity_type.as_str().to_string())],
            map_entity_row,
        )?)
    }

    /// Retrieve every entity in the database, in insertion (id) order, so
    /// selection ties break the same way on every run.
    pub fn get_all_entities(&self) -> Result<Vec<Entity>> {
        let rows = self.query_read(
            "SELECT e.id, e.name, e.type, e.frequency,
                    CAST(COALESCE((SELECT SUM(eo.weight) FROM entity_occurrences eo WHERE eo.entity_id = e.id), e.frequency) AS REAL),
                    e.first_seen, e.last_seen, e.trends, e.metadata
             FROM entities e
             ORDER BY e.id",
            vec![],
            map_entity_row,
        )?;
        Ok(rows)
    }

    /// Get the set of distinct file_paths for each entity (by entity_id).
    /// Returns a map from entity_id to a set of file paths.
    pub fn get_entity_file_sets(&self) -> Result<HashMap<i64, HashSet<String>>> {
        let rows = self.query_read(
            "SELECT entity_id, file_path FROM entity_occurrences",
            vec![],
            |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
        )?;
        let mut map: HashMap<i64, HashSet<String>> = HashMap::new();
        for (entity_id, file_path) in rows {
            map.entry(entity_id).or_default().insert(file_path);
        }
        Ok(map)
    }

    /// Get the set of file paths under each top-level folder.
    /// Returns a map from folder name (lowercase) to a set of source_ref paths.
    pub fn get_folder_file_sets(&self) -> Result<HashMap<String, HashSet<String>>> {
        let rows = self.query_read(
            "SELECT source_ref FROM docs WHERE source_ref LIKE '%/%'",
            vec![],
            |row| row.get::<String>(0),
        )?;
        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        for source_ref in rows {
            if let Some(slash) = source_ref.find('/') {
                let folder = source_ref[..slash].to_lowercase();
                map.entry(folder).or_default().insert(source_ref);
            }
        }
        Ok(map)
    }

    /// Get link entities whose source files live inside the given folders.
    ///
    /// For each folder, finds documents like `folder/Name.md` and returns
    /// any entity of type "link" whose name matches the doc stem (case-insensitive).
    pub fn get_folder_page_entity_links(&self, folders: &HashSet<&str>) -> Result<Vec<Entity>> {
        if folders.is_empty() {
            return Ok(Vec::new());
        }

        // Get all doc stems under the target folders
        let docs = self.query_read("SELECT source_ref FROM docs", vec![], |row| {
            row.get::<String>(0)
        })?;

        let mut stems: HashSet<String> = HashSet::new();
        for source_ref in &docs {
            if let Some(slash) = source_ref.find('/') {
                let folder = &source_ref[..slash];
                if folders.contains(&*folder.to_lowercase()) {
                    let filename = &source_ref[slash + 1..];
                    // Only direct children (no sub-subfolders)
                    if !filename.contains('/') {
                        let stem = filename
                            .strip_suffix(".md")
                            .unwrap_or(filename)
                            .to_lowercase();
                        stems.insert(stem);
                    }
                }
            }
        }

        if stems.is_empty() {
            return Ok(Vec::new());
        }

        // Find matching link entities with recent activity
        let all_entities = self.get_all_entities()?;
        Ok(all_entities
            .into_iter()
            .filter(|e| is_active_page_link(e) && stems.contains(&e.name.to_lowercase()))
            .collect())
    }

    /// Like `get_folder_page_entity_links`, but returns results grouped by folder.
    pub fn get_folder_page_entity_links_grouped(
        &self,
        folders: &HashSet<&str>,
    ) -> Result<HashMap<String, Vec<Entity>>> {
        let mut grouped = self.get_folder_page_entity_candidates_grouped(folders)?;
        for links in grouped.values_mut() {
            links.retain(is_active_page_link);
        }
        grouped.retain(|_, links| !links.is_empty());
        Ok(grouped)
    }

    /// Every link entity named like a direct child page of each folder, before
    /// the [`is_active_page_link`] gate. Folders without such pages are absent.
    pub fn get_folder_page_entity_candidates_grouped(
        &self,
        folders: &HashSet<&str>,
    ) -> Result<HashMap<String, Vec<Entity>>> {
        if folders.is_empty() {
            return Ok(HashMap::new());
        }

        let docs = self.query_read("SELECT source_ref FROM docs", vec![], |row| {
            row.get::<String>(0)
        })?;

        // Map: folder → set of direct-child doc stems. Folder identities can
        // be source-qualified (for example `markdown_<hash>/people` in a
        // multi-root workspace), so match the complete folder prefix rather
        // than assuming the first source-ref segment is the folder name.
        let mut folder_stems: HashMap<String, HashSet<String>> = HashMap::new();
        let normalized_folders = folders
            .iter()
            .map(|folder| {
                let folder = folder.to_lowercase();
                let prefix = format!("{folder}/");
                (folder, prefix)
            })
            .collect::<Vec<_>>();
        for source_ref in &docs {
            let normalized_source_ref = source_ref.to_lowercase();
            for (folder, prefix) in &normalized_folders {
                let Some(filename) = normalized_source_ref.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                if !filename.is_empty() && !filename.contains('/') {
                    let stem = filename.strip_suffix(".md").unwrap_or(filename).to_string();
                    folder_stems.entry(folder.clone()).or_default().insert(stem);
                }
            }
        }

        if folder_stems.is_empty() {
            return Ok(HashMap::new());
        }

        let all_entities = self.get_all_entities()?;
        let link_entities: Vec<&Entity> = all_entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Link)
            .collect();

        let mut result: HashMap<String, Vec<Entity>> = HashMap::new();
        for (folder, stems) in &folder_stems {
            let matches: Vec<Entity> = link_entities
                .iter()
                .filter(|e| stems.contains(&e.name.to_lowercase()))
                .cloned()
                .cloned()
                .collect();
            if !matches.is_empty() {
                result.insert(folder.clone(), matches);
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Context queries (for XML context builder)
    // -----------------------------------------------------------------------

    /// Derive folder structure from document `source_ref` paths.
    pub fn get_folder_structure(&self) -> Result<Vec<FolderStats>> {
        let top_folders = self.query_read(
            "SELECT
                CASE
                    WHEN INSTR(source_ref, '/') > 0
                        THEN SUBSTR(source_ref, 1, INSTR(source_ref, '/') - 1)
                    ELSE '__root__'
                END AS folder,
                COUNT(*) AS file_count,
                MAX(modified_at) AS most_recent
             FROM docs
             GROUP BY folder
             ORDER BY file_count DESC",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<Option<i64>>(2)?,
                ))
            },
        )?;

        let mut results = Vec::new();
        for (name, file_count, most_recent) in top_folders {
            if name == "__root__"
                || name == ".enzyme"
                || name == ".obsidian"
                || name == ".git"
                || name == ".trash"
                || name.starts_with('.')
            {
                continue;
            }

            let subfolders = self.query_read(
                "SELECT
                    SUBSTR(
                        source_ref,
                        LENGTH(?1) + 2,
                        CASE
                            WHEN INSTR(SUBSTR(source_ref, LENGTH(?1) + 2), '/') > 0
                                THEN INSTR(SUBSTR(source_ref, LENGTH(?1) + 2), '/') - 1
                            ELSE LENGTH(source_ref)
                        END
                    ) AS subfolder,
                    COUNT(*) AS file_count
                 FROM docs
                 WHERE source_ref LIKE ?1 || '/%'
                   AND INSTR(SUBSTR(source_ref, LENGTH(?1) + 2), '/') > 0
                 GROUP BY subfolder
                 ORDER BY file_count DESC
                 LIMIT 10",
                vec![Value::Text(name.clone())],
                |row| {
                    Ok(SubfolderStats {
                        name: row.get::<String>(0)?,
                        file_count: row.get::<i64>(1)?,
                    })
                },
            )?;

            results.push(FolderStats {
                name,
                file_count,
                most_recent,
                subfolders,
            });
        }

        Ok(results)
    }

    /// Get immediate child entities of a parent entity.
    pub fn get_child_entities(
        &self,
        parent_name: &str,
        entity_type: &str,
        limit: usize,
    ) -> Result<Vec<Entity>> {
        let like_pattern = format!("{}/%", parent_name);
        let not_like_pattern = format!("{}/%/%", parent_name);

        let rows = self.query_read(
            "SELECT e.id, e.name, e.type, e.frequency,
                    CAST(COALESCE((SELECT SUM(eo.weight) FROM entity_occurrences eo WHERE eo.entity_id = e.id), e.frequency) AS REAL),
                    e.first_seen, e.last_seen, e.trends, e.metadata
             FROM entities e
             WHERE e.name LIKE ?1 AND e.name NOT LIKE ?2 AND e.type = ?3
             ORDER BY 5 DESC
             LIMIT ?4",
            vec![
                Value::Text(like_pattern),
                Value::Text(not_like_pattern),
                Value::Text(entity_type.to_string()),
                Value::Integer(limit as i64),
            ],
            map_entity_row,
        )?;

        Ok(rows)
    }

    /// Get the most-referenced children of a folder, ranked by external wikilink count.
    pub fn get_folder_children_by_references(
        &self,
        folder_name: &str,
        limit: usize,
    ) -> Result<(Vec<FolderChild>, i64)> {
        let like_pattern = format!("{}/%", folder_name);
        let not_like_pattern = format!("{}/%/%", folder_name);

        let children_rows = self.query_read(
            "SELECT source_ref, modified_at
             FROM docs
             WHERE source_ref LIKE ?1 AND source_ref NOT LIKE ?2",
            vec![Value::Text(like_pattern), Value::Text(not_like_pattern)],
            |row| Ok((row.get::<String>(0)?, row.get::<Option<i64>>(1)?)),
        )?;

        let total_count = children_rows.len() as i64;
        let outside_pattern = format!("{}/%", folder_name);
        let mut children: Vec<FolderChild> = Vec::new();

        for (source_ref, modified_at) in &children_rows {
            let file_name = source_ref.rsplit('/').next().unwrap_or(source_ref);
            let stem = file_name.strip_suffix(".md").unwrap_or(file_name);

            let ref_count = self.query_read(
                "SELECT COUNT(DISTINCT dl.doc_id)
                 FROM doc_links dl
                 JOIN docs d ON d.id = dl.doc_id
                 WHERE dl.link = ?1 AND d.source_ref NOT LIKE ?2",
                vec![
                    Value::Text(stem.to_lowercase()),
                    Value::Text(outside_pattern.clone()),
                ],
                |row| row.get::<i64>(0),
            )?;
            let reference_count = ref_count.into_iter().next().unwrap_or(0);

            children.push(FolderChild {
                name: stem.to_string(),
                source_ref: source_ref.clone(),
                reference_count,
                last_seen: *modified_at,
            });
        }

        children.sort_by(|a, b| {
            b.reference_count
                .cmp(&a.reference_count)
                .then_with(|| a.name.cmp(&b.name))
        });
        children.truncate(limit);

        Ok((children, total_count))
    }

    /// Identify trending wikilinks that have source files directly inside a folder.
    pub fn get_folder_page_entities(
        &self,
        folder_name: &str,
        trending_links: &[&str],
    ) -> Result<Vec<String>> {
        if trending_links.is_empty() {
            return Ok(Vec::new());
        }

        let like_pattern = format!("{}/%", folder_name);
        let not_like_pattern = format!("{}/%/%", folder_name);

        let rows = self.query_read(
            "SELECT source_ref FROM docs
             WHERE source_ref LIKE ?1 AND source_ref NOT LIKE ?2",
            vec![Value::Text(like_pattern), Value::Text(not_like_pattern)],
            |row| row.get::<String>(0),
        )?;

        let folder_files: std::collections::HashSet<String> = rows
            .iter()
            .filter_map(|source_ref| {
                let file_name = source_ref.rsplit('/').next()?;
                let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
                Some(stem.to_lowercase())
            })
            .collect();

        let matching: Vec<String> = trending_links
            .iter()
            .filter(|link| folder_files.contains(link.to_lowercase().as_str()))
            .map(|link| link.to_string())
            .collect();

        Ok(matching)
    }

    /// Get recent occurrences for an entity, ordered by effective_date descending.
    pub fn get_recent_occurrences(
        &self,
        entity_id: i64,
        limit: usize,
    ) -> Result<Vec<EntityOccurrence>> {
        let rows = self.query_read(
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source, weight
             FROM entity_occurrences
             WHERE entity_id = ?1
             ORDER BY effective_date DESC
             LIMIT ?2",
            vec![Value::Integer(entity_id), Value::Integer(limit as i64)],
            |row| {
                Ok(EntityOccurrence {
                    id: Some(row.get::<i64>(0)?),
                    entity_id: row.get::<i64>(1)?,
                    file_path: row.get::<String>(2)?,
                    context: row.get::<Option<String>>(3)?,
                    created_at: row.get::<Option<i64>>(4)?.unwrap_or(0),
                    effective_date: row.get::<Option<i64>>(5)?,
                    date_source: row.get::<Option<String>>(6)?.unwrap_or_default(),
                    weight: row.get::<f64>(7)?,
                })
            },
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Catalyst operations
    // -----------------------------------------------------------------------

    /// Batch-insert catalysts.
    pub fn insert_catalysts(&self, catalysts: &[Catalyst]) -> Result<()> {
        let catalysts = catalysts.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for insert_catalysts")?;

            for cat in &catalysts {
                let generated_at = cat.generated_at.format("%Y-%m-%d %H:%M:%S").to_string();
                let metadata =
                    serde_json::to_string(&cat.metadata).unwrap_or_else(|_| "{}".to_string());
                tx.execute(
                    "INSERT OR REPLACE INTO catalysts
                     (id, text, entity, context, generated_at, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    vec![
                        Value::Text(cat.id.clone()),
                        Value::Text(cat.text.clone()),
                        Value::Text(cat.entity.clone()),
                        Value::Text(cat.context.clone()),
                        Value::Text(generated_at),
                        Value::Text(metadata),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit insert_catalysts")?;
            Ok(())
        })
    }

    /// Get all catalysts associated with a given entity name.
    pub fn get_catalysts_for_entity(&self, entity: &str) -> Result<Vec<Catalyst>> {
        let rows = self.query_read(
            "SELECT id, text, entity, context, generated_at, metadata
             FROM catalysts WHERE entity = ?1",
            vec![Value::Text(entity.to_string())],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get the set of entity names that have at least one catalyst.
    pub fn get_catalyzed_entity_names(&self) -> Result<std::collections::HashSet<String>> {
        let names = self.query_read("SELECT DISTINCT entity FROM catalysts", vec![], |row| {
            row.get::<String>(0)
        })?;
        Ok(names.into_iter().collect())
    }

    /// Get every catalyst in the database.
    pub fn get_all_catalysts(&self) -> Result<Vec<Catalyst>> {
        let rows = self.query_read(
            "SELECT id, text, entity, context, generated_at, metadata FROM catalysts",
            vec![],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get the most recent `generated_at` timestamp across all catalysts.
    pub fn get_latest_catalyst_timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        let rows = self.query_read("SELECT MAX(generated_at) FROM catalysts", vec![], |row| {
            row.get::<Option<String>>(0)
        })?;
        Ok(rows
            .into_iter()
            .next()
            .flatten()
            .map(|s| parse_datetime_utc(&s)))
    }

    /// Count distinct documents that mention a given entity (by entity name).
    pub fn get_entity_doc_count(&self, entity_name: &str) -> Result<usize> {
        let rows = self.query_read(
            "SELECT COUNT(DISTINCT eo.file_path)
             FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id
             WHERE e.name = ?1",
            vec![Value::Text(entity_name.to_string())],
            |row| row.get::<i64>(0),
        )?;
        Ok(rows.into_iter().next().unwrap_or(0) as usize)
    }

    /// Summarize the current document evidence for one entity.
    pub fn get_entity_refresh_stats(&self, entity_name: &str) -> Result<EntityRefreshStats> {
        let rows = self.query_read(
            "SELECT COUNT(DISTINCT eo.file_path),
                    MAX(COALESCE(eo.effective_date, eo.created_at))
             FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id
             WHERE e.name = ?1",
            vec![Value::Text(entity_name.to_string())],
            |row| {
                Ok(EntityRefreshStats {
                    doc_count: row.get::<i64>(0)? as usize,
                    latest_effective_date: row.get::<Option<i64>>(1)?,
                })
            },
        )?;
        Ok(rows.into_iter().next().unwrap_or(EntityRefreshStats {
            doc_count: 0,
            latest_effective_date: None,
        }))
    }

    /// Delete all catalysts for a given entity, keyed by (name, type).
    ///
    /// Entity names are not unique across types (a `#projects` tag and a
    /// `projects/` folder are distinct entities), so a name-only delete lets
    /// same-named entities clobber each other's freshly generated catalysts.
    /// The type lives in catalyst metadata; rows without one (written before
    /// the type was stamped) still match so legacy catalysts get replaced.
    pub fn delete_catalysts_for_entity(&self, entity: &str, entity_type: &str) -> Result<()> {
        let entity = entity.to_string();
        let entity_type = entity_type.to_string();
        block_in_place_sync(async move {
            let tx = self.conn.transaction().await?;
            let params = vec![
                Value::Text(entity.clone()),
                Value::Text(entity_type.clone()),
            ];
            tx.execute(
                "DELETE FROM catalysts WHERE entity = ?1
                   AND (json_extract(metadata, '$.entity_type') = ?2
                        OR json_extract(metadata, '$.entity_type') IS NULL)",
                params,
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    /// Per-entity catalyst context fingerprints, keyed by (entity, type).
    ///
    /// See `catalyst_entity_hashes` in the schema: a stored hash means the
    /// catalysts currently in the database for that entity were generated from
    /// exactly those inputs.
    pub fn get_catalyst_entity_hashes(&self) -> Result<HashMap<(String, String), String>> {
        let rows = self.query_read(
            "SELECT entity, entity_type, context_hash FROM catalyst_entity_hashes",
            vec![],
            |row| {
                Ok((
                    (row.get::<String>(0)?, row.get::<String>(1)?),
                    row.get::<String>(2)?,
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Record the context fingerprint for an entity's freshly stored catalysts.
    pub fn set_catalyst_entity_hash(
        &self,
        entity: &str,
        entity_type: &str,
        context_hash: &str,
        generated_at_ms: i64,
    ) -> Result<()> {
        self.execute_write(
            "INSERT INTO catalyst_entity_hashes
                 (entity, entity_type, context_hash, generated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(entity, entity_type) DO UPDATE SET
                 context_hash = excluded.context_hash,
                 generated_at_ms = excluded.generated_at_ms",
            vec![
                Value::Text(entity.to_string()),
                Value::Text(entity_type.to_string()),
                Value::Text(context_hash.to_string()),
                Value::Integer(generated_at_ms),
            ],
        )?;
        Ok(())
    }

    /// Drop an entity's context fingerprint (its catalysts are gone or stale).
    pub fn delete_catalyst_entity_hash(&self, entity: &str, entity_type: &str) -> Result<()> {
        self.execute_write(
            "DELETE FROM catalyst_entity_hashes WHERE entity = ?1 AND entity_type = ?2",
            vec![
                Value::Text(entity.to_string()),
                Value::Text(entity_type.to_string()),
            ],
        )?;
        Ok(())
    }

    /// Catalyst counts grouped by (entity, type). Rows written before the type
    /// was stamped into metadata report an empty type, matching
    /// [`Self::delete_catalysts_for_entity`]'s legacy-tolerant matching.
    pub fn get_catalyst_counts_by_entity(&self) -> Result<HashMap<(String, String), usize>> {
        let rows = self.query_read(
            "SELECT entity,
                    COALESCE(json_extract(metadata, '$.entity_type'), ''),
                    COUNT(*)
             FROM catalysts
             GROUP BY 1, 2",
            vec![],
            |row| {
                Ok((
                    (row.get::<String>(0)?, row.get::<String>(1)?),
                    row.get::<i64>(2)? as usize,
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Batch upsert catalyst similarity records.
    pub fn upsert_catalyst_similarities(&self, sims: &[CatalystSimilarity]) -> Result<()> {
        let sims = sims.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_catalyst_similarities")?;

            // Stay below SQLite's traditional 999-variable limit while
            // reducing one execute per row to one execute per 100 rows.
            const ROWS_PER_INSERT: usize = 100;
            for batch in sims.chunks(ROWS_PER_INSERT) {
                let mut sql = String::from(
                    "INSERT OR REPLACE INTO catalyst_similarities
                        (catalyst_id, catalyst_text, file_path, chunk_index,
                         similarity, chunk_metadata, timeline_data, created_at) VALUES ",
                );
                let mut params = Vec::with_capacity(batch.len() * 8);
                for (row, sim) in batch.iter().enumerate() {
                    if row > 0 {
                        sql.push(',');
                    }
                    let first = row * 8 + 1;
                    sql.push_str(&format!(
                        "(?{},?{},?{},?{},?{},?{},?{},?{})",
                        first,
                        first + 1,
                        first + 2,
                        first + 3,
                        first + 4,
                        first + 5,
                        first + 6,
                        first + 7
                    ));
                    params.extend([
                        Value::Text(sim.catalyst_id.clone()),
                        Value::Text(sim.catalyst_text.clone()),
                        Value::Text(sim.file_path.clone()),
                        Value::Integer(sim.chunk_index),
                        Value::Real(sim.similarity),
                        Value::Text(
                            serde_json::to_string(&sim.chunk_metadata)
                                .unwrap_or_else(|_| "{}".to_string()),
                        ),
                        Value::Text(
                            serde_json::to_string(&sim.timeline_data)
                                .unwrap_or_else(|_| "{}".to_string()),
                        ),
                        Value::Text(sim.created_at.format("%Y-%m-%d %H:%M:%S").to_string()),
                    ]);
                }
                tx.execute(&sql, params).await?;
            }

            tx.commit()
                .await
                .context("failed to commit upsert_catalyst_similarities")?;
            Ok(())
        })
    }

    /// Delete similarity rows for specific catalysts before recomputing them.
    pub fn delete_catalyst_similarities_for_catalysts(
        &self,
        catalyst_ids: &[String],
    ) -> Result<()> {
        if catalyst_ids.is_empty() {
            return Ok(());
        }

        let placeholders: Vec<String> = (1..=catalyst_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM catalyst_similarities WHERE catalyst_id IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<Value> = catalyst_ids.iter().cloned().map(Value::Text).collect();
        self.execute_write(&sql, params)?;
        Ok(())
    }

    /// Delete similarity rows for catalyst IDs that are no longer active.
    pub fn prune_catalyst_similarities_except(&self, catalyst_ids: &[String]) -> Result<()> {
        if catalyst_ids.is_empty() {
            self.execute_write("DELETE FROM catalyst_similarities", vec![])?;
            return Ok(());
        }

        let placeholders: Vec<String> = (1..=catalyst_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM catalyst_similarities WHERE catalyst_id NOT IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<Value> = catalyst_ids.iter().cloned().map(Value::Text).collect();
        self.execute_write(&sql, params)?;
        Ok(())
    }

    /// Remove persisted top-K rows whose chunk no longer has a current
    /// embedding, returning only the catalysts whose materialized top-K lost a
    /// row and therefore require an exact refill.
    pub fn prune_obsolete_catalyst_similarities(
        &self,
    ) -> Result<std::collections::HashSet<String>> {
        let affected = self.query_read(
            "SELECT DISTINCT catalyst_id FROM catalyst_similarities cs
             WHERE NOT EXISTS (
                 SELECT 1 FROM chunk_embeddings ce
                 WHERE ce.file_path = cs.file_path AND ce.chunk_index = cs.chunk_index
             )",
            vec![],
            |row| row.get::<String>(0),
        )?;
        self.execute_write(
            "DELETE FROM catalyst_similarities
             WHERE NOT EXISTS (
                 SELECT 1 FROM chunk_embeddings ce
                 WHERE ce.file_path = catalyst_similarities.file_path
                   AND ce.chunk_index = catalyst_similarities.chunk_index
             )",
            vec![],
        )?;
        Ok(affected.into_iter().collect())
    }

    /// Remove top-K rows for chunks whose vectors were replaced. Deleted
    /// chunks are handled by `prune_obsolete_catalyst_similarities`.
    pub fn delete_catalyst_similarities_for_chunks(
        &self,
        chunks: &[(String, u32)],
    ) -> Result<std::collections::HashSet<String>> {
        let mut affected = std::collections::HashSet::new();
        for (path, index) in chunks {
            let ids = self.query_read(
                "SELECT catalyst_id FROM catalyst_similarities
                 WHERE file_path = ?1 AND chunk_index = ?2",
                vec![Value::Text(path.clone()), Value::Integer(*index as i64)],
                |row| row.get::<String>(0),
            )?;
            affected.extend(ids);
            self.execute_write(
                "DELETE FROM catalyst_similarities
                 WHERE file_path = ?1 AND chunk_index = ?2",
                vec![Value::Text(path.clone()), Value::Integer(*index as i64)],
            )?;
        }
        Ok(affected)
    }

    /// Enforce the persisted per-catalyst top-K with deterministic tie breaks.
    pub fn trim_catalyst_similarities(&self, catalyst_id: &str, top_k: usize) -> Result<usize> {
        let count = self.execute_write(
            "DELETE FROM catalyst_similarities
             WHERE catalyst_id = ?1 AND id NOT IN (
                 SELECT id FROM catalyst_similarities WHERE catalyst_id = ?1
                 ORDER BY similarity DESC, file_path ASC, chunk_index ASC
                 LIMIT ?2
             )",
            vec![
                Value::Text(catalyst_id.to_string()),
                Value::Integer(top_k as i64),
            ],
        )?;
        Ok(count)
    }

    /// Get the top similarities for a given catalyst, ordered by similarity descending.
    pub fn get_similarities_for_catalyst(
        &self,
        catalyst_id: &str,
        limit: usize,
    ) -> Result<Vec<CatalystSimilarity>> {
        let rows = self.query_read(
            "SELECT id, catalyst_id, catalyst_text, file_path, chunk_index,
                    similarity, chunk_metadata, timeline_data, created_at
             FROM catalyst_similarities
             WHERE catalyst_id = ?1
             ORDER BY similarity DESC
             LIMIT ?2",
            vec![
                Value::Text(catalyst_id.to_string()),
                Value::Integer(limit as i64),
            ],
            map_catalyst_similarity_row,
        )?;
        Ok(rows)
    }

    /// Get top similarities for multiple catalysts in a single query.
    ///
    /// Returns at most `per_catalyst_limit` rows per catalyst, ordered by
    /// similarity descending.
    pub fn get_similarities_for_catalysts(
        &self,
        catalyst_ids: &[&str],
        per_catalyst_limit: usize,
    ) -> Result<Vec<CatalystSimilarity>> {
        if catalyst_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders: Vec<String> = (1..=catalyst_ids.len())
            .map(|i| format!("?{}", i))
            .collect();
        let in_clause = placeholders.join(", ");
        let limit_param = catalyst_ids.len() + 1;

        let sql = format!(
            "SELECT id, catalyst_id, catalyst_text, file_path, chunk_index,
                    similarity, chunk_metadata, timeline_data, created_at
             FROM (
                 SELECT *, ROW_NUMBER() OVER (
                     PARTITION BY catalyst_id ORDER BY similarity DESC
                 ) AS rn
                 FROM catalyst_similarities
                 WHERE catalyst_id IN ({in_clause})
             )
             WHERE rn <= ?{limit_param}"
        );

        let mut params: Vec<Value> = catalyst_ids
            .iter()
            .map(|id| Value::Text(id.to_string()))
            .collect();
        params.push(Value::Integer(per_catalyst_limit as i64));

        let rows = self.query_read(&sql, params, map_catalyst_similarity_row)?;
        Ok(rows)
    }

    /// Batch-fetch `modified_at` timestamps for multiple file paths.
    pub fn get_modified_at_batch(
        &self,
        file_paths: &[&str],
    ) -> Result<std::collections::HashMap<String, i64>> {
        if file_paths.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let placeholders: Vec<String> = (1..=file_paths.len()).map(|i| format!("?{}", i)).collect();
        let sql = format!(
            "SELECT source_ref, modified_at FROM docs WHERE source_ref IN ({})",
            placeholders.join(", ")
        );

        let params: Vec<Value> = file_paths
            .iter()
            .map(|p| Value::Text(p.to_string()))
            .collect();

        let rows = self.query_read(&sql, params, |row| {
            Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
        })?;

        Ok(rows.into_iter().collect())
    }

    // -----------------------------------------------------------------------
    // Chunk operations
    // -----------------------------------------------------------------------

    /// Find chunks containing a literal phrase, case-insensitively.
    ///
    /// This is deliberately a bounded verification path rather than a general
    /// lexical ranker. It lets callers prove that indexed material remains
    /// reachable even when the active catalyst surface is narrowly curated.
    pub fn get_chunks_containing_phrase(
        &self,
        phrase: &str,
        limit: usize,
    ) -> Result<Vec<FileChunk>> {
        let phrase = phrase.trim();
        if phrase.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self.query_read(
            "SELECT d.source_ref, c.chunk_index, c.content, c.entry_date
             FROM chunks c
             JOIN docs d ON d.id = c.doc_id
             WHERE instr(lower(c.content), lower(?1)) > 0
             ORDER BY d.modified_at DESC, d.source_ref, c.chunk_index
             LIMIT ?2",
            vec![
                Value::Text(phrase.to_string()),
                Value::Integer(limit as i64),
            ],
            |row| {
                Ok(FileChunk {
                    file_path: row.get::<String>(0)?,
                    chunk_index: row.get::<i64>(1)? as u32,
                    content: row.get::<String>(2)?,
                    entry_date: row.get::<Option<i64>>(3)?,
                })
            },
        )?)
    }

    /// Get all chunks belonging to documents that have been embedded.
    pub fn get_all_chunks_with_embeddings(&self) -> Result<Vec<(String, i64, String)>> {
        let rows = self.query_read(
            "SELECT d.source_ref, c.chunk_index, c.content
             FROM docs d
             JOIN chunks c ON d.id = c.doc_id
             WHERE d.embedded_at IS NOT NULL
             ORDER BY d.source_ref, c.chunk_index",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<String>(2)?,
                ))
            },
        )?;
        Ok(rows)
    }

    /// FTS5 is installed by init/refresh indexing. Older read-only
    /// vaults retain the in-memory search path until their next refresh.
    pub fn has_chunk_fts(&self) -> Result<bool> {
        Ok(!self
            .query_read(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='chunks_fts'",
                vec![],
                |row| row.get::<i64>(0),
            )?
            .is_empty())
    }

    /// Rank matching chunks with twice the weight on vault path words.
    pub fn search_chunks_fts(
        &self,
        query: &str,
        chunk_depth: usize,
        note_limit: usize,
    ) -> Result<Vec<(String, i64, f64)>> {
        if query.is_empty() || chunk_depth == 0 || note_limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self.query_read(
            "WITH hits AS MATERIALIZED (
                SELECT chunks_fts.rowid AS id, chunks_fts.rank AS score,
                       d.id AS doc_id, d.source_ref AS path, c.chunk_index
                FROM chunks_fts
                JOIN chunks c ON c.id = chunks_fts.rowid
                JOIN docs d ON d.id = c.doc_id
                WHERE chunks_fts MATCH ?1
                  AND rank MATCH 'bm25(2.0, 1.0)'
                  AND d.embedded_at IS NOT NULL
                ORDER BY rank LIMIT ?2
             ), ranked AS (
                SELECT path, chunk_index, score,
                       ROW_NUMBER() OVER (PARTITION BY doc_id ORDER BY score, id) AS note_row
                FROM hits
             )
             SELECT path, chunk_index, -score FROM ranked WHERE note_row = 1
             ORDER BY score, path LIMIT ?3",
            vec![
                Value::Text(query.to_string()),
                Value::Integer(chunk_depth as i64),
                Value::Integer(note_limit as i64),
            ],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<f64>(2)?,
                ))
            },
        )?)
    }

    /// Get all chunks with full metadata for the explore UI.
    pub fn get_all_chunks_enriched(
        &self,
    ) -> Result<Vec<(i64, i64, i64, String, String, i64, i64, i64, Option<String>)>> {
        let rows = self.query_read(
            "SELECT d.id, c.id, c.chunk_index, d.source_ref, c.content,
                    c.word_count, d.created_at, d.modified_at, d.title
             FROM docs d
             JOIN chunks c ON d.id = c.doc_id
             WHERE d.embedded_at IS NOT NULL
             ORDER BY d.source_ref, c.chunk_index",
            vec![],
            |row| {
                Ok((
                    row.get::<i64>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<i64>(2)?,
                    row.get::<String>(3)?,
                    row.get::<String>(4)?,
                    row.get::<i64>(5)?,
                    row.get::<i64>(6)?,
                    row.get::<i64>(7)?,
                    row.get::<Option<String>>(8)?,
                ))
            },
        )?;
        Ok(rows)
    }

    /// Get all tags grouped by doc_id.
    pub fn get_all_doc_tags(&self) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        let rows = self.query_read(
            "SELECT doc_id, tag FROM doc_tags ORDER BY doc_id, tag",
            vec![],
            |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
        )?;
        let mut map = std::collections::HashMap::new();
        for (doc_id, tag) in rows {
            map.entry(doc_id).or_insert_with(Vec::new).push(tag);
        }
        Ok(map)
    }

    /// Get tags for a document by its source_ref (relative vault path).
    pub fn get_tags_for_document(&self, source_ref: &str) -> Result<Vec<String>> {
        let rows = self.query_read(
            "SELECT dt.tag
             FROM doc_tags dt
             JOIN docs d ON d.id = dt.doc_id
             WHERE d.source_ref = ?1
             ORDER BY dt.tag",
            vec![Value::Text(source_ref.to_string())],
            |row| row.get::<String>(0),
        )?;
        Ok(rows)
    }

    /// Get the content of a specific chunk by file path and chunk index.
    pub fn get_chunk_content(&self, file_path: &str, chunk_index: i64) -> Result<Option<String>> {
        let rows = self.query_read(
            "SELECT c.content
             FROM chunks c
             JOIN docs d ON d.id = c.doc_id
             WHERE d.source_ref = ?1 AND c.chunk_index = ?2",
            vec![
                Value::Text(file_path.to_string()),
                Value::Integer(chunk_index),
            ],
            |row| row.get::<String>(0),
        )?;
        Ok(rows.into_iter().next())
    }

    // -----------------------------------------------------------------------
    // Config operations
    // -----------------------------------------------------------------------

    /// Set a configuration value (insert or update).
    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        self.execute_write(
            "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            vec![Value::Text(key.to_string()), Value::Text(value.to_string())],
        )?;
        Ok(())
    }

    /// Get a configuration value by key.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let rows = self.query_read(
            "SELECT value FROM config WHERE key = ?1",
            vec![Value::Text(key.to_string())],
            |row| row.get::<String>(0),
        )?;
        Ok(rows.into_iter().next())
    }

    /// Load read-only orientation from the most recent SQLite source refresh.
    pub fn get_source_refresh_status(
        &self,
        source_name: &str,
    ) -> Result<Option<(usize, usize, i64)>> {
        let rows = self.query_read(
            "SELECT row_count, bucket_count, refreshed_at_ms
             FROM source_refreshes WHERE source_name = ?1",
            vec![Value::Text(source_name.to_string())],
            |row| {
                Ok((
                    row.get::<i64>(0)? as usize,
                    row.get::<i64>(1)? as usize,
                    row.get::<i64>(2)?,
                ))
            },
        )?;
        Ok(rows.into_iter().next())
    }

    /// Record source orientation after document and occurrence reconciliation.
    pub fn set_source_refresh_status(
        &self,
        source_name: &str,
        row_count: usize,
        bucket_count: usize,
        refreshed_at_ms: i64,
    ) -> Result<()> {
        self.execute_write(
            "INSERT INTO source_refreshes
                (source_name, row_count, bucket_count, refreshed_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source_name) DO UPDATE SET
                row_count = excluded.row_count,
                bucket_count = excluded.bucket_count,
                refreshed_at_ms = excluded.refreshed_at_ms",
            vec![
                Value::Text(source_name.to_string()),
                Value::Integer(row_count as i64),
                Value::Integer(bucket_count as i64),
                Value::Integer(refreshed_at_ms),
            ],
        )?;
        Ok(())
    }

    /// Clear generated vector-derived state when the embedding backend changes.
    ///
    /// This preserves user notes, indexed documents, chunks, catalysts, targets,
    /// and config, but forces document embeddings and catalyst similarities to
    /// be rebuilt in the current embedding space.
    pub fn reset_generated_vector_state_if_backend_changed(
        &self,
        key: &str,
        backend_id: &str,
    ) -> Result<bool> {
        if self.get_config(key)?.as_deref() == Some(backend_id) {
            return Ok(false);
        }

        let has_generated_state = self
            .query_read(
                "SELECT EXISTS(SELECT 1 FROM chunk_embeddings)
                    OR EXISTS(SELECT 1 FROM catalyst_embeddings_packed)
                    OR EXISTS(SELECT 1 FROM catalyst_similarities)
                    OR EXISTS(SELECT 1 FROM context_cache)
                    OR EXISTS(
                        SELECT 1 FROM docs
                        WHERE embedded_at IS NOT NULL
                           OR embedded_content_hash IS NOT NULL
                    )",
                vec![],
                |row| row.get::<i64>(0),
            )?
            .into_iter()
            .next()
            .unwrap_or(0)
            != 0;

        let key = key.to_string();
        let backend_id = backend_id.to_string();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin generated vector reset")?;

            if has_generated_state {
                tx.execute(
                    "UPDATE docs SET embedded_at = NULL, embedded_content_hash = NULL",
                    (),
                )
                .await?;
                tx.execute("DELETE FROM chunk_embeddings", ()).await?;
                tx.execute("DELETE FROM catalyst_embeddings_packed", ())
                    .await?;
                tx.execute("DELETE FROM catalyst_similarities", ()).await?;
                tx.execute("DELETE FROM context_cache", ()).await?;
            }
            tx.execute(
                "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
                vec![Value::Text(key), Value::Text(backend_id)],
            )
            .await?;

            tx.commit()
                .await
                .context("failed to commit generated vector reset")?;
            Ok(has_generated_state)
        })
    }

    // -----------------------------------------------------------------------
    // Applied target operations
    // -----------------------------------------------------------------------

    /// Insert or update an applied target record.
    pub fn upsert_applied_target(&self, target: &AppliedTarget) -> Result<()> {
        let applied_at = target.applied_at.format("%Y-%m-%d %H:%M:%S").to_string();
        self.execute_write(
            "INSERT INTO applied_targets (target_path, doc_count, catalyst_count, applied_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(target_path) DO UPDATE SET
                 doc_count = excluded.doc_count,
                 catalyst_count = excluded.catalyst_count,
                 applied_at = excluded.applied_at",
            vec![
                Value::Text(target.target_path.clone()),
                Value::Integer(target.doc_count as i64),
                Value::Integer(target.catalyst_count as i64),
                Value::Text(applied_at),
            ],
        )?;
        Ok(())
    }

    /// Get all prepared external targets.
    pub fn get_all_applied_targets(&self) -> Result<Vec<AppliedTarget>> {
        let rows = self.query_read(
            "SELECT id, target_path, doc_count, catalyst_count, applied_at
             FROM applied_targets
             ORDER BY applied_at DESC",
            vec![],
            map_applied_target_row,
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Chunk embedding operations
    // -----------------------------------------------------------------------

    /// Batch-upsert chunk embeddings. Vectors are stored as little-endian f32 BLOBs.
    pub fn upsert_chunk_embeddings(&self, embeddings: &[(String, u32, Vec<f32>)]) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let embeddings = embeddings.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_chunk_embeddings")?;

            for (file_path, chunk_index, vector) in &embeddings {
                let blob = quantize::quantize_vector(vector);
                tx.execute(
                    "INSERT OR REPLACE INTO chunk_embeddings (file_path, chunk_index, vector)
                     VALUES (?1, ?2, ?3)",
                    vec![
                        Value::Text(file_path.clone()),
                        Value::Integer(*chunk_index as i64),
                        Value::Blob(blob),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit upsert_chunk_embeddings")?;
            Ok(())
        })
    }

    /// Replace chunk embeddings for many documents and mark them embedded in one transaction.
    pub fn replace_chunk_embeddings_for_documents(
        &self,
        file_paths: Vec<String>,
        embeddings: Vec<(String, u32, Vec<f32>)>,
        embedded_docs: Vec<(i64, String, Timestamp)>,
    ) -> Result<()> {
        if file_paths.is_empty() && embeddings.is_empty() && embedded_docs.is_empty() {
            return Ok(());
        }

        block_in_place_sync(async move {
            let tx = self.conn.transaction().await.context(
                "failed to begin transaction for replace_chunk_embeddings_for_documents",
            )?;

            for file_path in &file_paths {
                tx.execute(
                    "DELETE FROM chunk_embeddings WHERE file_path = ?1",
                    vec![Value::Text(file_path.clone())],
                )
                .await?;
            }

            for (file_path, chunk_index, vector) in &embeddings {
                let blob = quantize::quantize_vector(vector);
                tx.execute(
                    "INSERT OR REPLACE INTO chunk_embeddings (file_path, chunk_index, vector)
                     VALUES (?1, ?2, ?3)",
                    vec![
                        Value::Text(file_path.clone()),
                        Value::Integer(*chunk_index as i64),
                        Value::Blob(blob),
                    ],
                )
                .await?;
            }

            for (doc_id, content_hash, embedded_at) in &embedded_docs {
                tx.execute(
                    "UPDATE docs SET embedded_at = ?1, embedded_content_hash = ?2 WHERE id = ?3",
                    vec![
                        Value::Integer(*embedded_at),
                        Value::Text(content_hash.clone()),
                        Value::Integer(*doc_id),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit replace_chunk_embeddings_for_documents")?;
            Ok(())
        })
    }

    /// Load all chunk embeddings from the database.
    pub fn get_all_chunk_embeddings(&self) -> Result<Vec<(String, u32, Vec<f32>)>> {
        let rows = self.query_read(
            "SELECT file_path, chunk_index, vector FROM chunk_embeddings
             ORDER BY file_path, chunk_index",
            vec![],
            |row| {
                let file_path: String = row.get::<String>(0)?;
                let chunk_index: i64 = row.get::<i64>(1)?;
                let blob: Vec<u8> = row.get::<Vec<u8>>(2)?;
                Ok((
                    file_path,
                    chunk_index as u32,
                    quantize::decode_vector(&blob, EMBEDDING_DIMENSION),
                ))
            },
        )?;
        Ok(rows)
    }

    /// Scan stored vectors directly, retaining only the best chunk per note.
    /// This avoids materializing every decoded vector on each hybrid query.
    pub fn best_chunk_embedding_hits(&self, query: &[f32]) -> Result<HashMap<String, (i64, f64)>> {
        if let Some(path) = &self.local_path {
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let mut stmt =
                conn.prepare("SELECT file_path, chunk_index, vector FROM chunk_embeddings")?;
            let mut rows = stmt.query([])?;
            let mut hits: HashMap<String, (i64, f64)> = HashMap::new();
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                let chunk_index: i64 = row.get(1)?;
                let blob: &[u8] = row.get_ref(2)?.as_blob()?;
                let vec = quantize::decode_vector(blob, EMBEDDING_DIMENSION);
                let sim = Embedding::cosine_similarity(query, &vec) as f64;
                let slot = hits.entry(path).or_insert((chunk_index, f64::MIN));
                if sim > slot.1 {
                    *slot = (chunk_index, sim);
                }
            }
            return Ok(hits);
        }
        block_in_place_sync(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT file_path, chunk_index, vector FROM chunk_embeddings",
                    (),
                )
                .await?;
            let mut hits: HashMap<String, (i64, f64)> = HashMap::new();
            while let Some(row) = rows.next().await? {
                let path: String = row.get(0)?;
                let chunk_index: i64 = row.get(1)?;
                let blob: Vec<u8> = row.get(2)?;
                let vec = quantize::decode_vector(&blob, EMBEDDING_DIMENSION);
                let sim = Embedding::cosine_similarity(query, &vec) as f64;
                let slot = hits.entry(path).or_insert((chunk_index, f64::MIN));
                if sim > slot.1 {
                    *slot = (chunk_index, sim);
                }
            }
            Ok(hits)
        })
    }

    /// Delete chunk embeddings for a specific file (used before re-embedding).
    pub fn delete_chunk_embeddings_for_file(&self, file_path: &str) -> Result<()> {
        self.execute_write(
            "DELETE FROM chunk_embeddings WHERE file_path = ?1",
            vec![Value::Text(file_path.to_string())],
        )?;
        Ok(())
    }

    /// Prune chunk embeddings whose file_path no longer exists in the docs table.
    /// Returns the number of rows deleted.
    pub fn prune_stale_chunk_embeddings(&self) -> Result<usize> {
        let count = self.execute_write(
            "DELETE FROM chunk_embeddings
             WHERE file_path NOT IN (SELECT source_ref FROM docs)",
            vec![],
        )?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // Catalyst embedding operations (packed format)
    // -----------------------------------------------------------------------

    /// Batch-upsert catalyst embeddings using the packed BLOB format.
    ///
    /// All embeddings are stored as a single row in `catalyst_embeddings_packed`
    /// to eliminate per-row async overhead on the libsql read path.
    pub fn upsert_catalyst_embeddings(&self, embeddings: &[(&str, &[f32])]) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }

        let n = embeddings.len();
        let ids: Vec<String> = embeddings.iter().map(|(id, _)| id.to_string()).collect();
        let ids_json = serde_json::to_string(&ids).context("serialize catalyst ids")?;
        let all_vecs: Vec<u8> =
            quantize::quantize_packed(embeddings.iter().map(|(_, vector)| *vector));

        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("begin upsert_catalyst_embeddings")?;
            tx.execute("DELETE FROM catalyst_embeddings_packed", ())
                .await?;
            tx.execute(
                "INSERT INTO catalyst_embeddings_packed (count, ids, vectors) VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(n as i64),
                    Value::Text(ids_json),
                    Value::Blob(all_vecs),
                ],
            )
            .await?;
            tx.commit()
                .await
                .context("commit upsert_catalyst_embeddings")
        })
    }

    /// Load all catalyst embeddings from the packed table.
    pub fn get_all_catalyst_embeddings(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let rows = self.query_read(
            "SELECT ids, vectors FROM catalyst_embeddings_packed WHERE id = 1",
            vec![],
            |row| {
                let ids_json: String = row.get::<String>(0)?;
                let vectors_blob: Vec<u8> = row.get::<Vec<u8>>(1)?;
                Ok((ids_json, vectors_blob))
            },
        )?;

        let (ids_json, vectors_blob) = match rows.into_iter().next() {
            Some(pair) => pair,
            None => return Ok(vec![]),
        };

        let ids: Vec<String> =
            serde_json::from_str(&ids_json).context("deserialize catalyst ids")?;
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let count = ids.len();
        let vectors = quantize::decode_packed(&vectors_blob, count, EMBEDDING_DIMENSION);
        let results: Vec<(String, Vec<f32>)> = ids.into_iter().zip(vectors).collect();
        Ok(results)
    }

    pub fn get_epoch_catalyst_embeddings(&self, epoch_id: &str) -> Result<Vec<(String, Vec<f32>)>> {
        let row = self
            .query_read(
                "SELECT ids,vectors FROM catalyst_epoch_embeddings_packed WHERE epoch_id=?1",
                vec![Value::Text(epoch_id.to_string())],
                |row| Ok((row.get::<String>(0)?, row.get::<Vec<u8>>(1)?)),
            )?
            .into_iter()
            .next();
        let Some((ids_json, vectors_blob)) = row else {
            return Ok(Vec::new());
        };
        let ids: Vec<String> = serde_json::from_str(&ids_json)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let count = ids.len();
        let vectors = quantize::decode_packed(&vectors_blob, count, EMBEDDING_DIMENSION);
        Ok(ids.into_iter().zip(vectors).collect())
    }

    pub fn get_epoch_similarities_for_catalysts(
        &self,
        epoch_id: &str,
        catalyst_ids: &[&str],
        per_catalyst_limit: usize,
    ) -> Result<Vec<CatalystSimilarity>> {
        if catalyst_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = (2..=catalyst_ids.len() + 1)
            .map(|i| format!("?{i}"))
            .collect();
        let limit_param = catalyst_ids.len() + 2;
        let sql = format!(
            "SELECT rn,catalyst_id,catalyst_text,file_path,chunk_index,similarity,
                    chunk_metadata,timeline_data,created_at
             FROM (
               SELECT *, ROW_NUMBER() OVER (
                 PARTITION BY catalyst_id ORDER BY similarity DESC,file_path,chunk_index
               ) rn
               FROM catalyst_epoch_similarities
               WHERE epoch_id=?1 AND catalyst_id IN ({})
             ) WHERE rn <= ?{limit_param}",
            placeholders.join(",")
        );
        let mut params = vec![Value::Text(epoch_id.to_string())];
        params.extend(catalyst_ids.iter().map(|id| Value::Text((*id).to_string())));
        params.push(Value::Integer(per_catalyst_limit as i64));
        Ok(self.query_read(&sql, params, map_catalyst_similarity_row)?)
    }

    /// Stage a complete target projection under `epoch_id`. These rows remain
    /// invisible until the source database activates that epoch.
    pub fn stage_epoch_projection_from_shadow(
        &self,
        shadow: &Database,
        epoch_id: &str,
    ) -> Result<()> {
        let similarities = shadow.query_read(
            "SELECT catalyst_id,catalyst_text,file_path,chunk_index,similarity,
                    chunk_metadata,timeline_data,created_at FROM catalyst_similarities",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<i64>(3)?,
                    row.get::<f64>(4)?,
                    row.get::<String>(5)?,
                    row.get::<String>(6)?,
                    row.get::<String>(7)?,
                ))
            },
        )?;
        let packed = shadow
            .query_read(
                "SELECT count,ids,vectors FROM catalyst_embeddings_packed WHERE id=1",
                vec![],
                |row| {
                    Ok((
                        row.get::<i64>(0)?,
                        row.get::<String>(1)?,
                        row.get::<Vec<u8>>(2)?,
                    ))
                },
            )?
            .into_iter()
            .next();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("begin target epoch staging")?;
            tx.execute(
                "DELETE FROM catalyst_epoch_similarities WHERE epoch_id=?1",
                vec![Value::Text(epoch_id.to_string())],
            )
            .await?;
            tx.execute(
                "DELETE FROM catalyst_epoch_embeddings_packed WHERE epoch_id=?1",
                vec![Value::Text(epoch_id.to_string())],
            )
            .await?;
            if let Some((count, ids, vectors)) = packed {
                tx.execute(
                    "INSERT INTO catalyst_epoch_embeddings_packed(epoch_id,count,ids,vectors)
                     VALUES (?1,?2,?3,?4)",
                    vec![
                        Value::Text(epoch_id.to_string()),
                        Value::Integer(count),
                        Value::Text(ids),
                        Value::Blob(vectors),
                    ],
                )
                .await?;
            }
            for (cat_id, cat_text, path, index, similarity, metadata, timeline, created_at) in
                similarities
            {
                tx.execute(
                    "INSERT INTO catalyst_epoch_similarities
                     (epoch_id,catalyst_id,catalyst_text,file_path,chunk_index,similarity,
                      chunk_metadata,timeline_data,created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    vec![
                        Value::Text(epoch_id.to_string()),
                        Value::Text(cat_id),
                        Value::Text(cat_text),
                        Value::Text(path),
                        Value::Integer(index),
                        Value::Real(similarity),
                        Value::Text(metadata),
                        Value::Text(timeline),
                        Value::Text(created_at),
                    ],
                )
                .await?;
            }
            tx.commit().await.context("commit target epoch staging")?;
            Ok(())
        })
    }

    /// Atomically replace the active catalyst materialized view from a complete
    /// frozen shadow database and activate its durable epoch record. All data
    /// is loaded before the transaction begins, so readers observe either the
    /// complete old view or the complete new view.
    pub fn activate_epoch_from_shadow(
        &self,
        shadow: &Database,
        epoch_id: &str,
        completed_at_ms: i64,
    ) -> Result<()> {
        let catalysts = shadow.query_read(
            "SELECT id, text, entity, context, generated_at, metadata FROM catalysts",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<String>(3)?,
                    row.get::<String>(4)?,
                    row.get::<String>(5)?,
                ))
            },
        )?;
        // The per-entity context fingerprints travel with the catalysts they
        // describe: without them the next incremental run would see catalysts
        // with no recorded provenance and regenerate the whole vault.
        let entity_hashes = shadow.query_read(
            "SELECT entity, entity_type, context_hash, generated_at_ms
             FROM catalyst_entity_hashes",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<i64>(3)?,
                ))
            },
        )?;
        let packed = shadow
            .query_read(
                "SELECT count, ids, vectors FROM catalyst_embeddings_packed WHERE id=1",
                vec![],
                |row| {
                    Ok((
                        row.get::<i64>(0)?,
                        row.get::<String>(1)?,
                        row.get::<Vec<u8>>(2)?,
                    ))
                },
            )?
            .into_iter()
            .next();
        let catalyst_texts: HashMap<String, String> = catalysts
            .iter()
            .map(|(id, text, ..)| (id.clone(), text.clone()))
            .collect();
        if !catalysts.is_empty() {
            let (_, packed_ids_json, packed_vectors) = packed
                .as_ref()
                .context("shadow epoch is missing catalyst embeddings")?;
            let packed_ids: Vec<String> = serde_json::from_str(packed_ids_json)?;
            anyhow::ensure!(
                packed_ids.len() == catalysts.len() && !packed_vectors.is_empty(),
                "shadow epoch catalyst embedding set is incomplete"
            );
        }

        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .context("begin epoch activation")?;
            // Catch the completed frozen interpretation up to the exact live
            // corpus visible under this write transaction. Concurrent document
            // refresh writers run wholly before or after this commit.
            let mut chunk_rows = tx
                .query(
                    "SELECT file_path,chunk_index,vector FROM chunk_embeddings",
                    (),
                )
                .await?;
            let mut live_chunks = Vec::new();
            while let Some(row) = chunk_rows.next().await? {
                let path = row.get::<String>(0)?;
                let index = row.get::<i64>(1)?;
                let blob = row.get::<Vec<u8>>(2)?;
                live_chunks.push((
                    path,
                    index,
                    quantize::decode_vector(&blob, EMBEDDING_DIMENSION),
                ));
            }
            drop(chunk_rows);
            let mut live_similarities = Vec::new();
            if let Some((count, ids_json, vectors_blob)) = &packed {
                let ids: Vec<String> = serde_json::from_str(ids_json)?;
                if !ids.is_empty() {
                    let cat_vectors =
                        quantize::decode_packed(vectors_blob, *count as usize, EMBEDDING_DIMENSION);
                    for (cat_id, cat_vec) in ids.iter().zip(cat_vectors.iter()) {
                        let mut scored: Vec<(f64, &str, i64)> = live_chunks
                            .iter()
                            .map(|(path, index, vector)| {
                                (
                                    Embedding::cosine_similarity(cat_vec, vector) as f64,
                                    path.as_str(),
                                    *index,
                                )
                            })
                            .collect();
                        scored.sort_by(|a, b| {
                            b.0.partial_cmp(&a.0)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.1.cmp(b.1))
                                .then_with(|| a.2.cmp(&b.2))
                        });
                        scored.truncate(50);
                        for (similarity, path, index) in scored {
                            live_similarities.push((
                                cat_id.clone(),
                                catalyst_texts.get(cat_id).cloned().unwrap_or_default(),
                                path.to_string(),
                                index,
                                similarity,
                            ));
                        }
                    }
                }
            }
            tx.execute("DELETE FROM catalyst_similarities", ()).await?;
            tx.execute("DELETE FROM catalyst_embeddings_packed", ())
                .await?;
            tx.execute("DELETE FROM catalysts", ()).await?;
            tx.execute("DELETE FROM catalyst_entity_hashes", ()).await?;
            for (entity, entity_type, context_hash, generated_at_ms) in entity_hashes {
                tx.execute(
                    "INSERT INTO catalyst_entity_hashes
                     (entity,entity_type,context_hash,generated_at_ms)
                     VALUES (?1,?2,?3,?4)",
                    vec![
                        Value::Text(entity),
                        Value::Text(entity_type),
                        Value::Text(context_hash),
                        Value::Integer(generated_at_ms),
                    ],
                )
                .await?;
            }
            for (id, text, entity, context, generated_at, metadata) in catalysts {
                tx.execute(
                    "INSERT INTO catalysts (id,text,entity,context,generated_at,metadata)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    vec![
                        Value::Text(id),
                        Value::Text(text),
                        Value::Text(entity),
                        Value::Text(context),
                        Value::Text(generated_at),
                        Value::Text(metadata),
                    ],
                )
                .await?;
            }
            if let Some((count, ids, vectors)) = packed {
                tx.execute(
                    "INSERT INTO catalyst_embeddings_packed (id,count,ids,vectors)
                     VALUES (1,?1,?2,?3)",
                    vec![
                        Value::Integer(count),
                        Value::Text(ids),
                        Value::Blob(vectors),
                    ],
                )
                .await?;
            }
            for (cat_id, cat_text, path, index, similarity) in live_similarities {
                tx.execute(
                    "INSERT INTO catalyst_similarities
                     (catalyst_id,catalyst_text,file_path,chunk_index,similarity,
                      chunk_metadata,timeline_data,created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    vec![
                        Value::Text(cat_id),
                        Value::Text(cat_text),
                        Value::Text(path),
                        Value::Integer(index),
                        Value::Real(similarity),
                        Value::Text("{}".to_string()),
                        Value::Text("{}".to_string()),
                        Value::Text(Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()),
                    ],
                )
                .await?;
            }
            Database::mark_epoch_activated_on_tx(&tx, epoch_id, completed_at_ms).await?;
            tx.commit().await.context("commit epoch activation")?;
            Ok::<(), anyhow::Error>(())
        })
    }
}

// ---------------------------------------------------------------------------
// Row-mapping helpers
// ---------------------------------------------------------------------------

fn map_entity_row(row: libsql::Row) -> Result<Entity, libsql::Error> {
    let trends_str: String = row.get::<Option<String>>(7)?.unwrap_or_default();
    let trends: EntityTrends = serde_json::from_str(&trends_str).unwrap_or_default();

    let metadata_str: String = row.get::<Option<String>>(8)?.unwrap_or_default();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_str)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    Ok(Entity {
        id: Some(row.get::<i64>(0)?),
        name: row.get::<String>(1)?,
        entity_type: parse_entity_type(&row.get::<String>(2)?),
        frequency: row.get::<i64>(3)?,
        ranking_weight: row.get::<f64>(4)?,
        first_seen: row.get::<Option<i64>>(5)?,
        last_seen: row.get::<Option<i64>>(6)?,
        trends,
        metadata,
    })
}

fn map_catalyst_row(row: libsql::Row) -> Result<Catalyst, libsql::Error> {
    let generated_at_str: String = row.get::<Option<String>>(4)?.unwrap_or_default();
    let metadata_str: String = row.get::<Option<String>>(5)?.unwrap_or_default();

    Ok(Catalyst {
        id: row.get::<String>(0)?,
        text: row.get::<String>(1)?,
        entity: row.get::<String>(2)?,
        context: row.get::<String>(3)?,
        generated_at: parse_datetime_utc(&generated_at_str),
        metadata: serde_json::from_str(&metadata_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

fn map_catalyst_similarity_row(row: libsql::Row) -> Result<CatalystSimilarity, libsql::Error> {
    let chunk_meta_str: String = row.get::<Option<String>>(6)?.unwrap_or_default();
    let timeline_str: String = row.get::<Option<String>>(7)?.unwrap_or_default();
    let created_at_str: String = row.get::<Option<String>>(8)?.unwrap_or_default();

    Ok(CatalystSimilarity {
        id: Some(row.get::<i64>(0)?),
        catalyst_id: row.get::<String>(1)?,
        catalyst_text: row.get::<String>(2)?,
        file_path: row.get::<String>(3)?,
        chunk_index: row.get::<i64>(4)?,
        similarity: row.get::<f64>(5)?,
        chunk_metadata: serde_json::from_str(&chunk_meta_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        timeline_data: serde_json::from_str(&timeline_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        created_at: parse_datetime_utc(&created_at_str),
    })
}

fn map_applied_target_row(row: libsql::Row) -> Result<AppliedTarget, libsql::Error> {
    let applied_at_str: String = row.get::<Option<String>>(4)?.unwrap_or_default();

    Ok(AppliedTarget {
        id: Some(row.get::<i64>(0)?),
        target_path: row.get::<String>(1)?,
        doc_count: row.get::<i64>(2)?,
        catalyst_count: row.get::<i64>(3)?,
        applied_at: parse_datetime_utc(&applied_at_str),
    })
}

// Blob <-> Vec<f32> encoding lives in `crate::embedding::quantize` (int8_v1,
// with legacy f32 fallback).

/// Text for the lexical index's path column, which BM25 weights twice: the
/// vault path, the title, and the note's wikilink targets. Frontmatter is not
/// part of chunk text, so without this a note that names a person, account,
/// or project only in a field like `people: ["[[Sam Rivera]]"]` (or only in a
/// frontmatter `title`) could not be found by that name.
pub(crate) fn lexical_handle(source_ref: &str, title: Option<&str>, links: &[String]) -> String {
    let mut handle = source_ref.to_string();
    if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
        handle.push('\n');
        handle.push_str(title);
    }
    for link in links {
        handle.push('\n');
        handle.push_str(link);
    }
    handle
}
