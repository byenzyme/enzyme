//! An embedder that owns its own `.enzyme` programs builds one workspace's
//! engine configuration in memory and indexes it with `config_path: None`.
//!
//! Every test points `ENZYME_HOME` at a poisoned home (unparseable config and
//! program files) and `HOME` at an empty directory, then asserts both are
//! byte-for-byte unchanged: any lookup would either fail on the poison or
//! leave a trace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use enzyme_core::config::{EnzymeConfig, NotesRootConfig};
use enzyme_core::document::FileDiscovery;
use enzyme_core::initialize_sqlite_runtime;
use enzyme_core::kernel::{Bridged, SearchIndex, SearchOptions, ensure_searchable_sync};
use enzyme_core::models::EntityType;
use rusqlite::Connection;
use tempfile::TempDir;

struct Homes {
    _root: TempDir,
    enzyme_home: PathBuf,
    user_home: PathBuf,
}

/// Process-wide poisoned homes. The lock serializes tests that read the
/// environment so `set_var` never races another reader in this binary.
fn isolated() -> (MutexGuard<'static, ()>, &'static Homes) {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    static HOMES: OnceLock<Homes> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let homes = HOMES.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let enzyme_home = root.path().join("enzyme-home");
        let user_home = root.path().join("user-home");
        std::fs::create_dir_all(enzyme_home.join("configs")).unwrap();
        std::fs::create_dir_all(enzyme_home.join("workspaces")).unwrap();
        std::fs::create_dir_all(&user_home).unwrap();
        std::fs::write(enzyme_home.join("config.toml"), "this is = = not toml\n").unwrap();
        std::fs::write(
            enzyme_home.join("configs/poison.enzyme"),
            "workspace {{ unparseable\n",
        )
        .unwrap();
        // SAFETY: guarded by LOCK; every test in this binary reads the
        // environment only while holding it.
        unsafe {
            std::env::set_var("ENZYME_HOME", &enzyme_home);
            std::env::set_var("HOME", &user_home);
        }
        Homes {
            _root: root,
            enzyme_home,
            user_home,
        }
    });
    initialize_sqlite_runtime().unwrap();
    (guard, homes)
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.insert(path.clone(), Vec::new());
                stack.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn canonical_dir(path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// Markdown notes: one kept note with a frontmatter person and a literal
/// phrase, one note under the excluded `private/` folder.
fn notes_fixture(root: &Path) -> PathBuf {
    let notes = canonical_dir(root.join("notes"));
    write(
        &notes.join("harbor.md"),
        "---\npeople: [Jay Vidyarthi]\n---\nThe quartz harbor ledger records every crossing.",
    );
    write(
        &notes.join("private/secret.md"),
        "The sealed vault phrase must never be indexed.",
    );
    notes
}

fn messages_fixture(root: &Path) -> PathBuf {
    let path = root.join("messages.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, sender TEXT, sent_ms INTEGER, body TEXT);
         INSERT INTO messages VALUES
           (1, 'Ada', 1705276800000, 'Orchid telemetry arrived from the greenhouse.'),
           (2, 'Ada', 1707955200000, 'A second orchid telemetry report followed.');",
    )
    .unwrap();
    path
}

fn source_refs(index: &SearchIndex) -> Vec<String> {
    let mut refs = index
        .database()
        .query_read("SELECT source_ref FROM docs", vec![], |row| {
            row.get::<String>(0)
        })
        .unwrap();
    refs.sort();
    refs
}

fn program(source: &str, user_home: &Path) -> enzyme_spec::Program {
    enzyme_spec::resolve(vec![enzyme_spec::parse(source).unwrap()], user_home).unwrap()
}

/// Named workspace: the SQLite source comes from the workspace grammar. The
/// current grammar requires Markdown to be a workspace's only source and
/// `enzyme_spec::config_value` does not lower workspace-level exclusions or
/// frontmatter fields for named workspaces, so the notes root and those
/// fields are added to the in-memory config the embedder will pass.
#[test]
fn named_workspace_indexes_markdown_and_sqlite_from_in_memory_config() {
    let (_guard, homes) = isolated();
    let enzyme_before = snapshot(&homes.enzyme_home);
    let user_before = snapshot(&homes.user_home);

    let root = tempfile::tempdir().unwrap();
    let notes = notes_fixture(root.path());
    let messages = messages_fixture(root.path());
    let program = program(
        &format!(
            r#"
workspace "practice" {{
  source sqlite "messages" {{
    database "{}"
    query "SELECT id, sender, sent_ms, body FROM messages"
    id "id"
    who "sender"
    when "sent_ms" unit ms
    what "body"
  }}
  learn questions from source "messages"
}}
"#,
            messages.display()
        ),
        &homes.user_home,
    );

    let mut config = EnzymeConfig::from_program_workspace(program, "practice").unwrap();
    assert_eq!(config.sources["messages"].db, messages);
    assert!(config.has_curated_entities());
    config.notes_roots.push(NotesRootConfig {
        name: "notes".into(),
        path: notes.clone(),
        document_ref_prefix: None,
        exclusions: Vec::new(),
        writable: false,
    });
    config.excluded_folders = vec!["private".into()];
    config.frontmatter_link_fields = vec!["people".into()];

    let discovered = FileDiscovery::from_notes_roots(config.notes_roots.clone())
        .with_excluded_folders(config.excluded_folders.clone())
        .discover_named()
        .unwrap();
    assert_eq!(discovered.len(), 1);
    assert!(!discovered[0].writable, "notes-root writable must survive");

    // The vault path is an empty state directory: with notes roots supplied
    // it is never scanned for Markdown.
    let state = canonical_dir(root.path().join("state"));
    let db_path = state.join("index.db");
    let options = SearchOptions {
        workspace_config: Some(config.clone()),
        ..SearchOptions::new(&state, &db_path)
    };
    assert!(options.config_path.is_none());
    let index = ensure_searchable_sync(options, None).unwrap();

    assert_eq!(index.bridged, Bridged::NoGenerator);
    let refs = source_refs(&index);
    assert_eq!(refs.len(), 3, "{refs:?}");
    assert!(refs.contains(&"harbor.md".to_string()), "{refs:?}");
    assert_eq!(
        refs.iter()
            .filter(|r| r.starts_with("sqlite:messages/"))
            .count(),
        2,
        "{refs:?}"
    );
    assert_eq!(index.document_count, 3);
    assert_eq!(
        index.exact_phrase_search("quartz harbor", 5).unwrap().len(),
        1
    );
    assert_eq!(
        index
            .exact_phrase_search("orchid telemetry", 5)
            .unwrap()
            .len(),
        2
    );
    assert!(
        index
            .exact_phrase_search("sealed vault", 5)
            .unwrap()
            .is_empty()
    );
    let entities = index.database().get_all_entities().unwrap();
    assert!(
        entities
            .iter()
            .any(|e| { e.name == "jay vidyarthi" && e.entity_type == EntityType::Link })
    );
    assert!(
        entities
            .iter()
            .any(|e| { e.name == "sqlite:messages" && e.entity_type == EntityType::Collection })
    );

    // A later run with the source removed reconciles its documents away,
    // exactly as a file-loaded config does.
    drop(index);
    config.sources.clear();
    let options = SearchOptions {
        workspace_config: Some(config),
        ..SearchOptions::new(&state, &db_path)
    };
    let index = ensure_searchable_sync(options, None).unwrap();
    assert_eq!(source_refs(&index), ["harbor.md"]);

    assert_eq!(snapshot(&homes.enzyme_home), enzyme_before);
    assert_eq!(snapshot(&homes.user_home), user_before);
}

/// Single-Markdown workspace: the whole policy comes from the grammar and the
/// Markdown path is the indexing root, preserving the legacy path identity.
#[test]
fn single_markdown_workspace_indexes_from_program_alone() {
    let (_guard, homes) = isolated();
    let enzyme_before = snapshot(&homes.enzyme_home);
    let user_before = snapshot(&homes.user_home);

    let root = tempfile::tempdir().unwrap();
    let notes = notes_fixture(root.path());
    let program = program(
        &format!(
            r#"
workspace "notes" {{
  source markdown "notes" {{ path "{}" }}
  leave out folders {{ "private" }}
  references in fields ["people"]
}}
"#,
            notes.display()
        ),
        &homes.user_home,
    );
    let config = EnzymeConfig::from_program_workspace(program, "notes").unwrap();
    assert!(config.notes_roots.is_empty());
    assert!(config.sources.is_empty());

    let db_path = root.path().join("state/index.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let options = SearchOptions {
        workspace_config: Some(config),
        ..SearchOptions::new(&notes, &db_path)
    };
    let index = ensure_searchable_sync(options, None).unwrap();

    assert_eq!(source_refs(&index), ["harbor.md"]);
    assert!(
        index
            .exact_phrase_search("sealed vault", 5)
            .unwrap()
            .is_empty()
    );
    assert!(
        index
            .database()
            .get_all_entities()
            .unwrap()
            .iter()
            .any(|e| e.name == "jay vidyarthi" && e.entity_type == EntityType::Link)
    );
    assert!(
        !notes.join(".enzyme").exists(),
        "indexing must not write into the notes root"
    );

    assert_eq!(snapshot(&homes.enzyme_home), enzyme_before);
    assert_eq!(snapshot(&homes.user_home), user_before);
}

#[test]
fn config_path_and_workspace_config_are_mutually_exclusive() {
    let (_guard, _homes) = isolated();
    let root = tempfile::tempdir().unwrap();
    let options = SearchOptions {
        config_path: Some(root.path().join("config.toml")),
        workspace_config: Some(EnzymeConfig::default()),
        ..SearchOptions::new(root.path(), root.path().join("index.db"))
    };
    let error = ensure_searchable_sync(options, None).err().unwrap();
    assert!(error.to_string().contains("not both"), "{error}");
    assert!(!root.path().join("index.db").exists());
}
