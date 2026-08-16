use std::collections::HashMap;
use std::path::PathBuf;

use enzyme_core::config::{
    SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit,
};
use enzyme_core::db::Database;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::sqlite_source::{run_configured_sqlite_sources, run_sqlite_source};
use libsql::Value;
use rusqlite::{Connection, params};

fn source_config(path: PathBuf) -> SqliteSourceConfig {
    SqliteSourceConfig {
        db: path,
        query: "SELECT part AS id_part, seq AS id_seq, sender AS who, ts AS when_ts, \
                subject AS subject, body AS body, thread AS where_label FROM entries"
            .to_string(),
        roles: SqliteSourceRoles {
            id: vec!["id_part".to_string(), "id_seq".to_string()],
            who: vec!["who".to_string()],
            when: "when_ts".to_string(),
            what: vec!["subject".to_string(), "body".to_string()],
            r#where: vec!["where_label".to_string()],
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
    }
}

#[test]
fn sqlite_source_indexes_occurrences_and_dates_incrementally_alongside_markdown() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("workspace");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("note.md"),
        "# Filesystem note\n\nA Markdown-owned #filesystem document.",
    )
    .unwrap();

    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("source.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE entries (
                part INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                sender TEXT,
                ts INTEGER NOT NULL,
                subject TEXT,
                body TEXT,
                thread TEXT,
                PRIMARY KEY (part, seq)
             );",
        )
        .unwrap();
    source_db
        .execute(
            "INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                1_i64,
                1_i64,
                "Alice",
                1_700_000_000_123_i64,
                "Planning",
                "Meet tomorrow",
                "Project Alpha"
            ],
        )
        .unwrap();
    source_db
        .execute(
            "INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                1_i64,
                2_i64,
                Option::<String>::None,
                1_700_000_001_456_i64,
                Option::<String>::None,
                "Follow-up without optional roles",
                Option::<String>::None
            ],
        )
        .unwrap();

    let sources = HashMap::from([("messages".to_string(), source_config(source_path))]);
    let config = sources.get("messages").unwrap();
    let first = run_configured_sqlite_sources(&enzyme_db, &sources).unwrap();
    assert_eq!(first.sources_queried, 1);
    assert_eq!(first.rows_indexed, 2);

    // Markdown reconciliation owns only .md refs and must preserve source docs.
    let markdown = IndexingPipeline::new(&enzyme_db, vault.clone());
    let markdown_stats = markdown.run(true, |_| {}).unwrap();
    assert_eq!(markdown_stats.files_new, 1);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 3);

    let source_docs = enzyme_db
        .query_read(
            "SELECT source_ref, content, created_at FROM docs
             WHERE source_ref LIKE 'sqlite:messages/%' ORDER BY created_at",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(source_docs.len(), 2);
    assert!(source_docs[0].0.starts_with("sqlite:messages/"));
    assert!(source_docs[0].1.contains("Planning\n\nMeet tomorrow"));
    assert!(source_docs[0].1.contains("Source: messages"));
    assert_eq!(source_docs[0].2, 1_700_000_000_123);
    assert_eq!(source_docs[1].2, 1_700_000_001_456);

    let alice_links = enzyme_db
        .query_read(
            "SELECT COUNT(*) FROM doc_links WHERE link = 'Alice'",
            vec![],
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(alice_links, vec![1]);

    let source_occurrences = enzyme_db
        .query_read(
            "SELECT e.name, e.type, eo.effective_date, eo.date_source
             FROM entity_occurrences eo
             JOIN entities e ON e.id = eo.entity_id
             WHERE eo.file_path LIKE 'sqlite:messages/%'
             ORDER BY e.type, e.name",
            vec![],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<Option<i64>>(2)?,
                    row.get::<String>(3)?,
                ))
            },
        )
        .unwrap();
    assert!(source_occurrences.iter().any(|row| {
        row.0 == "Alice"
            && row.1 == "link"
            && row.2 == Some(1_700_000_000_123)
            && row.3 == "content"
    }));
    assert!(source_occurrences.iter().any(|row| {
        row.0 == "Project Alpha" && row.1 == "folder" && row.2 == Some(1_700_000_000_123)
    }));

    // The typed composite cursor makes an identical run a true no-op.
    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", config).unwrap(),
        0
    );
    assert_eq!(enzyme_db.get_document_count().unwrap(), 3);
    let cursor_count = enzyme_db
        .query_read(
            "SELECT COUNT(*) FROM source_cursors
             WHERE source_name = 'messages' AND cursor_kind = 'sqlite-composite-v1'",
            vec![],
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(cursor_count, vec![1]);

    source_db
        .execute(
            "INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                2_i64,
                1_i64,
                "Carol",
                1_700_000_002_789_i64,
                "New tuple",
                "Picked up incrementally",
                "Project Beta"
            ],
        )
        .unwrap();
    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", config).unwrap(),
        1
    );
    assert_eq!(enzyme_db.get_document_count().unwrap(), 4);

    // Removing the Markdown file deletes only the Markdown-owned document.
    std::fs::remove_file(vault.join("note.md")).unwrap();
    let refresh = IndexingPipeline::new(&enzyme_db, vault);
    let refresh_stats = refresh.run(false, |_| {}).unwrap();
    assert_eq!(refresh_stats.files_deleted, 1);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 3);
    let remaining = enzyme_db
        .query_read(
            "SELECT COUNT(*) FROM docs WHERE source_ref LIKE 'sqlite:messages/%'",
            Vec::<Value>::new(),
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(remaining, vec![3]);
}

#[test]
fn sqlite_source_populates_a_workspace_without_markdown_and_maps_role_arrays_independently() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("standalone");
    std::fs::create_dir_all(&vault).unwrap();

    let enzyme_db = Database::open_in_memory().unwrap();
    let source_path = temp.path().join("standalone.db");
    let source_db = Connection::open(&source_path).unwrap();
    source_db
        .execute_batch(
            "CREATE TABLE entries (
                part INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                sender TEXT,
                mention TEXT,
                ts INTEGER NOT NULL,
                subject TEXT,
                body TEXT,
                thread TEXT,
                mailbox TEXT,
                PRIMARY KEY (part, seq)
             );",
        )
        .unwrap();
    source_db
        .execute(
            "INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                7_i64,
                3_i64,
                "Alice",
                "Bob",
                1_700_000_003_000_i64,
                "Subject",
                "Body",
                "Thread A",
                "Inbox"
            ],
        )
        .unwrap();

    let mut config = source_config(source_path);
    config.query = "SELECT part AS id_part, seq AS id_seq, sender, mention, ts AS when_ts, \
                    subject, body, thread, mailbox FROM entries"
        .to_string();
    config.roles.who = vec!["sender".to_string(), "mention".to_string()];
    config.roles.r#where = vec!["thread".to_string(), "mailbox".to_string()];

    assert_eq!(
        run_sqlite_source(&enzyme_db, "messages", &config).unwrap(),
        1
    );
    let markdown_stats = IndexingPipeline::new(&enzyme_db, vault)
        .run(true, |_| {})
        .unwrap();
    assert_eq!(markdown_stats.files_discovered, 0);
    assert_eq!(enzyme_db.get_document_count().unwrap(), 1);

    let links = enzyme_db
        .query_read(
            "SELECT link FROM doc_links ORDER BY link",
            Vec::<Value>::new(),
            |row| row.get::<String>(0),
        )
        .unwrap();
    assert_eq!(links, vec!["Alice", "Bob"]);
    let folders = enzyme_db
        .query_read(
            "SELECT e.name FROM entities e
             JOIN entity_occurrences eo ON eo.entity_id = e.id
             WHERE e.type = 'folder' ORDER BY e.name",
            Vec::<Value>::new(),
            |row| row.get::<String>(0),
        )
        .unwrap();
    assert_eq!(folders, vec!["Inbox", "Thread A"]);

    let occurrence_count = enzyme_db
        .query_read(
            "SELECT COUNT(*) FROM entity_occurrences",
            Vec::<Value>::new(),
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(occurrence_count, vec![4]);
}
