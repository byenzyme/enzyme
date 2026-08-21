//! e2e regression: completion_error_policy
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! # What this pins down
//!
//! Hosted credentials are resolved once before generation starts. Completion
//! failures
//! must never re-enter credential resolution: a 401 is returned as a generation
//! failure, while a 402 also trips the payment-required circuit breaker. Neither
//! status may re-fetch free-config or replay with a newly minted lease.
//!
//! `completion_401_does_not_reresolve_credentials` pins the architecture boundary.
//! `completion_402_stops_generation_without_hosted_blame` pins the 402 policy.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm, Reply};

/// Key handed out by the first free-config fetch. This is the lease the LLM
/// will reject.
const KEY_INITIAL: &str = "enz-lease-initial";
/// Sentinel key returned only if generation incorrectly re-enters setup.
const KEY_FORBIDDEN: &str = "enz-lease-after-setup";

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

/// Run `enzyme init` with a hosted lease and reject its completion.
/// A second free-config reply is installed as a tripwire: correct generation
/// never
/// consumes it because credential resolution has already finished.
fn run_expired_lease_scenario(reject_status: u16, reject_body: &str) -> Scenario {
    let fixture = VaultFixture::new();
    // Logged in selects the hosted setup path.
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

    // Any second fetch exposes a forbidden generation-time re-resolution.
    api.set_free_config_replies(vec![
        free_config_reply(KEY_INITIAL),
        free_config_reply(KEY_FORBIDDEN),
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

/// Assert that completion cannot re-enter hosted credential setup.
#[track_caller]
fn assert_credentials_were_not_reresolved(scenario: &Scenario) {
    let auth_headers = scenario.llm.auth_headers();
    let diag = diagnostics(scenario);

    assert_eq!(
        scenario.api.free_config_calls(),
        1,
        "completion re-entered credential setup and fetched free-config again.\n{diag}"
    );
    assert!(
        auth_headers
            .iter()
            .any(|header| header == &bearer(KEY_INITIAL)),
        "scenario never exercised the configured hosted lease.\n{diag}"
    );
    assert!(
        auth_headers
            .iter()
            .all(|header| header != &bearer(KEY_FORBIDDEN)),
        "completion replayed with a lease minted after setup.\n{diag}"
    );
}

/// A 401 is a generation failure, not permission to resolve credentials again.
#[test]
fn completion_401_does_not_reresolve_credentials() {
    let scenario = run_expired_lease_scenario(
        401,
        &serde_json::json!({
            "error": { "message": "Invalid or expired credentials", "code": 401 }
        })
        .to_string(),
    );

    assert_credentials_were_not_reresolved(&scenario);
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
    assert_credentials_were_not_reresolved(&scenario);

    // Sanity: the hosted path really leased a key and hit the 402ing LLM.
    assert!(
        scenario.api.free_config_calls() >= 1 && scenario.llm.request_count() >= 1,
        "scenario never reached the LLM, so the 402 policy was never \
         exercised.\n{diag}"
    );

    // The breaker's neutral verdict: the status is named, generation stops,
    // and no top-up directive appears.
    assert!(
        scenario.output.contains("402") && scenario.output.contains("stopping catalyst generation"),
        "expected the completion 402 to stop generation with the neutral \
         provider-capacity message.\n{diag}"
    );
    assert!(
        !scenario.output.contains("app.enzyme.garden/plans"),
        "a completion 402 is not a billing-endpoint error and must never \
         surface the hosted top-up directive.\n{diag}"
    );
}
