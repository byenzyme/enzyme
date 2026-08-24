use std::collections::HashMap;
use std::path::PathBuf;

use enzyme_core::config::{
    SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit,
};
use enzyme_core::db::Database;
use enzyme_core::initialize_sqlite_runtime;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::sqlite_source::{run_configured_sqlite_sources, run_sqlite_source};
use libsql::Value;
use rusqlite::{Connection, params};

const JAN_2020: i64 = 1_579_046_400_000;
const JAN_2023: i64 = 1_673_740_800_000;
const JAN_2024: i64 = 1_705_276_800_000;
const FEB_2024: i64 = 1_707_955_200_000;

fn source_config(path: PathBuf) -> SqliteSourceConfig {
    SqliteSourceConfig {
        db: path,
        query: "SELECT id, sender AS who, ts AS when_ts, body, thread AS where_label \
                FROM entries"
            .to_string(),
        roles: SqliteSourceRoles {
            id: vec!["id".to_string()],
            who: vec!["who".to_string()],
            when: "when_ts".to_string(),
            what: vec!["body".to_string()],
            r#where: vec!["where_label".to_string()],
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
    }
}

fn create_entries(db: &Connection) {
    db.execute_batch(
        "CREATE TABLE entries (
            id INTEGER PRIMARY KEY,
            sender TEXT,
            ts INTEGER NOT NULL,
            body TEXT NOT NULL,
            thread TEXT
         );",
    )
    .unwrap();
}

fn insert_entry(
    db: &Connection,
    id: i64,
    sender: Option<&str>,
    ts: i64,
    body: &str,
    thread: Option<&str>,
) {
    db.execute(
        "INSERT INTO entries (id, sender, ts, body, thread) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, sender, ts, body, thread],
    )
    .unwrap();
}

fn mark_all_embedded(db: &Database) {
    db.execute_write(
        "UPDATE docs SET embedded_at = 1, embedded_content_hash = content_hash",
        Vec::<Value>::new(),
    )
    .unwrap();
}

#[test]
fn log_bucket_fingerprints_rebuild_skip_and_prune_exactly() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(
        &source_db,
        1,
        Some("Alice"),
        JAN_2024,
        "January A",
        Some("Thread A"),
    );
    insert_entry(
        &source_db,
        2,
        Some("Bob"),
        FEB_2024,
        "February A",
        Some("Thread A"),
    );
    insert_entry(
        &source_db,
        3,
        Some("Carol"),
        JAN_2024 + 1_000,
        "January B",
        Some("Thread B"),
    );

    let config = source_config(source_path);
    let first = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(first.rows_seen, 3);
    assert_eq!(first.buckets_seen, 3);
    assert_eq!(first.documents_rebuilt, 3);
    assert_eq!(first.documents_skipped, 0);
    assert_eq!(first.documents_pruned, 0);

    mark_all_embedded(&enzyme_db);
    let unchanged = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(unchanged.documents_rebuilt, 0);
    assert_eq!(unchanged.documents_skipped, 3);
    assert_eq!(unchanged.documents_pruned, 0);
    assert_eq!(
        enzyme_db.count_documents_needing_embedding(None).unwrap(),
        0,
        "fingerprint skips must preserve current embeddings"
    );

    // A new high id with a backdated timestamp changes only historical Thread A / January.
    insert_entry(
        &source_db,
        100,
        Some("Alice"),
        JAN_2024 + 2_000,
        "Backdated January A",
        Some("Thread A"),
    );
    let backdated = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(backdated.documents_rebuilt, 1);
    assert_eq!(backdated.documents_skipped, 2);
    assert_eq!(backdated.documents_pruned, 0);

    mark_all_embedded(&enzyme_db);
    source_db
        .execute(
            "UPDATE entries SET body = 'Edited February A' WHERE id = 2",
            [],
        )
        .unwrap();
    let edited = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(edited.documents_rebuilt, 1);
    assert_eq!(edited.documents_skipped, 2);
    assert_eq!(edited.documents_pruned, 0);

    mark_all_embedded(&enzyme_db);
    source_db
        .execute("DELETE FROM entries WHERE id = 100", [])
        .unwrap();
    let deleted_row = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(deleted_row.documents_rebuilt, 1);
    assert_eq!(deleted_row.documents_skipped, 2);
    assert_eq!(deleted_row.documents_pruned, 0);

    mark_all_embedded(&enzyme_db);
    source_db
        .execute("DELETE FROM entries WHERE id = 2", [])
        .unwrap();
    let deleted_bucket = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(deleted_bucket.documents_rebuilt, 0);
    assert_eq!(deleted_bucket.documents_skipped, 2);
    assert_eq!(deleted_bucket.documents_pruned, 1);

    // Simulate a restore that assigned different row ids to identical content.
    source_db
        .execute("UPDATE entries SET id = id + 1000", [])
        .unwrap();
    mark_all_embedded(&enzyme_db);
    let restored = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(restored.documents_rebuilt, 0);
    assert_eq!(restored.documents_skipped, 2);
    assert_eq!(restored.documents_pruned, 0);
    assert_eq!(
        enzyme_db.count_documents_needing_embedding(None).unwrap(),
        0
    );
}

#[test]
fn log_bucket_documents_are_dated_logs_and_occurrences_keep_message_dates() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let vault = temp.path().join("workspace");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("note.md"), "# Markdown\n\nStill indexed.").unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(
        &source_db,
        1,
        Some("Alice"),
        JAN_2020,
        "Met Will",
        Some("Friends"),
    );
    insert_entry(
        &source_db,
        2,
        Some("Alice"),
        JAN_2023,
        "Saw Will again",
        Some("Friends"),
    );

    let sources = HashMap::from([("messages".to_string(), source_config(source_path))]);
    let stats = run_configured_sqlite_sources(&enzyme_db, &sources).unwrap();
    assert_eq!(stats.sources_queried, 1);
    assert_eq!(stats.rows_seen, 2);
    assert_eq!(stats.buckets_seen, 2);

    let markdown = IndexingPipeline::new(&enzyme_db, vault);
    assert_eq!(markdown.run(true, |_| {}).unwrap().files_new, 1);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 3);

    let source_docs = enzyme_db
        .query_read(
            "SELECT source_ref, content, content_hash FROM docs
             WHERE source_ref LIKE 'sqlite:messages/%' ORDER BY source_ref",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(source_docs.len(), 2);
    assert!(
        source_docs
            .iter()
            .all(|doc| doc.0.matches('/').count() == 2)
    );
    assert!(source_docs.iter().all(|doc| doc.1.contains("Alice")));
    assert!(source_docs.iter().all(|doc| doc.2.len() == 64));

    let chunk_dates = enzyme_db
        .query_read(
            "SELECT c.entry_date FROM chunks c JOIN docs d ON d.id = c.doc_id
             WHERE d.source_ref LIKE 'sqlite:messages/%' ORDER BY c.entry_date",
            vec![],
            |row| row.get::<Option<i64>>(0),
        )
        .unwrap();
    assert_eq!(chunk_dates, vec![Some(JAN_2020), Some(JAN_2023)]);

    let alice_dates = enzyme_db
        .query_read(
            "SELECT eo.effective_date FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id
             WHERE e.name = 'Alice' AND e.type = 'link'
             ORDER BY eo.effective_date",
            vec![],
            |row| row.get::<Option<i64>>(0),
        )
        .unwrap();
    assert_eq!(alice_dates, vec![Some(JAN_2020), Some(JAN_2023)]);
    let alice = enzyme_db
        .get_all_entities()
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "Alice")
        .unwrap();
    assert_eq!(alice.first_seen, Some(JAN_2020));
    assert_eq!(alice.last_seen, Some(JAN_2023));
}

#[test]
fn null_where_rows_share_the_monthly_fallback_bucket() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(&source_db, 1, Some("A"), JAN_2024, "one", None);
    insert_entry(&source_db, 2, Some("B"), JAN_2024 + 1_000, "two", None);

    let stats = run_sqlite_source(&enzyme_db, "messages", &source_config(source_path)).unwrap();
    assert_eq!(stats.rows_seen, 2);
    assert_eq!(stats.buckets_seen, 1);
    assert_eq!(stats.documents_rebuilt, 1);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 1);
    let folders = enzyme_db
        .query_read("SELECT COUNT(*) FROM doc_links", vec![], |row| {
            row.get::<i64>(0)
        })
        .unwrap();
    assert_eq!(folders, vec![2]);
    let folder_occurrences = enzyme_db
        .query_read(
            "SELECT COUNT(*) FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id WHERE e.type = 'folder'",
            vec![],
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(folder_occurrences, vec![0]);
}

#[test]
fn source_without_where_keeps_one_document_per_row_with_full_scan_staleness() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(&source_db, 1, Some("A"), JAN_2024, "one", None);
    insert_entry(&source_db, 2, Some("B"), JAN_2024 + 1_000, "two", None);
    let mut config = source_config(source_path);
    config.roles.r#where.clear();

    let first = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(first.documents_rebuilt, 2);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 2);
    let unchanged = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(unchanged.documents_rebuilt, 0);
    assert_eq!(unchanged.documents_skipped, 2);

    source_db
        .execute("DELETE FROM entries WHERE id = 2", [])
        .unwrap();
    let deleted = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(deleted.documents_skipped, 1);
    assert_eq!(deleted.documents_pruned, 1);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 1);
}

#[test]
fn empty_what_rows_are_skipped_not_fatal() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    // Two content-bearing rows and one content-free row (empty body, as with
    // attachment-only / reaction messages). The empty row must be skipped and
    // counted, never abort the whole index.
    insert_entry(&source_db, 1, Some("Alice"), JAN_2024, "hello", Some("T"));
    insert_entry(&source_db, 2, Some("Bob"), JAN_2024 + 1_000, "", Some("T"));
    insert_entry(&source_db, 3, Some("Alice"), JAN_2024 + 2_000, "world", Some("T"));

    let config = source_config(source_path);
    let stats = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(stats.rows_skipped_empty, 1);
    assert_eq!(stats.rows_seen, 2);
    assert_eq!(stats.documents_rebuilt, 1);
    assert_eq!(stats.documents_pruned, 0);
}

#[test]
fn row_mode_detects_when_edit_not_just_body() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(&source_db, 1, Some("A"), JAN_2024, "one", None);
    insert_entry(&source_db, 2, Some("B"), JAN_2024 + 1_000, "two", None);
    let mut config = source_config(source_path);
    config.roles.r#where.clear();

    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", &config)
            .unwrap()
            .documents_rebuilt,
        2
    );

    // Change ONLY the timestamp (`when`) of row 1; body is untouched. The
    // rendered content_hash misses `when`, so without binding it into the
    // reconciliation hash this edit is silently skipped and the stored date rots.
    source_db
        .execute("UPDATE entries SET ts = ?1 WHERE id = 1", params![JAN_2023])
        .unwrap();
    let edited = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(edited.documents_rebuilt, 1);
    assert_eq!(edited.documents_skipped, 1);
}

#[test]
fn row_mode_detects_non_leading_who_edit() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    create_entries(&source_db);
    insert_entry(&source_db, 1, Some("A"), JAN_2024, "one", Some("second-who"));
    insert_entry(&source_db, 2, Some("B"), JAN_2024 + 1_000, "two", Some("x"));
    let mut config = source_config(source_path);
    config.roles.r#where.clear();
    // Two who columns; the leading who (sender) stays in the title, the second
    // who (thread column) is non-leading and only reachable via the who list.
    config.roles.who = vec!["who".to_string(), "where_label".to_string()];

    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", &config)
            .unwrap()
            .documents_rebuilt,
        2
    );

    // Edit the non-leading who of row 1 only; title/body unchanged.
    source_db
        .execute(
            "UPDATE entries SET thread = 'renamed' WHERE id = 1",
            [],
        )
        .unwrap();
    let edited = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(edited.documents_rebuilt, 1);
    assert_eq!(edited.documents_skipped, 1);
}
