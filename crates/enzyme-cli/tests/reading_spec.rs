mod support;
use std::{fs, time::Duration};
use support::{fixture::VaultFixture, mock::MockLlm};

fn write_spec(f: &VaultFixture, marker: &str) {
    let dir = f.enzyme_home().join("configs");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("vault.enzyme"),format!(r#"
settings {{ generation hosted updates disabled }}
profile decisions {{ seek "{marker}: recover why one approach won" notice ["tradeoffs", "rejected alternatives"] }}
learning {{ question budget 3 sample by recency }}
vault {:?} {{
 leave out folders ["private"]
 learn questions from tag "focus" about decisions
 when asked {{ "Use grep for exact phrases." retrieve passages through learned questions answer with sources }}
}}
"#,f.vault_path().to_string_lossy())).unwrap();
}
#[test]
fn init_and_refresh_use_rust_spec_and_regenerate_after_profile_edit() {
    let f = VaultFixture::empty();
    let llm = MockLlm::start();
    llm.set_ok_content(r#"{"catalysts":["Which migration tradeoff preserves source evidence?","What rejected alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#);
    for i in 0..3 {
        f.write_note(&format!("note-{i}.md"),&format!("---\ntags: [focus]\ncreated: 2026-08-0{}\n---\n# Migration evidence\n{}",i+1,"The team chose a reversible migration with auditable source evidence. Rejected alternatives traded traceability for speed. The decision depends on keeping old records available for comparison. ".repeat(20)));
    }
    f.write_note(
        "private/secret.md",
        "This excluded note must not be indexed. #focus",
    );
    write_spec(&f, "FIRST_PROFILE");
    let out = f
        .cli()
        .args(["init", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    out.assert_success();
    assert!(
        llm.requests().iter().any(|s| s.contains("FIRST_PROFILE")),
        "custom seek never reached LLM: {}",
        out.combined()
    );
    assert!(!f.enzyme_home().join("config.toml").exists());
    let db_path = f.vault_enzyme_dir().join("enzyme.db");
    // Locate the canonical database filename used by this checkout.
    let db_path = if db_path.exists() {
        db_path
    } else {
        f.vault_enzyme_dir().join("index.db")
    };
    let db = enzyme_core::db::Database::open(&db_path).unwrap();
    assert!(
        !db.get_all_document_hashes()
            .unwrap()
            .keys()
            .any(|p| p.contains("secret"))
    );
    let count = llm.request_count();
    write_spec(&f, "SECOND_PROFILE");
    let refreshed = f
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    refreshed.assert_success();
    let payload: serde_json::Value = serde_json::from_str(&refreshed.stdout).unwrap();
    let epoch = support::epoch::build_epoch_id(&payload);
    let worker = f
        .cli()
        .args(["refresh", "--detached", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    worker.assert_success();
    let state = support::epoch::wait_for_epoch_terminal(&db_path, &epoch, Duration::from_secs(60));
    assert_eq!(state, "active");
    assert!(llm.request_count() > count);
    assert!(
        llm.requests()
            .iter()
            .skip(count)
            .any(|s| s.contains("SECOND_PROFILE"))
    );
    let inspect = f
        .cli()
        .args(["spec", "inspect"])
        .arg(f.vault_path().to_string_lossy())
        .arg("--instructions")
        .run();
    inspect.assert_success();
    assert!(inspect.stdout.contains("petri --query"));
    let search = f
        .cli()
        .args(["catalyze", "reversible migration audit evidence"])
        .run();
    search.assert_success();
    assert!(search.stdout.contains("note-"));
}
#[test]
fn invalid_spec_fails_before_init_writes() {
    let f = VaultFixture::empty();
    fs::create_dir(f.enzyme_home().join("configs")).unwrap();
    fs::write(f.enzyme_home().join("configs/bad.enzyme"), "vault oops").unwrap();
    let out = f.cli().args(["init", "--stage", "index"]).run();
    out.assert_failure();
    assert!(!f.vault_enzyme_dir().join("enzyme.db").exists());
}

struct JevDecisions;
impl wiremock::Respond for JevDecisions {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use serde_json::{Value, json};
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body["model"].as_str().unwrap().starts_with("typesafe/jev"));
        let answers: serde_json::Map<String, Value> = body["questions"].as_object().unwrap().iter().map(|(id, q)| {
            let choice = if id.starts_with("select_") { "include" } else if id.starts_with("temporal_") { "time" } else { "decision_trace" };
            let probabilities: serde_json::Map<String, Value> = q["criteria"].as_object().unwrap().keys().map(|k| (k.clone(), json!(if k == choice { 1.0 } else { 0.0 }))).collect();
            (id.clone(), json!({"type":"choice", "choice":choice,"probabilities":probabilities,"confidence":0.95}))
        }).collect();
        wiremock::ResponseTemplate::new(200)
            .set_body_json(json!({"model":"jev-1.13.0","answers":answers}))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_generates_parseable_setup_and_preserves_existing_policies() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(JevDecisions)
        .mount(&server)
        .await;
    let f = VaultFixture::empty();
    for i in 0..3 {
        f.write_note(
            &format!("note-{i}.md"),
            "#focus We chose a reversible migration after comparing alternatives and their costs.",
        );
    }
    write_spec(&f, "OLD_CUSTOM_PROFILE");
    let file = f.enzyme_home().join("configs/vault.enzyme");
    let original = fs::read_to_string(&file).unwrap();
    let draft = f.enzyme_home().join("proposal.enzyme");
    let out = f
        .cli()
        .args(["compile", "-v", "--color", "never", "--output"])
        .arg(draft.to_string_lossy())
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.uri())
        .env("OPENAI_MODEL", "catalyst-only")
        .env("ENZYME_JEV_MODEL", "typesafe/jev-test")
        .run();
    out.assert_success();
    assert!(!out.stdout.contains('\u{1b}'));
    assert!(!out.stdout.contains("// Enzyme reading configuration"));
    assert_eq!(out.stderr.lines().count(), 1);
    assert!(out.stderr.starts_with("Saved proposal: "));
    assert!(out.stderr.trim().ends_with("s)"));
    assert_eq!(fs::read_to_string(&draft).unwrap(), out.stdout);
    assert_eq!(fs::read_to_string(&file).unwrap(), original);
    assert!(!f.vault_path().join(".enzyme").exists());
    assert!(!out.stdout.contains("OLD_CUSTOM_PROFILE"));
    let parsed = enzyme_spec::parse(&out.stdout).unwrap();
    assert!(parsed.profiles.is_empty());
    assert_eq!(
        out.stdout
            .matches("// Decision Trace (`decision_trace`)")
            .count(),
        1,
        "a built-in lens should be explained only at its first use"
    );
    let comment_prose = out
        .stdout
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("//"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        comment_prose.contains(
            enzyme_core::llm::get_profile("decision_trace")
                .unwrap()
                .user_description
        )
    );
    let explanation = out.stdout.find("// Decision Trace").unwrap();
    let first_use = out.stdout.find("about decision_trace").unwrap();
    assert!(explanation < first_use);
    let resolved = enzyme_spec::resolve(vec![parsed], &f.vault_path()).unwrap();
    let vault = &resolved.vaults[0];
    assert!(
        vault
            .readings
            .iter()
            .any(|r| r.entity == "#focus" && r.profile == "decision_trace")
    );
    assert!(
        vault
            .readings
            .iter()
            .find(|r| r.entity == "#focus")
            .unwrap()
            .definition
            .is_none(),
        "built-in profile references must not become frozen custom definitions"
    );
    assert!(out.stdout.contains("sample by recency"));
    assert!(!out.stdout.contains("sample across time"));
    assert!(!out.stdout.contains("favor "));
    assert_eq!(vault.learning.budget, Some(3));
    assert_eq!(vault.learning.sample.as_deref(), Some("across_time"));
    assert!(
        vault
            .readings
            .iter()
            .all(|r| r.learning.favor.as_deref() == Some("recent")),
        "explicit recent policy must survive Jev's equal proposal"
    );
    assert!(vault.exclusions.contains(&"private".into()));
    assert!(
        vault
            .retrieval
            .as_ref()
            .unwrap()
            .iter()
            .any(|s| s.contains("grep"))
    );
    // Install the reviewed proposal in the fixture and exercise the real config loader.
    fs::write(&file, &out.stdout).unwrap();
    let inspect = f
        .cli()
        .args(["spec", "inspect"])
        .arg(f.vault_path().to_string_lossy())
        .run();
    inspect.assert_success();
    assert!(inspect.stdout.contains("decision_trace"));
    let inspected: serde_json::Value = serde_json::from_str(&inspect.stdout).unwrap();
    assert!(
        inspected["reading_program"]
            .as_str()
            .unwrap()
            .contains("sample by recency")
    );
    let again = f
        .cli()
        .args(["compile", "--output"])
        .arg(draft.to_string_lossy())
        .run();
    again.assert_failure();
    assert_eq!(fs::read_to_string(&draft).unwrap(), out.stdout);
    let colored = f
        .cli()
        .args(["compile", "-v", "--dry-run", "--color", "always"])
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.uri())
        .run();
    colored.assert_success();
    assert!(colored.stdout.contains("\x1b[1;36mlearn\x1b[0m"));
    assert!(colored.stdout.contains("\x1b[35mdecision_trace\x1b[0m"));
    assert!(colored.stdout.contains("\x1b[32m"));
    assert!(colored.stderr.is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), out.stdout);
    fs::write(&file, format!("{}\nsettings {{ updates disabled }}\nvault \"/sibling\" {{ learn questions from tag \"shared\" about decision_trace }}\n", out.stdout)).unwrap();
    let quiet = f
        .cli()
        .arg("compile")
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.uri())
        .run();
    quiet.assert_success();
    assert!(quiet.stdout.is_empty());
    assert_eq!(quiet.stderr.lines().count(), 1);
    assert!(quiet.stderr.starts_with("Saved proposal: "));
    assert_eq!(
        enzyme_spec::source_files(&f.enzyme_home().join("configs"))
            .unwrap()
            .len(),
        1
    );
    let saved_program = enzyme_spec::parse(&fs::read_to_string(&file).unwrap()).unwrap();
    // compile writes the generated Markdown vault as a `workspace` block,
    // replaces it in place, and leaves the sibling `vault` block untouched.
    assert_eq!(saved_program.vaults.len(), 1);
    assert_eq!(saved_program.workspaces.len(), 1);
    assert!(saved_program.workspaces[0].markdown_path().is_some());
    assert_eq!(saved_program.settings.updates, Some(false));
    assert!(saved_program.vaults.iter().any(|v| v.path == "/sibling"));
    fs::write(&file, "vault broken {").unwrap();
    let invalid = f.cli().arg("compile").run();
    invalid.assert_failure();
    assert_eq!(fs::read_to_string(&file).unwrap(), "vault broken {");
}

#[test]
fn compile_requires_jev_instead_of_silently_using_scan_defaults() {
    let f = VaultFixture::new();
    let out = f.cli().arg("compile").env("OPENAI_API_KEY", "").run();
    out.assert_failure();
    assert!(out.stdout.is_empty());
    assert!(!f.vault_path().join(".enzyme").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_dry_run_leaves_new_workspace_unconfigured_then_default_saves() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(JevDecisions)
        .mount(&server)
        .await;
    let f = VaultFixture::new();
    let dry = f
        .cli()
        .args(["compile", "--dry-run"])
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.uri())
        .run();
    dry.assert_success();
    assert!(dry.stderr.is_empty());
    assert!(dry.stdout.contains("learn questions from"));
    assert!(!f.enzyme_home().join("configs").exists());
    assert!(!f.vault_path().join(".enzyme").exists());
    let saved = f
        .cli()
        .args(["compile", "-v", "--color", "never"])
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.uri())
        .run();
    saved.assert_success();
    let files = enzyme_spec::source_files(&f.enzyme_home().join("configs")).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(fs::read_to_string(&files[0].0).unwrap(), saved.stdout);
    let inspect = f
        .cli()
        .args(["spec", "inspect"])
        .arg(f.vault_path().to_string_lossy())
        .run();
    inspect.assert_success();
    assert!(inspect.stdout.contains("decision_trace"));
}

#[test]
fn selection_is_honored_by_init_inspect_and_prompt_dump() {
    let f = VaultFixture::empty();
    let llm = MockLlm::start();
    llm.set_ok_content(r#"{"catalysts":["Which migration tradeoff preserves evidence?","What alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#);
    for name in ["frequent", "recent"] {
        f.write_note(
            &format!("efforts/{name}.md"),
            &format!("# {name}\nAn ongoing migration effort."),
        );
    }
    let now = chrono::Utc::now();
    for (name, count, age) in [("frequent", 6, 180), ("recent", 3, 0)] {
        let date = (now - chrono::Duration::days(age)).format("%Y-%m-%d");
        for i in 0..count {
            f.write_note(&format!("notes/{name}-{i}.md"), &format!("---\ncreated: {date}\n---\n# Migration decision\n[[{name}]]\n{}", "The team chose a reversible migration with auditable source evidence. Rejected alternatives traded traceability for speed. The decision depends on keeping old records available for comparison. ".repeat(20)));
        }
    }
    let directory = f.enzyme_home().join("configs");
    fs::create_dir_all(&directory).unwrap();
    let file = directory.join("vault.enzyme");
    let source = format!(
        r#"
settings {{ generation hosted updates disabled }}
profile custom {{ seek "SELECTION_PROFILE: continuity" notice ["evidence"] }}
learning {{ question budget 3 }}
vault {:?} {{ learn questions from folder "efforts" including linked pages about custom
 select 1 by frequency }}
"#,
        f.vault_path().to_string_lossy()
    );
    fs::write(&file, &source).unwrap();
    let init = f
        .cli()
        .args(["init", "--quiet", "--use-env-llm"])
        .env_llm(&llm)
        .run();
    init.assert_success();
    let db = enzyme_core::db::Database::open(&f.vault_enzyme_dir().join("enzyme.db")).unwrap();
    assert!(
        !db.get_catalysts_for_entity("frequent").unwrap().is_empty(),
        "{}",
        init.combined()
    );
    assert!(db.get_catalysts_for_entity("recent").unwrap().is_empty());
    assert!(
        llm.requests()
            .iter()
            .any(|r| r.contains("SELECTION_PROFILE"))
    );
    let inspect = f
        .cli()
        .args(["spec", "inspect"])
        .arg(f.vault_path().to_string_lossy())
        .run();
    inspect.assert_success();
    let value: serde_json::Value = serde_json::from_str(&inspect.stdout).unwrap();
    assert_eq!(
        value["readingSpec"]["readings"][0]["learning"]["selection"]["amount"]["count"],
        1
    );
    fs::write(
        &file,
        source.replace("select 1 by frequency", "select 50% by recency up to 1"),
    )
    .unwrap();
    let dump_file = f.root().join("prompts.jsonl");
    let dump = f
        .cli()
        .args(["dump-prompts", "--output"])
        .arg(dump_file.to_string_lossy())
        .run();
    dump.assert_success();
    let rows: Vec<serde_json::Value> = fs::read_to_string(&dump_file)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(rows.iter().any(|r| r["entity"] == "recent"), "{rows:?}");
    assert!(!rows.iter().any(|r| r["entity"] == "frequent"));
    assert!(rows.iter().any(|r| {
        r["user_prompt"]
            .as_str()
            .is_some_and(|s| s.contains("SELECTION_PROFILE"))
    }));
}
