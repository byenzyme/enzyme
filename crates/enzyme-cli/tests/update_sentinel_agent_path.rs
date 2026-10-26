//! Black-box tests for the agent-driven auto-update path.
//!
//! Harness: `crates/enzyme-cli/tests/support/`.
//!
//! # Contract under test
//!
//! Agent-facing entry points (`enzyme refresh --quiet` — the hook hot path
//! also used behind MCP's `ensure_fresh` — plus `enzyme status` and
//! `enzyme init`) consult an update sentinel in the vault DB
//! (`sentinel.binary_version` / `sentinel.schema_fingerprint`, see
//! `enzyme_core::db::update_sentinel`). The refresh JSON `update` key uses a
//! two-state agent contract: `{"status":"ok"}` means nothing to do (an
//! optional `note` is relayed to the user); `{"status":"action_required",
//! "action":…,"reason":…}` means perform or relay the action string.
//!
//! 1. Sentinel matches the running binary → single config read, no extra work
//!    (`update.status == "ok"`).
//! 2. Binary changed but migrations fully reconciled the schema → sentinel
//!    advances for free, the corpus is untouched (`update.status == "ok"`).
//! 3. Binary changed AND the live schema structurally drifted from what this
//!    binary's schema + migrations produce → the index tables are rebuilt on
//!    the current schema while catalysts, auth (`$ENZYME_HOME/auth.json`), and
//!    global config survive, with no model calls (`update.status == "ok"`
//!    with a note saying the questions were kept).
//!
//! Every scenario drives the real `enzyme` binary as a subprocess and asserts
//! on durable state (vault DB, auth file) plus the JSON the agent sees.

mod support;

use enzyme_core::db::Database;

use support::fixture::VaultFixture;
use support::mock::MockLlm;

/// The version compiled into the binary under test (workspace-versioned with
/// this test crate).
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const BINARY_VERSION_KEY: &str = "sentinel.binary_version";

/// A well-formed catalyst payload so `enzyme init` genuinely succeeds instead
/// of failing catalyst generation on unparseable mock content.
fn catalyst_payload() -> String {
    serde_json::json!({
        "catalysts": [
            "the moment a plan survives contact with the actual hardware",
            "keeping a running tally of what the surprise costs actually were",
            "returning to a half-finished build after the season changed",
        ],
        "topic_names": [
            "plans meeting hardware",
            "surprise costs",
            "seasonal returns",
        ],
    })
    .to_string()
}

fn vault_db(fixture: &VaultFixture) -> Database {
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");
    assert!(
        db_path.exists(),
        "expected vault DB at {} — the vault was never initialized",
        db_path.display()
    );
    Database::open(&db_path).expect("open vault db")
}

/// Initialize the canary vault with a healthy mock LLM.
fn init_vault(llm: &MockLlm) -> VaultFixture {
    let fixture = VaultFixture::new();
    fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(llm)
        .run()
        .assert_success();
    fixture
}

/// Run the agent hot path (`refresh --quiet --use-env-llm`) and return its
/// parsed JSON output.
#[track_caller]
fn refresh_quiet(fixture: &VaultFixture, llm: &MockLlm) -> serde_json::Value {
    let out = fixture
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .env_llm(llm)
        .run();
    out.assert_success();
    serde_json::from_str(out.stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "refresh --quiet did not print parseable JSON ({e})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.stdout, out.stderr
        )
    })
}

/// Pretend an older binary wrote this vault by rewinding the stored sentinel
/// version. (The fingerprint is only consulted when versions differ, so this
/// alone models "binary updated, schema unchanged".)
fn rewind_sentinel_version(fixture: &VaultFixture) {
    vault_db(fixture)
        .set_config(BINARY_VERSION_KEY, "0.0.1-old")
        .expect("rewind sentinel version");
}

/// Structurally drift the live schema: an extra column no migration knows
/// about, exactly what a vault written by a different binary looks like when
/// `CREATE TABLE IF NOT EXISTS` + migrations cannot reconcile it.
fn drift_schema(fixture: &VaultFixture) {
    vault_db(fixture)
        .execute_write("ALTER TABLE docs ADD COLUMN legacy_probe TEXT", vec![])
        .expect("add drift column");
}

fn docs_has_drift_column(fixture: &VaultFixture) -> bool {
    let cols = vault_db(fixture)
        .query_read("PRAGMA table_info(\"docs\")", vec![], |row| {
            row.get::<String>(1)
        })
        .expect("table_info(docs)");
    cols.iter().any(|c| c == "legacy_probe")
}

fn sentinel_version(fixture: &VaultFixture) -> Option<String> {
    vault_db(fixture)
        .get_config(BINARY_VERSION_KEY)
        .expect("read sentinel version")
}

fn doc_count(fixture: &VaultFixture) -> i64 {
    vault_db(fixture).get_document_count().expect("doc count")
}

fn catalyst_count(fixture: &VaultFixture) -> usize {
    vault_db(fixture)
        .get_all_catalysts()
        .expect("catalysts")
        .len()
}

/// Scenario 1: freshly-initialized vault → the sentinel matches and the agent
/// path reports `ok` without touching the corpus.
#[test]
fn fresh_vault_reports_up_to_date() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);

    assert_eq!(
        sentinel_version(&fixture).as_deref(),
        Some(CURRENT_VERSION),
        "init must stamp the sentinel with the binary version"
    );

    let json = refresh_quiet(&fixture, &llm);
    assert_eq!(json["update"]["status"].as_str(), Some("ok"));
    assert!(json["update"].get("note").is_none());
}

/// Scenario 2: binary version changed, schema compatible → sentinel advances
/// for free; no regen (corpus rows and unrelated config survive).
#[test]
fn version_bump_without_drift_advances_sentinel_without_regen() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);

    let docs_before = doc_count(&fixture);
    assert!(docs_before > 0, "canary vault should index documents");
    rewind_sentinel_version(&fixture);
    // Canary marker that a full regen (DB reset) would destroy.
    vault_db(&fixture)
        .set_config("test.regen_canary", "survives-incremental")
        .expect("seed canary");

    let json = refresh_quiet(&fixture, &llm);
    assert_eq!(json["update"]["status"].as_str(), Some("ok"));
    assert!(json["update"].get("note").is_none());

    assert_eq!(sentinel_version(&fixture).as_deref(), Some(CURRENT_VERSION));
    assert_eq!(doc_count(&fixture), docs_before);
    assert_eq!(
        vault_db(&fixture)
            .get_config("test.regen_canary")
            .expect("read canary")
            .as_deref(),
        Some("survives-incremental"),
        "a compatible version bump must NOT reset the vault database"
    );
}

/// Scenario 3 (the core auto-update contract): binary version changed and the
/// schema drifted → the agent path rebuilds the index on the current schema,
/// keeps every catalyst without calling the model, tells the user once, and
/// leaves stored auth untouched.
#[test]
fn schema_drift_rebuilds_index_and_keeps_catalysts_and_auth() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);
    fixture.login("sk-test-preserved");
    let auth_path = fixture.enzyme_home().join("auth.json");
    let auth_before = std::fs::read_to_string(&auth_path).expect("read auth.json");
    let catalysts_before = catalyst_count(&fixture);
    assert!(catalysts_before > 0, "canary vault should have catalysts");

    rewind_sentinel_version(&fixture);
    drift_schema(&fixture);
    assert!(docs_has_drift_column(&fixture), "drift setup must stick");
    let requests_before = llm.request_count();

    let json = refresh_quiet(&fixture, &llm);
    assert_eq!(json["update"]["status"].as_str(), Some("ok"));
    assert!(
        json["update"]["note"]
            .as_str()
            .is_some_and(|note| note.contains("rebuilt its search index") && note.contains("kept")),
        "the rebuild must be relayed to the user; got update = {}",
        json["update"]
    );

    assert!(
        !docs_has_drift_column(&fixture),
        "the rebuild must recreate the index on the current schema"
    );
    assert_eq!(
        sentinel_version(&fixture).as_deref(),
        Some(CURRENT_VERSION),
        "the rebuild must stamp the sentinel so the next call is a fast no-op"
    );
    assert!(
        doc_count(&fixture) > 0,
        "the rebuild must re-index the vault"
    );
    assert_eq!(
        catalyst_count(&fixture),
        catalysts_before,
        "an index rebuild must keep catalysts"
    );
    assert_eq!(
        llm.request_count(),
        requests_before,
        "an index rebuild must not call the model"
    );

    let auth_after = std::fs::read_to_string(&auth_path).expect("auth.json must survive");
    assert_eq!(
        auth_before, auth_after,
        "auth config must survive the rebuild"
    );

    let status = fixture.cli().arg("status").run();
    status.assert_success();
    status.assert_contains("rebuilt its search index");

    // And the very next agent call is back on the fast path.
    let json = refresh_quiet(&fixture, &llm);
    assert_eq!(json["update"]["status"].as_str(), Some("ok"));
    assert!(json["update"].get("note").is_none());
}

/// Scenario 4: `enzyme status` reports a pending regen read-only — it must
/// flag `regen_required` without performing the regen itself.
#[test]
fn status_reports_pending_regen_without_performing_it() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);

    rewind_sentinel_version(&fixture);
    drift_schema(&fixture);

    let out = fixture.cli().arg("status").run();
    out.assert_success();
    out.assert_contains("needs rebuilding");

    assert!(
        docs_has_drift_column(&fixture),
        "status must stay read-only and not trigger the regen"
    );
    assert_eq!(sentinel_version(&fixture).as_deref(), Some("0.0.1-old"));
}

/// Scenario 5: an explicit `enzyme init` (no --force) on a drifted vault
/// rebuilds the index on the current schema and keeps its catalysts.
#[test]
fn plain_init_rebuilds_index_and_keeps_catalysts_on_drift() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);

    let catalysts_before = catalyst_count(&fixture);
    rewind_sentinel_version(&fixture);
    drift_schema(&fixture);

    fixture
        .cli()
        .arg("init")
        .arg("--quiet")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run()
        .assert_success();

    assert!(
        !docs_has_drift_column(&fixture),
        "plain init must rebuild the index when the schema drifted"
    );
    assert_eq!(sentinel_version(&fixture).as_deref(), Some(CURRENT_VERSION));
    assert_eq!(catalyst_count(&fixture), catalysts_before);
}

/// Scenario 6: catalysts generated under an older catalyst-logic version → the
/// refresh schedules their regeneration and says why, once.
#[test]
fn catalyst_logic_change_regenerates_in_background_and_says_so_once() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());
    let fixture = init_vault(&llm);
    vault_db(&fixture)
        .execute_write(
            "UPDATE catalyst_epochs
             SET generation_contract_json =
                     json_set(generation_contract_json, '$.prompt_version', 'catalyst-prompts.older'),
                 generation_contract = 'older-contract'
             WHERE epoch_id = (SELECT active_epoch_id FROM catalyst_epoch_state)",
            vec![],
        )
        .expect("age the active epoch");
    vault_db(&fixture)
        .execute_write(
            "UPDATE catalyst_epoch_state SET active_generation_contract = 'older-contract'",
            vec![],
        )
        .expect("age the active contract");

    let json = refresh_quiet(&fixture, &llm);
    assert_eq!(json["update"]["status"].as_str(), Some("ok"));
    assert!(
        json["update"]["note"]
            .as_str()
            .is_some_and(|note| note.contains("generates questions differently")),
        "a catalyst-logic change must be relayed to the user; got update = {}",
        json["update"]
    );
    assert_eq!(
        json["epoch"]["due_reason"].as_str(),
        Some("contractchanged")
    );

    let json = refresh_quiet(&fixture, &llm);
    assert!(
        json["update"].get("note").is_none(),
        "the notice is relayed once; got update = {}",
        json["update"]
    );
}
