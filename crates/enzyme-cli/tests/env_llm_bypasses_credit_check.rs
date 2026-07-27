//! e2e regression: env_llm_bypasses_credit_check
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! CLAIM UNDER TEST
//! ----------------
//! `crates/enzyme-cli/src/commands/init.rs:611` fences `init`'s hosted credit
//! deduction behind `ConfigProvider::ServerFallback`. Under the current
//! catalyst-epoch architecture, the *refresh* path's equivalent preflight
//! lives inside `build_and_activate_epoch` (the detached
//! `enzyme refresh --detached --quiet` worker spawned by
//! `run_refresh_interactive`/`run_refresh_fast`), gated the same way. Under
//! `--use-env-llm` the resolved provider is `ConfigProvider::Env`, so the
//! claim is that `deduct_credits` is never reached and the hosted Enzyme API
//! is never consulted for billing at all — whether that work happens inline
//! (`init`) or inside the background worker (`refresh`).
//!
//! These tests hold that claim to account end-to-end: a real `enzyme` subprocess,
//! a real (fake) OpenAI-compatible LLM that answers successfully, and a real
//! (fake) Enzyme API whose `/v1/credits/deduct` is hard-wired to 402 with a zero
//! balance. If the CLI touches hosted credits at all, the counter is non-zero and
//! these tests fail.
//!
//! The user IS logged in (`auth.json` is seeded), so the *only* thing standing
//! between the run and a 402 is the provider fence. Without the login seed the
//! preflight would be skipped for an unrelated reason and the test would be
//! vacuous.

mod support;

use support::epoch::{
    background_refresh_log, build_epoch_id, wait_for_epoch_terminal, WORKER_TERMINAL_TIMEOUT,
};
use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A well-formed catalyst response body in the object format
/// `parse_catalyst_response` prefers (`crates/enzyme-core/src/llm/catalyst_prompt.rs`).
/// A trivial `{}` stub parses as zero catalysts and makes `init` bail before the
/// run is meaningful, so the fake LLM must answer with something real-shaped.
fn catalyst_content() -> String {
    let catalysts = [
        "What does this material cost the people who maintain it over time?",
        "Where does the writing quietly assume continuity that is not guaranteed?",
        "Which commitments here would be expensive to reverse later?",
        "How much of the value depends on attention that is already spoken for?",
        "Why does the framing foreclose alternatives that seem cheaper?",
        "Against what friction does this position hold its shape?",
        "Whose labor becomes invisible once this approach settles in?",
    ];
    let topics = [
        "maintenance cost",
        "assumed continuity",
        "reversal expense",
        "attention budget",
        "foreclosed options",
        "structural friction",
        "invisible labor",
    ];
    serde_json::json!({ "catalysts": catalysts, "topicNames": topics }).to_string()
}

/// Render every observable fact about a run, so a failure explains itself.
fn diagnostics(label: &str, api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "\n=== {label} ===\n\
         deduct calls (POST /v1/credits/deduct): {}\n\
         deduct request bodies: {:?}\n\
         free-config calls (GET {}): {}\n\
         mock LLM requests: {}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        api.deduct_calls(),
        api.deduct_requests(),
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
        llm.request_count(),
        out.code,
        out.success,
        out.stdout,
        out.stderr,
    )
}

/// `enzyme init --use-env-llm` against a hosted API that would 402 on any
/// credit deduction. Correct behavior: the deduct endpoint is never called.
#[test]
fn init_with_env_llm_never_deducts_hosted_credits() {
    let fixture = VaultFixture::new();
    // Logged in: the credit preflight's `AuthToken::load` will succeed, so the
    // ServerFallback provider fence is the only thing that can stop the deduct.
    fixture.login("k-hosted-test");

    let api = MockEnzymeApi::start();
    // Any deduction attempt is fatal on the server side: 402, zero remaining.
    api.set_deduct_always_402(0.0);
    api.set_credit_balance(0.0);

    // A healthy env LLM, so catalyst generation genuinely runs.
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics("init --use-env-llm", &api, &llm, &out);

    // Non-vacuity: the run must actually have reached LLM work. If the LLM was
    // never called, the credit gate was never in play and a zero deduct count
    // would prove nothing.
    assert!(
        llm.request_count() > 0,
        "TEST IS VACUOUS: the env LLM was never called, so the run never reached \
         catalyst generation and the credit gate was never exercised.{diag}"
    );

    // THE CLAIM: `--use-env-llm` must never consult hosted credits.
    assert_eq!(
        api.deduct_calls(),
        0,
        "REGRESSION: `--use-env-llm` consulted hosted credits.\n\
         expected: 0 calls to POST /v1/credits/deduct\n\
         actual:   {} call(s)\n\
         init.rs:611 is supposed to fence the credit preflight behind \
         ConfigProvider::ServerFallback, but the Env provider reached it anyway.{diag}",
        api.deduct_calls(),
    );

    // ...and must not fetch hosted LLM credentials either: with env credentials
    // supplied there is no reason to talk to the hosted free-config endpoint.
    assert_eq!(
        api.free_config_calls(),
        0,
        "REGRESSION: `--use-env-llm` fetched hosted LLM credentials.\n\
         expected: 0 calls to GET {}\n\
         actual:   {} call(s){diag}",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
    );

    // The run must not be derailed by hosted billing in any form.
    assert!(
        !out.contains("app.enzyme.garden"),
        "REGRESSION: a hosted-billing message surfaced during a `--use-env-llm` run.{diag}"
    );
    assert!(
        !out.contains("credits remaining"),
        "REGRESSION: a hosted credit balance was reported during a `--use-env-llm` run.{diag}"
    );
    assert!(
        out.success,
        "expected `init --use-env-llm` to succeed against a healthy env LLM.{diag}"
    );
}

/// Same claim for the refresh path's separate preflight, which now lives
/// inside `build_and_activate_epoch` — the detached
/// `enzyme refresh --detached --quiet` worker that `run_refresh_interactive`
/// spawns once a catalyst epoch is due (see `crates/enzyme-core/src/pipeline/epoch.rs`).
/// Catalyst (LLM) work no longer runs inline in `refresh`, so this test must
/// force the epoch to be due (`--full`; there is no more entity-mention
/// staleness threshold) and wait for the spawned worker to reach a terminal
/// status before it can observe whether hosted credits were touched.
#[test]
fn refresh_with_env_llm_never_deducts_hosted_credits() {
    let fixture = VaultFixture::new();
    fixture.login("k-hosted-test");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    // Seed the index while the hosted API still answers deducts happily, so a
    // failure here cannot be blamed on the setup run.
    let init_out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    assert!(
        init_out.success,
        "setup `init --use-env-llm` failed; refresh assertions would be meaningless.{}",
        diagnostics("setup init", &api, &llm, &init_out)
    );

    // Now arm the 402 and give refresh something to chew on.
    api.set_deduct_always_402(0.0);
    api.set_credit_balance(0.0);
    for i in 0..4 {
        fixture.write_note(
            &format!("projects/new-project-{i}.md"),
            &format!(
                "# New Project {i}\n\nFresh project material added after init so refresh \
                 has genuinely new local work to do.\n"
            ),
        );
    }
    let llm_calls_after_init = llm.request_count();

    let out = fixture
        .cli()
        .args(["refresh", "--use-env-llm", "--full"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics("refresh --use-env-llm --full", &api, &llm, &out);

    let stdout_json: serde_json::Value = serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("refresh did not print parseable JSON on stdout ({e}).{diag}"));
    assert_eq!(
        stdout_json["epoch"]["due"].as_bool(),
        Some(true),
        "TEST IS VACUOUS: `--full` did not make the catalyst epoch due.{diag}"
    );
    assert_eq!(
        stdout_json["background_spawned"].as_bool(),
        Some(true),
        "TEST IS VACUOUS: refresh reported background_spawned=false, so no worker ran.{diag}"
    );

    // Deterministically finish the scheduled epoch. `spawn_background_refresh`
    // double-forks a worker while this test's own `refresh` process may still
    // hold its own open SQLite connection to the same file, and the worker's
    // `Database::open()` can lose a schema-lock race against it and die
    // immediately (observed empirically). Production self-heals this — a
    // scheduled-but-unclaimed epoch reports `launch_needed: true` again next
    // time — so invoke the identical detached-worker subcommand ourselves
    // once our own `refresh` process has fully exited (no more competing
    // connection): a no-op if the auto-spawned worker already claimed the
    // epoch, a reliable retry if it did not.
    let _ = fixture
        .cli()
        .args(["refresh", "--detached", "--quiet", "--use-env-llm"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let epoch_id = build_epoch_id(&stdout_json);
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");
    let status = wait_for_epoch_terminal(&db_path, &epoch_id, WORKER_TERMINAL_TIMEOUT);
    let worker_log = background_refresh_log(&db_path);

    // Non-vacuity: refresh's worker must have reached LLM work of its own,
    // otherwise a zero deduct count says nothing about the refresh preflight.
    // (The epoch's own success/failure is not this test's concern — only
    // whether hosted billing was ever touched while doing the attempt.)
    assert!(
        llm.request_count() > llm_calls_after_init,
        "TEST IS VACUOUS: `refresh --use-env-llm`'s background worker (epoch {epoch_id}, \
         ended in status {status:?}) issued no LLM requests of its own (count was \
         {llm_calls_after_init} after init and is {} now), so the refresh credit \
         preflight in build_and_activate_epoch was never exercised.{diag}\n\
         --- background_refresh.log ---\n{worker_log}",
        llm.request_count(),
    );

    assert_eq!(
        api.deduct_calls(),
        0,
        "REGRESSION: `refresh --use-env-llm` consulted hosted credits.\n\
         expected: 0 calls to POST /v1/credits/deduct across init+refresh\n\
         actual:   {} call(s)\n\
         build_and_activate_epoch is supposed to fence the refresh credit preflight \
         behind ConfigProvider::ServerFallback.{diag}\n\
         --- background_refresh.log ---\n{worker_log}",
        api.deduct_calls(),
    );
    assert_eq!(
        api.free_config_calls(),
        0,
        "REGRESSION: `refresh --use-env-llm` fetched hosted LLM credentials.\n\
         expected: 0 calls to GET {}\n\
         actual:   {} call(s){diag}",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
    );
    assert!(
        !out.contains("app.enzyme.garden"),
        "REGRESSION: a hosted-billing message surfaced during a `--use-env-llm` refresh.{diag}"
    );
}

/// POSITIVE CONTROL — not a bug reproduction.
///
/// The two tests above assert an *absence* (`deduct_calls == 0`). That is only
/// meaningful if the counter can fire at all. This runs the same vault in HOSTED
/// mode (no `--use-env-llm`), where the free-config endpoint hands back the mock
/// LLM and the provider resolves to `ServerFallback`. The credit preflight is
/// then expected to run, proving the mock's deduct route is genuinely wired to
/// the code path the other tests claim is skipped.
#[test]
fn hosted_mode_control_does_deduct_credits() {
    let fixture = VaultFixture::new();
    fixture.login("k-hosted-test");

    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    let api = MockEnzymeApi::start();
    api.set_credit_balance(100.0);
    api.set_free_config("enz-free-test", &llm.base_url(), "test-model");

    let out = fixture.cli().arg("init").enzyme_api(&api).run();

    let diag = diagnostics("init (hosted control)", &api, &llm, &out);

    assert!(
        api.deduct_calls() > 0,
        "CONTROL FAILED: hosted `init` never called POST /v1/credits/deduct, so the \
         `deduct_calls() == 0` assertions in the --use-env-llm tests above prove \
         nothing — the mock route may simply be unreachable.{diag}"
    );
}
