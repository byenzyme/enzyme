use std::path::Path;

use enzyme_core::config::{EnzymeConfig, EnzymePaths};
use enzyme_core::db::Database;
use enzyme_core::initialize_sqlite_runtime;
use enzyme_core::llm::LlmClient;
use enzyme_core::pipeline::catalysts::{CatalystConfig, CatalystPipeline};
use enzyme_core::pipeline::selection::SelectionPipeline;
use enzyme_core::sqlite_source::run_sqlite_source;
use rusqlite::{Connection, params};

#[test]
fn selected_who_link_uses_only_its_sqlite_source_for_ranking_and_context() {
    initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("messages.sqlite");
    let source = Connection::open(&path).unwrap();
    source
        .execute_batch(
            "CREATE TABLE messages (
        id INTEGER PRIMARY KEY, source_name TEXT NOT NULL, document_ref TEXT NOT NULL,
        person TEXT NOT NULL, sent_at INTEGER NOT NULL, body TEXT NOT NULL
    )",
        )
        .unwrap();
    for (id, source_name, person, body) in [
        (
            1,
            "messages",
            "Alice",
            "MESSAGE_ALICE_FIRST describes the migration boundary and the decision record in detail.",
        ),
        (
            2,
            "messages",
            "Alice",
            "MESSAGE_ALICE_SECOND identifies the follow up work needed for that migration boundary.",
        ),
        (
            3,
            "messages",
            "Bob",
            "MESSAGE_BOB raises a concern about the same decision record.",
        ),
        (
            4,
            "other",
            "Alice",
            "OTHER_ALICE_SECRET discusses an unrelated private project.",
        ),
        (
            5,
            "other",
            "Bob",
            "OTHER_BOB_FIRST discusses a separate project.",
        ),
        (
            6,
            "other",
            "Bob",
            "OTHER_BOB_SECOND discusses a separate project.",
        ),
        (
            7,
            "other",
            "Carol",
            "OTHER_CAROL discusses a separate project.",
        ),
    ] {
        source
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    source_name,
                    format!("sqlite:{source_name}/custom-{id}"),
                    person,
                    1_775_000_000_000i64 + id * 1_000,
                    body
                ],
            )
            .unwrap();
    }
    drop(source);

    let program = enzyme_spec::resolve(vec![enzyme_spec::parse(&format!(r#"
workspace "conversations" {{
  source sqlite "messages" {{
    database "{}"
    query "SELECT id, document_ref, person, sent_at, body FROM messages WHERE source_name = 'messages'"
    id "id"
    document ref "document_ref"
    who "person"
    when "sent_at" unit ms
    what "body"
  }}
  source sqlite "other" {{
    database "{}"
    query "SELECT id, document_ref, person, sent_at, body FROM messages WHERE source_name = 'other'"
    id "id"
    document ref "document_ref"
    who "person"
    when "sent_at" unit ms
    what "body"
  }}
  learn questions from source "messages" including who links select 1 by frequency
}}"#, path.display(), path.display())).unwrap()], Path::new("/home/test")).unwrap();
    let workspace = EnzymePaths::workspace_root("conversations").unwrap();
    let config = EnzymeConfig::from_readable_program(&workspace, program).unwrap();
    assert!(config.reading_spec.as_ref().unwrap().readings[0].include_who_links);
    let db = Database::open_in_memory().unwrap();
    for (name, source_config) in &config.sources {
        run_sqlite_source(&db, name, source_config).unwrap();
    }
    let selection = SelectionPipeline::new(&db, &workspace)
        .run_with_config(Some(&config))
        .unwrap();
    assert_eq!(selection.diagnostics.who_links[0].selected[0].name, "Alice");
    assert_eq!(selection.diagnostics.who_links[0].selected[0].documents, 2);
    assert!(
        !selection
            .entities
            .iter()
            .any(|e| e.name == "Bob" || e.name == "Carol")
    );

    let llm = LlmClient::with_base_url("plan", "https://plan.invalid/v1");
    let jobs = CatalystPipeline::new(&db, &llm, None)
        .plan_jobs(
            &selection.entities,
            &CatalystConfig {
                reading_spec: config.reading_spec.as_ref(),
                entity_profiles: &selection.entity_profiles,
                catalysts_per_entity: 1,
                min_context_documents: 1,
                min_context_tokens: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let alice = jobs
        .jobs
        .iter()
        .find(|job| job.entity_name == "Alice")
        .expect("Alice job");
    assert!(alice.user_prompt.contains("MESSAGE_ALICE_FIRST"));
    assert!(!alice.user_prompt.contains("OTHER_ALICE_SECRET"));
    assert!(!alice.user_prompt.contains("sqlite:other/"));
}
