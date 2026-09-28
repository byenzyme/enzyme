use std::collections::HashMap;
use std::path::PathBuf;

use enzyme_core::config::{
    OneOrManyStrings, SqliteMultiValueWhoRole, SqliteSourceConfig, SqliteSourceFilter,
    SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit, SqliteWhoRole,
};
use enzyme_core::db::Database;
use enzyme_core::initialize_sqlite_runtime;
use enzyme_core::models::EntityType;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::sqlite_source::{
    run_configured_sqlite_sources, run_sqlite_source, source_refresh_staleness,
};
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
            document_ref: None,
            who: SqliteWhoRole::Columns(OneOrManyStrings::Many(vec!["who".to_string()])),
            when: "when_ts".to_string(),
            what: vec!["body".to_string()],
            r#where: vec!["where_label".to_string()],
            weight: None,
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
        filter: None,
    }
}

fn supplied_document_ref_config(path: PathBuf) -> SqliteSourceConfig {
    SqliteSourceConfig {
        db: path,
        query: "SELECT id, document_ref, sender AS who, ts AS when_ts, body FROM evidence"
            .to_string(),
        roles: SqliteSourceRoles {
            id: vec!["id".to_string()],
            document_ref: Some("document_ref".to_string()),
            who: SqliteWhoRole::Columns(OneOrManyStrings::Many(vec!["who".to_string()])),
            when: "when_ts".to_string(),
            what: vec!["body".to_string()],
            r#where: Vec::new(),
            weight: None,
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
        filter: None,
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

#[test]
fn supplied_document_refs_round_trip_into_documents_and_occurrences() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("evidence.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE evidence (
                id INTEGER PRIMARY KEY,
                document_ref TEXT NOT NULL,
                sender TEXT,
                ts INTEGER NOT NULL,
                body TEXT NOT NULL
             );",
        )
        .unwrap();
    for (id, document_ref, sender, body) in [
        (
            1,
            "sqlite:mail-stable/thread/account-a/thread-9",
            "Alice",
            "First evidence",
        ),
        (
            2,
            "sqlite:mail-stable/thread/account-a/thread-10",
            "Bob",
            "Second evidence",
        ),
    ] {
        source_db
            .execute(
                "INSERT INTO evidence VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, document_ref, sender, JAN_2024 + id * 1_000, body],
            )
            .unwrap();
    }

    let enzyme_db = Database::open_in_memory().unwrap();
    let config = supplied_document_ref_config(source_path);
    let first = run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap();
    assert_eq!(first.documents_rebuilt, 2);
    let refs = enzyme_db.get_all_source_refs().unwrap();
    assert_eq!(
        refs,
        vec![
            "sqlite:mail-stable/thread/account-a/thread-10".to_string(),
            "sqlite:mail-stable/thread/account-a/thread-9".to_string(),
        ]
    );
    let occurrence_refs = enzyme_db
        .query_read(
            "SELECT DISTINCT file_path FROM entity_occurrences ORDER BY file_path",
            vec![],
            |row| row.get::<String>(0),
        )
        .unwrap();
    assert_eq!(occurrence_refs, refs);

    let unchanged = run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap();
    assert_eq!(unchanged.documents_rebuilt, 0);
    assert_eq!(unchanged.documents_skipped, 2);

    source_db
        .execute("DELETE FROM evidence WHERE id = 1", [])
        .unwrap();
    let deleted = run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap();
    assert_eq!(deleted.documents_pruned, 1);
    assert_eq!(
        enzyme_db.get_all_source_refs().unwrap(),
        vec!["sqlite:mail-stable/thread/account-a/thread-10".to_string()]
    );
}

#[test]
fn invalid_or_duplicate_supplied_document_refs_fail_before_index_mutation() {
    initialize_sqlite_runtime().unwrap();
    for (label, sql) in [
        (
            "empty",
            "INSERT INTO evidence VALUES (1, '', 'Alice', 1705276800000, 'body')",
        ),
        (
            "null",
            "INSERT INTO evidence VALUES (1, NULL, 'Alice', 1705276800000, 'body')",
        ),
        (
            "numeric",
            "INSERT INTO evidence VALUES (1, 42, 'Alice', 1705276800000, 'body')",
        ),
        (
            "foreign",
            "INSERT INTO evidence VALUES (1, 'sqlite:other/thread-1', 'Alice', 1705276800000, 'body')",
        ),
        (
            "duplicate",
            "INSERT INTO evidence VALUES
                (1, 'sqlite:mail-stable/thread-1', 'Alice', 1705276800000, 'one'),
                (2, 'sqlite:mail-stable/thread-1', 'Bob', 1705276801000, 'two')",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join(format!("{label}.db"));
        let source_db = Connection::open(&source_path).unwrap();
        source_db
            .execute_batch(
                "CREATE TABLE evidence (
                    id INTEGER PRIMARY KEY,
                    document_ref,
                    sender TEXT,
                    ts INTEGER NOT NULL,
                    body TEXT NOT NULL
                 );",
            )
            .unwrap();
        source_db.execute_batch(sql).unwrap();
        let enzyme_db = Database::open_in_memory().unwrap();
        let error = run_sqlite_source(
            &enzyme_db,
            "mail-stable",
            &supplied_document_ref_config(source_path),
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty(), "invalid case {label}");
        assert_eq!(enzyme_db.get_document_count().unwrap(), 0, "case {label}");
    }
}

#[test]
fn invalid_supplied_ref_on_empty_content_preserves_coherent_snapshot() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("invalid-empty.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE evidence (
                id INTEGER PRIMARY KEY,
                document_ref TEXT,
                sender TEXT,
                ts INTEGER NOT NULL,
                body TEXT NOT NULL
             );
             INSERT INTO evidence VALUES
                (1, 'sqlite:mail-stable/thread-1', 'Alice', 1705276800000, 'coherent');",
        )
        .unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let config = supplied_document_ref_config(source_path);
    run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap();

    source_db
        .execute(
            "UPDATE evidence SET document_ref = 'sqlite:other/thread-1', body = '' WHERE id = 1",
            [],
        )
        .unwrap();
    let error = run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap_err();
    assert!(error.to_string().contains("must stay inside namespace"));
    assert_eq!(
        enzyme_db.get_all_source_refs().unwrap(),
        vec!["sqlite:mail-stable/thread-1".to_string()]
    );
}

#[test]
fn supplied_document_ref_is_rejected_for_bucket_mode() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("bucket.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE evidence (
                id INTEGER PRIMARY KEY,
                document_ref TEXT,
                sender TEXT,
                ts INTEGER NOT NULL,
                body TEXT NOT NULL,
                thread TEXT
             );",
        )
        .unwrap();
    let mut config = supplied_document_ref_config(source_path);
    config.query =
        "SELECT id, document_ref, sender AS who, ts AS when_ts, body, thread FROM evidence"
            .to_string();
    config.roles.r#where = vec!["thread".to_string()];
    let enzyme_db = Database::open_in_memory().unwrap();
    let error = run_sqlite_source(&enzyme_db, "mail-stable", &config).unwrap_err();
    assert!(error.to_string().contains("row-mode"));
    assert_eq!(enzyme_db.get_document_count().unwrap(), 0);
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
    let source_occurrences = enzyme_db.query_read(
        "SELECT COUNT(*) FROM entity_occurrences eo JOIN entities e ON e.id = eo.entity_id WHERE e.name = 'sqlite:messages' AND e.type = 'collection'",
        Vec::<Value>::new(),
        |row| row.get::<i64>(0),
    ).unwrap();
    assert_eq!(source_occurrences, vec![3]);
    let thread_occurrences = enzyme_db.query_read(
        "SELECT COUNT(*) FROM entity_occurrences eo JOIN entities e ON e.id = eo.entity_id WHERE e.name = 'sqlite:messages/thread/Thread A' AND e.type = 'collection'",
        Vec::<Value>::new(),
        |row| row.get::<i64>(0),
    ).unwrap();
    assert_eq!(thread_occurrences, vec![2]);

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
    assert!(source_docs.iter().all(|doc| !doc.1.contains(" — Alice")));
    assert!(source_docs.iter().any(|doc| doc.1.contains("Met Will")));
    assert!(
        source_docs
            .iter()
            .any(|doc| doc.1.contains("Saw Will again"))
    );
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
    insert_entry(
        &source_db,
        3,
        Some("Alice"),
        JAN_2024 + 2_000,
        "world",
        Some("T"),
    );

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
    insert_entry(
        &source_db,
        1,
        Some("A"),
        JAN_2024,
        "one",
        Some("second-who"),
    );
    insert_entry(&source_db, 2, Some("B"), JAN_2024 + 1_000, "two", Some("x"));
    let mut config = source_config(source_path);
    config.roles.r#where.clear();
    // Two who columns; the leading who (sender) stays in the title, the second
    // who (thread column) is non-leading and only reachable via the who list.
    config.roles.who = SqliteWhoRole::Columns(OneOrManyStrings::Many(vec![
        "who".to_string(),
        "where_label".to_string(),
    ]));

    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", &config)
            .unwrap()
            .documents_rebuilt,
        2
    );

    // Edit the non-leading who of row 1 only; title/body unchanged.
    source_db
        .execute("UPDATE entries SET thread = 'renamed' WHERE id = 1", [])
        .unwrap();
    let edited = run_sqlite_source(&enzyme_db, "messages", &config).unwrap();
    assert_eq!(edited.documents_rebuilt, 1);
    assert_eq!(edited.documents_skipped, 1);
}

#[test]
fn multi_value_who_and_weight_emit_additive_participant_occurrences() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("participants.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                participants TEXT NOT NULL,
                ts INTEGER NOT NULL,
                body TEXT NOT NULL,
                importance REAL
             );
             INSERT INTO messages VALUES
                (1, '[\"Alice\",\"Bob\",\"Alice\"]', 1705276800000, 'json row', 2.5);",
        )
        .unwrap();

    let mut config = source_config(source_path.clone());
    config.query = "SELECT id, participants, ts AS when_ts, body, importance FROM messages".into();
    config.roles.who = SqliteWhoRole::MultiValue(SqliteMultiValueWhoRole::JsonArray {
        column: "participants".into(),
    });
    config.roles.r#where.clear();
    config.roles.weight = Some("importance".into());
    run_sqlite_source(&enzyme_db, "json", &config).unwrap();

    for name in ["Alice", "Bob"] {
        let entity = enzyme_db
            .get_entities_by_names(&[(name, EntityType::Link)])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            entity.frequency, 1,
            "duplicates within one array are collapsed"
        );
        assert_eq!(entity.ranking_weight, 2.5);
        assert_eq!(
            enzyme_db
                .get_entity_occurrences(entity.id.unwrap())
                .unwrap()[0]
                .weight,
            2.5
        );
    }

    source_db
        .execute(
            "UPDATE messages SET participants = 'Carol | Dave', importance = NULL WHERE id = 1",
            [],
        )
        .unwrap();
    config.roles.who = SqliteWhoRole::MultiValue(SqliteMultiValueWhoRole::Delimited {
        column: "participants".into(),
        delimiter: "|".into(),
    });
    run_sqlite_source(&enzyme_db, "delimited", &config).unwrap();
    for name in ["Carol", "Dave"] {
        let entity = enzyme_db
            .get_entities_by_names(&[(name, EntityType::Link)])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            entity.ranking_weight, 1.0,
            "NULL weight uses the additive default"
        );
    }
}

#[test]
fn source_refresh_staleness_uses_engine_owned_success_watermark() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.db");
    Connection::open(&source_path).unwrap();
    let enzyme_db = Database::open_in_memory().unwrap();

    let never = source_refresh_staleness(&enzyme_db, "messages", &source_path).unwrap();
    assert!(never.stale);
    assert_eq!(never.reason.as_deref(), Some("never_refreshed"));

    enzyme_db
        .set_source_refresh_status("messages", 0, 0, 0)
        .unwrap();
    let changed = source_refresh_staleness(&enzyme_db, "messages", &source_path).unwrap();
    assert!(changed.stale);
    assert_eq!(
        changed.reason.as_deref(),
        Some("source_modified_after_refresh")
    );

    enzyme_db
        .set_source_refresh_status(
            "messages",
            0,
            0,
            chrono::Utc::now().timestamp_millis() + 1_000,
        )
        .unwrap();
    assert!(
        !source_refresh_staleness(&enzyme_db, "messages", &source_path)
            .unwrap()
            .stale
    );

    std::fs::remove_file(&source_path).unwrap();
    let unavailable = source_refresh_staleness(&enzyme_db, "messages", &source_path).unwrap();
    assert!(unavailable.stale);
    assert_eq!(unavailable.reason.as_deref(), Some("source_unavailable"));
}

#[test]
fn template_filter_excludes_mundane_rows_and_reconciles_changed_decisions() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("notes.db");
    let source = Connection::open(&source_path).unwrap();
    source.execute_batch(
        "CREATE TABLE entries (id INTEGER PRIMARY KEY, sender TEXT, ts INTEGER, body TEXT, thread TEXT);
         INSERT INTO entries VALUES
           (1, 'me', 1700000000000, 'Groceries\nMilk\nEggs\nBread\nCoffee\nBananas\nOlive oil', 'Notes'),
           (2, 'me', 1700000000000, 'I have been thinking about how conversations change when I listen longer. What surprised me was that the pause created space for a better question, and I want to try that again.', 'Notes');",
    ).unwrap();
    let db = Database::open_in_memory().unwrap();
    let mut config = source_config(source_path);
    config.roles.r#where.clear();
    config.filter = Some(SqliteSourceFilter::AppleNotesContentV1);

    let first = run_sqlite_source(&db, "notes", &config).unwrap();
    assert_eq!(first.rows_seen, 2);
    assert_eq!(first.rows_filtered, 1);
    assert_eq!(db.get_all_source_refs().unwrap().len(), 1);

    source
        .execute(
            "UPDATE entries SET body = ?1 WHERE id = 2",
            ["To do today\nCall dentist\nBuy milk\nReturn package\nSchedule oil change"],
        )
        .unwrap();
    let second = run_sqlite_source(&db, "notes", &config).unwrap();
    assert_eq!(second.rows_filtered, 2);
    assert_eq!(second.documents_pruned, 1);
    assert!(db.get_all_source_refs().unwrap().is_empty());
}
