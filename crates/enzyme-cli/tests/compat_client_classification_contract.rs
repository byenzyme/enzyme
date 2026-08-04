//! e2e contract suite: compat_client_classification_contract
//!
//! Harness: `crates/enzyme-cli/tests/support/` (`support::mock`, `support::fixture`).
//!
//! These tests pin the IMPLEMENTED credit/error-handling policy:
//! **billing errors can only come from billing endpoints.** The only two
//! billing endpoints are `POST /v1/credits/deduct` and the hosted free-config
//! fetch. A completions call is never a billing endpoint, so no completion
//! error — whatever its status or body — may be reported as an Enzyme-account
//! billing verdict.
//!
//! ───────────────────────────────────────────────────────────────────────────
//! D1 — classification is typed, status-driven, and mode-neutral.
//!
//!   The classifier is `is_payment_required_error(&anyhow::Error)` at
//!   `crates/enzyme-core/src/pipeline/catalysts.rs`: it downcasts to the typed
//!   `LlmApiError` and fires ONLY on `status == 402`. Body substrings never
//!   participate (request ids, token counts, quota values like `4029`, and a
//!   body phrase like "Insufficient credits" are all inert). A completions 402
//!   trips the existing `credits_exhausted` short-circuit (stop hammering a
//!   402ing provider mid-run) and aborts with a MODE-NEUTRAL provider-capacity
//!   message that carries no hosted-billing language. No credential-mode
//!   parameter is needed: BYOK runs never call a billing endpoint at all, and
//!   hosted runs get the same neutral verdict because the 402 came from the
//!   provider pool, not from the account.
//!
//!   The table this suite pins:
//!     • 429 body containing "4029"        -> never a billing verdict (any mode)
//!     • 200 free-text "Insufficient..."   -> never a billing verdict (any mode)
//!     • genuine completions 402            -> neutral capacity abort (any mode);
//!                                             never a top-up directive
//!     • free-config 402                    -> the ONE place top-up guidance
//!                                             belongs (see the D2-adjacent
//!                                             control at the bottom)
//!
//! D2 — deduct-after-success: a run that generates 0 catalysts must net ZERO
//!      credits. `init` deducts 3.0 only AFTER generation, and only when
//!      `catalysts_generated > 0` (no refund endpoint exists or is needed).
//!      A failed hosted run therefore never touches `/v1/credits/deduct`.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

// ── shared markers ─────────────────────────────────────────────────────────

/// Any of these appearing in output means the run treated the failure as
/// ENZYME HOSTED credit exhaustion (the wrong verdict for a BYOK provider
/// error, and the wrong verdict for anything that isn't a genuine hosted 402).
const HOSTED_BILLING_MARKERS: &[&str] = &[
    "app.enzyme.garden",
    "catalyst generation aborted",
    "credits remaining",
];

fn any_hosted_marker(out: &CliOutput) -> Option<&'static str> {
    HOSTED_BILLING_MARKERS
        .iter()
        .copied()
        .find(|m| out.contains(m))
}

// ── shared fixtures ────────────────────────────────────────────────────────

/// A well-formed catalyst response (object format) so the pipeline treats a 200
/// as a genuine success rather than bailing on an empty parse.
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

/// A well-formed 200 catalyst response whose GENERATED TEXT legitimately
/// contains the phrase "Insufficient credits" (row 2 of the D1 table): a model
/// writing *about* billing must not trip credit-exhaustion classification.
fn catalyst_content_mentioning_insufficient_credits() -> String {
    let catalysts = [
        "Why does the ledger report \"Insufficient credits\" while the pool still shows a balance?",
        "Where does the budget assume credits that were never actually funded?",
        "Which downstream steps silently continue after an Insufficient credits warning?",
        "How would you distinguish a genuine 402 from a provider that merely echoes the phrase?",
        "What does the phrase Insufficient credits obscure about who is actually paying?",
        "Against which invariant does the credit accounting quietly break?",
        "Whose approval is assumed by the auto-top-up path?",
    ];
    let topics = [
        "ledger mismatch",
        "unfunded budget",
        "silent continuation",
        "402 disambiguation",
        "who pays",
        "broken invariant",
        "auto top-up",
    ];
    serde_json::json!({ "catalysts": catalysts, "topicNames": topics }).to_string()
}

/// Sum of the `amount` field over every `/v1/credits/deduct` request body — the
/// net amount billed. Defer-until-success (or a compensating refund) brings
/// this back to 0.0 for a run that produced nothing.
fn net_credits_billed(api: &MockEnzymeApi) -> f64 {
    api.deduct_requests()
        .iter()
        .filter_map(|b| serde_json::from_str::<serde_json::Value>(b).ok())
        .filter_map(|v| v["amount"].as_f64())
        .sum()
}

/// Last `catalysts_generated` value emitted on the `--json-progress` JSONL.
/// `None` means the discovery-completion event never fired = zero catalysts.
fn catalysts_generated(out: &CliOutput) -> Option<u64> {
    out.combined()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| {
            v.get("catalysts_generated")
                .or_else(|| v.pointer("/data/catalysts_generated"))
                .and_then(serde_json::Value::as_u64)
        })
        .last()
}

fn diagnostics(label: &str, api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "\n=== {label} ===\n\
         exit code:            {:?} (success={})\n\
         deduct calls:         {}\n\
         deduct bodies:        {:?}\n\
         net credits billed:   {}\n\
         free-config calls:    {}\n\
         LLM requests:         {}\n\
         catalysts_generated:  {:?}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        out.code,
        out.success,
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

// ═══════════════════════════════════════════════════════════════════════════
// D1 — classification by (credential mode + HTTP status)
// ═══════════════════════════════════════════════════════════════════════════

/// CONTROL (expected GREEN). HOSTED mode + a genuine completions HTTP 402 with
/// an OpenRouter "Insufficient credits" body. Under the implemented policy a
/// completions 402 is a PROVIDER condition even on the hosted path (the shared
/// pool is dry / the lease is spent); the account's credits were never the
/// problem, so no top-up directive may appear. The run must stop with the
/// neutral capacity abort — proving the 402 breaker genuinely fires — and must
/// bill nothing (deduct-after-success + 0 catalysts).
#[test]
fn hosted_genuine_402_stops_generation_without_top_up_blame() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_pool_dry_402(); // 402 + "Insufficient credits" body

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0); // high, so the low-balance nag never fires
    api.set_free_config("k-hosted", &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--json-progress"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("hosted genuine 402 (control)", &api, &llm, &out);

    // Non-vacuity: hosted path really ran and really hit the failing LLM.
    assert!(api.free_config_calls() > 0, "hosted free-config never used.{diag}");
    assert!(llm.request_count() > 0, "LLM never reached.{diag}");

    // The 402 breaker must fire and stop generation with the neutral message:
    // the status is named, capacity is blamed, and BYOK is offered.
    assert!(
        out.contains("402") && out.contains("stopping catalyst generation"),
        "BREAKER DID NOT FIRE (or lost its message): a genuine completions 402 in \
         HOSTED mode must stop generation with the neutral provider-capacity \
         abort. If this control is red, the D1 negative tests below prove \
         nothing (the breaker might simply never fire).{diag}"
    );
    assert!(
        out.contains("capacity may be temporarily exhausted"),
        "the abort must attribute the 402 to provider capacity, not billing.{diag}"
    );

    // ...and it must NOT be laundered into hosted billing: no top-up language,
    // and no charge for a run that produced nothing.
    if let Some(marker) = any_hosted_marker(&out) {
        panic!(
            "POLICY VIOLATION: a completions 402 (a non-billing endpoint) surfaced \
             the hosted-billing marker {marker:?}. Billing errors can only come \
             from billing endpoints; topping up cannot refill the provider pool.{diag}"
        );
    }
    assert_eq!(
        net_credits_billed(&api),
        0.0,
        "a hosted run that produced nothing must net zero credits.{diag}"
    );
}

/// CONTROL (expected GREEN — documents the already-shipped 4029 fix). A Google
/// AI Studio 429 whose only "402" is inside the quota value `4029` must never be
/// classified as credit exhaustion, in EITHER mode. Run here in HOSTED mode
/// (the harder case: hosted credits exist, so a misclassification is tempting).
#[test]
fn incidental_429_4029_is_not_hosted_billing() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_llm_429_google_4029(); // body contains "4029", never "status 402 "

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);
    api.set_free_config("k-hosted", &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--json-progress"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("incidental 429/4029 (control)", &api, &llm, &out);

    assert!(llm.request_count() > 0, "LLM never reached.{diag}");

    // A provider quota 429 is not payment-required. The run may fail (0
    // catalysts), but it must NOT be laundered into the hosted-billing abort.
    if let Some(marker) = any_hosted_marker(&out) {
        panic!(
            "MISCLASSIFIED: a 429 quota error (\"4029\") was reported as Enzyme \
             credit exhaustion via marker {marker:?}. Classification must key on \
             HTTP status, not the digits of a quota value.{diag}"
        );
    }
}

/// The CORE D1 gate (BYOK isolation).
///
/// Under `--use-env-llm` the user brings their own key; Enzyme hosted credits
/// are not funding this inference. A genuine provider HTTP 402 is therefore a
/// PROVIDER error, never "Enzyme credit exhaustion". The CLI must NOT emit
/// any hosted-billing message and must NOT touch hosted endpoints at all.
#[test]
fn env_llm_genuine_402_is_not_hosted_billing_red_by_design() {
    let fixture = VaultFixture::new();
    // Logged in on purpose: proves the refusal to bill is a CLASSIFICATION
    // decision, not merely "there was no token to bill".
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_pool_dry_402(); // genuine 402 + "Insufficient credits"

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--json-progress"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("env-llm genuine 402 (RED)", &api, &llm, &out);

    // Non-vacuity: the BYOK LLM path was actually exercised.
    assert!(
        llm.request_count() > 0,
        "TEST VACUOUS: env LLM never called, so classification was never reached.{diag}"
    );

    // (a) No hosted-billing verdict for a BYOK provider error.
    if let Some(marker) = any_hosted_marker(&out) {
        panic!(
            "D1 VIOLATION (credential-mode gate): a genuine provider HTTP 402 under \
             --use-env-llm was laundered into Enzyme hosted billing (marker {marker:?}). \
             Under BYOK, hosted credits fund nothing; classification must depend on \
             (credential mode + HTTP status). Expected: reported as a provider error.{diag}"
        );
    }

    // (b) A BYOK run must never consult hosted credits.
    assert_eq!(
        api.deduct_calls(),
        0,
        "D1/R2 VIOLATION: a --use-env-llm run touched POST /v1/credits/deduct.{diag}"
    );
    assert_eq!(
        api.free_config_calls(),
        0,
        "D1/R2 VIOLATION: a --use-env-llm run fetched hosted free-config credentials.{diag}"
    );
}

/// D1 gate (b): status, not substrings.
///
/// A provider returns HTTP **503** whose body happens to carry the free-text
/// phrase "Insufficient credits" (a garbled upstream / proxy error). This is not
/// a payment-required condition — the status is 503 — and under `--use-env-llm`
/// it could not be Enzyme billing regardless. Classification keys on the typed
/// HTTP status, so the phrase must be inert.
#[test]
fn env_llm_insufficient_credits_phrase_non_402_is_not_hosted_billing_red_by_design() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    // 503, but the body echoes the phrase the current classifier keys on.
    let body = serde_json::json!({
        "error": {
            "code": 503,
            "message": "Upstream provider unavailable. Insufficient credits could not be \
                        verified; the account service did not respond.",
            "type": "service_unavailable"
        }
    })
    .to_string();
    llm.set_response(503, body);

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--json-progress"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("env-llm 503 w/ phrase (RED)", &api, &llm, &out);

    assert!(
        llm.request_count() > 0,
        "TEST VACUOUS: env LLM never called.{diag}"
    );

    if let Some(marker) = any_hosted_marker(&out) {
        panic!(
            "D1 VIOLATION (substring, not status): an HTTP 503 provider error whose \
             body merely contains the words \"Insufficient credits\" was classified as \
             payment-required and surfaced hosted-billing marker {marker:?} — under \
             --use-env-llm no less. Classification must key on HTTP status (503 != 402) \
             and credential mode, not on a body substring.{diag}"
        );
    }

    assert_eq!(
        api.deduct_calls(),
        0,
        "R2 VIOLATION: a --use-env-llm run touched hosted credits.{diag}"
    );
}

/// CONTROL (expected GREEN — row 2 of the table). A successful HTTP **200**
/// completion whose GENERATED catalyst text legitimately contains the phrase
/// "Insufficient credits" must be treated as a normal success: catalysts are
/// produced and no hosted-billing verdict appears. (A 200 never enters the
/// error path, so the current code already handles this — but it is a load-
/// bearing row of the D1 contract and a regression here would be serious.)
#[test]
fn benign_200_mentioning_insufficient_credits_still_succeeds() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content_mentioning_insufficient_credits());

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--json-progress"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("benign 200 mentioning phrase (control)", &api, &llm, &out);

    assert!(llm.request_count() > 0, "LLM never reached.{diag}");

    if let Some(marker) = any_hosted_marker(&out) {
        panic!(
            "MISCLASSIFIED SUCCESS: a 200 completion whose catalyst text merely mentions \
             \"Insufficient credits\" produced hosted-billing marker {marker:?}. A \
             successful response is never credit exhaustion.{diag}"
        );
    }
    out.assert_success();
    assert!(
        catalysts_generated(&out).unwrap_or(0) > 0,
        "expected catalysts to be generated from the healthy 200 response.{diag}"
    );
    assert_eq!(
        api.deduct_calls(),
        0,
        "--use-env-llm must never touch hosted credits.{diag}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// D2 — defer credit deduction until success (0 catalysts => net 0 credits)
// ═══════════════════════════════════════════════════════════════════════════

/// The D2 gate.
///
/// HOSTED mode. The leased LLM 402s on every completion, so the run generates
/// zero catalysts. The CLI defers the deduction until success, so a run that
/// produced nothing never calls `/v1/credits/deduct` at all: net billed 0.0.
#[test]
fn hosted_zero_catalysts_nets_zero_credits_red_by_design() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_pool_dry_402(); // 402 on every completion -> 0 catalysts

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);
    api.set_free_config("k-hosted", &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--json-progress"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("hosted 0 catalysts net-zero (RED)", &api, &llm, &out);

    // Non-vacuity: the hosted path and the failing LLM both really ran.
    assert!(api.free_config_calls() > 0, "hosted path not exercised.{diag}");
    assert!(llm.request_count() > 0, "LLM never reached.{diag}");
    // Deduct-after-success: a failed run must never even consult the deduct
    // endpoint (there is no up-front charge to compensate any more).
    assert_eq!(
        api.deduct_calls(),
        0,
        "D2 VIOLATION: a hosted run that produced nothing called \
         POST /v1/credits/deduct — the deduction must be deferred until at \
         least one catalyst is persisted.{diag}"
    );

    // The premise: nothing was produced.
    assert_eq!(
        catalysts_generated(&out).unwrap_or(0),
        0,
        "expected 0 catalysts (every completion 402'd).{diag}"
    );

    // The gate: a run that produced nothing must cost nothing.
    assert_eq!(
        net_credits_billed(&api),
        0.0,
        "D2 VIOLATION (credit leak): a hosted run generated 0 catalysts but net-billed \
         {} credits. The new CLI must defer the deduction until success (or refund on \
         failure) so the net credit change is 0.0.{diag}",
        net_credits_billed(&api),
    );
}

/// CONTROL (expected GREEN). The BYOK counterpart of the D2 gate: a
/// `--use-env-llm` run that fails to produce anything must also net zero — and
/// this already holds trivially, because BYOK never deducts hosted credits at
/// all. Proves the net-zero property is not vacuously "0 because the deduct
/// route is unreachable in this suite".
#[test]
fn env_llm_zero_catalysts_nets_zero_credits() {
    let fixture = VaultFixture::new();
    fixture.login("enz-user-key");

    let llm = MockLlm::start();
    llm.set_response(500, r#"{"error":{"message":"upstream exploded","code":500}}"#);

    let api = MockEnzymeApi::start();
    api.set_credit_balance(1_000.0);

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--json-progress"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("env-llm 0 catalysts net-zero (control)", &api, &llm, &out);

    assert!(llm.request_count() > 0, "LLM never reached.{diag}");
    assert_eq!(
        catalysts_generated(&out).unwrap_or(0),
        0,
        "expected 0 catalysts (every completion 500'd).{diag}"
    );
    assert_eq!(
        net_credits_billed(&api),
        0.0,
        "BYOK run must never bill hosted credits.{diag}"
    );
    assert_eq!(
        api.deduct_calls(),
        0,
        "BYOK run must never call the deduct endpoint.{diag}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Billing endpoints — where top-up guidance DOES belong
// ═══════════════════════════════════════════════════════════════════════════

/// CONTROL (expected GREEN). The free-config fetch IS a billing endpoint: a
/// structured 402 from it means the account's Enzyme credits are genuinely
/// exhausted, and this is the one place the top-up directive belongs. The CLI
/// must surface it (with the BYOK escape hatch), must not claim the server was
/// unreachable, and must not touch the LLM or the deduct endpoint (no lease
/// was ever minted).
#[test]
fn free_config_402_carries_top_up_guidance() {
    let fixture = VaultFixture::new();
    fixture.login("enz-broke-user");

    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content()); // healthy, but must never be reached

    let api = MockEnzymeApi::start();
    api.set_free_config_402_full(0.0); // deployed shape: { error, credits, message }

    let out = fixture
        .cli()
        .args(["init", "--json-progress"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("free-config 402 top-up (control)", &api, &llm, &out);

    // Non-vacuity: the billing endpoint was really consulted and really refused.
    assert!(api.free_config_calls() > 0, "free-config never consulted.{diag}");
    assert_eq!(llm.request_count(), 0, "no lease was minted; the LLM must not be reached.{diag}");
    assert_eq!(api.deduct_calls(), 0, "nothing to bill for a refused run.{diag}");

    // The billing verdict is CORRECT here, so the top-up directive must appear...
    assert!(
        out.contains("app.enzyme.garden/plans"),
        "a genuine billing-endpoint 402 must carry the top-up directive — this is \
         the ONE place that language belongs.{diag}"
    );
    // ...alongside the BYOK escape hatch...
    assert!(
        out.contains("--use-env-llm"),
        "a hosted credit refusal must offer the BYOK escape hatch.{diag}"
    );
    // ...and the reachable server must not be misreported as a network failure.
    assert!(
        !out.contains("Enzyme server unreachable"),
        "a clean 402 from a reachable billing endpoint must never be reported as \
         the server being unreachable.{diag}"
    );
}
