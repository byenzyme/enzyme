//! A multi-source `.enzyme` workspace loaded through the file-based runtime
//! path (`configs/*.enzyme` + `EnzymeConfig::load_at`), then indexed, proves
//! the language lowering matches what the engine consumes.
use std::fs;

use enzyme_core::config::EnzymeConfig;
use enzyme_core::db::Database;
use enzyme_core::initialize_sqlite_runtime;
use enzyme_core::models::EntityType;
use enzyme_core::pipeline::indexing::IndexingPipeline;

#[test]
fn multi_source_program_loads_roots_sources_exclusions_and_readings() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("enzyme-home");
    let workspace = home.join("workspaces/practice");
    let configs = home.join("configs");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&configs).unwrap();
    let notes = temp.path().join("notes");
    let library = temp.path().join("library");
    for dir in [
        notes.join("people"),
        notes.join("archive"),
        notes.join("inbox"),
        library.join("archive"),
        library.join("papers"),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(notes.join("people/ada.md"), "# Ada\n#craft [[Babbage]]\n").unwrap();
    fs::write(notes.join("archive/old.md"), "# Old\n#craft\n").unwrap();
    fs::write(library.join("archive/old.md"), "# Old\n").unwrap();
    fs::write(library.join("papers/engines.md"), "# Engines\n#craft\n").unwrap();
    let database = temp.path().join("ledger.db");
    fs::write(&database, []).unwrap();
    let notes = notes.canonicalize().unwrap();
    let library = library.canonicalize().unwrap();
    fs::write(
        configs.join("practice.enzyme"),
        format!(
            r#"
workspace "practice" {{
  source markdown "notes" {{ path "{notes}" }}
  source markdown "library" {{ path "{library}" }}
  source sqlite "mail" {{
    database "{database}"
    query "SELECT id, sender, sent_ms, body FROM mail"
    id "id"
    who "sender"
    when "sent_ms" unit ms
    what "body"
  }}

  remember in folder "inbox" in source "notes" create note {{ "Link the session." }}

  learn questions from folder "notes/people" including linked pages about relationships
  learn questions from folder "library/papers"
  learn questions from log "journal"
  learn questions from source "mail"
  leave out folders {{ "archive" }}
  leave out tags {{ "draft" }}
  leave out links {{ "Noise" }}
  references in fields {{ "people" }}
  prepare up to 300 documents per run newest first
  question budget 9
}}
"#,
            notes = notes.display(),
            library = library.display(),
            database = database.display(),
        ),
    )
    .unwrap();

    let config = EnzymeConfig::load_at(&workspace, &home.join("config.toml"))
        .unwrap()
        .expect("the bound workspace has a config");

    // Markdown roots: sorted by name; only the create-note source is writable.
    let roots: Vec<(&str, &std::path::Path, bool)> = config
        .notes_roots
        .iter()
        .map(|root| (root.name.as_str(), root.path.as_path(), root.writable))
        .collect();
    assert_eq!(
        roots,
        [
            ("library", library.as_path(), false),
            ("notes", notes.as_path(), true)
        ]
    );
    assert!(config.notes_roots.iter().all(|root| root.exclusions.is_empty()
        && root.document_ref_prefix.is_none()));
    assert_eq!(config.notes_path.as_deref(), Some(notes.as_path()));

    // SQLite source and workspace-wide body settings.
    assert_eq!(config.sources.len(), 1);
    assert_eq!(config.sources["mail"].db, database);
    assert_eq!(config.excluded_folders, ["archive"]);
    assert_eq!(config.excluded_tags, ["draft"]);
    assert_eq!(config.excluded_links, ["Noise"]);
    assert_eq!(config.frontmatter_link_fields, ["people"]);
    assert_eq!(config.max_embedding_files, Some(300));
    assert_eq!(
        config.trending.as_ref().unwrap().min_top_catalysts,
        Some(9)
    );

    // Readings arrive both as the compiled spec and as catalyzed entities.
    let spec = config.reading_spec.as_ref().unwrap();
    assert_eq!(spec.path, "workspace:practice");
    let entities: Vec<&str> = spec.readings.iter().map(|r| r.entity.as_str()).collect();
    assert_eq!(
        entities,
        [
            "folder:notes/people",
            "folder:library/papers",
            "log:journal",
            "collection:sqlite:mail"
        ]
    );
    assert!(spec.readings[0].include_linked_pages);
    assert_eq!(spec.readings[0].profile, "relational");
    assert_eq!(config.catalyzed_entities.len(), 4);
    let preferences = config.parse_preferences();
    assert!(
        preferences
            .entities
            .iter()
            .any(|e| e.entity_type == EntityType::AppendLog && e.name == "journal")
    );

    // The create-note policy is resolved against its root and compiled to guidance.
    assert_eq!(
        spec.note_policies[0].resolved_folder().unwrap(),
        notes.join("inbox")
    );
    let guidance = enzyme_spec::instructions(spec);
    assert!(
        guidance.contains(&format!("## Create notes in {}", notes.join("inbox").display())),
        "{guidance}"
    );
    assert!(guidance.contains("Link the session."), "{guidance}");

    // Indexing with the loaded roots yields the root-qualified folder entities
    // the readings name, and the exclusion applies inside every root.
    initialize_sqlite_runtime().unwrap();
    let db = Database::open_in_memory().unwrap();
    IndexingPipeline::new(&db, workspace.clone())
        .with_notes_roots(config.notes_roots.clone())
        .with_excluded_folders(config.excluded_folders.clone())
        .run(true, |_| {})
        .unwrap();
    let mut refs = db.get_all_source_refs().unwrap();
    refs.sort();
    assert_eq!(refs, ["library/papers/engines.md", "notes/people/ada.md"]);
    let folders: Vec<String> = db
        .get_all_entities()
        .unwrap()
        .into_iter()
        .filter(|entity| entity.entity_type == EntityType::Folder)
        .map(|entity| entity.name.to_lowercase())
        .collect();
    for reading in ["notes/people", "library/papers"] {
        assert!(folders.iter().any(|f| f == reading), "{reading} in {folders:?}");
    }
}
