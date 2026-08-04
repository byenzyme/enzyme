//! e2e regression: credit_402_substring
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! # The contract
//!
//! The catalyst pipeline's payment-required breaker
//! (`is_payment_required_error`, `enzyme-core/src/pipeline/catalysts.rs`)
//! classifies on the typed `LlmApiError` HTTP status — it fires only on a
//! genuine 402. Body substrings never participate: a provider body that
//! happens to contain the three characters `402` anywhere (a request id, a
//! token count, a quota value like `quota_limit_value 4029`) must stay inert.
//! And no completion error, in any mode, is ever an Enzyme-account billing
//! verdict — billing errors can only come from billing endpoints.
//!
//! # What this test does
//!
//! Runs a real `enzyme init --use-env-llm` subprocess against a fake
//! OpenAI-compatible LLM that answers HTTP **429** with a Google-AI-Studio-shaped
//! quota body in which `402` appears only *incidentally*, inside `4029`.
//!
//! # Correct behavior (what is asserted)
//!
//! A BYOK provider rate-limit must be reported as a provider rate-limit: the run
//! must not claim "Insufficient credits", must not send a BYOK user to
//! `app.enzyme.garden` to top up an account that is not paying for this
//! inference, and must surface the actual upstream failure.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A Google AI Studio style quota-exhausted body. HTTP status is 429, and the
/// only occurrence of "402" in the whole payload is inside `4029` — a quota
/// *value*, not a payment-required status code.
const INCIDENTAL_402_BODY: &str = r#"{"error":{"code":429,"message":"Resource exhausted: quota_limit_value 4029 for quota metric 'generate_requests_per_model_per_day'","status":"RESOURCE_EXHAUSTED"}}"#;

/// Substrings that would prove the run mistook this for hosted credit exhaustion.
const HOSTED_BILLING_MARKERS: &[&str] = &[
    "Insufficient credits",
    "app.enzyme.garden",
    "credits remaining",
    "top up",
];

/// Evidence that the failure was correctly attributed to the upstream provider.
const PROVIDER_ATTRIBUTION_MARKERS: &[&str] = &[
    "429",
    "RESOURCE_EXHAUSTED",
    "Resource exhausted",
    "quota_limit_value",
    "rate limit",
    "rate-limited",
    "rate limited",
];

fn diagnostics(out: &CliOutput, llm: &MockLlm, api: &MockEnzymeApi) -> String {
    format!(
        "\n--- mock LLM served 429 with body ---\n{INCIDENTAL_402_BODY}\n\
         --- mock LLM requests: {} | mock Enzyme deduct calls: {} ---\n\
         --- exit code: {:?} (success={}) ---\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        llm.request_count(),
        api.deduct_calls(),
        out.code,
        out.success,
        out.stdout,
        out.stderr,
    )
}

#[test]
fn incidental_402_in_provider_error_is_not_reported_as_credit_exhaustion() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    // Every catalyst call fails the same way: a provider quota error whose body
    // incidentally contains "402" inside the number 4029.
    llm.set_response(429, INCIDENTAL_402_BODY);

    let vault = VaultFixture::new();
    let out = vault
        .cli()
        .args(["init", "--use-env-llm"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics(&out, &llm, &api);

    // Sanity: the LLM path was actually exercised. If this fails, the test never
    // reached the code under test and the assertions below are meaningless.
    assert!(
        llm.request_count() > 0,
        "expected the CLI to call the env LLM at least once under --use-env-llm, \
         but the mock LLM received 0 requests — the catalyst path was never reached.{diag}"
    );

    // ── The bug ──────────────────────────────────────────────────────────────
    // A BYOK provider's 429 must never be laundered into an Enzyme hosted
    // billing error.
    for marker in HOSTED_BILLING_MARKERS {
        assert!(
            !out.contains(marker),
            "MISATTRIBUTED PROVIDER ERROR: the provider returned HTTP 429 \
             (RESOURCE_EXHAUSTED, quota_limit_value 4029) under --use-env-llm, where \
             Enzyme hosted credits are not involved at all, but the CLI output \
             contains the hosted-billing marker {marker:?}.\n\
             Expected: the failure reported as an upstream provider rate limit.\n\
             Actual:   reported as Enzyme credit exhaustion.\n\
             The breaker must classify on the typed HTTP status (LlmApiError), \
             never on body substrings like the \"402\" inside \"4029\".{diag}"
        );
    }

    // ── Positive half: the real cause must actually be surfaced ─────────────
    assert!(
        PROVIDER_ATTRIBUTION_MARKERS
            .iter()
            .any(|marker| out.contains(marker)),
        "UNATTRIBUTED FAILURE: catalyst generation failed because the env LLM \
         returned HTTP 429, but the CLI output surfaces none of the provider \
         attribution markers {PROVIDER_ATTRIBUTION_MARKERS:?}. A BYOK user has no \
         way to tell what actually went wrong.{diag}"
    );

    // ── Corollary: nothing about this run should touch hosted credits ───────
    assert_eq!(
        api.deduct_calls(),
        0,
        "expected --use-env-llm to never deduct hosted credits, but the mock \
         Enzyme API saw {} deduct call(s).{diag}",
        api.deduct_calls(),
    );
}
