use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::params;

use crate::document::ProcessedDocument;
use crate::models::*;

use super::Database;

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
// Repository
// ---------------------------------------------------------------------------

/// High-level CRUD layer over the Enzyme [`Database`].
///
/// All methods take `&self` because [`Database`] internally manages separate
/// read and write connections, and the write connection uses SQLite WAL mode
/// so `&self` is sufficient (no `&mut self` needed).
pub struct Repository<'a> {
    db: &'a Database,
}

impl<'a> Repository<'a> {
    /// Wrap an existing [`Database`] handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    // -----------------------------------------------------------------------
    // Document operations
    // -----------------------------------------------------------------------

    /// Insert or replace a document and its associated tags, links, and chunks.
    ///
    /// Uses a transaction on the write connection.  Returns the `doc_id`.
    pub fn upsert_document(&self, doc: &ProcessedDocument) -> Result<i64> {
        let conn = self.db.write_conn();
        let indexed_at = chrono::Utc::now().timestamp_millis();

        let metadata = doc
            .frontmatter
            .as_ref()
            .map(|fm| serde_json::to_string(&fm.extra).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());

        let tx = conn.unchecked_transaction()
            .context("failed to begin transaction for upsert_document")?;

        // Look up existing doc_id so we can delete children first.
        let existing_id: Option<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM docs WHERE source_ref = ?1")?;
            let mut rows = stmt.query(params![doc.source_ref])?;
            match rows.next()? {
                Some(row) => Some(row.get(0)?),
                None => None,
            }
        };

        // Delete old children if the document already exists.
        if let Some(old_id) = existing_id {
            tx.execute("DELETE FROM doc_tags WHERE doc_id = ?1", params![old_id])?;
            tx.execute("DELETE FROM doc_links WHERE doc_id = ?1", params![old_id])?;
            tx.execute("DELETE FROM chunks WHERE doc_id = ?1", params![old_id])?;
        }

        // INSERT OR REPLACE the document row.
        tx.execute(
            "INSERT OR REPLACE INTO docs (
                source_ref, title, content, content_hash,
                word_count, char_count,
                created_at, created_source,
                modified_at, modified_source,
                indexed_at, metadata
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
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
            ],
        )?;

        // Retrieve the doc_id (may be new or the same after REPLACE).
        let doc_id: i64 = {
            let mut stmt = tx.prepare("SELECT id FROM docs WHERE source_ref = ?1")?;
            stmt.query_row(params![doc.source_ref], |row| row.get(0))?
        };

        // Insert tags.
        {
            let mut stmt =
                tx.prepare("INSERT OR IGNORE INTO doc_tags (doc_id, tag) VALUES (?1, ?2)")?;
            for tag in &doc.tags {
                stmt.execute(params![doc_id, tag])?;
            }
        }

        // Insert links.
        {
            let mut stmt =
                tx.prepare("INSERT OR IGNORE INTO doc_links (doc_id, link) VALUES (?1, ?2)")?;
            for link in &doc.links {
                stmt.execute(params![doc_id, link])?;
            }
        }

        // Insert chunks.
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let now = chrono::Utc::now().timestamp_millis();
            for (i, chunk_content) in doc.chunks.iter().enumerate() {
                let word_count = chunk_content.split_whitespace().count() as i64;
                stmt.execute(params![doc_id, i as i64, chunk_content, word_count, now])?;
            }
        }

        tx.commit().context("failed to commit upsert_document")?;
        Ok(doc_id)
    }

    /// Retrieve a single document by its `source_ref` (relative vault path).
    pub fn get_document_by_path(&self, source_ref: &str) -> Result<Option<Document>> {
        let rows = self.db.query_read(
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
        let rows = self.db.query_read(
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
    pub fn get_documents_needing_embedding(
        &self,
        max_count: Option<usize>,
    ) -> Result<Vec<(i64, String, Vec<String>)>> {
        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match max_count {
            Some(n) => (
                "SELECT id, source_ref FROM docs
                 WHERE embedded_content_hash IS NULL
                    OR embedded_content_hash != content_hash
                    OR embedded_at IS NULL
                 ORDER BY created_at DESC
                 LIMIT ?1"
                    .to_string(),
                vec![Box::new(n as i64)],
            ),
            None => (
                "SELECT id, source_ref FROM docs
                 WHERE embedded_content_hash IS NULL
                    OR embedded_content_hash != content_hash
                    OR embedded_at IS NULL
                 ORDER BY created_at DESC"
                    .to_string(),
                vec![],
            ),
        };
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let docs = self.db.query_read(&sql, &param_refs, |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut results = Vec::with_capacity(docs.len());
        for (doc_id, source_ref) in docs {
            let chunks = self.db.query_read(
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
        self.db.execute_write(
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

        let conn = self.db.write_conn();
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

    /// Return the total number of documents in the database.
    pub fn get_document_count(&self) -> Result<i64> {
        let rows = self
            .db
            .query_read("SELECT COUNT(*) FROM docs", &[], |row| row.get::<_, i64>(0))?;
        Ok(rows.into_iter().next().unwrap_or(0))
    }

    // -----------------------------------------------------------------------
    // Entity operations
    // -----------------------------------------------------------------------

    /// Insert or update an entity and record an occurrence.
    ///
    /// If the entity does not exist, it is created.  If it does, its frequency
    /// is incremented and `last_seen` is updated to the maximum of its current
    /// value and the occurrence's `effective_date`.  Returns the `entity_id`.
    pub fn upsert_entity(
        &self,
        name: &str,
        entity_type: &EntityType,
        occurrence: &EntityOccurrence,
    ) -> Result<i64> {
        let conn = self.db.write_conn();
        let tx = conn
            .unchecked_transaction()
            .context("failed to begin transaction for upsert_entity")?;

        let effective_date = occurrence.effective_date;
        let type_str = entity_type.as_str();

        // INSERT OR IGNORE — creates the row only if it doesn't exist yet.
        tx.execute(
            "INSERT OR IGNORE INTO entities (name, type, frequency, first_seen, last_seen)
             VALUES (?1, ?2, 0, ?3, ?4)",
            params![name, type_str, effective_date, effective_date],
        )?;

        // UPDATE — increment frequency, refresh last_seen.
        tx.execute(
            "UPDATE entities
             SET frequency = frequency + 1,
                 last_seen = MAX(COALESCE(last_seen, 0), COALESCE(?1, 0))
             WHERE name = ?2 AND type = ?3",
            params![effective_date, name, type_str],
        )?;

        // Retrieve the entity id.
        let entity_id: i64 = {
            let mut stmt = tx.prepare("SELECT id FROM entities WHERE name = ?1 AND type = ?2")?;
            stmt.query_row(params![name, type_str], |row| row.get(0))?
        };

        // Insert the occurrence.
        tx.execute(
            "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at, effective_date, date_source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entity_id,
                occurrence.file_path,
                occurrence.context,
                occurrence.created_at,
                occurrence.effective_date,
                occurrence.date_source,
            ],
        )?;

        tx.commit().context("failed to commit upsert_entity")?;
        Ok(entity_id)
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
            let rows = self.db.query_read(
                "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
                 FROM entities ORDER BY frequency DESC LIMIT ?1",
                &[&(limit as i64) as &dyn rusqlite::types::ToSql],
                map_entity_row,
            )?;
            Ok(rows)
        }
    }

    /// Update the trends JSON blob for an entity.
    pub fn update_entity_trends(&self, entity_id: i64, trends: &EntityTrends) -> Result<()> {
        let trends_json =
            serde_json::to_string(trends).context("failed to serialize EntityTrends")?;
        self.db.execute_write(
            "UPDATE entities SET trends = ?1 WHERE id = ?2",
            &[
                &trends_json as &dyn rusqlite::types::ToSql,
                &entity_id,
            ],
        )?;
        Ok(())
    }

    /// Get all occurrences for a given entity.
    pub fn get_entity_occurrences(&self, entity_id: i64) -> Result<Vec<EntityOccurrence>> {
        let rows = self.db.query_read(
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
            let rows = self.db.query_read(
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
        let rows = self.db.query_read(
            "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
             FROM entities",
            &[],
            map_entity_row,
        )?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Catalyst operations
    // -----------------------------------------------------------------------

    /// Batch-insert catalysts.
    pub fn insert_catalysts(&self, catalysts: &[Catalyst]) -> Result<()> {
        let conn = self.db.write_conn();
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
        let rows = self.db.query_read(
            "SELECT id, text, entity, context, generated_at, metadata
             FROM catalysts WHERE entity = ?1",
            &[&entity as &dyn rusqlite::types::ToSql],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get every catalyst in the database.
    pub fn get_all_catalysts(&self) -> Result<Vec<Catalyst>> {
        let rows = self.db.query_read(
            "SELECT id, text, entity, context, generated_at, metadata FROM catalysts",
            &[],
            map_catalyst_row,
        )?;
        Ok(rows)
    }

    /// Get the most recent `generated_at` timestamp across all catalysts.
    pub fn get_latest_catalyst_timestamp(&self) -> Result<Option<DateTime<Utc>>> {
        let rows = self.db.query_read(
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
        let rows = self.db.query_read(
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
        self.db
            .execute_write("DELETE FROM catalysts WHERE entity = ?1", &[&entity])?;
        Ok(())
    }

    /// Batch upsert catalyst similarity records.
    pub fn upsert_catalyst_similarities(&self, sims: &[CatalystSimilarity]) -> Result<()> {
        let conn = self.db.write_conn();
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
        let rows = self.db.query_read(
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
        let rows = self.db.query_read(
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

    /// Get tags for a document by its source_ref (relative vault path).
    pub fn get_tags_for_document(&self, source_ref: &str) -> Result<Vec<String>> {
        let rows = self.db.query_read(
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
        let rows = self.db.query_read(
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
        self.db.execute_write(
            "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            &[&key as &dyn rusqlite::types::ToSql, &value],
        )?;
        Ok(())
    }

    /// Get a configuration value by key.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let rows = self.db.query_read(
            "SELECT value FROM config WHERE key = ?1",
            &[&key as &dyn rusqlite::types::ToSql],
            |row| row.get::<_, String>(0),
        )?;
        Ok(rows.into_iter().next())
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
