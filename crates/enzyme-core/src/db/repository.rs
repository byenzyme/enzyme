use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::params;

use crate::document::ProcessedDocument;
use crate::models::*;

use super::connection::Database;

// ---------------------------------------------------------------------------
// Context query types
// ---------------------------------------------------------------------------

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
        _ => EntityType::Tag,
    }
}

/// Parse a `DateTime<Utc>` from a SQLite TEXT column produced by `datetime('now')`.
///
/// SQLite's `datetime('now')` produces strings like `"2024-06-15 14:30:00"`.
/// We also accept RFC 3339 for values written explicitly.
fn parse_datetime_utc(s: &str) -> DateTime<Utc> {
    // Try RFC 3339 first (e.g., "2024-06-15T14:30:00Z" or with offset).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    // SQLite datetime format: "YYYY-MM-DD HH:MM:SS"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return ndt.and_utc();
    }
    // Fallback: epoch
    DateTime::UNIX_EPOCH
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
    /// only fsyncs once. Prepared statements are reused across all documents.
    pub fn upsert_documents_batch(&self, docs: &[ProcessedDocument]) -> Result<Vec<i64>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.write_conn();
        let indexed_at = chrono::Utc::now().timestamp_millis();
        let chunk_now = chrono::Utc::now().timestamp_millis();

        let tx = conn.unchecked_transaction()
            .context("failed to begin transaction for upsert_documents_batch")?;

        let mut doc_ids = Vec::with_capacity(docs.len());

        // Prepare all statements once, reuse for every document.
        let mut lookup_stmt = tx.prepare("SELECT id FROM docs WHERE source_ref = ?1")?;
        let mut del_tags_stmt = tx.prepare("DELETE FROM doc_tags WHERE doc_id = ?1")?;
        let mut del_links_stmt = tx.prepare("DELETE FROM doc_links WHERE doc_id = ?1")?;
        let mut del_chunks_stmt = tx.prepare("DELETE FROM chunks WHERE doc_id = ?1")?;
        let mut insert_doc_stmt = tx.prepare(
            "INSERT OR REPLACE INTO docs (
                source_ref, title, content, content_hash,
                word_count, char_count,
                created_at, created_source,
                modified_at, modified_source,
                indexed_at, metadata
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let mut get_id_stmt = tx.prepare("SELECT id FROM docs WHERE source_ref = ?1")?;
        let mut tag_stmt = tx.prepare("INSERT OR IGNORE INTO doc_tags (doc_id, tag) VALUES (?1, ?2)")?;
        let mut link_stmt = tx.prepare("INSERT OR IGNORE INTO doc_links (doc_id, link) VALUES (?1, ?2)")?;
        let mut chunk_stmt = tx.prepare(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        for doc in docs {
            let metadata = doc
                .frontmatter
                .as_ref()
                .map(|fm| serde_json::to_string(&fm.extra).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());

            // Delete old children if the document already exists.
            let existing_id: Option<i64> = {
                let mut rows = lookup_stmt.query(params![doc.source_ref])?;
                match rows.next()? {
                    Some(row) => Some(row.get(0)?),
                    None => None,
                }
            };
            if let Some(old_id) = existing_id {
                del_tags_stmt.execute(params![old_id])?;
                del_links_stmt.execute(params![old_id])?;
                del_chunks_stmt.execute(params![old_id])?;
            }

            insert_doc_stmt.execute(params![
                doc.source_ref,
                doc.title,
                doc.content,
                doc.content_hash,
                doc.word_count,
                doc.char_count,
                doc.created_at,
                doc.created_source.as_str(),
                doc.modified_at,
                doc.modified_source.as_str(),
                indexed_at,
                metadata,
            ])?;

            let doc_id: i64 = get_id_stmt.query_row(params![doc.source_ref], |row| row.get(0))?;

            for tag in &doc.tags {
                tag_stmt.execute(params![doc_id, tag])?;
            }
            for link in &doc.links {
                link_stmt.execute(params![doc_id, link])?;
            }
            for (i, chunk_content) in doc.chunks.iter().enumerate() {
                let word_count = chunk_content.split_whitespace().count() as i64;
                chunk_stmt.execute(params![doc_id, i as i64, chunk_content, word_count, chunk_now])?;
            }

            doc_ids.push(doc_id);
        }

        drop(lookup_stmt);
        drop(del_tags_stmt);
        drop(del_links_stmt);
        drop(del_chunks_stmt);
        drop(insert_doc_stmt);
        drop(get_id_stmt);
        drop(tag_stmt);
        drop(link_stmt);
        drop(chunk_stmt);

        tx.commit().context("failed to commit upsert_documents_batch")?;
        Ok(doc_ids)
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
            &[&source_ref as &dyn rusqlite::types::ToSql],
            |row| {
                Ok(Document {
                    id: Some(row.get(0)?),
                    source_ref: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    content_hash: row.get(4)?,
                    word_count: row.get(5)?,
                    char_count: row.get(6)?,
                    created_at: row.get(7)?,
                    created_source: parse_date_source(
                        &row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    ),
                    modified_at: row.get(9)?,
                    modified_source: parse_date_source(
                        &row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    ),
                    indexed_at: row.get(11)?,
                    embedded_at: row.get(12)?,
                    embedded_content_hash: row.get(13)?,
                    metadata: row
                        .get::<_, Option<String>>(14)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                })
            },
        )?;

        Ok(rows.into_iter().next())
    }

    /// Return a map of `source_ref -> content_hash` for every document in the database.
    pub fn get_all_document_hashes(&self) -> Result<HashMap<String, String>> {
        let rows = self.query_read(
            "SELECT source_ref, content_hash FROM docs",
            &[],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Find documents whose embeddings are stale or missing.
    ///
    /// Returns `(doc_id, source_ref, chunk_contents)` for each document
    /// where `embedded_content_hash != content_hash` or `embedded_at IS NULL`.
    ///
    /// When `max_count` is `Some(n)`, returns at most `n` documents ordered by
    /// `created_at DESC` (most recent first), matching the Python behaviour of
    /// capping embedding runs to the N most recent documents.
    ///
    /// When `indexed_since` is `Some(ts)`, only returns documents whose
    /// `indexed_at >= ts` — used by the fast refresh path to avoid processing
    /// the full historical backlog.
    pub fn get_documents_needing_embedding(
        &self,
        max_count: Option<usize>,
        indexed_since: Option<i64>,
    ) -> Result<Vec<(i64, String, Vec<String>)>> {
        // Build query dynamically — parameter indices depend on which clauses are active.
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut sql = String::from(
            "SELECT id, source_ref FROM docs
             WHERE (embedded_content_hash IS NULL
                OR embedded_content_hash != content_hash
                OR embedded_at IS NULL)",
        );

        if let Some(ts) = indexed_since {
            params.push(Box::new(ts));
            sql.push_str(&format!(" AND indexed_at >= ?{}", params.len()));
        }

        sql.push_str(" ORDER BY created_at DESC");

        if let Some(n) = max_count {
            params.push(Box::new(n as i64));
            sql.push_str(&format!(" LIMIT ?{}", params.len()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let docs = self.query_read(&sql, &param_refs, |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut results = Vec::with_capacity(docs.len());
        for (doc_id, source_ref) in docs {
            let chunks = self.query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                &[&doc_id as &dyn rusqlite::types::ToSql],
                |row| row.get::<_, String>(0),
            )?;
            results.push((doc_id, source_ref, chunks));
        }

        Ok(results)
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
            &[
                &now as &dyn rusqlite::types::ToSql,
                &content_hash,
                &doc_id,
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

        let conn = self.write_conn();
        let placeholders: Vec<String> = (1..=paths.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM docs WHERE source_ref IN ({})",
            placeholders.join(", ")
        );

        let params: Vec<&dyn rusqlite::types::ToSql> =
            paths.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();

        let count = conn.execute(&sql, params.as_slice())?;
        Ok(count)
    }

    /// Return all `source_ref` values from the docs table.
    pub fn get_all_source_refs(&self) -> Result<Vec<String>> {
        Ok(self
            .query_read("SELECT source_ref FROM docs", &[], |row| {
                row.get::<_, String>(0)
            })?)
    }

    /// Return the total number of documents in the database.
    pub fn get_document_count(&self) -> Result<i64> {
        let rows = self
            .query_read("SELECT COUNT(*) FROM docs", &[], |row| row.get::<_, i64>(0))?;
        Ok(rows.into_iter().next().unwrap_or(0))
    }

    // -----------------------------------------------------------------------
    // Entity operations
    // -----------------------------------------------------------------------

    /// Batch-insert or update entities and record occurrences.
    ///
    /// All operations run inside a **single transaction** with prepared
    /// statements reused across all entries.  Each entry is a
    /// `(name, EntityType, EntityOccurrence)` tuple.
    pub fn upsert_entities_batch(
        &self,
        entries: &[(&str, &EntityType, &EntityOccurrence)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for upsert_entities_batch")?;

        let mut insert_ignore_stmt = tx.prepare(
            "INSERT OR IGNORE INTO entities (name, type, frequency, first_seen, last_seen)
             VALUES (?1, ?2, 0, ?3, ?4)",
        )?;
        let mut update_stmt = tx.prepare(
            "UPDATE entities
             SET frequency = frequency + 1,
                 last_seen = MAX(COALESCE(last_seen, 0), COALESCE(?1, 0))
             WHERE name = ?2 AND type = ?3",
        )?;
        let mut lookup_stmt = tx.prepare(
            "SELECT id FROM entities WHERE name = ?1 AND type = ?2",
        )?;
        let mut occ_stmt = tx.prepare(
            "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at, effective_date, date_source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        for &(name, entity_type, occurrence) in entries {
            let effective_date = occurrence.effective_date;
            let type_str = entity_type.as_str();

            insert_ignore_stmt.execute(params![name, type_str, effective_date, effective_date])?;
            update_stmt.execute(params![effective_date, name, type_str])?;

            let entity_id: i64 = lookup_stmt.query_row(params![name, type_str], |row| row.get(0))?;

            occ_stmt.execute(params![
                entity_id,
                occurrence.file_path,
                occurrence.context,
                occurrence.created_at,
                occurrence.effective_date,
                occurrence.date_source,
            ])?;
        }

        drop(insert_ignore_stmt);
        drop(update_stmt);
        drop(lookup_stmt);
        drop(occ_stmt);

        tx.commit().context("failed to commit upsert_entities_batch")?;
        Ok(())
    }

    /// Get all occurrences for a given entity.
    pub fn get_entity_occurrences(&self, entity_id: i64) -> Result<Vec<EntityOccurrence>> {
        let rows = self.query_read(
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source
             FROM entity_occurrences WHERE entity_id = ?1",
            &[&entity_id as &dyn rusqlite::types::ToSql],
            |row| {
                Ok(EntityOccurrence {
                    id: Some(row.get(0)?),
                    entity_id: row.get(1)?,
                    file_path: row.get(2)?,
                    context: row.get(3)?,
                    created_at: row.get(4)?,
                    effective_date: row.get(5)?,
                    date_source: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                })
            },
        )?;
        Ok(rows)
    }

    /// Get the top entities, ordered by the given column.
    ///
    /// If `order_by` is `"recency_score"`, entities are fetched, their trends
    /// JSON is parsed, and they are sorted by `recency_score` descending.
    /// Otherwise, entities are ordered by `frequency DESC`.
    pub fn get_top_entities(&self, limit: usize, order_by: &str) -> Result<Vec<Entity>> {
        if order_by == "recency_score" {
            // Fetch all entities, parse trends, sort by recency_score, take limit.
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
                "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
                 FROM entities ORDER BY frequency DESC LIMIT ?1",
                &[&(limit as i64) as &dyn rusqlite::types::ToSql],
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
        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for update_entity_trends_batch")?;

        let mut stmt = tx.prepare("UPDATE entities SET trends = ?1 WHERE id = ?2")?;
        for (entity_id, trends) in updates {
            let trends_json =
                serde_json::to_string(trends).context("failed to serialize EntityTrends")?;
            stmt.execute(params![trends_json, entity_id])?;
        }

        drop(stmt);
        tx.commit().context("failed to commit update_entity_trends_batch")?;
        Ok(())
    }

    /// Fetch all occurrence timestamps in one query, grouped by entity_id.
    ///
    /// Returns `(entity_id, effective_date, created_at)` for every occurrence,
    /// ordered by entity_id so the caller can stream-group.
    pub fn get_all_occurrence_dates(&self) -> Result<Vec<(i64, Option<i64>, i64)>> {
        let rows = self.query_read(
            "SELECT entity_id, effective_date, created_at
             FROM entity_occurrences
             ORDER BY entity_id",
            &[],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(rows)
    }

    /// Get trending entities with guaranteed type diversity.
    ///
    /// Phase 1: Take guaranteed minimums from each type bucket (tags, links, folders).
    /// Phase 2: Merge remaining entities, sort by recency, fill to total_limit.
    /// Only includes root-level entities (no `/` in name for links/folders).
    pub fn get_trending_entities(
        &self,
        min_tags: usize,
        min_links: usize,
        min_folders: usize,
        total_limit: usize,
    ) -> Result<Vec<Entity>> {
        let all = self.get_all_entities()?;

        // Filter to root-level only (no path separators in name)
        // Tags can have nested paths like "enzyme/pmf" — those are allowed
        let is_root_level = |e: &Entity| -> bool {
            match e.entity_type {
                EntityType::Tag => true, // tags can be nested (e.g. #enzyme/pmf)
                EntityType::Link | EntityType::Folder => !e.name.contains('/'),
            }
        };

        let root_entities: Vec<Entity> = all
            .into_iter()
            .filter(|e| is_root_level(e))
            .collect();

        // Separate by type and sort each by recency_score descending
        let mut tags: Vec<Entity> = root_entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Tag)
            .cloned()
            .collect();
        let mut links: Vec<Entity> = root_entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Link)
            .cloned()
            .collect();
        let mut folders: Vec<Entity> = root_entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Folder)
            .cloned()
            .collect();

        let sort_by_recency = |a: &Entity, b: &Entity| -> std::cmp::Ordering {
            b.trends
                .recency_score
                .partial_cmp(&a.trends.recency_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        };

        tags.sort_by(sort_by_recency);
        links.sort_by(sort_by_recency);
        folders.sort_by(sort_by_recency);

        // Phase 1: guaranteed minimums
        let mut selected: Vec<Entity> = Vec::new();
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

        for entity in tags.iter().take(min_tags) {
            used.insert(entity.name.clone());
            selected.push(entity.clone());
        }
        for entity in links.iter().take(min_links) {
            used.insert(entity.name.clone());
            selected.push(entity.clone());
        }
        for entity in folders.iter().take(min_folders) {
            used.insert(entity.name.clone());
            selected.push(entity.clone());
        }

        // Phase 2: fill remaining slots from all types
        if selected.len() < total_limit {
            let mut remaining: Vec<Entity> = root_entities
                .into_iter()
                .filter(|e| !used.contains(&e.name))
                .collect();
            remaining.sort_by(sort_by_recency);

            for entity in remaining {
                if selected.len() >= total_limit {
                    break;
                }
                selected.push(entity);
            }
        }

        // Final sort by recency descending
        selected.sort_by(sort_by_recency);
        selected.truncate(total_limit);

        Ok(selected)
    }

    /// Look up entities by their (name, type) pairs.
    ///
    /// Returns only entities that exist in the database. The order may differ
    /// from the input.
    pub fn get_entities_by_names(&self, names: &[(&str, EntityType)]) -> Result<Vec<Entity>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        for (name, entity_type) in names {
            let rows = self.query_read(
                "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
                 FROM entities WHERE name = ?1 AND type = ?2",
                &[
                    &name.to_lowercase() as &dyn rusqlite::types::ToSql,
                    &entity_type.as_str(),
                ],
                map_entity_row,
            )?;

            results.extend(rows);
        }

        Ok(results)
    }

    /// Retrieve every entity in the database.
    pub fn get_all_entities(&self) -> Result<Vec<Entity>> {
        let rows = self.query_read(
            "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
             FROM entities",
            &[],
            map_entity_row,
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Context queries (for XML context builder)
    // -----------------------------------------------------------------------

    /// Derive folder structure from document `source_ref` paths.
    ///
    /// Returns `(folder_name, file_count, most_recent_modified_at)` for each
    /// top-level folder, ordered by file count descending.
    pub fn get_folder_structure(&self) -> Result<Vec<FolderStats>> {
        // Top-level folders: first path segment of source_ref
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
            &[],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
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

            // Get subfolders for this parent
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
                &[&name as &dyn rusqlite::types::ToSql],
                |row| {
                    Ok(SubfolderStats {
                        name: row.get::<_, String>(0)?,
                        file_count: row.get::<_, i64>(1)?,
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
    ///
    /// For tags: children of `#enzyme` are `#enzyme/pmf`, `#enzyme/search`, etc.
    /// Returns at most `limit` children, ordered by frequency descending.
    pub fn get_child_entities(
        &self,
        parent_name: &str,
        entity_type: &str,
        limit: usize,
    ) -> Result<Vec<Entity>> {
        let like_pattern = format!("{}/%", parent_name);
        let not_like_pattern = format!("{}/%/%", parent_name);

        let rows = self.query_read(
            "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
             FROM entities
             WHERE name LIKE ?1 AND name NOT LIKE ?2 AND type = ?3
             ORDER BY frequency DESC
             LIMIT ?4",
            &[
                &like_pattern as &dyn rusqlite::types::ToSql,
                &not_like_pattern,
                &entity_type,
                &(limit as i64),
            ],
            map_entity_row,
        )?;

        Ok(rows)
    }

    /// Get the most-referenced children of a folder, ranked by external wikilink count.
    ///
    /// "Children" are documents whose `source_ref` matches `{folder}/%.md` but
    /// NOT `{folder}/%/%.md` (i.e. immediate children only).  Reference counts
    /// come from `doc_links` rows where the linking document lives *outside*
    /// the folder.
    ///
    /// Returns `(children, total_child_count)`.
    pub fn get_folder_children_by_references(
        &self,
        folder_name: &str,
        limit: usize,
    ) -> Result<(Vec<FolderChild>, i64)> {
        let like_pattern = format!("{}/%", folder_name);
        let not_like_pattern = format!("{}/%/%", folder_name);

        // Get all immediate children with their modified_at
        let children_rows = self.query_read(
            "SELECT source_ref, modified_at
             FROM docs
             WHERE source_ref LIKE ?1 AND source_ref NOT LIKE ?2",
            &[
                &like_pattern as &dyn rusqlite::types::ToSql,
                &not_like_pattern,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;

        let total_count = children_rows.len() as i64;

        // For each child, extract the file stem and count external references
        let outside_pattern = format!("{}/%", folder_name);
        let mut children: Vec<FolderChild> = Vec::new();

        for (source_ref, modified_at) in &children_rows {
            // Extract file stem: "people/Alice Jones.md" -> "Alice Jones"
            let file_name = source_ref
                .rsplit('/')
                .next()
                .unwrap_or(source_ref);
            let stem = file_name.strip_suffix(".md").unwrap_or(file_name);

            // Count references from documents OUTSIDE this folder
            let ref_count = self.query_read(
                "SELECT COUNT(DISTINCT dl.doc_id)
                 FROM doc_links dl
                 JOIN docs d ON d.id = dl.doc_id
                 WHERE dl.link = ?1 AND d.source_ref NOT LIKE ?2",
                &[
                    &stem.to_lowercase() as &dyn rusqlite::types::ToSql,
                    &outside_pattern,
                ],
                |row| row.get::<_, i64>(0),
            )?;
            let reference_count = ref_count.into_iter().next().unwrap_or(0);

            children.push(FolderChild {
                name: stem.to_string(),
                source_ref: source_ref.clone(),
                reference_count,
                last_seen: *modified_at,
            });
        }

        // Sort by reference_count desc, then by name for stability
        children.sort_by(|a, b| {
            b.reference_count
                .cmp(&a.reference_count)
                .then_with(|| a.name.cmp(&b.name))
        });
        children.truncate(limit);

        Ok((children, total_count))
    }

    /// Identify trending wikilinks that have source files directly inside a folder.
    ///
    /// For example, if `folder_name = "people"` and `people/Alice Smith.md` exists,
    /// and `"alice smith"` is in `trending_links`, this returns `["alice smith"]`.
    ///
    /// This is the basis for determining expandable folders: a folder whose files
    /// correspond to trending wikilinks should be expanded so catalysts cover the
    /// children instead of (or in addition to) the folder as a flat entity.
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

        // Get file stems of immediate children in this folder
        let rows = self.query_read(
            "SELECT source_ref FROM docs
             WHERE source_ref LIKE ?1 AND source_ref NOT LIKE ?2",
            &[
                &like_pattern as &dyn rusqlite::types::ToSql,
                &not_like_pattern,
            ],
            |row| row.get::<_, String>(0),
        )?;

        // Extract lowercase file stems
        let folder_files: std::collections::HashSet<String> = rows
            .iter()
            .filter_map(|source_ref| {
                let file_name = source_ref.rsplit('/').next()?;
                let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
                Some(stem.to_lowercase())
            })
            .collect();

        // Match against trending wikilinks
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
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source
             FROM entity_occurrences
             WHERE entity_id = ?1
             ORDER BY effective_date DESC
             LIMIT ?2",
            &[
                &entity_id as &dyn rusqlite::types::ToSql,
                &(limit as i64),
            ],
            |row| {
                Ok(EntityOccurrence {
                    id: Some(row.get(0)?),
                    entity_id: row.get(1)?,
                    file_path: row.get(2)?,
                    context: row.get(3)?,
                    created_at: row.get(4)?,
                    effective_date: row.get(5)?,
                    date_source: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
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
        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for insert_catalysts")?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO catalysts (id, text, entity, context, generated_at, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for cat in catalysts {
                let generated_at = cat.generated_at.format("%Y-%m-%d %H:%M:%S").to_string();
                let metadata = serde_json::to_string(&cat.metadata).unwrap_or_else(|_| "{}".to_string());
                stmt.execute(params![
                    cat.id,
                    cat.text,
                    cat.entity,
                    cat.context,
                    generated_at,
                    metadata,
                ])?;
            }
        }

        tx.commit().context("failed to commit insert_catalysts")?;
        Ok(())
    }

    /// Get all catalysts associated with a given entity name.
    pub fn get_catalysts_for_entity(&self, entity: &str) -> Result<Vec<Catalyst>> {
        let rows = self.query_read(
            "SELECT id, text, entity, context, generated_at, metadata
             FROM catalysts WHERE entity = ?1",
            &[&entity as &dyn rusqlite::types::ToSql],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get the set of entity names that have at least one catalyst.
    pub fn get_catalyzed_entity_names(&self) -> Result<std::collections::HashSet<String>> {
        let names = self.query_read(
            "SELECT DISTINCT entity FROM catalysts",
            &[],
            |row| row.get::<_, String>(0),
        )?;
        Ok(names.into_iter().collect())
    }

    /// Get every catalyst in the database.
    pub fn get_all_catalysts(&self) -> Result<Vec<Catalyst>> {
        let rows = self.query_read(
            "SELECT id, text, entity, context, generated_at, metadata FROM catalysts",
            &[],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get the most recent `generated_at` timestamp across all catalysts.
    pub fn get_latest_catalyst_timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        let rows = self.query_read(
            "SELECT MAX(generated_at) FROM catalysts",
            &[],
            |row| row.get::<_, Option<String>>(0),
        )?;
        Ok(rows.into_iter().next().flatten().map(|s| parse_datetime_utc(&s)))
    }

    /// Count distinct documents that mention a given entity (by entity name).
    ///
    /// Uses `entity_occurrences` to count unique file paths.
    pub fn get_entity_doc_count(&self, entity_name: &str) -> Result<usize> {
        let rows = self.query_read(
            "SELECT COUNT(DISTINCT eo.file_path)
             FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id
             WHERE e.name = ?1",
            &[&entity_name as &dyn rusqlite::types::ToSql],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(rows.into_iter().next().unwrap_or(0) as usize)
    }

    /// Delete all catalysts for a given entity name.
    pub fn delete_catalysts_for_entity(&self, entity: &str) -> Result<()> {
        self
            .execute_write("DELETE FROM catalysts WHERE entity = ?1", &[&entity])?;
        Ok(())
    }

    /// Batch upsert catalyst similarity records.
    pub fn upsert_catalyst_similarities(&self, sims: &[CatalystSimilarity]) -> Result<()> {
        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for upsert_catalyst_similarities")?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO catalyst_similarities
                    (catalyst_id, catalyst_text, file_path, chunk_index,
                     similarity, chunk_metadata, timeline_data, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for sim in sims {
                let chunk_meta =
                    serde_json::to_string(&sim.chunk_metadata).unwrap_or_else(|_| "{}".to_string());
                let timeline =
                    serde_json::to_string(&sim.timeline_data).unwrap_or_else(|_| "{}".to_string());
                let created_at = sim.created_at.format("%Y-%m-%d %H:%M:%S").to_string();
                stmt.execute(params![
                    sim.catalyst_id,
                    sim.catalyst_text,
                    sim.file_path,
                    sim.chunk_index,
                    sim.similarity,
                    chunk_meta,
                    timeline,
                    created_at,
                ])?;
            }
        }

        tx.commit()
            .context("failed to commit upsert_catalyst_similarities")?;
        Ok(())
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
            &[
                &catalyst_id as &dyn rusqlite::types::ToSql,
                &(limit as i64),
            ],
            map_catalyst_similarity_row,
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Chunk operations
    // -----------------------------------------------------------------------

    /// Get all chunks belonging to documents that have been embedded.
    ///
    /// Returns `(source_ref, chunk_index, content)`.
    pub fn get_all_chunks_with_embeddings(&self) -> Result<Vec<(String, i64, String)>> {
        let rows = self.query_read(
            "SELECT d.source_ref, c.chunk_index, c.content
             FROM docs d
             JOIN chunks c ON d.id = c.doc_id
             WHERE d.embedded_at IS NOT NULL
             ORDER BY d.source_ref, c.chunk_index",
            &[],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        Ok(rows)
    }

    /// Get all chunks with full metadata for the explore UI.
    ///
    /// Returns enriched chunk data including doc metadata, tags, and word counts.
    pub fn get_all_chunks_enriched(
        &self,
    ) -> Result<Vec<(i64, i64, i64, String, String, i64, i64, i64, Option<String>)>> {
        // (doc_id, chunk_id, chunk_index, file_path, content, chunk_word_count, created_at, modified_at, title)
        let rows = self.query_read(
            "SELECT d.id, c.id, c.chunk_index, d.source_ref, c.content,
                    c.word_count, d.created_at, d.modified_at, d.title
             FROM docs d
             JOIN chunks c ON d.id = c.doc_id
             WHERE d.embedded_at IS NOT NULL
             ORDER BY d.source_ref, c.chunk_index",
            &[],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )?;
        Ok(rows)
    }

    /// Get all tags grouped by doc_id.
    pub fn get_all_doc_tags(&self) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        let rows = self.query_read(
            "SELECT doc_id, tag FROM doc_tags ORDER BY doc_id, tag",
            &[],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
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
            &[&source_ref as &dyn rusqlite::types::ToSql],
            |row| row.get::<_, String>(0),
        )?;
        Ok(rows)
    }

    /// Get the content of a specific chunk by file path and chunk index.
    pub fn get_chunk_content(
        &self,
        file_path: &str,
        chunk_index: i64,
    ) -> Result<Option<String>> {
        let rows = self.query_read(
            "SELECT c.content
             FROM chunks c
             JOIN docs d ON d.id = c.doc_id
             WHERE d.source_ref = ?1 AND c.chunk_index = ?2",
            &[
                &file_path as &dyn rusqlite::types::ToSql,
                &chunk_index,
            ],
            |row| row.get::<_, String>(0),
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
            &[&key as &dyn rusqlite::types::ToSql, &value],
        )?;
        Ok(())
    }

    /// Get a configuration value by key.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let rows = self.query_read(
            "SELECT value FROM config WHERE key = ?1",
            &[&key as &dyn rusqlite::types::ToSql],
            |row| row.get::<_, String>(0),
        )?;
        Ok(rows.into_iter().next())
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
            &[
                &target.target_path as &dyn rusqlite::types::ToSql,
                &target.doc_count,
                &target.catalyst_count,
                &applied_at,
            ],
        )?;
        Ok(())
    }

    /// Get all applied targets.
    pub fn get_all_applied_targets(&self) -> Result<Vec<AppliedTarget>> {
        let rows = self.query_read(
            "SELECT id, target_path, doc_count, catalyst_count, applied_at
             FROM applied_targets
             ORDER BY applied_at DESC",
            &[],
            map_applied_target_row,
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Chunk embedding operations
    // -----------------------------------------------------------------------

    /// Batch-upsert chunk embeddings. Vectors are stored as little-endian f32 BLOBs.
    pub fn upsert_chunk_embeddings(
        &self,
        embeddings: &[(String, u32, Vec<f32>)],
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for upsert_chunk_embeddings")?;
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO chunk_embeddings (file_path, chunk_index, vector)
             VALUES (?1, ?2, ?3)",
        )?;
        for (file_path, chunk_index, vector) in embeddings {
            let blob = f32_vec_to_blob(vector);
            stmt.execute(params![file_path, chunk_index, blob])?;
        }
        drop(stmt);
        tx.commit().context("failed to commit upsert_chunk_embeddings")?;
        Ok(())
    }

    /// Load all chunk embeddings from the database.
    pub fn get_all_chunk_embeddings(&self) -> Result<Vec<(String, u32, Vec<f32>)>> {
        let rows = self.query_read(
            "SELECT file_path, chunk_index, vector FROM chunk_embeddings
             ORDER BY file_path, chunk_index",
            &[],
            |row| {
                let file_path: String = row.get(0)?;
                let chunk_index: u32 = row.get(1)?;
                let blob: Vec<u8> = row.get(2)?;
                Ok((file_path, chunk_index, blob_to_f32_vec(&blob)))
            },
        )?;
        Ok(rows)
    }

    /// Delete chunk embeddings for a specific file (used before re-embedding).
    pub fn delete_chunk_embeddings_for_file(&self, file_path: &str) -> Result<()> {
        self.execute_write(
            "DELETE FROM chunk_embeddings WHERE file_path = ?1",
            &[&file_path],
        )?;
        Ok(())
    }

    /// Prune chunk embeddings whose file_path no longer exists in the docs table.
    /// Returns the number of rows deleted.
    pub fn prune_stale_chunk_embeddings(&self) -> Result<usize> {
        let count = self.execute_write(
            "DELETE FROM chunk_embeddings
             WHERE file_path NOT IN (SELECT source_ref FROM docs)",
            &[],
        )?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // Catalyst embedding operations
    // -----------------------------------------------------------------------

    /// Batch-upsert catalyst embeddings.
    pub fn upsert_catalyst_embeddings(
        &self,
        embeddings: &[(&str, &[f32])],
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let conn = self.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for upsert_catalyst_embeddings")?;
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO catalyst_embeddings (catalyst_id, vector)
             VALUES (?1, ?2)",
        )?;
        for (catalyst_id, vector) in embeddings {
            let blob = f32_vec_to_blob(vector);
            stmt.execute(params![catalyst_id, blob])?;
        }
        drop(stmt);
        tx.commit().context("failed to commit upsert_catalyst_embeddings")?;
        Ok(())
    }

    /// Load all catalyst embeddings from the database.
    pub fn get_all_catalyst_embeddings(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let rows = self.query_read(
            "SELECT catalyst_id, vector FROM catalyst_embeddings",
            &[],
            |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, blob_to_f32_vec(&blob)))
            },
        )?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Row-mapping helpers
// ---------------------------------------------------------------------------

/// Map a row from the `entities` table to an [`Entity`].
fn map_entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
    let trends_str: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
    let trends: EntityTrends =
        serde_json::from_str(&trends_str).unwrap_or_default();

    let metadata_str: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or(serde_json::Value::Object(Default::default()));

    Ok(Entity {
        id: Some(row.get(0)?),
        name: row.get(1)?,
        entity_type: parse_entity_type(&row.get::<_, String>(2)?),
        frequency: row.get(3)?,
        first_seen: row.get(4)?,
        last_seen: row.get(5)?,
        trends,
        metadata,
    })
}

/// Map a row from the `catalysts` table to a [`Catalyst`].
fn map_catalyst_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Catalyst> {
    let generated_at_str: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
    let metadata_str: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();

    Ok(Catalyst {
        id: row.get(0)?,
        text: row.get(1)?,
        entity: row.get(2)?,
        context: row.get(3)?,
        generated_at: parse_datetime_utc(&generated_at_str),
        metadata: serde_json::from_str(&metadata_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

/// Map a row from the `catalyst_similarities` table to a [`CatalystSimilarity`].
fn map_catalyst_similarity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CatalystSimilarity> {
    let chunk_meta_str: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
    let timeline_str: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
    let created_at_str: String = row.get::<_, Option<String>>(8)?.unwrap_or_default();

    Ok(CatalystSimilarity {
        id: Some(row.get(0)?),
        catalyst_id: row.get(1)?,
        catalyst_text: row.get(2)?,
        file_path: row.get(3)?,
        chunk_index: row.get(4)?,
        similarity: row.get(5)?,
        chunk_metadata: serde_json::from_str(&chunk_meta_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        timeline_data: serde_json::from_str(&timeline_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        created_at: parse_datetime_utc(&created_at_str),
    })
}

/// Map a row from the `applied_targets` table to an [`AppliedTarget`].
fn map_applied_target_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppliedTarget> {
    let applied_at_str: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();

    Ok(AppliedTarget {
        id: Some(row.get(0)?),
        target_path: row.get(1)?,
        doc_count: row.get(2)?,
        catalyst_count: row.get(3)?,
        applied_at: parse_datetime_utc(&applied_at_str),
    })
}

// ---------------------------------------------------------------------------
// Blob <-> Vec<f32> helpers
// ---------------------------------------------------------------------------

/// Convert a `&[f32]` to a little-endian byte vector for BLOB storage.
fn f32_vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for &x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

/// Convert a BLOB back to `Vec<f32>` (little-endian).
fn blob_to_f32_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}
