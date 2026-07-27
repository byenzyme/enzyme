//! e2e regression: env_llm_bypasses_credit_check
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! CLAIM UNDER TEST
//! ----------------
//! `crates/enzyme-cli/src/commands/init.rs:611` (and the sibling refresh preflight
//! at `init.rs:1352`) fence the hosted credit deduction behind
//! `ConfigProvider::ServerFallback`. Under `--use-env-llm` the resolved provider
//! is `ConfigProvider::Env`, so the claim is that `deduct_credits` is never
//! reached and the hosted Enzyme API is never consulted for billing at all.
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

/// Same claim for the refresh path's separate preflight (`init.rs:1352`).
/// Refresh is seeded by a preceding `init --use-env-llm` on the same vault, then
/// a new note is written so there is genuinely stale work to do.
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
    // `NEW_MENTIONS_THRESHOLD` in enzyme-core/src/pipeline/staleness.rs is 3, so
    // an entity goes stale once its doc count grows by 3+. Add 4 notes to the
    // existing `projects/` entity to guarantee `StalenessResult::Stale` and
    // therefore reach the refresh credit preflight.
    for i in 0..4 {
        fixture.write_note(
            &format!("projects/new-project-{i}.md"),
            &format!(
                "# New Project {i}\n\nFresh project material added after init so the \
                 projects entity crosses the staleness threshold.\n"
            ),
        );
    }
    let llm_calls_after_init = llm.request_count();

    let out = fixture
        .cli()
        .arg("refresh")
        .arg("--use-env-llm")
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics("refresh --use-env-llm", &api, &llm, &out);

    // Non-vacuity: refresh must have reached LLM work of its own, otherwise a
    // zero deduct count says nothing about the refresh preflight.
    assert!(
        llm.request_count() > llm_calls_after_init,
        "TEST IS VACUOUS: `refresh --use-env-llm` issued no LLM requests of its own \
         (count was {llm_calls_after_init} after init and is {} now), so the refresh \
         credit preflight at init.rs:1352 was never exercised.{diag}",
        llm.request_count(),
    );

    assert_eq!(
        api.deduct_calls(),
        0,
        "REGRESSION: `refresh --use-env-llm` consulted hosted credits.\n\
         expected: 0 calls to POST /v1/credits/deduct across init+refresh\n\
         actual:   {} call(s)\n\
         init.rs:1352 is supposed to fence the refresh credit preflight behind \
         ConfigProvider::ServerFallback.{diag}",
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
