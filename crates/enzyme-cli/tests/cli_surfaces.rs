//! e2e: explicit generator choice (`--llm`), versioned JSON surfaces, and the
//! `--workspace` flag with its `--collection` alias.
//!
//! Every run uses a temp `ENZYME_HOME`, the canary vault (or a SQLite-backed
//! named workspace), a mock OpenAI-compatible LLM, and a mock Enzyme API that
//! counts hosted calls. Nothing reaches the network.

mod support;

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

const EXACT_PHRASE: &str = "phased array kit";

fn catalyst_content() -> String {
    serde_json::json!({
        "catalysts": [
            "What does the antenna mounting decision trade away for reliability?",
            "Where does the irrigation plan assume water that may not arrive?",
            "Which retreat conversations reshape the next season of projects?",
            "How does tool scarcity in the garage steer which builds happen?",
            "Why do daily notes return to the same unfinished commitments?",
            "Against what constraint does the garden schedule hold its shape?",
            "Whose advice keeps resurfacing when a build stalls?",
        ],
        "topicNames": ["tradeoffs", "water", "retreat", "tools", "commitments", "schedule", "advice"],
    })
    .to_string()
}

fn json_stdout(out: &CliOutput) -> Value {
    assert!(out.success, "command failed:\n{}", out.combined());
    serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{}", out.stdout))
}

fn file_digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

/// Index the canary vault with catalysts from the mock env LLM.
fn initialized_with_catalysts() -> (VaultFixture, MockLlm) {
    let fixture = VaultFixture::new();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    fixture
        .cli()
        .args(["init", "--quiet", "--llm", "env"])
        .env_llm(&llm)
        .run()
        .assert_success();
    assert!(llm.request_count() > 0, "setup never reached the mock LLM");
    (fixture, llm)
}

// ── --llm ────────────────────────────────────────────────────────────────

#[test]
fn llm_env_uses_only_openai_env_never_enzyme_auth_or_cache() {
    let fixture = VaultFixture::new();
    // Logged in with a hosted lease ready: env mode must ignore both.
    fixture.login("k-hosted-test");
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config("hosted-key", &llm.base_url(), "hosted-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet", "--llm", "env"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    out.assert_success();
    assert!(llm.request_count() > 0, "{}", out.combined());
    assert!(
        llm.auth_headers()
            .iter()
            .all(|header| header.contains("sk-test-env-key")),
        "env mode sent a non-env key: {:?}",
        llm.auth_headers()
    );
    assert_eq!(api.free_config_calls(), 0, "env mode fetched hosted config");
    assert_eq!(api.deduct_calls(), 0, "env mode touched hosted credits");
    assert!(
        !fixture.api_cache_exists(),
        "env mode wrote the shared hosted lease cache"
    );
}

#[test]
fn use_env_llm_is_an_alias_of_llm_env() {
    let fixture = VaultFixture::new();
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    fixture
        .cli()
        .args(["init", "--quiet", "--use-env-llm"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run()
        .assert_success();
    assert!(llm.request_count() > 0);
    assert_eq!(api.free_config_calls(), 0);

    let conflict = fixture
        .cli()
        .args(["init", "--use-env-llm", "--llm", "hosted"])
        .run();
    assert_eq!(conflict.code, Some(2), "{}", conflict.combined());
}

#[test]
fn llm_env_without_key_fails_before_touching_the_index() {
    let fixture = VaultFixture::new();
    let out = fixture.cli().args(["init", "--llm", "env"]).run();
    out.assert_failure().assert_contains("OPENAI_API_KEY");
    assert_eq!(out.code, Some(3), "{}", out.combined());
    assert!(
        !fixture.vault_enzyme_dir().join("enzyme.db").exists(),
        "a failed env preflight must not create the index"
    );
}

#[test]
fn llm_local_without_model_fails_clearly_and_never_falls_back() {
    let fixture = VaultFixture::new();
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    api.set_free_config("hosted-key", &llm.base_url(), "hosted-model");

    let out = fixture
        .cli()
        .args(["init", "--llm", "local"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    out.assert_failure().assert_contains("--llm local");
    assert_eq!(
        llm.request_count(),
        0,
        "local mode fell back to a remote LLM"
    );
    assert_eq!(api.free_config_calls(), 0, "local mode fell back to hosted");
    assert!(!fixture.vault_enzyme_dir().join("enzyme.db").exists());
}

#[test]
fn llm_hosted_ignores_env_credentials() {
    let fixture = VaultFixture::new();
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config("hosted-key", &llm.base_url(), "hosted-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet", "--llm", "hosted"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    out.assert_success();
    assert!(api.free_config_calls() > 0, "{}", out.combined());
    assert!(llm.request_count() > 0, "{}", out.combined());
    assert!(
        llm.auth_headers()
            .iter()
            .all(|header| header.contains("hosted-key")),
        "hosted mode used the env key: {:?}",
        llm.auth_headers()
    );
}

#[test]
fn llm_none_indexes_and_refreshes_without_generation_or_network() {
    let fixture = VaultFixture::new();
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config("hosted-key", &llm.base_url(), "hosted-model");

    let init = json_stdout(
        &fixture
            .cli()
            .args(["init", "--quiet", "--llm", "none"])
            .enzyme_api(&api)
            .env_llm(&llm)
            .run(),
    );
    assert_eq!(init["catalysts_generated"], 0);
    assert!(init["indexing"]["files_discovered"].as_u64().unwrap() > 0);
    assert_eq!(init["warnings"], serde_json::json!([]));

    // `refresh --quiet` otherwise spawns a detached binary-update check that
    // writes the same index; keep this run about generation only.
    std::fs::write(
        fixture.enzyme_home().join("config.toml"),
        "[update]\nauto = false\n",
    )
    .unwrap();
    fixture.write_note("projects/new.md", "# New\n\nA fresh #radio note.\n");
    let refresh = json_stdout(
        &fixture
            .cli()
            .args(["refresh", "--quiet", "--llm", "none"])
            .enzyme_api(&api)
            .env_llm(&llm)
            .run(),
    );
    assert_eq!(refresh["background_spawned"], false);
    assert_eq!(refresh["epoch"]["scheduled"], false);

    assert_eq!(llm.request_count(), 0, "--llm none called an LLM");
    assert_eq!(
        api.free_config_calls(),
        0,
        "--llm none fetched hosted config"
    );
}

// ── search / catalyze ────────────────────────────────────────────────────

#[test]
fn search_json_returns_catalyst_direct_and_exact_hits_read_only() {
    let (fixture, _llm) = initialized_with_catalysts();
    let db = fixture.vault_enzyme_dir().join("enzyme.db");
    let before = file_digest(&db);

    let out = json_stdout(
        &fixture
            .cli()
            .args(["search", "antenna mounting decision", "--json", "-n", "3"])
            .args(["--phrase", EXACT_PHRASE])
            .run(),
    );

    assert_eq!(out["schema"], "enzyme.search.v1");
    assert_eq!(out["bridged"], "ready");
    assert!(out["document_count"].as_u64().unwrap() > 0);
    assert_eq!(out["phrase"], EXACT_PHRASE);
    let catalyst_hits = out["catalyst_hits"].as_array().unwrap();
    assert!(!catalyst_hits.is_empty(), "{out:#}");
    assert!(catalyst_hits.len() <= 3);
    let first = &catalyst_hits[0];
    for key in [
        "path",
        "score",
        "content",
        "via_catalyst_id",
        "via_catalyst_text",
    ] {
        assert!(
            first.get(key).is_some(),
            "catalyst hit lacks {key}: {first:#}"
        );
    }
    assert!(first["via_catalyst_id"].is_string(), "{first:#}");
    assert!(
        !out["direct_hits"].as_array().unwrap().is_empty(),
        "{out:#}"
    );
    assert!(
        !out["top_catalysts"].as_array().unwrap().is_empty(),
        "{out:#}"
    );
    let exact = out["exact_hits"].as_array().unwrap();
    assert!(
        exact
            .iter()
            .any(|hit| hit["path"] == "projects/heliograph-antenna.md"
                && hit["via_catalyst_id"].is_null()),
        "{out:#}"
    );

    assert_eq!(file_digest(&db), before, "search wrote the index");
}

#[test]
fn search_json_without_catalysts_reports_unbridged_direct_hits() {
    let fixture = VaultFixture::new();
    fixture
        .cli()
        .args(["init", "--quiet", "--llm", "none"])
        .run()
        .assert_success();

    let out = json_stdout(&fixture.cli().args(["search", EXACT_PHRASE, "--json"]).run());
    assert_eq!(out["bridged"], "no_generator");
    assert_eq!(out["catalyst_hits"], serde_json::json!([]));
    assert!(
        !out["direct_hits"].as_array().unwrap().is_empty(),
        "{out:#}"
    );
    assert!(!out["exact_hits"].as_array().unwrap().is_empty(), "{out:#}");
}

#[test]
fn search_on_uninitialized_vault_fails_without_creating_an_index() {
    let fixture = VaultFixture::new();
    let out = fixture.cli().args(["search", "anything", "--json"]).run();
    out.assert_failure().assert_contains("enzyme init");
    assert_eq!(out.code, Some(3), "{}", out.combined());
    assert!(!fixture.vault_enzyme_dir().join("enzyme.db").exists());
}

#[test]
fn catalyze_on_uninitialized_vault_prints_envelope_and_exits_3() {
    let fixture = VaultFixture::new();
    let out = fixture.cli().args(["catalyze", "anything", "--json"]).run();
    assert_eq!(out.code, Some(3), "{}", out.combined());
    let envelope: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert!(envelope["error"].as_str().unwrap().contains("enzyme init"));
    assert!(!fixture.vault_enzyme_dir().join("enzyme.db").exists());
}

#[test]
fn init_whose_catalysts_all_fail_exits_5_with_summary() {
    let fixture = VaultFixture::new();
    let llm = MockLlm::start();
    llm.set_response(500, r#"{"error":{"message":"down","type":"server_error"}}"#);
    let out = fixture
        .cli()
        .args(["init", "--quiet", "--llm", "env"])
        .env_llm(&llm)
        .run();
    assert_eq!(out.code, Some(5), "{}", out.combined());
    let summary: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(summary["status"], "failed", "{summary:#}");
    assert!(summary["indexing"]["files_discovered"].as_u64().unwrap() > 0);
    // The index is still built and searchable.
    let search = json_stdout(&fixture.cli().args(["search", "antenna", "--json"]).run());
    assert!(search["document_count"].as_u64().unwrap() > 0);
}

#[test]
fn catalyze_json_forces_the_json_envelope() {
    let (fixture, _llm) = initialized_with_catalysts();
    let out = json_stdout(&fixture.cli().args(["catalyze", "antenna", "--json"]).run());
    assert_eq!(out["search_strategy"], "hybrid");
    assert!(!out["results"].as_array().unwrap().is_empty());

    let legacy = json_stdout(
        &fixture
            .cli()
            .args(["catalyze", "antenna", "--json", "--routing", "catalysts"])
            .run(),
    );
    assert_eq!(legacy["search_strategy"], "catalyze");
    assert!(legacy["results"].is_array());
}

// ── status ───────────────────────────────────────────────────────────────

#[test]
fn status_json_reports_readiness_and_index_manifest() {
    let (fixture, _llm) = initialized_with_catalysts();
    let out = json_stdout(&fixture.cli().args(["status", "--json"]).run());

    assert_eq!(out["schema"], "enzyme.status.v1");
    assert_eq!(out["initialized"], true);
    assert!(out["documents"].as_i64().unwrap() > 0);
    let selection = &out["selection"];
    assert!(selection["selected"].as_u64().unwrap() > 0, "{out:#}");
    assert!(selection["ready"].as_u64().unwrap() > 0, "{out:#}");
    let entity = &selection["entities"][0];
    for key in ["name", "type", "state", "catalysts"] {
        assert!(entity.get(key).is_some(), "entity lacks {key}: {entity:#}");
    }
    assert!(out["index"]["built_by"].is_string(), "{out:#}");
    assert!(out["index"]["epoch"].get("active_epoch_id").is_some());
    assert!(out["sources"].is_array());

    // Text output is still the human rendering.
    let text = fixture.cli().arg("status").run();
    text.assert_success();
    assert!(serde_json::from_str::<Value>(text.stdout.trim()).is_err());
}

#[test]
fn status_json_explains_entities_without_catalysts() {
    let fixture = VaultFixture::new();
    fixture
        .cli()
        .args(["init", "--quiet", "--llm", "none"])
        .run()
        .assert_success();

    let pending = json_stdout(&fixture.cli().args(["status", "--json"]).run());
    assert_eq!(pending["selection"]["ready"], 0, "{pending:#}");
    assert!(
        pending["selection"]["pending"].as_u64().unwrap() > 0,
        "{pending:#}"
    );

    // Thin-context thresholds above the corpus size turn every job into a skip.
    let skipped = json_stdout(
        &fixture
            .cli()
            .args(["status", "--json"])
            .env("ENZYME_MIN_CONTEXT_DOCUMENTS", "1000")
            .run(),
    );
    let entities = skipped["selection"]["entities"].as_array().unwrap();
    assert!(!entities.is_empty());
    for entity in entities {
        assert_eq!(entity["state"], "skipped", "{entity:#}");
        assert_eq!(entity["skip_reason"]["kind"], "thin_context", "{entity:#}");
    }
}

#[test]
fn status_json_reports_markdown_freshness() {
    let (fixture, _llm) = initialized_with_catalysts();
    let fresh = json_stdout(&fixture.cli().args(["status", "--json"]).run());
    assert_eq!(fresh["markdown"]["stale"], false, "{fresh:#}");
    assert!(fresh["markdown"]["files_discovered"].as_u64().unwrap() > 0);

    fixture.write_note("brand-new.md", "# Brand new\nUnindexed thoughts.\n");
    fn first_note(dir: &Path) -> Option<std::path::PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .ok()?
            .map(|entry| entry.unwrap().path())
            .filter(|path| !path.ends_with(".enzyme") && !path.ends_with("brand-new.md"))
            .collect();
        entries.sort();
        entries.iter().find_map(|path| {
            if path.is_dir() {
                first_note(path)
            } else {
                path.extension().is_some_and(|ext| ext == "md").then(|| path.clone())
            }
        })
    }
    let existing = first_note(&fixture.vault_path()).expect("canary vault has a note");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .append(true)
        .open(&existing)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let stale = json_stdout(&fixture.cli().args(["status", "--json"]).run());
    assert_eq!(stale["markdown"]["stale"], true, "{stale:#}");
    assert_eq!(stale["markdown"]["files_new"], 1, "{stale:#}");
    assert_eq!(stale["markdown"]["files_modified"], 1, "{stale:#}");
    assert_eq!(stale["markdown"]["files_deleted"], 0, "{stale:#}");
}

#[test]
fn status_json_reports_outdated_schema_without_migrating() {
    let (fixture, _llm) = initialized_with_catalysts();
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute_batch("DROP TABLE doc_links; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    let before = file_digest(&db_path);
    let out = json_stdout(&fixture.cli().args(["status", "--json"]).run());
    assert_eq!(out["schema_outdated"], true, "{out:#}");
    assert_eq!(out["update"]["status"], "schema_outdated", "{out:#}");
    assert_eq!(file_digest(&db_path), before, "status must not migrate the index");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let restored: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'doc_links'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(restored, 0, "status recreated a dropped table");
    drop(db);

    let search = fixture.cli().args(["search", "antenna", "--json"]).run();
    assert_eq!(search.code, Some(3), "{}", search.combined());
    search.assert_contains("older enzyme");
    assert_eq!(file_digest(&db_path), before, "search must not migrate the index");
}

#[test]
fn status_json_on_uninitialized_vault_answers_not_initialized() {
    let fixture = VaultFixture::new();
    let out = json_stdout(&fixture.cli().args(["status", "--json"]).run());
    assert_eq!(out["schema"], "enzyme.status.v1");
    assert_eq!(out["initialized"], false);
}

// ── spec profiles / model list ───────────────────────────────────────────

#[test]
fn spec_profiles_and_model_list_emit_versioned_json() {
    let fixture = VaultFixture::new();
    let profiles = json_stdout(&fixture.cli().args(["spec", "profiles", "--json"]).run());
    assert_eq!(profiles["schema"], "enzyme.profiles.v1");
    let list = profiles["profiles"].as_array().unwrap();
    assert!(list.iter().any(|p| p["default"] == true));
    for key in ["key", "display_name", "description", "definition"] {
        assert!(list[0].get(key).is_some(), "profile lacks {key}");
    }
    assert!(!profiles["formats"].as_array().unwrap().is_empty());

    let models = json_stdout(&fixture.cli().args(["model", "list", "--json"]).run());
    assert_eq!(models["schema"], "enzyme.models.v1");
    let models_dir = fixture.enzyme_home().join("models");
    assert_eq!(models["models_dir"], models_dir.display().to_string());
    let first = &models["models"][0];
    for key in ["name", "installed", "url", "sha256", "size_bytes"] {
        assert!(first.get(key).is_some(), "model lacks {key}: {first:#}");
    }
}

// ── --workspace / --collection ───────────────────────────────────────────

/// A home declaring one SQLite-backed named workspace, `messages`.
fn named_workspace_home() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let source_db = root.path().join("messages.db");
    let source = rusqlite::Connection::open(&source_db).unwrap();
    source
        .execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sent_at INTEGER NOT NULL,
                                    body TEXT NOT NULL, thread TEXT NOT NULL);
             INSERT INTO messages VALUES (1, 1700000000000, 'first message', 'Thread A');",
        )
        .unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"
[update]
auto = false

[workspaces.messages.sources.messages]
db = "{}"
query = "SELECT id, sent_at, body, thread FROM messages ORDER BY id"
roles = {{ id = "id", when = "sent_at", what = "body", where = "thread" }}
timestamp = {{ unit = "ms" }}
"#,
            source_db.display()
        ),
    )
    .unwrap();
    root
}

fn run_in_home(root: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
    command
        .args(args)
        .env("ENZYME_HOME", root.join("home"))
        .env("HOME", root)
        .env("ENZYME_FREE_CONFIG_URL", "http://127.0.0.1:9/free-config")
        .env("ENZYME_API_URL", "http://127.0.0.1:9")
        .env("ENZYME_UPDATE_API_BASE", "http://127.0.0.1:9")
        .env("ENZYME_UPDATE_RELEASE_BASE", "http://127.0.0.1:9")
        .env_remove("OPENAI_API_KEY");
    command.output().unwrap()
}

fn stdout_json(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn workspace_flag_and_collection_alias_address_the_same_workspace() {
    let root = named_workspace_home();
    let root = root.path();

    let init = run_in_home(
        root,
        &[
            "--workspace",
            "messages",
            "init",
            "--quiet",
            "--llm",
            "none",
        ],
    );
    stdout_json(&init);

    let by_workspace = stdout_json(&run_in_home(
        root,
        &["--workspace", "messages", "status", "--json"],
    ));
    let by_collection = stdout_json(&run_in_home(
        root,
        &["status", "--json", "--collection", "messages"],
    ));
    assert_eq!(by_workspace["workspace"], "messages");
    assert_eq!(by_workspace["initialized"], true);
    assert_eq!(by_workspace["database"], by_collection["database"]);
    assert_eq!(by_workspace["documents"], by_collection["documents"]);
    let source = &by_workspace["sources"][0];
    assert_eq!(source["name"], "messages");
    assert_eq!(source["stale"], false, "{source:#}");

    let scan = stdout_json(&run_in_home(
        root,
        &["scan", "--json", "--workspace", "messages"],
    ));
    assert_eq!(scan["schema_version"], "scan.v2");

    let search = stdout_json(&run_in_home(
        root,
        &[
            "--workspace",
            "messages",
            "search",
            "first message",
            "--json",
        ],
    ));
    assert_eq!(search["schema"], "enzyme.search.v1");
    assert!(search["document_count"].as_u64().unwrap() > 0, "{search:#}");
}
