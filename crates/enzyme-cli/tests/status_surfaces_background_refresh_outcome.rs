//! e2e regression: status_surfaces_background_refresh_outcome
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`,
//! `support::fixture`, `support::epoch`).
//!
//! # What this pins down (useenzyme/enzyme#11 fail-loud)
//!
//! `enzyme refresh` runs catalyst epoch builds in a detached background worker
//! (`spawn_background_refresh` -> `enzyme refresh --detached --quiet`). On
//! total generation failure the worker correctly keeps the previous active
//! epoch (no data loss), but historically the failure was recorded ONLY in
//! `.enzyme/background_refresh.log`: the foreground refresh exits 0 by design
//! and plain `enzyme status` showed a healthy vault. That silence cost real
//! debugging time.
//!
//! The fail-loud contract, asserted below:
//!
//! 1. When a background epoch build fails, the worker persists a structured
//!    outcome under the vault DB config key `refresh.last_background_outcome`,
//!    and `enzyme status` prints a visible warning naming the failure, the
//!    carried-forward catalysts, and the log to consult — while the previous
//!    catalysts really are still intact.
//! 2. When a later background build succeeds, the same key is overwritten, the
//!    warning disappears, and status shows a quiet "Refreshed:" line instead —
//!    a stale failure must not haunt a healthy vault.

mod support;

use std::time::Duration;

use enzyme_core::db::Database;

use support::epoch::{
    build_epoch_id, wait_for_epoch_terminal, wait_until, WORKER_TERMINAL_TIMEOUT,
};
use support::fixture::{CliOutput, VaultFixture};
use support::mock::MockLlm;

/// Config key the detached worker writes its terminal outcome to
/// (see `crates/enzyme-cli/src/commands/refresh_outcome.rs`).
const OUTCOME_KEY: &str = "refresh.last_background_outcome";

/// The visible marker line prefix `enzyme status` must print on failure.
const FAILURE_MARKER: &str = "Last background refresh failed";

/// A well-formed catalyst payload so init and the recovery refresh genuinely
/// succeed rather than silently degrading.
fn catalyst_payload() -> String {
    serde_json::json!({
        "catalysts": [
            "the moment a plan survives contact with the actual hardware",
            "keeping a running tally of what the surprise costs actually were",
            "returning to a half-finished build after the season changed",
            "scheduling around someone else's stated preference",
            "the difference between what code requires and what fits",
            "noticing a fix only worked because of where it was placed",
            "logging a measurement you do not yet know how to use",
        ],
        "topic_names": [
            "plans meeting hardware",
            "surprise costs",
            "seasonal returns",
            "scheduling around others",
            "code versus fit",
            "placement-dependent fixes",
            "measurements without a use",
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

fn catalyst_count(fixture: &VaultFixture) -> usize {
    vault_db(fixture)
        .get_all_catalysts()
        .expect("read catalysts")
        .len()
}

/// The raw persisted outcome JSON, if any. Reading durable state (not CLI
/// output) proves the record itself exists, independent of formatting.
fn stored_outcome(fixture: &VaultFixture) -> Option<serde_json::Value> {
    let raw = vault_db(fixture)
        .get_config(OUTCOME_KEY)
        .expect("read outcome config key")?;
    Some(serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("stored background refresh outcome is not valid JSON ({e}): {raw}")
    }))
}

/// Poll until the persisted outcome reports `want_status`. The worker writes
/// the outcome around the same time it flips `catalyst_epochs` to a terminal
/// status, so waiting on the epoch alone would race the config write.
#[track_caller]
fn wait_for_outcome_status(fixture: &VaultFixture, want_status: &str) {
    wait_until(
        || {
            stored_outcome(fixture)
                .and_then(|o| o.get("status").and_then(|s| s.as_str().map(String::from)))
                .as_deref()
                == Some(want_status)
        },
        Duration::from_secs(15),
        &format!(
            "vault config key {OUTCOME_KEY:?} to report status {want_status:?} \
             (current: {:?})",
            stored_outcome(fixture)
        ),
    );
}

/// Run `enzyme refresh --use-env-llm --full` (forcing the catalyst epoch due),
/// deterministically retry the detached worker launch (see
/// cache_epoch_stale_path.rs for why the auto-spawned worker can lose a
/// schema-lock race), wait for the epoch to reach a terminal status, and
/// return `(refresh output, epoch_id, terminal status)`.
#[track_caller]
fn force_refresh_and_wait_for_worker(
    fixture: &VaultFixture,
    llm: &MockLlm,
) -> (CliOutput, String, String) {
    let refresh = fixture
        .cli()
        .args(["refresh", "--use-env-llm", "--full"])
        .env_llm(llm)
        .run();

    let stdout_json: serde_json::Value = serde_json::from_str(refresh.stdout.trim())
        .unwrap_or_else(|e| {
            panic!(
                "refresh did not print parseable JSON on stdout ({e}).\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                refresh.stdout, refresh.stderr
            )
        });
    assert_eq!(
        stdout_json["background_spawned"].as_bool(),
        Some(true),
        "TEST IS VACUOUS: refresh reported background_spawned=false, so no worker ran \
         and no background outcome could be produced.\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        refresh.stdout,
        refresh.stderr
    );

    let epoch_id = build_epoch_id(&stdout_json);
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");

    // Deterministic worker-launch retry; no-op if the auto-spawned worker
    // already claimed the epoch.
    let _ = fixture
        .cli()
        .args(["refresh", "--detached", "--quiet", "--use-env-llm"])
        .env_llm(llm)
        .run();

    let status = wait_for_epoch_terminal(&db_path, &epoch_id, WORKER_TERMINAL_TIMEOUT);
    (refresh, epoch_id, status)
}

fn run_status(fixture: &VaultFixture) -> CliOutput {
    fixture.cli().arg("status").run()
}

#[test]
fn status_surfaces_failed_background_refresh_and_clears_on_success() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());

    // ── Init: healthy LLM, catalysts exist, no outcome recorded yet ────────
    let fixture = VaultFixture::new();
    fixture
        .cli()
        .args(["init", "--use-env-llm"])
        .env_llm(&llm)
        .run()
        .assert_success();

    let catalysts_before = catalyst_count(&fixture);
    assert!(
        catalysts_before > 0,
        "init produced no catalysts; a later refresh could not carry anything forward"
    );
    run_status(&fixture)
        .assert_success()
        .assert_not_contains(FAILURE_MARKER);

    // ── A refresh whose completions all fail ──────────────────────────────
    fixture.write_note(
        "daily/2026-08-01.md",
        "---\ndate: 2026-08-01\ntags: [daily]\n---\n\n# 2026-08-01\n\n- CANARY-FAILLOUD1 drip manifold rebuilt.\n",
    );
    llm.set_response(500, r#"{"error":{"message":"upstream exploded"}}"#);

    let (refresh, epoch_id, status) = force_refresh_and_wait_for_worker(&fixture, &llm);
    // Detached-by-design: the foreground refresh hands off and exits 0 even
    // though the build will fail. Status, not the exit code, is fail-loud.
    refresh.assert_success();
    assert_eq!(
        status,
        "failed",
        "expected the forced catalyst epoch {epoch_id} to fail against a broken LLM.\n\
         --- background_refresh.log ---\n{}",
        support::epoch::background_refresh_log(&fixture.vault_enzyme_dir().join("enzyme.db")),
    );
    wait_for_outcome_status(&fixture, "failed");

    // The durable record has the structured shape status surfacing relies on.
    let outcome = stored_outcome(&fixture).expect("failure outcome recorded");
    assert_eq!(outcome["catalysts_generated"], 0, "outcome: {outcome}");
    assert_eq!(
        outcome["carried_forward"].as_u64(),
        Some(catalysts_before as u64),
        "the failed build must report the still-active prior catalysts as carried \
         forward. outcome: {outcome}"
    );
    assert!(
        outcome["error"].as_str().is_some_and(|e| !e.is_empty()),
        "failure outcome must carry an error message. outcome: {outcome}"
    );
    assert!(
        outcome["finished_at_ms"].as_i64().is_some_and(|t| t > 0),
        "outcome: {outcome}"
    );

    // No data loss: the previous catalysts are untouched.
    assert_eq!(
        catalyst_count(&fixture),
        catalysts_before,
        "a failed background build must leave the active catalysts intact"
    );

    // The fail-loud line itself.
    let failed_status = run_status(&fixture);
    failed_status
        .assert_success()
        .assert_contains(FAILURE_MARKER)
        .assert_contains("previous catalysts carried forward")
        .assert_contains(".enzyme/background_refresh.log");

    // ── A later successful refresh clears the stale failure ───────────────
    llm.set_ok_content(&catalyst_payload());
    // Bump the corpus so scheduling creates a fresh epoch instead of holding
    // the failed one inside its 15-minute retry backoff window.
    fixture.write_note(
        "daily/2026-08-02.md",
        "---\ndate: 2026-08-02\ntags: [daily]\n---\n\n# 2026-08-02\n\n- CANARY-FAILLOUD2 regulator reseated at 12 psi.\n",
    );

    let (recovery, recovery_epoch_id, recovery_status) =
        force_refresh_and_wait_for_worker(&fixture, &llm);
    recovery.assert_success();
    assert_eq!(
        recovery_status,
        "active",
        "expected the recovery epoch {recovery_epoch_id} to activate against a healthy \
         LLM.\n--- background_refresh.log ---\n{}",
        support::epoch::background_refresh_log(&fixture.vault_enzyme_dir().join("enzyme.db")),
    );
    wait_for_outcome_status(&fixture, "succeeded");

    let outcome = stored_outcome(&fixture).expect("success outcome recorded");
    assert!(
        outcome["catalysts_generated"].as_u64().is_some_and(|n| n > 0),
        "a successful build must report generated catalysts. outcome: {outcome}"
    );
    assert!(
        outcome.get("error").is_none() || outcome["error"].is_null(),
        "a success outcome must not carry an error. outcome: {outcome}"
    );

    let healthy_status = run_status(&fixture);
    healthy_status
        .assert_success()
        .assert_not_contains(FAILURE_MARKER)
        .assert_contains("Refreshed:");
}
