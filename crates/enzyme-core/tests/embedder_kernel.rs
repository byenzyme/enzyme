//! Black-box acceptance tests for the non-CLI embedder boundary.
//!
//! Keep this test restricted to public `enzyme_core` APIs. In particular, an
//! embedder must not need CLI path discovery, account state, or a Tokio runtime
//! merely to build and query an explicitly located index.

use std::path::PathBuf;

use enzyme_core::kernel::{
    Bridged, Generator, SearchOptions, ensure_searchable, ensure_searchable_sync,
};
use enzyme_core::llm::model_registry;
use tempfile::TempDir;

fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
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
    let model_store = root.path().join("weights");
    std::fs::create_dir_all(&model_store).expect("model store");

    (root, vault, db_path, config_path, model_store)
}

#[test]
fn sync_consumer_uses_explicit_paths_and_never_resolves_a_hosted_generator() {
    let (_root, vault, db_path, config_path, model_store) = fixture();

    // A hidden call to the product's hosted fallback would make provisioning
    // fail against this deliberately unreachable endpoint. Supplying no
    // Generator must instead be a complete, offline policy choice.
    unsafe {
        std::env::set_var(
            "ENZYME_FREE_CONFIG_URL",
            "http://127.0.0.1:9/implicit-hosted-fallback-is-a-bug",
        );
    }

    let mut options = SearchOptions::new(&vault, &db_path);
    options.config_path = Some(config_path);
    assert!(
        Generator::local_from_paths(&model_store, options.config_path.as_ref().unwrap())
            .expect("explicit generator resolution")
            .is_none()
    );
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
    let (_root, vault, db_path, config_path, model_store) = fixture();
    let mut options = SearchOptions::new(vault, db_path);
    options.config_path = Some(config_path);
    assert!(
        Generator::local_from_paths(&model_store, options.config_path.as_ref().unwrap())
            .expect("explicit generator resolution")
            .is_none()
    );

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

#[test]
fn model_store_and_config_file_are_selected_independently() {
    let root = tempfile::tempdir().expect("temp root");
    let model_store = root.path().join("weights-owned-by-embedder");
    let config_path = root.path().join("config-owned-by-embedder/settings.toml");
    std::fs::create_dir_all(&model_store).expect("model store");
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("config parent");
    std::fs::write(
        model_store.join("consumer-model.gguf"),
        b"path-resolution fixture, not model weights",
    )
    .expect("model fixture");
    std::fs::write(
        &config_path,
        "[llm]\nmode = \"local\"\nlocal_model = \"consumer-model\"\n",
    )
    .expect("model selection config");

    let selected = model_registry::active_in(&model_store, &config_path)
        .expect("model selected from independent paths");
    assert_eq!(selected.0, "consumer-model");
    assert_eq!(selected.1, model_store.join("consumer-model.gguf"));
    assert!(!config_path.parent().unwrap().join("models").exists());
}

#[test]
fn explicit_local_generator_resolution_has_no_hosted_fallback() {
    let root = tempfile::tempdir().expect("temp root");
    let empty_model_store = root.path().join("deliberately-empty-weights");
    let config_path = root.path().join("independent/settings.toml");
    std::fs::create_dir_all(&empty_model_store).expect("model store");
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("config parent");
    std::fs::write(
        &config_path,
        "[llm]\nmode = \"local\"\nlocal_model = \"missing-model\"\n",
    )
    .expect("model selection config");

    assert!(
        Generator::local_from_paths(&empty_model_store, &config_path)
            .expect("explicit local resolution")
            .is_none(),
        "a missing explicit local model must not become an implicit hosted generator"
    );
}
