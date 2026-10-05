//! Multi-source workspaces, host source kinds, and create-note policies.
use enzyme_spec::*;
use std::path::Path;

fn resolve_one(source: &str) -> anyhow::Result<Program> {
    resolve(vec![parse(source)?], Path::new("/home/demo"))
}

fn error(source: &str) -> String {
    match parse(source).and_then(|p| resolve(vec![p], Path::new("/home/demo"))) {
        Ok(_) => panic!("expected an error for {source}"),
        Err(error) => format!("{error:#}"),
    }
}

fn assert_roundtrip(source: &str) -> Program {
    let parsed = parse(source).unwrap();
    let rendered = render_program(&parsed);
    let reparsed = parse(&rendered).unwrap_or_else(|e| panic!("{e:#}\n{rendered}"));
    assert_eq!(reparsed, parsed, "{rendered}");
    assert_eq!(render_program(&reparsed), rendered, "rendering is canonical");
    parsed
}

const PRACTICE: &str = r#"
workspace "practice" {
  source markdown "notes" { path "/srv/notes" }
  source markdown "library" { path "/srv/library" }
  source google-mail "mail" {
    account "me@example.com"
    query "-in:spam -in:trash"
    backfill days 365
  }
  source google-calendar "calendar" { account "me@example.com" }

  remember in folder "inbox" in source "notes" create note

  leave out folders { "archive" }
  learn questions from folder "notes/people" including linked pages about relationships
  learn questions from source "mail"
}
"#;

fn mail_as_sqlite(name: &str) -> Source {
    Source::Sqlite(SqliteSource {
        name: name.into(),
        db: "/srv/ledger.db".into(),
        query: "SELECT id, sender, sent_ms, body FROM mail".into(),
        id: vec!["id".into()],
        document_ref: None,
        who: SqliteWho::Columns {
            columns: vec!["sender".into()],
        },
        when: "sent_ms".into(),
        what: vec!["body".into()],
        where_columns: vec![],
        weight: None,
        timestamp_unit: "ms".into(),
        timestamp_epoch: None,
        filter: None,
    })
}

#[test]
fn host_sources_parse_in_order_and_roundtrip() {
    let program = assert_roundtrip(PRACTICE);
    let workspace = &program.workspaces[0];
    let kinds: Vec<&str> = workspace.sources.iter().map(Source::kind).collect();
    assert_eq!(
        kinds,
        ["markdown", "markdown", "google-mail", "google-calendar"]
    );
    let hosts: Vec<&HostSource> = workspace.host_sources().collect();
    assert_eq!(hosts[0].name, "mail");
    let keys: Vec<&str> = hosts[0].fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, ["account", "query", "backfill days"]);
    assert_eq!(hosts[0].field("backfill days"), Some(&HostValue::Integer(365)));
    assert_eq!(
        hosts[1].field("account"),
        Some(&HostValue::Text("me@example.com".into()))
    );
    let rendered = render_program(&program);
    assert!(
        rendered.contains("  source google-calendar \"calendar\" { account \"me@example.com\" }\n"),
        "{rendered}"
    );
    assert!(rendered.contains("    backfill days 365\n"), "{rendered}");
}

#[test]
fn host_field_values_cover_text_integer_bool_and_list() {
    let source = r#"workspace "w" {
  source granola "meetings" {
    account "me@example.com"
    time range "last_30_days"
    workspace only false
    include archived true
    labels { "one" "two" }
  }
  source google-meet "meet" {}
}"#;
    let program = assert_roundtrip(source);
    let host = program.workspaces[0].host_sources().next().unwrap();
    assert_eq!(host.kind, "granola");
    assert_eq!(host.field("workspace only"), Some(&HostValue::Bool(false)));
    assert_eq!(host.field("include archived"), Some(&HostValue::Bool(true)));
    assert_eq!(
        host.field("labels"),
        Some(&HostValue::List(vec!["one".into(), "two".into()]))
    );
    let json = serde_json::to_value(&program.workspaces[0].sources[0]).unwrap();
    assert_eq!(json["kind"], "host");
    assert_eq!(json["host_kind"], "granola");
    let back: Source = serde_json::from_value(json).unwrap();
    assert_eq!(back, program.workspaces[0].sources[0]);
}

#[test]
fn host_source_errors_are_clear() {
    for (source, expected) in [
        (
            r#"workspace "w" { source google-mail "m" { account "a" account "b" } }"#,
            "repeats field \"account\"",
        ),
        (
            r#"workspace "w" { source google-mail "m" { backfill days } }"#,
            "needs a value",
        ),
        (
            r#"workspace "w" { source google-mail "m" { backfill days 1.5 } }"#,
            "non-negative integer",
        ),
        (
            r#"workspace "w" { source google-mail "Mail" { } source markdown "mail" { path "/n" } }"#,
            "duplicate workspace source mail",
        ),
        (
            r#"workspace "w" { source "m" { } }"#,
            "expected a source kind",
        ),
    ] {
        let message = error(source);
        assert!(message.contains(expected), "{source}: {message}");
    }
}

#[test]
fn resolve_rejects_unlowered_host_sources_and_host_lowering_fixes_readings() {
    let message = error(PRACTICE);
    assert!(
        message.contains("source google-mail \"mail\"")
            && message.contains("must lower it before resolution"),
        "{message}"
    );

    let mut program = parse(PRACTICE).unwrap();
    let mut seen = vec![];
    program
        .lower_host_sources(|workspace, host| {
            seen.push((workspace.to_string(), host.kind.clone()));
            Ok(match host.kind.as_str() {
                "google-mail" => Some(mail_as_sqlite(&host.name)),
                "google-calendar" => Some(Source::Sqlite(SqliteSource {
                    name: host.name.clone(),
                    ..match mail_as_sqlite("x") {
                        Source::Sqlite(s) => s,
                        _ => unreachable!(),
                    }
                })),
                _ => None,
            })
        })
        .unwrap();
    assert_eq!(
        seen,
        [
            ("practice".to_string(), "google-mail".to_string()),
            ("practice".to_string(), "google-calendar".to_string())
        ]
    );
    let resolved = resolve(vec![program], Path::new("/home/demo")).unwrap();
    let vault = &resolved.vaults[0];
    assert_eq!(vault.path, "workspace:practice");
    let entities: Vec<&str> = vault.readings.iter().map(|r| r.entity.as_str()).collect();
    assert_eq!(entities, ["folder:notes/people", "collection:sqlite:mail"]);

    // A lowering may not rename the source or return another host source.
    let mut renamed = parse(PRACTICE).unwrap();
    let message = format!(
        "{:#}",
        renamed
            .lower_host_sources(|_, _| Ok(Some(mail_as_sqlite("other"))))
            .unwrap_err()
    );
    assert!(message.contains("must keep the name"), "{message}");
    let mut hosted = parse(PRACTICE).unwrap();
    let message = format!(
        "{:#}",
        hosted
            .lower_host_sources(|_, host| Ok(Some(Source::Host(host.clone()))))
            .unwrap_err()
    );
    assert!(message.contains("must lower to markdown"), "{message}");
}

#[test]
fn named_workspace_lowers_markdown_roots_body_and_writability() {
    let source = r#"
workspace "practice" {
  source sqlite "mail" {
    database "/srv/ledger.db"
    query "SELECT id, sender, sent_ms, body FROM mail"
    id "id"
    who "sender"
    when "sent_ms" unit ms
    what "body"
  }
  source markdown "notes" { path "~/notes" }
  source markdown "library" { path "/srv/library" }
  prepare up to 500 documents per run newest first
  question budget 12
  produce theses
  learn questions from folder "notes/people" including linked pages about relationships
  learn questions from folder "library" about decisions
  learn questions from tag "craft"
  learn questions from log "journal"
  learn questions from tags matching "proj-*"
  learn questions from source "mail"
  leave out folders { "archive" }
  leave out tags { "draft" }
  leave out links { "Excluded" }
  references in fields { "people" }
  remember in folder "inbox" in source "notes" when { "A meeting ended." } create note { "Link the sources." }
  remember in "observations.md" when { "A preference changed." append observation { "Date it." } }
  when asked { answer with sources }
  project questions into "/srv/code"
}
"#;
    assert_roundtrip(source);
    let resolved = resolve_one(source).unwrap();
    let config = config_value(&resolved);
    let workspace = &config["workspaces"]["practice"];
    assert_eq!(
        workspace["sources"]["notes"],
        serde_json::json!({"path": "/home/demo/notes", "writable": true})
    );
    assert_eq!(
        workspace["sources"]["library"],
        serde_json::json!({"path": "/srv/library", "writable": false})
    );
    assert_eq!(workspace["sources"]["mail"]["db"], "/srv/ledger.db");
    assert_eq!(workspace["excluded_folders"], serde_json::json!(["archive"]));
    assert_eq!(workspace["excluded_tags"], serde_json::json!(["draft"]));
    assert_eq!(workspace["excluded_links"], serde_json::json!(["Excluded"]));
    assert_eq!(
        workspace["frontmatter_link_fields"],
        serde_json::json!(["people"])
    );
    assert_eq!(workspace["max_embedding_files"], 500);
    assert_eq!(workspace["min_top_catalysts"], 12);
    assert_eq!(workspace["catalyst_format"], "thesis");
    assert_eq!(
        workspace["entities"],
        serde_json::json!([
            {"folder:notes/people": {"profile": "relational", "expandable": true}},
            {"folder:library": {"profile": "decision_trace", "expandable": false}},
            "#craft",
            "log:journal",
            "collection:sqlite:mail"
        ])
    );
    assert_eq!(workspace["targets"], serde_json::json!(["/srv/code"]));
    // Markdown roots are sources of the named scope, never path-keyed vaults.
    let vault_keys: Vec<&String> = config["vaults"].as_object().unwrap().keys().collect();
    assert_eq!(vault_keys, ["workspace:practice"]);

    let vault = &resolved.vaults[0];
    assert_eq!(vault.path, "workspace:practice");
    assert!(vault.readings.iter().any(|r| r.pattern));
    assert_eq!(
        vault.note_policies[0].resolved_folder().unwrap(),
        Path::new("/home/demo/notes/inbox")
    );
    let text = instructions(vault);
    assert!(text.contains("## Create notes in /home/demo/notes/inbox"), "{text}");
    assert!(text.contains("- A meeting ended."), "{text}");
    assert!(text.contains("- Link the sources."), "{text}");
    assert!(text.contains("## Remember in observations.md"), "{text}");
    assert!(text.contains("cite their source files"), "{text}");
}

#[test]
fn without_create_note_every_markdown_root_is_read_only() {
    let resolved = resolve_one(
        r#"workspace "w" {
  source markdown "a" { path "/a" }
  source markdown "b" { path "/b" }
}"#,
    )
    .unwrap();
    let config = config_value(&resolved);
    assert_eq!(config["workspaces"]["w"]["sources"]["a"]["writable"], false);
    assert_eq!(config["workspaces"]["w"]["sources"]["b"]["writable"], false);
}

#[test]
fn several_markdown_roots_need_root_qualified_folders() {
    let base = |reading: &str| {
        format!(
            r#"workspace "w" {{
  source markdown "notes" {{ path "/n" }}
  source markdown "library" {{ path "/l" }}
  leave out folders {{ "archive" }}
  {reading}
}}"#
        )
    };
    let message = error(&base(r#"learn questions from folder "people""#));
    assert!(
        message.contains("must start with the name of one of its Markdown sources (notes, library)")
            && message.contains("\"notes/people\""),
        "{message}"
    );
    // Source names match case-insensitively, like the engine's folder entities.
    assert!(resolve_one(&base(r#"learn questions from folder "Notes/people""#)).is_ok());
    // The root of one source is its name.
    assert!(resolve_one(&base(r#"learn questions from folder "library""#)).is_ok());
    // Exclusions apply root-relative inside every root.
    let message = error(&base(r#"learn questions from folder "library/archive/old""#));
    assert!(message.contains("is excluded"), "{message}");
    // A root named like an excluded folder is not itself excluded.
    let named_archive = r#"workspace "w" {
  source markdown "archive" { path "/a" }
  source markdown "notes" { path "/n" }
  leave out folders { "archive" }
  learn questions from folder "archive/people"
}"#;
    assert!(resolve_one(named_archive).is_ok());
    // SQLite `where` labels are unprefixed folders, so they stay valid.
    let with_where = r#"workspace "w" {
  source markdown "notes" { path "/n" }
  source markdown "library" { path "/l" }
  source sqlite "chat" {
    database "/c.db"
    query "SELECT id, ts, body, room FROM m"
    id "id"
    when "ts" unit s
    what "body"
    where "room"
  }
  learn questions from folder "general"
}"#;
    assert!(resolve_one(with_where).is_ok());
}

#[test]
fn one_markdown_root_with_other_sources_keeps_root_relative_folders() {
    let source = r#"workspace "w" {
  source markdown "notes" { path "/n" }
  source arena "boards" { channels { channel 7 as "Board" } }
  leave out folders { "private" }
  learn questions from folder "people"
  learn questions from channel "Board"
  remember in folder "." create note
}"#;
    assert_roundtrip(source);
    let resolved = resolve_one(source).unwrap();
    assert_eq!(resolved.vaults[0].readings[0].entity, "folder:people");
    assert_eq!(
        resolved.vaults[0].note_policies[0].resolved_folder().unwrap(),
        Path::new("/n")
    );
    let config = config_value(&resolved);
    assert_eq!(config["workspaces"]["w"]["sources"]["notes"]["writable"], true);
    let message = error(&source.replace("folder \"people\"", "folder \"private/x\""));
    assert!(message.contains("is excluded"), "{message}");
}

#[test]
fn single_markdown_workspace_keeps_legacy_lowering_with_create_note() {
    let source = r#"workspace "notes" {
  source markdown "notes" { path "/tmp/enzyme-create-note-legacy" }

  learn questions from folder "people"

  remember in folder "inbox" when {
    "A meeting ended."
  } create note {
    "Start with the decision."
  }
}
"#;
    let program = assert_roundtrip(source);
    let rendered = render_program(&program);
    assert!(rendered.contains(source), "{rendered}");
    let resolved = resolve_one(source).unwrap();
    let config = config_value(&resolved);
    assert!(config["workspaces"].as_object().unwrap().is_empty());
    assert!(config["vaults"]["/tmp/enzyme-create-note-legacy"].is_object());
    let vault = &resolved.vaults[0];
    assert_eq!(vault.path, "/tmp/enzyme-create-note-legacy");
    let text = instructions(vault);
    assert!(
        text.contains("## Create notes in /tmp/enzyme-create-note-legacy/inbox"),
        "{text}"
    );
    assert!(text.contains("never overwrite"), "{text}");
}

#[test]
fn create_note_forms_roundtrip() {
    for policy in [
        r#"remember in folder "inbox" create note"#,
        r#"remember in folder "." in source "notes" create note"#,
        r#"remember in folder "meetings/2026" in source "notes" when { "A call ended." "Mira asked." } create note"#,
        r#"remember in folder "inbox" in source "notes" create note { "Title it plainly." }"#,
    ] {
        let source = format!(
            r#"workspace "w" {{ source markdown "notes" {{ path "/n" }} source sqlite "s" {{ database "/s.db" query "SELECT 1" id "id" when "t" unit s what "b" }} {policy} }}"#
        );
        let program = assert_roundtrip(&source);
        assert_eq!(program.workspaces[0].note_policies.len(), 1, "{policy}");
    }
    let vault = assert_roundtrip(r#"vault "/n" { remember in folder "inbox" create note }"#);
    assert_eq!(vault.vaults[0].note_policies[0].folder, "inbox");
}

#[test]
fn create_note_errors_are_clear() {
    let two = |policy: &str| {
        format!(
            r#"workspace "w" {{ source markdown "a" {{ path "/a" }} source markdown "b" {{ path "/b" }} source sqlite "s" {{ database "/s.db" query "SELECT 1" id "id" when "t" unit s what "b" }} {policy} }}"#
        )
    };
    for (source, expected) in [
        (
            two(r#"remember in folder "inbox" create note"#),
            "needs in source \"…\" naming one of the Markdown sources a, b",
        ),
        (
            two(r#"remember in folder "inbox" in source "s" create note"#),
            "declares no Markdown source with that name",
        ),
        (
            two(r#"remember in folder "/abs" in source "a" create note"#),
            "must be relative to the Markdown source root",
        ),
        (
            two(r#"remember in folder "~/x" in source "a" create note"#),
            "must be relative",
        ),
        (
            two(r#"remember in folder "x/../../y" in source "a" create note"#),
            "must not leave the Markdown source root",
        ),
        (
            two(r#"remember in folder "x" in source "a" when { } create note"#),
            "at least one condition",
        ),
        (
            two(r#"remember in folder "x" in source "a" create note { }"#),
            "guidance must not be empty",
        ),
        (
            two(r#"remember in folder "x" in source "a""#),
            "expected create note",
        ),
        (
            two(
                r#"remember in folder "x" in source "a" create note remember in folder "x" in source "A" create note"#,
            ),
            "duplicate create note policy",
        ),
        (
            r#"workspace "w" { source sqlite "s" { database "/s.db" query "SELECT 1" id "id" when "t" unit s what "b" } remember in folder "x" create note }"#.to_string(),
            "declares none",
        ),
        (
            r#"vault "/n" { remember in folder "x" in source "a" create note }"#.to_string(),
            "in source applies only inside a workspace",
        ),
    ] {
        let message = error(&source);
        assert!(message.contains(expected), "{source}: {message}");
    }
}

#[test]
fn vault_body_statements_work_in_source_only_workspaces() {
    let source = r#"workspace "chat" {
  source sqlite "messages" {
    database "/m.db"
    query "SELECT id, sender, ts, body FROM m"
    id "id"
    who "sender"
    when "ts" unit s
    what "body"
  }
  learning { as of latest evidence sample by recency }
  learn questions from source "messages" including who links select 3 by frequency
  learn questions from link "Ada"
  leave out links { "Bot" }
  when asked { "Quote the message." retrieve passages through learned questions }
  remember in "/m/observations.md" when { "A fact changed." append observation { "Date it." } }
}"#;
    assert_roundtrip(source);
    let resolved = resolve_one(source).unwrap();
    let vault = &resolved.vaults[0];
    assert_eq!(vault.learning.as_of, Some(AsOf::LatestEvidence));
    assert!(vault.retrieval.is_some());
    assert_eq!(vault.memories.len(), 1);
    assert_eq!(
        config_value(&resolved)["workspaces"]["chat"]["excluded_links"],
        serde_json::json!(["Bot"])
    );
    let message = error(&source.replace(
        "learn questions from link \"Ada\"",
        "learn questions from tag \"x\"",
    ));
    assert!(
        message.contains("workspace reading kind tag has no declared source"),
        "{message}"
    );
}

#[test]
fn sources_may_follow_body_statements_and_render_first() {
    let program = assert_roundtrip(
        r#"workspace "w" {
  learn questions from folder "a/x"
  source markdown "a" { path "/a" }
  leave out tags { "t" }
  source markdown "b" { path "/b" }
}"#,
    );
    let rendered = render_program(&program);
    assert!(
        rendered.contains(
            "workspace \"w\" {\n  source markdown \"a\" { path \"/a\" }\n  source markdown \"b\" { path \"/b\" }\n\n  learn questions from folder \"a/x\"\n"
        ),
        "{rendered}"
    );
}
