//! e2e regression: credit_refund_on_failure
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! HOSTED mode (no `--use-env-llm`). The fake Enzyme API hands out a working
//! free-config credential and tracks a credit balance; the fake LLM then fails
//! every completion with HTTP 402. Catalyst generation therefore produces
//! nothing.
//!
//! CORRECT BEHAVIOR (what this test asserts): the user's net credit change
//! across such a run is ZERO. Credits taken up front must be given back when
//! the work they paid for produced no catalysts.
//!
//! ACTUAL BEHAVIOR (the bug, unfixed): `deduct_credits` fires *before* any LLM
//! work (`init.rs:611` and `init.rs:1352`) and there is no refund path on any
//! failure branch, so the run bills 3.0 credits for zero catalysts.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// Body the fake LLM returns for every chat-completion request.
fn llm_402_body() -> String {
    serde_json::json!({
        "error": {
            "message": "Insufficient credits to run this request.",
            "type": "insufficient_quota",
            "code": 402,
        }
    })
    .to_string()
}

/// Sum of the `amount` field over every `/v1/credits/deduct` request body.
///
/// This is the net amount billed to the user. A refund implemented as a
/// compensating negative deduct would bring this back to 0.0; so would simply
/// not deducting until the work has succeeded.
fn net_credits_billed(api: &MockEnzymeApi) -> f64 {
    api.deduct_requests()
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter_map(|v| v["amount"].as_f64())
        .sum()
}

/// Pull `catalysts_generated` out of the `--json-progress` JSONL on stderr.
///
/// Returns `None` when no discovery-completion event was ever emitted, which
/// itself means zero catalysts were generated.
fn catalysts_generated(out: &CliOutput) -> Option<u64> {
    out.combined()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter_map(|v| {
            v.get("catalysts_generated")
                .or_else(|| v.pointer("/data/catalysts_generated"))
                .and_then(serde_json::Value::as_u64)
        })
        .last()
}

/// Compact dump of everything the run did, for assertion failure messages.
fn diagnostics(api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "--- run diagnostics ---\n\
         exit code:            {:?}\n\
         deduct calls:         {}\n\
         deduct bodies:        {:?}\n\
         net credits billed:   {}\n\
         free-config calls:    {}\n\
         LLM requests:         {}\n\
         catalysts_generated:  {:?}\n\
         --- stdout ---\n{}\n\
         --- stderr ---\n{}",
        out.code,
        api.deduct_calls(),
        api.deduct_requests(),
        net_credits_billed(api),
        api.free_config_calls(),
        llm.request_count(),
        catalysts_generated(out),
        out.stdout,
        out.stderr,
    )
}

#[test]
#[ignore = "known defect: hosted credits are not refunded when catalyst generation produces nothing"]
fn hosted_init_refunds_credits_when_catalyst_generation_produces_nothing() {
    let fixture = VaultFixture::new();

    // Fake LLM: reachable, authenticates fine, but 402s every completion.
    let llm = MockLlm::start();
    llm.set_response(402, llm_402_body());

    // Fake Enzyme API: hands out a hosted credential pointing at that LLM, and
    // tracks a 100.0-credit balance behind /v1/credits/deduct.
    let api = MockEnzymeApi::start();
    api.set_credit_balance(100.0);
    api.set_free_config("k-hosted", &llm.base_url(), "test-model");

    // Hosted mode requires a login token for the credit preflight to fire.
    fixture.login("enz-user-key");

    let out = fixture
        .cli()
        .enzyme_api(&api)
        .args(["init", "--json-progress"])
        .run();

    let billed = net_credits_billed(&api);
    let generated = catalysts_generated(&out).unwrap_or(0);
    let diag = diagnostics(&api, &llm, &out);

    // Sanity: this test is only meaningful if the run actually went through the
    // hosted path and actually reached the (failing) LLM. If either of these
    // trips, the fixture is wrong, not the product.
    assert!(
        api.free_config_calls() > 0,
        "expected the hosted free-config endpoint to be used; it never was, so \
         this run did not exercise the hosted credential path\n{diag}"
    );
    assert!(
        llm.request_count() > 0,
        "expected catalyst generation to reach the mock LLM; it never did, so \
         the 402 failure under test never happened\n{diag}"
    );

    // The premise: the run produced no catalysts at all.
    assert_eq!(
        generated, 0,
        "expected 0 catalysts generated (every LLM completion returned 402), \
         but the run reported {generated}\n{diag}"
    );

    // The bug. Correct behavior: a run that generated nothing costs nothing.
    assert_eq!(
        billed,
        0.0,
        "CREDIT LEAK: hosted init generated {generated} catalysts but billed the \
         user {billed} credits with no refund. Expected a net credit change of 0.0 \
         across a run that produced nothing — credits deducted up front \
         (init.rs:611 / init.rs:1352) must be refunded when catalyst generation \
         fails. Observed {} deduct call(s) and no compensating refund.\n{diag}",
        api.deduct_calls(),
    );
}
