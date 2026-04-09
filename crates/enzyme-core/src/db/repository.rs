use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use libsql::Value;

use crate::document::ProcessedDocument;
use crate::models::*;

use super::connection::{block_in_place_sync, Database};

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
            let tx = self.conn
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
                    tx.execute("DELETE FROM doc_tags WHERE doc_id = ?1", vec![Value::Integer(old_id)]).await?;
                    tx.execute("DELETE FROM doc_links WHERE doc_id = ?1", vec![Value::Integer(old_id)]).await?;
                    tx.execute("DELETE FROM chunks WHERE doc_id = ?1", vec![Value::Integer(old_id)]).await?;
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
                for (i, chunk_content) in doc.chunks.iter().enumerate() {
                    let word_count = chunk_content.split_whitespace().count() as i64;
                    tx.execute(
                        "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        vec![
                            Value::Integer(doc_id),
                            Value::Integer(i as i64),
                            Value::Text(chunk_content.clone()),
                            Value::Integer(word_count),
                            Value::Integer(now),
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

    /// Return a map of `source_ref -> content_hash` for every document in the database.
    pub fn get_all_document_hashes(&self) -> Result<HashMap<String, String>> {
        let rows = self.query_read(
            "SELECT source_ref, content_hash FROM docs",
            vec![],
            |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Find documents whose embeddings are stale or missing.
    pub fn get_documents_needing_embedding(
        &self,
        max_count: Option<usize>,
        indexed_since: Option<i64>,
    ) -> Result<Vec<(i64, String, Vec<String>)>> {
        let mut params: Vec<Value> = Vec::new();
        let mut sql = String::from(
            "SELECT id, source_ref FROM docs
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
            Ok((row.get::<i64>(0)?, row.get::<String>(1)?))
        })?;

        let mut results = Vec::with_capacity(docs.len());
        for (doc_id, source_ref) in docs {
            let chunks = self.query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                vec![Value::Integer(doc_id)],
                |row| row.get::<String>(0),
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
        Ok(count)
    }

    /// Return all `source_ref` values from the docs table.
    pub fn get_all_source_refs(&self) -> Result<Vec<String>> {
        Ok(self.query_read("SELECT source_ref FROM docs", vec![], |row| {
            row.get::<String>(0)
        })?)
    }

    /// Return the total number of documents in the database.
    pub fn get_document_count(&self) -> Result<i64> {
        let rows = self.query_read("SELECT COUNT(*) FROM docs", vec![], |row| {
            row.get::<i64>(0)
        })?;
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
                (name.to_string(), entity_type.as_str().to_string(), occ.clone())
            })
            .collect();

        block_in_place_sync(async move {
            let tx = self.conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_entities_batch")?;

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
                     (entity_id, file_path, context, created_at, effective_date, date_source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    vec![
                        Value::Integer(entity_id),
                        Value::Text(occurrence.file_path.clone()),
                        opt_val(occurrence.context.clone()),
                        Value::Integer(occurrence.created_at),
                        opt_val(occurrence.effective_date),
                        Value::Text(occurrence.date_source.clone()),
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

    /// Get all occurrences for a given entity.
    pub fn get_entity_occurrences(&self, entity_id: i64) -> Result<Vec<EntityOccurrence>> {
        let rows = self.query_read(
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source
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
                })
            },
        )?;
        Ok(rows)
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
                "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
                 FROM entities ORDER BY frequency DESC LIMIT ?1",
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
            let tx = self.conn
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

    /// Fetch all occurrence timestamps in one query, grouped by entity_id.
    pub fn get_all_occurrence_dates(&self) -> Result<Vec<(i64, Option<i64>, i64)>> {
        let rows = self.query_read(
            "SELECT entity_id, effective_date, created_at
             FROM entity_occurrences
             ORDER BY entity_id",
            vec![],
            |row| Ok((row.get::<i64>(0)?, row.get::<Option<i64>>(1)?, row.get::<i64>(2)?)),
        )?;
        Ok(rows)
    }

    /// Get trending entities with guaranteed type diversity.
    /// Structural/tooling folder names that should never be trending candidates.
    const EXCLUDED_FOLDER_NAMES: &'static [&'static str] = &[
        ".claude", ".obsidian", ".enzyme", ".git", ".trash",
        "templates", "templater", "config", "migrations",
    ];

    pub fn get_trending_entities(
        &self,
        min_tags: usize,
        min_links: usize,
        min_folders: usize,
        total_limit: usize,
    ) -> Result<Vec<Entity>> {
        let all = self.get_all_entities()?;

        let is_root_level = |e: &Entity| -> bool {
            match e.entity_type {
                EntityType::Tag => true,
                EntityType::Link | EntityType::Folder => !e.name.contains('/'),
            }
        };

        let is_excluded_folder = |e: &Entity| -> bool {
            e.entity_type == EntityType::Folder
                && Self::EXCLUDED_FOLDER_NAMES.contains(&e.name.to_lowercase().as_str())
        };

        // Links referenced in fewer than 3 files are likely document titles, not concepts
        let is_low_reference_link = |e: &Entity| -> bool {
            e.entity_type == EntityType::Link && e.frequency < 3
        };

        let root_entities: Vec<Entity> = all
            .into_iter()
            .filter(|e| is_root_level(e) && !is_excluded_folder(e) && !is_low_reference_link(e))
            .collect();

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

        selected.sort_by(sort_by_recency);
        selected.truncate(total_limit);

        Ok(selected)
    }

    /// Look up entities by their (name, type) pairs.
    pub fn get_entities_by_names(&self, names: &[(&str, EntityType)]) -> Result<Vec<Entity>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        for (name, entity_type) in names {
            let rows = self.query_read(
                "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
                 FROM entities WHERE name = ?1 AND type = ?2",
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

    /// Retrieve every entity in the database.
    pub fn get_all_entities(&self) -> Result<Vec<Entity>> {
        let rows = self.query_read(
            "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
             FROM entities",
            vec![],
            map_entity_row,
        )?;
        Ok(rows)
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
            "SELECT id, name, type, frequency, first_seen, last_seen, trends, metadata
             FROM entities
             WHERE name LIKE ?1 AND name NOT LIKE ?2 AND type = ?3
             ORDER BY frequency DESC
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
            "SELECT id, entity_id, file_path, context, created_at, effective_date, date_source
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
            let tx = self.conn
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
        let names = self.query_read(
            "SELECT DISTINCT entity FROM catalysts",
            vec![],
            |row| row.get::<String>(0),
        )?;
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
        let rows = self.query_read(
            "SELECT MAX(generated_at) FROM catalysts",
            vec![],
            |row| row.get::<Option<String>>(0),
        )?;
        Ok(rows.into_iter().next().flatten().map(|s| parse_datetime_utc(&s)))
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

    /// Delete all catalysts for a given entity name.
    pub fn delete_catalysts_for_entity(&self, entity: &str) -> Result<()> {
        self.execute_write(
            "DELETE FROM catalysts WHERE entity = ?1",
            vec![Value::Text(entity.to_string())],
        )?;
        Ok(())
    }

    /// Batch upsert catalyst similarity records.
    pub fn upsert_catalyst_similarities(&self, sims: &[CatalystSimilarity]) -> Result<()> {
        let sims = sims.to_vec();
        block_in_place_sync(async move {
            let tx = self.conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_catalyst_similarities")?;

            for sim in &sims {
                let chunk_meta =
                    serde_json::to_string(&sim.chunk_metadata).unwrap_or_else(|_| "{}".to_string());
                let timeline =
                    serde_json::to_string(&sim.timeline_data).unwrap_or_else(|_| "{}".to_string());
                let created_at = sim.created_at.format("%Y-%m-%d %H:%M:%S").to_string();
                tx.execute(
                    "INSERT OR REPLACE INTO catalyst_similarities
                        (catalyst_id, catalyst_text, file_path, chunk_index,
                         similarity, chunk_metadata, timeline_data, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    vec![
                        Value::Text(sim.catalyst_id.clone()),
                        Value::Text(sim.catalyst_text.clone()),
                        Value::Text(sim.file_path.clone()),
                        Value::Integer(sim.chunk_index as i64),
                        Value::Real(sim.similarity),
                        Value::Text(chunk_meta),
                        Value::Text(timeline),
                        Value::Text(created_at),
                    ],
                )
                .await?;
            }

            tx.commit()
                .await
                .context("failed to commit upsert_catalyst_similarities")?;
            Ok(())
        })
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

    // -----------------------------------------------------------------------
    // Chunk operations
    // -----------------------------------------------------------------------

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
            vec![Value::Text(file_path.to_string()), Value::Integer(chunk_index)],
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

    /// Get all applied targets.
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
    pub fn upsert_chunk_embeddings(
        &self,
        embeddings: &[(String, u32, Vec<f32>)],
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let embeddings = embeddings.to_vec();
        block_in_place_sync(async move {
            let tx = self.conn
                .transaction()
                .await
                .context("failed to begin transaction for upsert_chunk_embeddings")?;

            for (file_path, chunk_index, vector) in &embeddings {
                let blob = f32_vec_to_blob(vector);
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
                Ok((file_path, chunk_index as u32, blob_to_f32_vec(&blob)))
            },
        )?;
        Ok(rows)
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
    pub fn upsert_catalyst_embeddings(
        &self,
        embeddings: &[(&str, &[f32])],
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }

        let n = embeddings.len();
        let ids: Vec<String> = embeddings.iter().map(|(id, _)| id.to_string()).collect();
        let ids_json = serde_json::to_string(&ids).context("serialize catalyst ids")?;
        let dim = embeddings[0].1.len();
        let mut all_vecs: Vec<u8> = Vec::with_capacity(n * dim * 4);
        for (_, vector) in embeddings {
            all_vecs.extend_from_slice(&f32_vec_to_blob(vector));
        }

        block_in_place_sync(async move {
            let tx = self.conn
                .transaction()
                .await
                .context("begin upsert_catalyst_embeddings")?;
            tx.execute("DELETE FROM catalyst_embeddings_packed", ()).await?;
            tx.execute(
                "INSERT INTO catalyst_embeddings_packed (count, ids, vectors) VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(n as i64),
                    Value::Text(ids_json),
                    Value::Blob(all_vecs),
                ],
            )
            .await?;
            tx.commit().await.context("commit upsert_catalyst_embeddings")
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

        let ids: Vec<String> = serde_json::from_str(&ids_json)
            .context("deserialize catalyst ids")?;
        let embedding_dim = if ids.is_empty() { return Ok(vec![]); } else { vectors_blob.len() / ids.len() };
        let results: Vec<(String, Vec<f32>)> = ids
            .into_iter()
            .zip(vectors_blob.chunks(embedding_dim).map(blob_to_f32_vec))
            .collect();
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Row-mapping helpers
// ---------------------------------------------------------------------------

fn map_entity_row(row: libsql::Row) -> Result<Entity, libsql::Error> {
    let trends_str: String = row.get::<Option<String>>(6)?.unwrap_or_default();
    let trends: EntityTrends = serde_json::from_str(&trends_str).unwrap_or_default();

    let metadata_str: String = row.get::<Option<String>>(7)?.unwrap_or_default();
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or(serde_json::Value::Object(Default::default()));

    Ok(Entity {
        id: Some(row.get::<i64>(0)?),
        name: row.get::<String>(1)?,
        entity_type: parse_entity_type(&row.get::<String>(2)?),
        frequency: row.get::<i64>(3)?,
        first_seen: row.get::<Option<i64>>(4)?,
        last_seen: row.get::<Option<i64>>(5)?,
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

// ---------------------------------------------------------------------------
// Blob <-> Vec<f32> helpers
// ---------------------------------------------------------------------------

fn f32_vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for &x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

fn blob_to_f32_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}
