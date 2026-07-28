//! Database layer for Enzyme vaults.
//!
//! This module owns connection management, schema definition, and migration
//! execution.  The key design decision vs. the Python version is that tags and
//! links are stored in proper junction tables (`doc_tags`, `doc_links`) rather
//! than JSON blobs, enabling efficient queries and indexing.

pub(crate) mod connection;
mod errors;
pub mod migrations;
pub mod repository;
pub mod schema;

pub use connection::Database;
pub use errors::DbError;
pub use repository::{
    DocumentForEmbedding, EntityRefreshStats, FileChunk, FolderChild, FolderStats, SubfolderStats,
};

#[cfg(test)]
mod tests {
    use super::*;
    use libsql::Value;

    // -- Helpers --------------------------------------------------------------

    /// Create a fresh in-memory database with all migrations applied.
    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
    }

    /// Compare two vectors that may have round-tripped through int8
    /// quantized embedding storage; exact float equality doesn't hold there.
    fn assert_vectors_approx_eq(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len(), "vector length mismatch");
        for (a, e) in actual.iter().zip(expected.iter()) {
            assert!(
                (a - e).abs() <= 0.05,
                "vectors differ beyond quantization tolerance: {actual:?} vs {expected:?}"
            );
        }
    }

    // -- Schema creation ------------------------------------------------------

    #[test]
    fn test_schema_creates_all_tables() {
        let db = test_db();

        let tables: Vec<String> = db
            .query_read(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                vec![],
                |row| row.get::<String>(0),
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
                vec![],
                |row| row.get::<String>(0),
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

        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, word_count, char_count, created_at, modified_at, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            vec![
                Value::Text("notes/rust.md".into()),
                Value::Text("Rust Notes".into()),
                Value::Text("Some content about Rust".into()),
                Value::Text("abc123".into()),
                Value::Integer(42),
                Value::Integer(200),
                Value::Integer(now),
                Value::Integer(now),
                Value::Integer(now),
            ],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = ?1",
                vec![Value::Text("notes/rust.md".into())],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        for tag in &["rust", "programming", "systems"] {
            db.execute_write(
                "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, ?2)",
                vec![Value::Integer(doc_id), Value::Text(tag.to_string())],
            )
            .unwrap();
        }

        for link in &["memory-safety", "ownership"] {
            db.execute_write(
                "INSERT INTO doc_links (doc_id, link) VALUES (?1, ?2)",
                vec![Value::Integer(doc_id), Value::Text(link.to_string())],
            )
            .unwrap();
        }

        let tags: Vec<String> = db
            .query_read(
                "SELECT tag FROM doc_tags WHERE doc_id = ?1 ORDER BY tag",
                vec![Value::Integer(doc_id)],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["programming", "rust", "systems"]);

        let links: Vec<String> = db
            .query_read(
                "SELECT link FROM doc_links WHERE doc_id = ?1 ORDER BY link",
                vec![Value::Integer(doc_id)],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(links, vec!["memory-safety", "ownership"]);

        let docs_with_rust: Vec<String> = db
            .query_read(
                "SELECT d.source_ref FROM docs d JOIN doc_tags dt ON d.id = dt.doc_id WHERE dt.tag = ?1",
                vec![Value::Text("rust".into())],
                |row| row.get::<String>(0),
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
            vec![Value::Integer(now)],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'doc.md'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        for i in 0..3 {
            db.execute_write(
                "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    Value::Integer(doc_id),
                    Value::Integer(i),
                    Value::Text(format!("Chunk {i} content")),
                    Value::Integer(10),
                    Value::Integer(now),
                ],
            )
            .unwrap();
        }

        let chunks: Vec<(i64, String)> = db
            .query_read(
                "SELECT chunk_index, content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                vec![Value::Integer(doc_id)],
                |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
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
            vec![Value::Integer(now)],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'del.md'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'test')",
            vec![Value::Integer(doc_id)],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO doc_links (doc_id, link) VALUES (?1, 'link')",
            vec![Value::Integer(doc_id)],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, created_at) VALUES (?1, 0, 'c', ?2)",
            vec![Value::Integer(doc_id), Value::Integer(now)],
        )
        .unwrap();

        db.execute_write(
            "DELETE FROM docs WHERE id = ?1",
            vec![Value::Integer(doc_id)],
        )
        .unwrap();

        let tag_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM doc_tags", vec![], |row| {
                row.get::<i64>(0)
            })
            .unwrap();
        assert_eq!(tag_count[0], 0);

        let link_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM doc_links", vec![], |row| {
                row.get::<i64>(0)
            })
            .unwrap();
        assert_eq!(link_count[0], 0);

        let chunk_count: Vec<i64> = db
            .query_read("SELECT COUNT(*) FROM chunks", vec![], |row| {
                row.get::<i64>(0)
            })
            .unwrap();
        assert_eq!(chunk_count[0], 0);
    }

    // -- Transactions ---------------------------------------------------------

    #[test]
    fn test_transaction_commit() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Use BEGIN/COMMIT to test transaction commit semantics.
        db.execute_write("BEGIN", vec![]).unwrap();
        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('tx.md', 'h', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();
        db.execute_write("COMMIT", vec![]).unwrap();

        let count: Vec<i64> = db
            .query_read(
                "SELECT COUNT(*) FROM docs WHERE source_ref = 'tx.md'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap();
        assert_eq!(count[0], 1);
    }

    #[test]
    fn test_transaction_rollback() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        // Use BEGIN/ROLLBACK to test that uncommitted changes are discarded.
        db.execute_write("BEGIN", vec![]).unwrap();
        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('rollback.md', 'h', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();
        db.execute_write("ROLLBACK", vec![]).unwrap();

        let count: Vec<i64> = db
            .query_read(
                "SELECT COUNT(*) FROM docs WHERE source_ref = 'rollback.md'",
                vec![],
                |row| row.get::<i64>(0),
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
            .query_read("PRAGMA journal_mode", vec![], |row| row.get::<String>(0))
            .unwrap();
        assert_eq!(mode[0].to_lowercase(), "wal");
    }

    // -- Foreign keys ON ------------------------------------------------------

    #[test]
    fn test_foreign_keys_enabled() {
        let db = test_db();

        let fk: Vec<i64> = db
            .query_read("PRAGMA foreign_keys", vec![], |row| row.get::<i64>(0))
            .unwrap();
        assert_eq!(fk[0], 1, "foreign_keys pragma should be ON");
    }

    // -- Migrations idempotency -----------------------------------------------

    #[test]
    fn test_migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idempotent.db");

        {
            let _db = Database::open(&db_path).unwrap();
        }
        {
            let db = Database::open(&db_path).unwrap();

            let tables: Vec<String> = db
                .query_read(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                    vec![],
                    |row| row.get::<String>(0),
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

    #[test]
    fn test_context_slim_migration_excerpts_oversized_and_short_circuits() {
        use crate::models::INLINE_CONTEXT_MAX_WORDS;
        const SLIM_KEY: &str = "migration.context_slim_v1";

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context_slim.db");

        // Oversized body with distinct bookends and a middle marker the excerpt
        // must drop.
        let big_context = format!(
            "STARTMARKER {} MIDDLEMARKER {} ENDMARKER",
            "word ".repeat(300),
            "word ".repeat(300),
        );

        {
            // Opening a fresh DB runs the migration against an empty
            // `entity_occurrences` table, which sets the flag immediately.
            // Remove it here to simulate a legacy pre-migration database that
            // already has oversized/normal contexts on disk.
            let db = Database::open(&path).unwrap();
            db.execute_write(
                &format!("DELETE FROM config WHERE key = '{SLIM_KEY}'"),
                vec![],
            )
            .unwrap();

            db.execute_write(
                "INSERT INTO entities (id, name, type) VALUES (1, 'test', 'tag')",
                vec![],
            )
            .unwrap();

            db.execute_write(
                "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at) \
                 VALUES (1, 'big.md', ?1, 0)",
                vec![Value::Text(big_context.clone())],
            )
            .unwrap();
            db.execute_write(
                "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at) \
                 VALUES (1, 'small.md', 'short text', 0)",
                vec![],
            )
            .unwrap();
        }

        {
            // Reopening should find the flag missing, scan, replace the
            // oversized context with a bounded excerpt, and preserve the
            // normal one.
            let db = Database::open(&path).unwrap();

            let big_ctx: Option<String> = db
                .query_read(
                    "SELECT context FROM entity_occurrences WHERE file_path = 'big.md'",
                    vec![],
                    |row| row.get::<Option<String>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .flatten();
            let big_ctx = big_ctx.expect("oversized context should be replaced by an excerpt");
            assert!(
                big_ctx.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS,
                "excerpt must stay under the inline-context filter"
            );
            assert!(
                big_ctx.contains("STARTMARKER") && big_ctx.contains("ENDMARKER"),
                "excerpt should retain both bookends"
            );
            assert!(
                !big_ctx.contains("MIDDLEMARKER"),
                "excerpt should drop the middle of the body"
            );

            let small_ctx: Option<String> = db
                .query_read(
                    "SELECT context FROM entity_occurrences WHERE file_path = 'small.md'",
                    vec![],
                    |row| row.get::<Option<String>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .flatten();
            assert_eq!(small_ctx.as_deref(), Some("short text"));

            let flag: Vec<String> = db
                .query_read(
                    &format!("SELECT value FROM config WHERE key = '{SLIM_KEY}'"),
                    vec![],
                    |row| row.get::<String>(0),
                )
                .unwrap();
            assert_eq!(flag.len(), 1, "migration flag should be set after running");

            // Insert another oversized row directly, bypassing the migration
            // path entirely, to verify the next open short-circuits.
            db.execute_write(
                "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at) \
                 VALUES (1, 'big2.md', ?1, 0)",
                vec![Value::Text(big_context.clone())],
            )
            .unwrap();
        }

        {
            // Since the flag is already set, this open must not rescan —
            // the newly inserted oversized row should survive at full length.
            let db = Database::open(&path).unwrap();
            let big2_ctx: Option<String> = db
                .query_read(
                    "SELECT context FROM entity_occurrences WHERE file_path = 'big2.md'",
                    vec![],
                    |row| row.get::<Option<String>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .flatten();
            let big2_ctx = big2_ctx.expect("row should survive the short-circuited open");
            assert!(
                big2_ctx.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS,
                "second open should short-circuit on the config flag and not re-excerpt"
            );
        }
    }

    #[test]
    fn test_context_excerpt_backfill_restores_nulled_long_notes() {
        use crate::models::INLINE_CONTEXT_MAX_WORDS;
        const BACKFILL_KEY: &str = "migration.context_excerpt_backfill_v1";

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context_backfill.db");

        let raw_doc = format!(
            "---\ntags: [big]\n---\nSTARTMARKER {} MIDDLEMARKER {} ENDMARKER",
            "word ".repeat(300),
            "word ".repeat(300),
        );

        {
            // Simulate a vault that already ran the *original* slim migration:
            // the doc body is on disk, but the frontmatter-tag occurrence's
            // context was nulled out. Clear the backfill flag so it runs.
            let db = Database::open(&path).unwrap();
            db.execute_write(
                &format!("DELETE FROM config WHERE key = '{BACKFILL_KEY}'"),
                vec![],
            )
            .unwrap();
            db.execute_write(
                "INSERT INTO entities (id, name, type) VALUES (1, 'big', 'tag')",
                vec![],
            )
            .unwrap();
            db.execute_write(
                "INSERT INTO docs \
                 (source_ref, content, content_hash, created_at, modified_at, indexed_at) \
                 VALUES ('big.md', ?1, 'h', 0, 0, 0)",
                vec![Value::Text(raw_doc)],
            )
            .unwrap();
            // Null context — the state the original slim migration left behind.
            db.execute_write(
                "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at) \
                 VALUES (1, 'big.md', NULL, 0)",
                vec![],
            )
            .unwrap();
        }

        {
            // Reopening runs the backfill, which reconstructs the bounded
            // excerpt from the doc body.
            let db = Database::open(&path).unwrap();
            let ctx: Option<String> = db
                .query_read(
                    "SELECT context FROM entity_occurrences WHERE file_path = 'big.md'",
                    vec![],
                    |row| row.get::<Option<String>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .flatten();
            let ctx = ctx.expect("backfill should restore a context from the doc body");
            assert!(
                ctx.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS,
                "backfilled excerpt must stay under the inline-context filter"
            );
            assert!(
                ctx.contains("STARTMARKER") && ctx.contains("ENDMARKER"),
                "backfilled excerpt should retain both bookends"
            );
            assert!(
                !ctx.contains("MIDDLEMARKER"),
                "backfilled excerpt should drop the middle"
            );
            // Frontmatter must not leak into the excerpt.
            assert!(!ctx.contains("tags:"), "frontmatter should be stripped");
        }
    }

    #[test]
    fn test_embedding_int8_migration_converts_legacy_blobs_and_short_circuits() {
        use crate::embedding::quantize;
        use crate::models::EMBEDDING_DIMENSION;
        const EMBED_KEY: &str = "migration.embedding_int8_v1";
        let dim = EMBEDDING_DIMENSION;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedding_int8.db");

        let legacy_chunk_vec: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001) - 0.25).collect();
        let legacy_chunk_blob: Vec<u8> = legacy_chunk_vec
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();

        let legacy_cat_vec_a: Vec<f32> = (0..dim).map(|i| i as f32 * 0.0005).collect();
        let legacy_cat_vec_b: Vec<f32> = (0..dim).map(|i| -(i as f32 * 0.0003)).collect();
        let legacy_packed_blob: Vec<u8> = legacy_cat_vec_a
            .iter()
            .chain(legacy_cat_vec_b.iter())
            .flat_map(|x| x.to_le_bytes())
            .collect();

        {
            // Opening a fresh DB runs the migration against empty tables,
            // which sets the flag immediately. Remove it here to simulate a
            // legacy pre-migration database that already has raw f32 blobs
            // on disk.
            let db = Database::open(&path).unwrap();
            db.execute_write(
                &format!("DELETE FROM config WHERE key = '{EMBED_KEY}'"),
                vec![],
            )
            .unwrap();

            db.execute_write(
                "INSERT INTO chunk_embeddings (file_path, chunk_index, vector) VALUES ('a.md', 0, ?1)",
                vec![Value::Blob(legacy_chunk_blob.clone())],
            )
            .unwrap();

            db.execute_write(
                "INSERT INTO catalyst_embeddings_packed (id, count, ids, vectors) VALUES (1, 2, ?1, ?2)",
                vec![
                    Value::Text(serde_json::to_string(&vec!["cat-a", "cat-b"]).unwrap()),
                    Value::Blob(legacy_packed_blob.clone()),
                ],
            )
            .unwrap();
        }

        {
            // Reopening should find the flag missing, rewrite both blobs to
            // int8_v1, and record completion.
            let db = Database::open(&path).unwrap();

            let chunk_blob: Vec<u8> = db
                .query_read(
                    "SELECT vector FROM chunk_embeddings WHERE file_path='a.md' AND chunk_index=0",
                    vec![],
                    |r| r.get::<Vec<u8>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                chunk_blob.len(),
                dim + 4,
                "chunk blob should be rewritten to the int8_v1 length"
            );
            let decoded = quantize::dequantize_int8(&chunk_blob);
            assert_vectors_approx_eq(&decoded, &legacy_chunk_vec);

            let packed_blob: Vec<u8> = db
                .query_read(
                    "SELECT vectors FROM catalyst_embeddings_packed WHERE id=1",
                    vec![],
                    |r| r.get::<Vec<u8>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                packed_blob.len(),
                2 * 4 + 2 * dim,
                "packed blob should be rewritten to the int8_v1 length"
            );
            let decoded_packed = quantize::dequantize_packed(&packed_blob, 2);
            assert_vectors_approx_eq(&decoded_packed[0], &legacy_cat_vec_a);
            assert_vectors_approx_eq(&decoded_packed[1], &legacy_cat_vec_b);

            let format_flag = db.get_config(quantize::VECTOR_FORMAT_CONFIG_KEY).unwrap();
            assert_eq!(format_flag.as_deref(), Some(quantize::VECTOR_FORMAT_INT8_V1));

            let migration_flag: Vec<String> = db
                .query_read(
                    &format!("SELECT value FROM config WHERE key = '{EMBED_KEY}'"),
                    vec![],
                    |row| row.get::<String>(0),
                )
                .unwrap();
            assert_eq!(migration_flag.len(), 1, "migration flag should be set after running");

            // Insert another legacy blob directly, bypassing the migration
            // path entirely, to verify the next open short-circuits.
            let bypass_vec = vec![9.0f32; dim];
            let bypass_blob: Vec<u8> = bypass_vec.iter().flat_map(|x| x.to_le_bytes()).collect();
            db.execute_write(
                "INSERT INTO chunk_embeddings (file_path, chunk_index, vector) VALUES ('bypass.md', 0, ?1)",
                vec![Value::Blob(bypass_blob)],
            )
            .unwrap();
        }

        {
            // Since the flag is already set, this open must not rescan — the
            // newly inserted legacy blob survives untouched. General readers
            // must still decode it correctly via defensive length detection.
            let db = Database::open(&path).unwrap();
            let bypass_blob: Vec<u8> = db
                .query_read(
                    "SELECT vector FROM chunk_embeddings WHERE file_path='bypass.md' AND chunk_index=0",
                    vec![],
                    |r| r.get::<Vec<u8>>(0),
                )
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                bypass_blob.len(),
                dim * 4,
                "short-circuited migration must not rescan/rewrite existing rows"
            );

            let all = db.get_all_chunk_embeddings().unwrap();
            let bypass_entry = all.iter().find(|(path, _, _)| path == "bypass.md").unwrap();
            assert_vectors_approx_eq(&bypass_entry.2, &vec![9.0f32; dim]);
        }
    }

    #[test]
    fn test_legacy_database_gains_epoch_state_without_losing_catalysts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let db = Database::open(&path).unwrap();
            db.execute_write(
                "INSERT INTO catalysts (id,text,entity) VALUES ('legacy','text','entity')",
                vec![],
            )
            .unwrap();
            db.execute_write("DROP TABLE catalyst_epoch_state", vec![])
                .unwrap();
            db.execute_write("DROP TABLE catalyst_epochs", vec![])
                .unwrap();
        }
        let migrated = Database::open(&path).unwrap();
        assert_eq!(migrated.get_all_catalysts().unwrap()[0].id, "legacy");
        assert_eq!(migrated.get_epoch_state().unwrap().corpus_generation, 0);
    }

    // -- Config table ---------------------------------------------------------

    #[test]
    fn test_config_key_value_store() {
        let db = test_db();

        db.execute_write(
            "INSERT INTO config (key, value) VALUES ('vault_name', 'My Vault')",
            vec![],
        )
        .unwrap();

        let val: Vec<String> = db
            .query_read(
                "SELECT value FROM config WHERE key = 'vault_name'",
                vec![],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(val[0], "My Vault");

        db.execute_write(
            "INSERT INTO config (key, value) VALUES ('vault_name', 'Renamed Vault')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            vec![],
        )
        .unwrap();

        let val: Vec<String> = db
            .query_read(
                "SELECT value FROM config WHERE key = 'vault_name'",
                vec![],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(val[0], "Renamed Vault");
    }

    #[test]
    fn test_backend_change_resets_only_generated_vector_state() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.set_config("vault_name", "My Vault").unwrap();
        db.set_config("embedding.backend", "old:384").unwrap();
        db.execute_write(
            "INSERT INTO docs
                (source_ref, title, content, content_hash, created_at, modified_at, indexed_at,
                 embedded_at, embedded_content_hash)
             VALUES ('a.md', 'A', 'body', 'h1', ?1, ?1, ?1, ?1, 'h1')",
            vec![Value::Integer(now)],
        )
        .unwrap();
        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'a.md'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, 0, 'chunk a', 2, ?2)",
            vec![Value::Integer(doc_id), Value::Integer(now)],
        )
        .unwrap();
        db.execute_write(
            "INSERT INTO catalysts (id, text, entity) VALUES ('cat-1', 'topic', 'a')",
            vec![],
        )
        .unwrap();
        db.upsert_chunk_embeddings(&[("a.md".to_string(), 0, vec![0.1, 0.2])])
            .unwrap();
        db.upsert_catalyst_embeddings(&[("cat-1", &[0.3_f32, 0.4_f32])])
            .unwrap();
        db.execute_write(
            "INSERT INTO catalyst_similarities
                (catalyst_id, catalyst_text, file_path, chunk_index, similarity)
             VALUES ('cat-1', 'topic', 'a.md', 0, 0.9)",
            vec![],
        )
        .unwrap();

        let changed = db
            .reset_generated_vector_state_if_backend_changed("embedding.backend", "ese:384")
            .unwrap();

        assert!(changed);
        assert_eq!(
            db.get_config("embedding.backend").unwrap().as_deref(),
            Some("ese:384")
        );
        assert_eq!(
            db.get_config("vault_name").unwrap().as_deref(),
            Some("My Vault")
        );
        assert_eq!(db.get_document_count().unwrap(), 1);
        assert_eq!(
            db.query_read("SELECT COUNT(*) FROM chunks", vec![], |row| {
                row.get::<i64>(0)
            })
            .unwrap()[0],
            1
        );
        assert_eq!(
            db.query_read("SELECT COUNT(*) FROM catalysts", vec![], |row| {
                row.get::<i64>(0)
            })
            .unwrap()[0],
            1
        );
        let doc = db.get_document_by_path("a.md").unwrap().unwrap();
        assert_eq!(doc.embedded_at, None);
        assert_eq!(doc.embedded_content_hash, None);
        assert!(db.get_all_chunk_embeddings().unwrap().is_empty());
        assert!(db.get_all_catalyst_embeddings().unwrap().is_empty());
        assert_eq!(
            db.query_read(
                "SELECT COUNT(*) FROM catalyst_similarities",
                vec![],
                |row| { row.get::<i64>(0) }
            )
            .unwrap()[0],
            0
        );
    }

    #[test]
    fn test_backend_guard_is_noop_when_metadata_matches() {
        let db = test_db();

        db.set_config("embedding.backend", "ese:384").unwrap();
        db.upsert_chunk_embeddings(&[("a.md".to_string(), 0, vec![0.1, 0.2])])
            .unwrap();

        let changed = db
            .reset_generated_vector_state_if_backend_changed("embedding.backend", "ese:384")
            .unwrap();

        assert!(!changed);
        assert_eq!(db.get_all_chunk_embeddings().unwrap().len(), 1);
    }

    // -- Entities + occurrences -----------------------------------------------

    #[test]
    fn test_entities_and_occurrences() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)",
            vec![
                Value::Text("rust".into()),
                Value::Text("tag".into()),
                Value::Integer(5),
                Value::Integer(now),
                Value::Integer(now),
            ],
        )
        .unwrap();

        let entity_id: i64 = db
            .query_read(
                "SELECT id FROM entities WHERE name = 'rust' AND type = 'tag'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO entity_occurrences (entity_id, file_path, context, created_at, effective_date)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            vec![
                Value::Integer(entity_id),
                Value::Text("notes/rust.md".into()),
                Value::Text("discussing ownership model".into()),
                Value::Integer(now),
                Value::Integer(now),
            ],
        )
        .unwrap();

        let occurrences: Vec<(String, String)> = db
            .query_read(
                "SELECT eo.file_path, e.name
                 FROM entity_occurrences eo
                 JOIN entities e ON e.id = eo.entity_id
                 WHERE e.name = 'rust'",
                vec![],
                |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
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
            vec![
                Value::Text("cat-001".into()),
                Value::Text("What patterns exist in systems programming?".into()),
                Value::Text("rust".into()),
                Value::Text("default".into()),
            ],
        )
        .unwrap();

        db.execute_write(
            "INSERT INTO catalyst_similarities (catalyst_id, catalyst_text, file_path, chunk_index, similarity)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            vec![
                Value::Text("cat-001".into()),
                Value::Text("What patterns exist in systems programming?".into()),
                Value::Text("notes/rust.md".into()),
                Value::Integer(0),
                Value::Real(0.87),
            ],
        )
        .unwrap();

        let sims: Vec<(String, f64)> = db
            .query_read(
                "SELECT file_path, similarity FROM catalyst_similarities WHERE catalyst_id = 'cat-001'",
                vec![],
                |row| Ok((row.get::<String>(0)?, row.get::<f64>(1)?)),
            )
            .unwrap();

        assert_eq!(sims.len(), 1);
        assert_eq!(sims[0].0, "notes/rust.md");
        assert!((sims[0].1 - 0.87).abs() < 1e-6);
    }

    #[test]
    fn test_delete_catalysts_for_entity_is_type_keyed() {
        let db = test_db();

        for (id, entity, metadata) in [
            ("cat-tag", "projects", r#"{"entity_type":"tag"}"#),
            ("cat-folder", "projects", r#"{"entity_type":"folder"}"#),
            ("cat-legacy", "projects", "{}"),
            ("cat-other", "daily", r#"{"entity_type":"tag"}"#),
        ] {
            db.execute_write(
                "INSERT INTO catalysts (id, text, entity, metadata) VALUES (?1, 'q', ?2, ?3)",
                vec![
                    Value::Text(id.into()),
                    Value::Text(entity.into()),
                    Value::Text(metadata.into()),
                ],
            )
            .unwrap();
        }

        db.delete_catalysts_for_entity("projects", "tag").unwrap();

        let remaining: Vec<String> = db
            .query_read(
                "SELECT id FROM catalysts ORDER BY id",
                vec![],
                |row| row.get::<String>(0),
            )
            .unwrap();
        // The same-named folder entity's catalysts survive; the tag's own and
        // the untyped legacy row are replaced; unrelated entities untouched.
        assert_eq!(remaining, vec!["cat-folder", "cat-other"]);
    }

    // -- Unique constraints ---------------------------------------------------

    #[test]
    fn test_unique_source_ref_constraint() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('dup.md', 'h', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();

        let result = db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('dup.md', 'h2', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        );

        assert!(result.is_err(), "duplicate source_ref should be rejected");
    }

    #[test]
    fn test_unique_doc_tag_pair() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at) VALUES ('u.md', 'h', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'u.md'",
                vec![],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'dup')",
            vec![Value::Integer(doc_id)],
        )
        .unwrap();

        let result = db.execute_write(
            "INSERT INTO doc_tags (doc_id, tag) VALUES (?1, 'dup')",
            vec![Value::Integer(doc_id)],
        );
        assert!(
            result.is_err(),
            "duplicate (doc_id, tag) should be rejected"
        );
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
                vec![Value::Integer(ids[0])],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["rust", "test"]);

        let links: Vec<String> = db
            .query_read(
                "SELECT link FROM doc_links WHERE doc_id = ?1",
                vec![Value::Integer(ids[0])],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(links, vec!["other-note"]);

        let chunks: Vec<String> = db
            .query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                vec![Value::Integer(ids[0])],
                |row| row.get::<String>(0),
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

        db.upsert_documents_batch(&[make_doc(
            "v1",
            vec!["old-tag".into()],
            vec!["old chunk".into()],
        )])
        .unwrap();

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
                vec![Value::Integer(ids[0])],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(tags, vec!["new-tag"]);

        let chunks: Vec<String> = db
            .query_read(
                "SELECT content FROM chunks WHERE doc_id = ?1 ORDER BY chunk_index",
                vec![Value::Integer(ids[0])],
                |row| row.get::<String>(0),
            )
            .unwrap();
        assert_eq!(chunks, vec!["new chunk a", "new chunk b"]);

        assert_eq!(db.get_document_count().unwrap(), 1);
    }

    // -- get_trending_entities ------------------------------------------------

    #[test]
    fn test_get_trending_entities_type_diversity() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

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
                 VALUES (?1, ?2, 3, ?3, ?3, ?4)",
                vec![
                    Value::Text(name.to_string()),
                    Value::Text(etype.to_string()),
                    Value::Integer(now),
                    Value::Text(trends.to_string()),
                ],
            )
            .unwrap();
        }

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

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('a.md', 'h1', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();
        let id1: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'a.md'",
                vec![],
                |r| r.get::<i64>(0),
            )
            .unwrap()[0];
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, 0, 'chunk a', 2, ?2)",
            vec![Value::Integer(id1), Value::Integer(now)],
        )
        .unwrap();

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, embedded_content_hash, embedded_at, created_at, modified_at, indexed_at)
             VALUES ('b.md', 'h2', 'h2', ?1, ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, embedded_content_hash, embedded_at, created_at, modified_at, indexed_at)
             VALUES ('c.md', 'h3-new', 'h3-old', ?1, ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();
        let id3: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'c.md'",
                vec![],
                |r| r.get::<i64>(0),
            )
            .unwrap()[0];
        db.execute_write(
            "INSERT INTO chunks (doc_id, chunk_index, content, word_count, created_at)
             VALUES (?1, 0, 'chunk c', 2, ?2)",
            vec![Value::Integer(id3), Value::Integer(now)],
        )
        .unwrap();

        let needing = db.get_documents_needing_embedding(None, None).unwrap();
        let refs: Vec<&str> = needing.iter().map(|doc| doc.source_ref.as_str()).collect();
        assert!(refs.contains(&"a.md"));
        assert!(refs.contains(&"c.md"));
        assert!(!refs.contains(&"b.md"));
        assert!(needing.iter().any(|doc| doc.content_hash == "h1"));
        assert!(needing.iter().any(|doc| doc.content_hash == "h3-new"));

        let limited = db.get_documents_needing_embedding(Some(1), None).unwrap();
        assert_eq!(limited.len(), 1);

        let future = now + 100_000;
        let filtered = db
            .get_documents_needing_embedding(None, Some(future))
            .unwrap();
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_replace_chunk_embeddings_for_documents_bulk() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        db.execute_write(
            "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
             VALUES ('a.md', 'h1', ?1, ?1, ?1)",
            vec![Value::Integer(now)],
        )
        .unwrap();
        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = 'a.md'",
                vec![],
                |r| r.get::<i64>(0),
            )
            .unwrap()[0];

        db.upsert_chunk_embeddings(&[("a.md".to_string(), 0, vec![0.1, 0.2])])
            .unwrap();

        db.replace_chunk_embeddings_for_documents(
            vec!["a.md".to_string()],
            vec![
                ("a.md".to_string(), 0, vec![1.0, 2.0]),
                ("a.md".to_string(), 1, vec![3.0, 4.0]),
            ],
            vec![(doc_id, "h1".to_string(), now + 1)],
        )
        .unwrap();

        let embeddings = db.get_all_chunk_embeddings().unwrap();
        assert_eq!(embeddings.len(), 2);
        // Vectors round-trip through int8 quantized storage, so compare
        // approximately rather than bit-for-bit (see the `quantize` module's
        // roundtrip error-bound tests for the precise guarantee).
        assert_eq!(embeddings[0].0, "a.md");
        assert_eq!(embeddings[0].1, 0);
        assert_vectors_approx_eq(&embeddings[0].2, &[1.0, 2.0]);
        assert_eq!(embeddings[1].0, "a.md");
        assert_eq!(embeddings[1].1, 1);
        assert_vectors_approx_eq(&embeddings[1].2, &[3.0, 4.0]);

        let doc = db.get_document_by_path("a.md").unwrap().unwrap();
        assert_eq!(doc.embedded_content_hash.as_deref(), Some("h1"));
        assert_eq!(doc.embedded_at, Some(now + 1));
    }

    // -- get_folder_children_by_references ------------------------------------

    #[test]
    fn test_get_folder_children_by_references() {
        let db = test_db();
        let now = 1_700_000_000_000i64;

        for name in &["people/Alice.md", "people/Bob.md"] {
            db.execute_write(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                 VALUES (?1, 'h', ?2, ?2, ?2)",
                vec![Value::Text(name.to_string()), Value::Integer(now)],
            )
            .unwrap();
        }

        for (i, source) in ["notes/journal.md", "notes/meeting.md"].iter().enumerate() {
            db.execute_write(
                "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                 VALUES (?1, ?2, ?3, ?3, ?3)",
                vec![
                    Value::Text(source.to_string()),
                    Value::Text(format!("h{}", i + 10)),
                    Value::Integer(now),
                ],
            )
            .unwrap();
            let doc_id: i64 = db
                .query_read(
                    "SELECT id FROM docs WHERE source_ref = ?1",
                    vec![Value::Text(source.to_string())],
                    |r| r.get::<i64>(0),
                )
                .unwrap()[0];
            db.execute_write(
                "INSERT INTO doc_links (doc_id, link) VALUES (?1, 'alice')",
                vec![Value::Integer(doc_id)],
            )
            .unwrap();
        }

        let (children, total) = db.get_folder_children_by_references("people", 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(children[0].name, "Alice");
        assert_eq!(children[0].reference_count, 2);
        assert_eq!(children[1].name, "Bob");
        assert_eq!(children[1].reference_count, 0);
    }
}
