//! e2e regression: refresh_on_402
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! # What this pins down
//!
//! A free-tier (`ConfigProvider::ServerFallback`) run leases an *ephemeral* LLM
//! key from the Enzyme free-config endpoint. Those leases expire. `LlmClient`
//! already knows how to recover: when the upstream LLM rejects the lease it
//! calls `try_refresh_key`, which invalidates the free-config cache, re-fetches
//! a brand new key, and replays the request with it.
//!
//! That recovery is deliberately gated on HTTP **401 only**. A **402** on a
//! completion is handled by a different, agreed policy: it trips the catalyst
//! pipeline's payment-required circuit breaker (typed on `LlmApiError.status
//! == 402`), which stops generation with a MODE-NEUTRAL provider-capacity
//! message. Billing errors can only come from billing endpoints (free-config
//! and `/v1/credits/deduct`), so the completion 402 must never surface a
//! hosted top-up directive — and reminting is not attempted, because hammering
//! a 402ing provider mid-run is exactly what the breaker exists to prevent.
//!
//! `refresh_on_401_remints_the_key` pins the 401 recovery (free-config hit a
//! second time, replayed request carries the new key, no run-ending error).
//! `completion_402_stops_generation_without_hosted_blame` pins the 402 policy.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm, Reply};

/// Key handed out by the first free-config fetch. This is the lease the LLM
/// will reject.
const KEY_INITIAL: &str = "enz-lease-initial";
/// Key handed out by every subsequent free-config fetch, i.e. the re-mint.
/// Seeing this on the wire is the proof that `try_refresh_key` ran.
const KEY_REMINTED: &str = "enz-lease-reminted";

/// Enzyme account token seeded into `$ENZYME_HOME/auth.json`; makes the CLI
/// take the hosted (`ServerFallback`) path rather than BYOK/env.
const ACCOUNT_TOKEN: &str = "enz-account-token";

/// Everything one scenario observed, kept alive so assertions can read counters
/// after the CLI subprocess has exited.
struct Scenario {
    api: MockEnzymeApi,
    llm: MockLlm,
    output: CliOutput,
}

/// Run `enzyme init` on the hosted free-tier path with a mock LLM that rejects
/// the first request with `reject_status`, and a free-config endpoint that can
/// mint a second, working key.
///
/// The reply queues are ordered so that a *correct* client observes:
///   free-config -> KEY_INITIAL, LLM -> reject, free-config -> KEY_REMINTED,
///   LLM -> 200.
fn run_expired_lease_scenario(reject_status: u16, reject_body: &str) -> Scenario {
    let fixture = VaultFixture::new();
    // Logged in => hosted credentials => ResolvedConfig.provider == ServerFallback
    // => LlmClient.free_tier == true, which is what arms the refresh path at all.
    fixture.login(ACCOUNT_TOKEN);

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();

    let free_config_reply = |key: &str| {
        Reply::ok_json(
            serde_json::json!({
                "api_key": key,
                "base_url": llm.base_url(),
                "model": "test-model",
            })
            .to_string(),
        )
    };

    // First fetch leases KEY_INITIAL; every later fetch mints KEY_REMINTED
    // (the last entry of a reply queue repeats forever).
    api.set_free_config_replies(vec![
        free_config_reply(KEY_INITIAL),
        free_config_reply(KEY_REMINTED),
    ]);

    // The dead lease is rejected once; anything after that succeeds. Only the
    // single request that loses the race sees the rejection, so this is stable
    // regardless of how many entities the pipeline fans out to.
    llm.set_response_sequence(vec![
        Reply::new(reject_status, reject_body),
        Reply::ok_json(MockLlm::completion_body("{}")),
    ]);

    let output = fixture
        .cli()
        .enzyme_api(&api)
        .args(["init", "--quiet"])
        .run();

    Scenario { api, llm, output }
}

/// Render the full observed interaction, for failure messages.
fn diagnostics(scenario: &Scenario) -> String {
    format!(
        "--- observed ---\n\
         free-config fetches: {}\n\
         LLM requests: {}\n\
         LLM Authorization headers (in order): {:#?}\n\
         exit code: {:?}\n\
         --- cli stdout ---\n{}\n\
         --- cli stderr ---\n{}",
        scenario.api.free_config_calls(),
        scenario.llm.request_count(),
        scenario.llm.auth_headers(),
        scenario.output.code,
        scenario.output.stdout,
        scenario.output.stderr,
    )
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// Assert the three observable consequences of a successful lease re-mint.
///
/// `status_label` is only used to make the failure message say which status was
/// under test.
#[track_caller]
fn assert_lease_was_reminted(scenario: &Scenario, status_label: &str) {
    let auth_headers = scenario.llm.auth_headers();

    // Sanity: the run actually got far enough to lease a key and call the LLM.
    // Without this a broken fixture would look like a refresh bug.
    assert!(
        scenario.api.free_config_calls() >= 1 && !auth_headers.is_empty(),
        "scenario never reached the LLM at all, so nothing about key refresh \
         could be observed — the fixture, not the refresh logic, is broken.\n{}",
        diagnostics(scenario)
    );
    assert!(
        auth_headers.iter().any(|h| h == &bearer(KEY_INITIAL)),
        "expected the first LLM request to carry the initially leased key {:?}, \
         but no request did — the hosted free-tier path was not exercised.\n{}",
        bearer(KEY_INITIAL),
        diagnostics(scenario)
    );

    // (1) The dead lease must trigger a re-fetch of free-config.
    assert!(
        scenario.api.free_config_calls() >= 2,
        "expected the LLM's HTTP {status} (expired free-tier lease) to trigger \
         try_refresh_key, which invalidates the free-config cache and re-fetches \
         a new key — so free-config should have been hit at least 2 times \
         (initial lease + re-mint).\n\
         actual: free-config was hit {calls} time(s), i.e. no re-mint was \
         attempted and the dead lease was reused.\n{diag}",
        status = status_label,
        calls = scenario.api.free_config_calls(),
        diag = diagnostics(scenario)
    );

    // (2) The replayed request must actually use the re-minted key.
    assert!(
        auth_headers.iter().any(|h| h == &bearer(KEY_REMINTED)),
        "expected at least one LLM request to be replayed with the re-minted key \
         {want:?} after the HTTP {status}.\n\
         actual: no request ever carried it; every request used the dead lease.\n{diag}",
        want = bearer(KEY_REMINTED),
        status = status_label,
        diag = diagnostics(scenario)
    );

    // (3) Having recovered, the dead lease must not surface as a run-ending error.
    assert!(
        !scenario.output.contains("app.enzyme.garden/plans"),
        "expected the HTTP {status} to be absorbed by the key refresh and never \
         reach the user, but the run aborted with the hosted-billing message.\n{diag}",
        status = status_label,
        diag = diagnostics(scenario)
    );
}

/// CONTROL. A 401 from the LLM is the already-supported case: it must re-mint
/// the lease and replay. If this test ever fails, the harness or the hosted
/// path is broken and the 402 result below means nothing.
#[test]
fn refresh_on_401_remints_the_key() {
    let scenario = run_expired_lease_scenario(
        401,
        &serde_json::json!({
            "error": { "message": "Invalid or expired credentials", "code": 401 }
        })
        .to_string(),
    );

    assert_lease_was_reminted(&scenario, "401");
}

/// POLICY. A 402 from the LLM completion trips the payment-required breaker:
/// generation stops with the mode-neutral capacity message. No hosted-billing
/// blame may surface — the account's credits were never consulted by this
/// failure — and the run must not be silently reported as a success.
#[test]
fn completion_402_stops_generation_without_hosted_blame() {
    let scenario = run_expired_lease_scenario(
        402,
        &serde_json::json!({
            "error": { "message": "Insufficient credits", "code": 402 }
        })
        .to_string(),
    );
    let diag = diagnostics(&scenario);

    // Sanity: the hosted path really leased a key and hit the 402ing LLM.
    assert!(
        scenario.api.free_config_calls() >= 1 && scenario.llm.request_count() >= 1,
        "scenario never reached the LLM, so the 402 policy was never \
         exercised.\n{diag}"
    );

    // The breaker's neutral verdict: the status is named, generation stops,
    // and no top-up directive appears.
    assert!(
        scenario.output.contains("402")
            && scenario.output.contains("stopping catalyst generation"),
        "expected the completion 402 to stop generation with the neutral \
         provider-capacity message.\n{diag}"
    );
    assert!(
        !scenario.output.contains("app.enzyme.garden/plans"),
        "a completion 402 is not a billing-endpoint error and must never \
         surface the hosted top-up directive.\n{diag}"
    );
}
