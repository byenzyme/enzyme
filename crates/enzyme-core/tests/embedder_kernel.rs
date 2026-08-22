//! Black-box acceptance tests for the non-CLI embedder boundary.
//!
//! Keep this test restricted to public `enzyme_core` APIs. In particular, an
//! embedder must not need CLI path discovery, account state, or a Tokio runtime
//! merely to build and query an explicitly located index.

use std::path::PathBuf;

use enzyme_core::kernel::{Bridged, SearchOptions, ensure_searchable, ensure_searchable_sync};
use tempfile::TempDir;

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
