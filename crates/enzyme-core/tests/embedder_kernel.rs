//! Black-box acceptance tests for the non-CLI embedder boundary.
//!
//! Keep this test restricted to public `enzyme_core` APIs. In particular, an
//! embedder must not need CLI path discovery, account state, or a Tokio runtime
//! merely to build and query an explicitly located index.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use enzyme_core::kernel::{Bridged, SearchOptions, ensure_searchable, ensure_searchable_sync};
use rusqlite::Connection;
use tempfile::TempDir;

fn sqlite_runtime_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("temp root");
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("ignored")).expect("vault folders");
    std::fs::write(
        vault.join("kept.md"),
        "Explicit library boundaries make embedded semantic search dependable.",
    )
    .expect("kept note");
    std::fs::write(
        vault.join("ignored/secret.md"),
        "This configured folder must not enter the explicit index.",
    )
    .expect("excluded note");

    let db_path = root.path().join("state/search.sqlite3");
    std::fs::create_dir_all(db_path.parent().unwrap()).expect("database parent");
    let config_path = root.path().join("settings/search.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("config parent");
    let vault_key = vault
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    std::fs::write(
        &config_path,
        format!("[vaults.\"{vault_key}\"]\nexcluded_folders = [\"ignored\"]\n"),
    )
    .expect("explicit config");

    (root, vault, db_path, config_path)
}

#[test]
fn sync_consumer_uses_explicit_paths_without_a_generator() {
    let _sqlite_runtime = sqlite_runtime_lock().lock().unwrap();
    let (_root, vault, db_path, config_path) = fixture();

    let mut options = SearchOptions::new(&vault, &db_path);
    options.config_path = Some(config_path);
    let index = ensure_searchable_sync(options, None).expect("offline provisioning");

    assert_eq!(
        index.document_count, 1,
        "explicit config must drive indexing"
    );
    assert_eq!(index.bridged, Bridged::NoGenerator);
    assert!(
        db_path.exists(),
        "the caller-selected database must be created"
    );
    assert!(
        !index
            .direct_search("embedded semantic search", 5)
            .expect("direct search")
            .is_empty()
    );
}

#[test]
fn async_consumer_works_on_a_current_thread_runtime() {
    let _sqlite_runtime = sqlite_runtime_lock().lock().unwrap();
    let (_root, vault, db_path, config_path) = fixture();
    let mut options = SearchOptions::new(vault, db_path);
    options.config_path = Some(config_path);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(async {
            let index = ensure_searchable(options, None)
                .await
                .expect("async provisioning");
            assert_eq!(index.document_count, 1);
        });
}

fn write_sqlite_kernel_config(
    config_path: &std::path::Path,
    vault: &std::path::Path,
    source_path: &std::path::Path,
    source_name: Option<&str>,
) {
    let vault_key = vault
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let source = source_name.map_or_else(String::new, |source_name| {
        format!(
            r#"
[vaults."{vault_key}".sources.{source_name}]
db = "{}"
query = "SELECT id, sent_at, body, thread FROM messages ORDER BY id"
roles = {{ id = "id", when = "sent_at", what = "body", where = "thread" }}
timestamp = {{ unit = "ms" }}
"#,
            source_path.display()
        )
    });
    std::fs::write(config_path, format!("[vaults.\"{vault_key}\"]\n{source}")).unwrap();
}

fn sqlite_document_count(db_path: &std::path::Path, namespace: &str) -> i64 {
    Connection::open(db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM docs WHERE source_ref LIKE ?1",
            [format!("sqlite:{namespace}/%")],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn production_kernel_runs_sources_before_embedding_and_reconciles_renames() {
    let _sqlite_runtime = sqlite_runtime_lock().lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("note.md"), "# Note\n\nMarkdown remains.").unwrap();
    let source_path = root.path().join("messages.db");
    Connection::open(&source_path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                sent_at INTEGER NOT NULL,
                body TEXT NOT NULL,
                thread TEXT NOT NULL
             );
             INSERT INTO messages VALUES
                (1, 1700000000000, 'kernel source row', 'Thread A');",
        )
        .unwrap();

    let db_path = root.path().join("state/search.sqlite3");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let config_path = root.path().join("config.toml");
    write_sqlite_kernel_config(&config_path, &vault, &source_path, Some("messages"));

    let mut options = SearchOptions::new(&vault, &db_path);
    options.config_path = Some(config_path.clone());
    let first = ensure_searchable_sync(options, None).unwrap();
    assert_eq!(first.document_count, 2);
    drop(first);
    assert_eq!(sqlite_document_count(&db_path, "messages"), 1);
    let embedded_source_chunks: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM chunk_embeddings WHERE file_path LIKE 'sqlite:messages/%'",
            (),
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        embedded_source_chunks > 0,
        "configured SQLite documents must reach embedding in the production kernel"
    );

    write_sqlite_kernel_config(&config_path, &vault, &source_path, Some("renamed"));
    let mut options = SearchOptions::new(&vault, &db_path);
    options.config_path = Some(config_path.clone());
    let renamed = ensure_searchable_sync(options, None).unwrap();
    assert_eq!(renamed.document_count, 2);
    drop(renamed);
    assert_eq!(sqlite_document_count(&db_path, "messages"), 0);
    assert_eq!(sqlite_document_count(&db_path, "renamed"), 1);

    write_sqlite_kernel_config(&config_path, &vault, &source_path, None);
    let mut options = SearchOptions::new(&vault, &db_path);
    options.config_path = Some(config_path);
    let removed = ensure_searchable_sync(options, None).unwrap();
    assert_eq!(removed.document_count, 1);
    drop(removed);
    assert_eq!(sqlite_document_count(&db_path, "renamed"), 0);
}
