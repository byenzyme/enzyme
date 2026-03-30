//! Database layer for Enzyme vaults.
//!
//! This module owns connection management, schema definition, and migration
//! execution.  The key design decision vs. the Python version is that tags and
//! links are stored in proper junction tables (`doc_tags`, `doc_links`) rather
//! than JSON blobs, enabling efficient queries and indexing.

mod connection;
mod errors;
pub mod migrations;
pub mod repository;
pub mod schema;

pub use connection::Database;
pub use errors::DbError;
pub use repository::{FolderChild, FolderStats, SubfolderStats};

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    // -- Helpers --------------------------------------------------------------

    /// Create a fresh in-memory database with all migrations applied.
    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
    }

    // -- Schema creation ------------------------------------------------------

    #[test]
    fn test_schema_creates_all_tables() {
        let db = test_db();

        let tables: Vec<String> = db
            .query_read(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                &[],
                |row| row.get(0),
            )
            .unwrap();

        for expected in schema::EXPECTED_TABLES {
            assert!(
                tables.contains(&expected.to_string()),
                "missing table: {expected}"
            );
        }
    }

    #[test]
    fn test_schema_creates_all_indexes() {
        let db = test_db();

        let indexes: Vec<String> = db
            .query_read(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                &[],
                |row| row.get(0),
            )
            .unwrap();

        for expected in schema::EXPECTED_INDEXES {
            assert!(
                indexes.contains(&expected.to_string()),
                "missing index: {expected}"
            );
        }
    }

    // -- Document + junction table round-trip ----------------------------------

    #[test]
    fn test_insert_and_query_document_with_tags_and_links() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Insert a document.
        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, word_count, char_count, created_at, modified_at, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            &[
                &"notes/rust.md" as &dyn rusqlite::types::ToSql,
                &"Rust Notes",
                &"Some content about Rust",
                &"abc123",
                &42i64,
                &200i64,
                &now,
                &now,
                &now,
            ],
        )
        .unwrap();

        // Get the doc id.
        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = ?1",
                &[&"notes/rust.md"],
                |row| row.get(0),
            )
            .unwrap()[0];

        // Insert tags via junction table.
        for tag in &["rust", "programming", "systems"] {
            db.execute_write(
                "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, ?2)",
                &[&doc_id as &dyn rusqlite::types::ToSql, tag],
            )
            .unwrap();
        }

        // Insert links via junction table.
        for link in &["memory-safety", "ownership"] {
            db.execute_write(
                "INSERT INTO doc_links (doc_id, link) VALUES (?1, ?2)",
                &[&doc_id as &dyn rusqlite::types::ToSql, link],
            )
            .unwrap();
        }

        // Query tags for the document.
        let tags: Vec<String> = db
            .query_read(
                "SELECT tag FROM doc_tags WHERE doc_id = ?1 ORDER BY tag",
                &[&doc_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["programming", "rust", "systems"]);

        // Query links for the document.
        let links: Vec<String> = db
            .query_read(
                "SELECT link FROM doc_links WHERE doc_id = ?1 ORDER BY link",
                &[&doc_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, vec!["memory-safety", "ownership"]);

        // Reverse lookup: find docs by tag.
        let docs_with_rust: Vec<String> = db
            .query_read(
                "SELECT d.source_ref FROM docs d JOIN doc_tags dt ON d.id = dt.doc_id WHERE dt.tag = ?1",
                &[&"rust"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(docs_with_rust, vec!["notes/rust.md"]);
    }

    // -- Chunks ----------------------------------------------------------------

    #[test]
    fn test_insert_and_query_chunks() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('doc.md', 'hash1', ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'doc.md'",
                &[],
                |row| row.get(0),
            )
            .unwrap()[0];

        for i in 0..3 {
            db.execute_write(
                "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    &doc_id as &dyn rusqlite::types::ToSql,
                    &(i as i64),
                    &format!("Chunk {i} content"),
                    &10i64,
                    &now,
                ],
            )
            .unwrap();
        }

        let chunks: Vec<(i64, String)> = db
            .query_read(
                "SELECT chunk_index, content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                &[&doc_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].1, "Chunk 0 content");
        assert_eq!(chunks[2].0, 2);
    }

    // -- Foreign key cascade --------------------------------------------------

    #[test]
    fn test_cascade_delete_removes_tags_links_chunks() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('del.md', 'hash', ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'del.md'",
                &[],
                |row| row.get(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'test')",
            &[&doc_id],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO doc_links (doc_id, link) VALUES (?1, 'link')",
            &[&doc_id],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, created_at) VALUES (?1, 0, 'c', ?2)",
            &[&doc_id as &dyn rusqlite::types::ToSql, &now],
        )
        .unwrap();

        // Delete the parent document.
        db.execute_write("DELETE FROM docs WHERE id = ?1", &[&doc_id])
            .unwrap();

        // All children should be gone.
        let tag_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM doc_tags", &[], |row| row.get(0))
            .unwrap();
        assert_eq!(tag_count[0], 0);

        let link_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM doc_links", &[], |row| row.get(0))
            .unwrap();
        assert_eq!(link_count[0], 0);

        let chunk_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM chunks", &[], |row| row.get(0))
            .unwrap();
        assert_eq!(chunk_count[0], 0);
    }

    // -- Transactions ---------------------------------------------------------

    #[test]
    fn test_transaction_commit() {
        let mut db = test_db();
        let now = 1_700_000_000_000i64;

        db.transaction(|tx| {
            tx.execute(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('tx.md', 'h', ?1, ?1, ?1)",
                params![now],
            )?;
            Ok(())
        })
        .unwrap();

        let count: Vec<i64> = db
            .query_read(
                "SELECT COUNT(*) FROM docs WHERE source_ref = 'tx.md'",
                &[],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count[0], 1);
    }

    #[test]
    fn test_transaction_rollback() {
        let mut db = test_db();
        let now = 1_700_000_000_000i64;

        let result: Result<(), DbError> = db.transaction(|tx| {
            tx.execute(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('rollback.md', 'h', ?1, ?1, ?1)",
                params![now],
            )?;
            // Force an error to trigger rollback.
            Err(DbError::Migration("intentional test rollback".into()))
        });

        assert!(result.is_err());

        // The insert should NOT have persisted.
        let count: Vec<i64> = db
            .query_read(
                "SELECT COUNT(*) FROM docs WHERE source_ref = 'rollback.md'",
                &[],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count[0], 0);
    }

    // -- WAL mode -------------------------------------------------------------

    #[test]
    fn test_wal_mode_active_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();

        let mode: Vec<String> = db
            .query_read("PRAGMA journal_mode", &[], |row| row.get(0))
            .unwrap();
        assert_eq!(mode[0].to_lowercase(), "wal");
    }

    // -- Foreign keys ON ------------------------------------------------------

    #[test]
    fn test_foreign_keys_enabled() {
        let db = test_db();

        let fk: Vec<i64> = db
            .query_read("PRAGMA foreign_keys", &[], |row| row.get(0))
            .unwrap();
        assert_eq!(fk[0], 1, "foreign_keys pragma should be ON");
    }

    // -- Migrations idempotency -----------------------------------------------

    #[test]
    fn test_migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idempotent.db");

        // Open twice -- the second open should be a no-op for migrations.
        {
            let _db = Database::open(&db_path).unwrap();
        }
        {
            let db = Database::open(&db_path).unwrap();

            // Verify tables still exist after second open.
            let tables: Vec<String> = db
                .query_read(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                    &[],
                    |row| row.get(0),
                )
                .unwrap();

            for expected in schema::EXPECTED_TABLES {
                assert!(
                    tables.contains(&expected.to_string()),
                    "missing table after re-open: {expected}"
                );
            }
        }
    }

    // -- Config table ---------------------------------------------------------

    #[test]
    fn test_config_key_value_store() {
        let db = test_db();

        db.execute_write(
            "INSERT INTO config (key, value) VALUES ('vault_name', 'My Vault')",
            &[],
        )
        .unwrap();

        let val: Vec<String> = db
            .query_read(
                "SELECT value FROM config WHERE key = 'vault_name'",
                &[],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val[0], "My Vault");

        // Upsert with ON CONFLICT.
        db.execute_write(
            "INSERT INTO config (key, value) VALUES ('vault_name', 'Renamed Vault')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            &[],
        )
        .unwrap();

        let val: Vec<String> = db
            .query_read(
                "SELECT value FROM config WHERE key = 'vault_name'",
                &[],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val[0], "Renamed Vault");
    }

    // -- Entities + occurrences -----------------------------------------------

    #[test]
    fn test_entities_and_occurrences() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                &"rust" as &dyn rusqlite::types::ToSql,
                &"tag",
                &5i64,
                &now,
                &now,
            ],
        )
        .unwrap();

        let entity_id: i64 = db
            .query_read(
                "SELECT id FROM entities WHERE name = 'rust' AND type = 'tag'",
                &[],
                |row| row.get(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at, effective_date)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                &entity_id as &dyn rusqlite::types::ToSql,
                &"notes/rust.md",
                &"discussing ownership model",
                &now,
                &now,
            ],
        )
        .unwrap();

        let occurrences: Vec<(String, String)> = db
            .query_read(
                "SELECT eo.file_path, e.name
                 FROM entity_occurrences eo
                 JOIN entities e ON e.id = eo.entity_id
                 WHERE e.name = 'rust'",
                &[],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].0, "notes/rust.md");
    }

    // -- Catalysts + similarities ---------------------------------------------

    #[test]
    fn test_catalysts_and_similarities() {
        let db = test_db();

        db.execute_write(
            "INSERT INTO catalysts (id, text, entity, context) VALUES (?1, ?2, ?3, ?4)",
            &[
                &"cat-001" as &dyn rusqlite::types::ToSql,
                &"What patterns exist in systems programming?",
                &"rust",
                &"default",
            ],
        )
        .unwrap();

        db.execute_write(
            "INSERT INTO catalyst_similarities (catalyst_id, catalyst_text, file_path, chunk_index, similarity)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                &"cat-001" as &dyn rusqlite::types::ToSql,
                &"What patterns exist in systems programming?",
                &"notes/rust.md",
                &0i64,
                &0.87f64,
            ],
        )
        .unwrap();

        let sims: Vec<(String, f64)> = db
            .query_read(
                "SELECT file_path, similarity FROM catalyst_similarities WHERE catalyst_id = 'cat-001'",
                &[],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(sims.len(), 1);
        assert_eq!(sims[0].0, "notes/rust.md");
        assert!((sims[0].1 - 0.87).abs() < 1e-6);
    }

    // -- Unique constraints ---------------------------------------------------

    #[test]
    fn test_unique_source_ref_constraint() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('dup.md', 'h', ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();

        let result = db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('dup.md', 'h2', ?1, ?1, ?1)",
            &[&now],
        );

        assert!(result.is_err(), "duplicate source_ref should be rejected");
    }

    #[test]
    fn test_unique_doc_tag_pair() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('u.md', 'h', ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'u.md'",
                &[],
                |row| row.get(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'dup')",
            &[&doc_id],
        )
        .unwrap();

        let result = db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'dup')",
            &[&doc_id],
        );
        assert!(result.is_err(), "duplicate (doc_id, tag) should be rejected");
    }

    // -- upsert_documents_batch -----------------------------------------------

    #[test]
    fn test_upsert_documents_batch_inserts_with_tags_links_chunks() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        let doc = crate::document::ProcessedDocument {
            source_ref: "notes/test.md".into(),
            title: Some("Test".into()),
            content: "hello world".into(),
            content_hash: "hash1".into(),
            word_count: 2,
            char_count: 11,
            created_at: now,
            created_source: crate::models::DateSource::Filesystem,
            modified_at: now,
            modified_source: crate::models::DateSource::Filesystem,
            tags: vec!["rust".into(), "test".into()],
            links: vec!["other-note".into()],
            folders: vec![],
            chunks: vec!["hello".into(), "world".into()],
            frontmatter: None,
        };

        let ids = db.upsert_documents_batch(&[doc]).unwrap();
        assert_eq!(ids.len(), 1);

        let tags: Vec<String> = db
            .query_read(
                "SELECT tag FROM doc_tags WHERE doc_id = ?1 ORDER BY tag",
                &[&ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["rust", "test"]);

        let links: Vec<String> = db
            .query_read(
                "SELECT link FROM doc_links WHERE doc_id = ?1",
                &[&ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, vec!["other-note"]);

        let chunks: Vec<String> = db
            .query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                &[&ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunks, vec!["hello", "world"]);
    }

    #[test]
    fn test_upsert_documents_batch_replaces_existing() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        let make_doc = |hash: &str, tags: Vec<String>, chunks: Vec<String>| {
            crate::document::ProcessedDocument {
                source_ref: "notes/test.md".into(),
                title: Some("Test".into()),
                content: "content".into(),
                content_hash: hash.into(),
                word_count: 1,
                char_count: 7,
                created_at: now,
                created_source: crate::models::DateSource::Filesystem,
                modified_at: now,
                modified_source: crate::models::DateSource::Filesystem,
                tags,
                links: vec![],
                folders: vec![],
                chunks,
                frontmatter: None,
            }
        };

        // Insert v1
        db.upsert_documents_batch(&[make_doc(
            "v1",
            vec!["old-tag".into()],
            vec!["old chunk".into()],
        )])
        .unwrap();

        // Upsert v2 — should replace tags and chunks
        let ids = db
            .upsert_documents_batch(&[make_doc(
                "v2",
                vec!["new-tag".into()],
                vec!["new chunk a".into(), "new chunk b".into()],
            )])
            .unwrap();

        let tags: Vec<String> = db
            .query_read(
                "SELECT tag FROM doc_tags WHERE doc_id = ?1",
                &[&ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["new-tag"]);

        let chunks: Vec<String> = db
            .query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                &[&ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunks, vec!["new chunk a", "new chunk b"]);

        // Only one document should exist
        assert_eq!(db.get_document_count().unwrap(), 1);
    }

    // -- get_trending_entities ------------------------------------------------

    #[test]
    fn test_get_trending_entities_type_diversity() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Insert entities of each type with varying recency scores
        let types_and_names: Vec<(&str, &str, f64)> = vec![
            ("tag", "rust", 0.9),
            ("tag", "python", 0.8),
            ("tag", "go", 0.7),
            ("link", "alice", 0.95),
            ("link", "bob", 0.6),
            ("folder", "projects", 0.85),
            ("folder", "archive", 0.3),
        ];

        for (etype, name, score) in &types_and_names {
            let trends = serde_json::json!({ "recency_score": score });
            db.execute_write(
                "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends)
                 VALUES (?1, ?2, 1, ?3, ?3, ?4)",
                &[
                    &name.to_string() as &dyn rusqlite::types::ToSql,
                    &etype.to_string(),
                    &now,
                    &trends.to_string(),
                ],
            )
            .unwrap();
        }

        // Request min 2 tags, 1 link, 1 folder, total 5
        let entities = db.get_trending_entities(2, 1, 1, 5).unwrap();
        assert_eq!(entities.len(), 5);

        let tags: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == crate::models::EntityType::Tag)
            .map(|e| e.name.as_str())
            .collect();
        let links: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == crate::models::EntityType::Link)
            .map(|e| e.name.as_str())
            .collect();
        let folders: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == crate::models::EntityType::Folder)
            .map(|e| e.name.as_str())
            .collect();

        assert!(tags.len() >= 2, "should have at least 2 tags");
        assert!(links.len() >= 1, "should have at least 1 link");
        assert!(folders.len() >= 1, "should have at least 1 folder");
    }

    // -- get_documents_needing_embedding --------------------------------------

    #[test]
    fn test_get_documents_needing_embedding_filters() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Doc 1: not embedded (embedded_at IS NULL)
        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('a.md', 'h1', ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();
        let id1: i64 = db
            .query_read("SELECT id FROM docs WHERE source_ref = 'a.md'", &[], |r| r.get(0))
            .unwrap()[0];
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, 0, 'chunk a', 2, ?2)",
            &[&id1 as &dyn rusqlite::types::ToSql, &now],
        )
        .unwrap();

        // Doc 2: embedded and up to date
        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, embedded_content_hash, embedded_at, created_at, modified_at, indexed_at)
             VALUES ('b.md', 'h2', 'h2', ?1, ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();

        // Doc 3: embedded but stale (hash mismatch)
        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, embedded_content_hash, embedded_at, created_at, modified_at, indexed_at)
             VALUES ('c.md', 'h3-new', 'h3-old', ?1, ?1, ?1, ?1)",
            &[&now],
        )
        .unwrap();
        let id3: i64 = db
            .query_read("SELECT id FROM docs WHERE source_ref = 'c.md'", &[], |r| r.get(0))
            .unwrap()[0];
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, 0, 'chunk c', 2, ?2)",
            &[&id3 as &dyn rusqlite::types::ToSql, &now],
        )
        .unwrap();

        // Without filters: should return docs a.md and c.md
        let needing = db.get_documents_needing_embedding(None, None).unwrap();
        let refs: Vec<&str> = needing.iter().map(|(_, r, _)| r.as_str()).collect();
        assert!(refs.contains(&"a.md"));
        assert!(refs.contains(&"c.md"));
        assert!(!refs.contains(&"b.md"));

        // With max_count = 1: should return only 1
        let limited = db.get_documents_needing_embedding(Some(1), None).unwrap();
        assert_eq!(limited.len(), 1);

        // With indexed_since filter (future timestamp): should return none
        let future = now + 100_000;
        let filtered = db
            .get_documents_needing_embedding(None, Some(future))
            .unwrap();
        assert!(filtered.is_empty());
    }

    // -- get_folder_children_by_references ------------------------------------

    #[test]
    fn test_get_folder_children_by_references() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Create folder children: people/Alice.md, people/Bob.md
        for name in &["people/Alice.md", "people/Bob.md"] {
            db.execute_write(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                 VALUES (?1, 'h', ?2, ?2, ?2)",
                &[&name.to_string() as &dyn rusqlite::types::ToSql, &now],
            )
            .unwrap();
        }

        // Create a doc outside the folder that links to "alice" twice (via two docs)
        for (i, source) in ["notes/journal.md", "notes/meeting.md"].iter().enumerate() {
            db.execute_write(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                 VALUES (?1, ?2, ?3, ?3, ?3)",
                &[
                    &source.to_string() as &dyn rusqlite::types::ToSql,
                    &format!("h{}", i + 10),
                    &now,
                ],
            )
            .unwrap();
            let doc_id: i64 = db
                .query_read(
                    "SELECT id FROM docs WHERE source_ref = ?1",
                    &[&source.to_string()],
                    |r| r.get(0),
                )
                .unwrap()[0];
            db.execute_write(
                "INSERT INTO doc_links (doc_id, link) VALUES (?1, 'alice')",
                &[&doc_id],
            )
            .unwrap();
        }

        let (children, total) = db.get_folder_children_by_references("people", 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(children[0].name, "Alice");
        assert_eq!(children[0].reference_count, 2);
        // Bob has 0 external references
        assert_eq!(children[1].name, "Bob");
        assert_eq!(children[1].reference_count, 0);
    }
}
