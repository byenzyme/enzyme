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
pub use repository::Repository;

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
}
