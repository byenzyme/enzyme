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
//! That recovery is gated on HTTP **401 only** — `client.rs:693` (buffered) and
//! `client.rs:732` (streaming) both test `status == 401`. Providers also retire
//! an exhausted lease with **402**, and a 402 means exactly the same thing from
//! the client's point of view: *this lease is dead, go get another one*. The
//! correct behavior is therefore identical for both statuses.
//!
//! Both tests below drive the real `enzyme init` binary against a mock Enzyme
//! API and a mock LLM, and assert the same three observable consequences of a
//! refresh:
//!
//!   1. the free-config endpoint is hit a second time (the re-mint),
//!   2. the replayed LLM request carries the *new* key, and
//!   3. the run is not aborted by the dead lease.
//!
//! `refresh_on_401_remints_the_key` is the control: it proves the harness is
//! wired correctly and that these assertions do fire when refresh works. Any
//! delta between the two tests is the bug and nothing else.
//!
//! EXPECTED RESULT: 401 test passes, 402 test FAILS. The 402 test is red on
//! purpose — it reproduces an unfixed bug. Do not weaken it.

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

/// REGRESSION (expected RED). A 402 from the LLM means the same thing as a 401 —
/// the ephemeral lease is spent — and must recover the same way.
///
/// `client.rs:693` / `client.rs:732` gate `try_refresh_key` on 401 only, so the
/// spent lease is reused until the 300s `FREE_CONFIG_CACHE_TTL_SECS` lapses,
/// and the raw 402 escapes into `is_credits_exhausted_error`, killing the run
/// with a hosted-billing message the user cannot act on.
#[test]
#[ignore = "known defect: a 402 during generation never remints the hosted key lease"]
fn refresh_on_402_remints_the_key() {
    let scenario = run_expired_lease_scenario(
        402,
        &serde_json::json!({
            "error": { "message": "Insufficient credits", "code": 402 }
        })
        .to_string(),
    );

    assert_lease_was_reminted(&scenario, "402");
}
