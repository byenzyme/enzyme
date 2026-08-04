//! e2e compat group: `compat_additive_response_tolerance`
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! SKEW AXIS: **OLD CLI x NEW SERVER** (R1) — the binding constraint.
//!
//! Every test drives the *current* `enzyme` binary (`env!("CARGO_BIN_EXE_enzyme")`,
//! the fixed "old CLI" artifact) as a subprocess against a mock Enzyme API whose
//! free-config response is shaped like a *future* server would return it. The old
//! CLI is fixed; the server behavior is emulated at the HTTP mock layer.
//!
//! Matrix cell 8 + rule R1: server responses are additive-only and must never
//! remove/rename the three load-bearing fields `api_key` / `base_url` / `model`.
//!
//! Two halves:
//!
//!   POSITIVE (additive tolerance, must be GREEN):
//!     A new server returns free-config success carrying EXTRA fields the current
//!     CLI has never seen (`tier`, `reason`, `pool_remaining`, `lease_id`, nested
//!     objects, arrays, nulls). The current CLI's `FreeConfigResponse`
//!     (client.rs:97) has NO `#[serde(deny_unknown_fields)]`, so it must parse
//!     cleanly and thread `api_key`/`base_url`/`model` through to the LLM exactly
//!     as if the extra fields were absent.
//!
//!   NEGATIVE (load-bearing contract, must be GREEN on current CLI):
//!     A response MISSING one of the three load-bearing fields must be a hard,
//!     clean failure: the hosted config is REJECTED (serde requires all three),
//!     so the leased key never reaches the LLM, nothing is deducted, and the CLI
//!     does not crash or silently proceed on a partial/garbage config. This PINS
//!     the three fields as load-bearing so a future server change can never drop
//!     one silently without the old CLI noticing (that is exactly why R1 exists).
//!
//! A companion `*_red_by_design` (ignored) documents the IDEAL diagnostic the
//! new CLI should give when a load-bearing field is missing — the current CLI
//! mislabels it as "server unreachable", which is a diagnostic gap, not a
//! correctness/compat break.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A well-formed catalyst response body (object form). A trivial `{}` parses as
/// zero catalysts and makes `init` bail before the run is meaningful, so the
/// fake LLM must answer with something real-shaped for the pipeline to run to
/// completion. Mirrors the baseline group's helper.
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

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// Render every observable fact about a run, so a failure explains itself.
fn diagnostics(label: &str, api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "\n=== {label} ===\n\
         free-config calls (GET {}): {}\n\
         free-config auth headers: {:?}\n\
         deduct calls: {}\n\
         LLM requests: {}\n\
         LLM Authorization headers: {:?}\n\
         refund calls: {} | account calls: {}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
        api.free_config_auth_headers(),
        api.deduct_calls(),
        llm.request_count(),
        llm.auth_headers(),
        api.refund_calls(),
        api.account_calls(),
        out.code,
        out.success,
        out.stdout,
        out.stderr,
    )
}

/// The current CLI must never speak to an endpoint only the *new* CLI contract
/// introduces (R2). Any additive-response run must leave these at zero.
#[track_caller]
fn assert_no_new_cli_endpoints(api: &MockEnzymeApi, diag: &str) {
    assert_eq!(
        api.refund_calls(),
        0,
        "current CLI must never call POST /v1/credits/refund (new-CLI contract only).{diag}"
    );
    assert_eq!(
        api.account_calls(),
        0,
        "current CLI must never call GET /v1/account (new-CLI contract only).{diag}"
    );
}

/// A run must never panic / dump a Rust backtrace, no matter how odd the server
/// response is.
#[track_caller]
fn assert_no_panic(out: &CliOutput, diag: &str) {
    assert!(
        !out.contains("panicked") && !out.contains("RUST_BACKTRACE"),
        "the CLI must not panic on any server free-config response.{diag}"
    );
}

// ===========================================================================
// POSITIVE — additive tolerance (cell 8, R1). Must be GREEN.
// ===========================================================================

/// NEW SERVER returns free-config success with a pile of extra fields the
/// current CLI has never seen — including scalars, a null, an array, and a
/// nested object. The current CLI must parse it (no `deny_unknown_fields`),
/// ignore every unknown field, and use `api_key`/`base_url`/`model` exactly as
/// if they were the only fields present:
///   * `base_url` correct  -> the leased key actually reaches the mock LLM;
///   * `api_key` correct   -> that key is the Bearer presented to the LLM;
///   * `model` correct     -> the distinctive model name is threaded into the
///                            catalyst-generation request bodies.
#[test]
fn additive_extra_fields_parse_and_core_three_are_used() {
    const LEASED_KEY: &str = "enz-additive-lease";
    const MODEL: &str = "openrouter/future-model-probe";

    let fixture = VaultFixture::new();
    fixture.login("k-additive-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_credit_balance(100.0);

    // A future-server body: the deployed 3 core + provider/disclaimer/expires_at,
    // PLUS a spray of unknown fields of varied JSON types the current CLI has
    // never seen. Written raw so the exact future shape is explicit.
    api.set_free_config_json(serde_json::json!({
        "api_key": LEASED_KEY,
        "base_url": llm.base_url(),
        "model": MODEL,
        // deployed-but-ignored fields:
        "provider": "openrouter-pro-included",
        "disclaimer": "Included AI. Quality may vary.",
        "expires_at": 4102444800u64,
        // brand-new future fields of assorted types:
        "tier": "pro",
        "reason": "pool_headroom_ok",
        "pool_remaining": 1234.56,
        "lease_id": "lease_abcdef0123456789",
        "rate_limit": { "requests_per_min": 60, "tokens_per_min": 120000 },
        "included_models": ["a", "b", "c"],
        "deprecation": serde_json::Value::Null,
        "nested": { "deep": { "deeper": [1, 2, 3] } },
    }));

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("additive extra fields", &api, &llm, &out);

    assert_no_panic(&out, &diag);
    assert!(
        out.success,
        "an additive-but-well-formed free-config response must not break the old \
         CLI; extra unknown fields are ignored by `FreeConfigResponse`.{diag}"
    );
    assert!(
        api.free_config_calls() >= 1,
        "expected the hosted free-config endpoint to be consulted.{diag}"
    );
    // base_url was used correctly: the leased key reached the (mock) LLM at all.
    assert!(
        llm.request_count() > 0,
        "expected the additive lease's base_url to be used to call the LLM.{diag}"
    );
    // api_key was used correctly: that exact key is the Bearer presented.
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(LEASED_KEY)),
        "expected the leased key {LEASED_KEY:?} (from the additive response) to be \
         presented to the LLM, proving `api_key` survived the extra fields.{diag}"
    );
    // model was used correctly: the distinctive model name is threaded into the
    // catalyst-generation request bodies.
    assert!(
        llm.requests().iter().any(|b| b.contains(MODEL)),
        "expected the additive response's model {MODEL:?} to appear in an LLM \
         request body, proving `model` survived the extra fields.{diag}"
    );
    // A healthy account must not see a billing error from a well-formed response.
    assert!(
        !out.contains("Insufficient credits"),
        "an additive success response must not surface a billing error.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

/// Belt-and-suspenders variant of the above: even when the additive fields
/// collide *namewise* with things the CLI reasons about elsewhere (e.g. a
/// top-level `credits`, `error`, `message` on a 200 body — the shapes it reads
/// on a 402), the old CLI must still treat a 200 as success and ignore them.
/// This guards against a future server that annotates a success body with
/// billing metadata.
#[test]
fn additive_billing_shaped_fields_on_200_are_ignored() {
    const LEASED_KEY: &str = "enz-additive-billing";

    let fixture = VaultFixture::new();
    fixture.login("k-additive-billing-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_credit_balance(100.0);

    api.set_free_config_json(serde_json::json!({
        "api_key": LEASED_KEY,
        "base_url": llm.base_url(),
        "model": "test-model",
        // Fields whose NAMES the CLI reads on the 402 path — must be inert on 200:
        "error": "none",
        "credits": 0.0,
        "message": "Visit app.enzyme.garden/plans to top up.",
        "required": 3.0,
    }));

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("additive billing-shaped 200", &api, &llm, &out);

    assert_no_panic(&out, &diag);
    assert!(
        out.success,
        "a 200 success carrying billing-shaped extra fields must still be treated \
         as success — the HTTP status, not the body, decides.{diag}"
    );
    assert!(
        llm.request_count() > 0 && llm.auth_headers().iter().any(|h| h == &bearer(LEASED_KEY)),
        "the leased key must still reach the LLM despite the inert billing \
         fields.{diag}"
    );
    // Crucial: a `credits: 0` / `message: ...top up` decoration on a 200 must NOT
    // be mistaken for the 402 credits-exhausted refusal.
    assert!(
        !out.contains("Visit app.enzyme.garden/plans"),
        "billing-shaped fields on a 200 body must not trigger the \
         credits-exhausted refusal message.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ===========================================================================
// NEGATIVE — the three fields are load-bearing (R1). Must be GREEN on the
// current CLI: a missing field is REJECTED, so the hosted lease is never used.
// ===========================================================================

/// Drive `init` against a free-config 200 whose body is `body`, then assert the
/// load-bearing contract: the hosted config was rejected (serde requires all
/// three core fields), so nothing reached the LLM, nothing was deducted, and the
/// CLI neither crashed nor silently proceeded on a partial config.
#[track_caller]
fn assert_missing_field_is_load_bearing(label: &str, body: serde_json::Value) {
    let fixture = VaultFixture::new();
    fixture.login("k-missing-field-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    // Healthy LLM: if the CLI ever DID use a partial config, this would answer,
    // so a zero request count is meaningful (not a coincidental LLM failure).
    llm.set_ok_content(&catalyst_content());
    api.set_credit_balance(100.0);
    api.set_free_config_json(body);

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics(label, &api, &llm, &out);

    // The server WAS reached...
    assert!(
        api.free_config_calls() >= 1,
        "expected the CLI to fetch free-config.{diag}"
    );
    // ...but the malformed lease must be rejected: no key ever reaches the LLM.
    assert_eq!(
        llm.request_count(),
        0,
        "a free-config response missing a load-bearing field must be REJECTED — \
         the leased config must never reach the LLM (proves the field is \
         load-bearing / required by serde).{diag}"
    );
    // A config that can't be used must never charge the account (D2 safety):
    assert_eq!(
        api.deduct_calls(),
        0,
        "a rejected/unusable hosted config must not deduct any credits.{diag}"
    );
    // No crash, no garbage.
    assert_no_panic(&out, &diag);
    // The CLI must not falsely claim catalyst generation succeeded on a lease it
    // could not parse.
    assert!(
        !out.contains("catalysts generated") || out.contains("skipping"),
        "the CLI must not report catalyst generation as having run on an \
         unparseable hosted lease.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

/// `api_key` is load-bearing: a response dropping it (but otherwise additive-rich)
/// is rejected and the lease is never used.
#[test]
fn missing_api_key_is_load_bearing() {
    let llm = MockLlm::start();
    assert_missing_field_is_load_bearing(
        "missing api_key",
        serde_json::json!({
            // api_key intentionally omitted
            "base_url": llm.base_url(),
            "model": "test-model",
            "provider": "openrouter-free",
            "tier": "free",
            "lease_id": "lease_no_key",
        }),
    );
}

/// `base_url` is load-bearing.
#[test]
fn missing_base_url_is_load_bearing() {
    assert_missing_field_is_load_bearing(
        "missing base_url",
        serde_json::json!({
            "api_key": "enz-no-baseurl",
            // base_url intentionally omitted
            "model": "test-model",
            "provider": "openrouter-free",
            "tier": "free",
            "lease_id": "lease_no_baseurl",
        }),
    );
}

/// `model` is load-bearing.
#[test]
fn missing_model_is_load_bearing() {
    let llm = MockLlm::start();
    assert_missing_field_is_load_bearing(
        "missing model",
        serde_json::json!({
            "api_key": "enz-no-model",
            "base_url": llm.base_url(),
            // model intentionally omitted
            "provider": "openrouter-free",
            "tier": "free",
            "lease_id": "lease_no_model",
        }),
    );
}

/// A renamed field is equivalent to a dropped one: if a future server renamed
/// `api_key` to (e.g.) `apiKey`, the old CLI must reject it rather than silently
/// proceed with no key. Pins that renaming is as breaking as removal (R1).
#[test]
fn renamed_core_field_is_rejected() {
    let llm = MockLlm::start();
    assert_missing_field_is_load_bearing(
        "renamed api_key -> apiKey",
        serde_json::json!({
            "apiKey": "enz-renamed-key",   // wrong name
            "base_url": llm.base_url(),
            "model": "test-model",
            "provider": "openrouter-free",
        }),
    );
}

// ===========================================================================
// RED BY DESIGN — the IDEAL diagnostic for a dropped load-bearing field.
// ===========================================================================

/// When free-config returns a 200 that is MISSING `model`, the server was plainly
/// REACHABLE and answered — it just handed back a body the CLI cannot use. The
/// current CLI funnels the parse error into the same catch-all quiet-mode line as
/// a network outage (init.rs:829):
///
///   "No API key found and Enzyme server unreachable — skipping catalyst
///    generation. Set OPENAI_API_KEY or ensure network access."
///
/// The CORRECT behavior — asserted here, NOT satisfied by the current CLI — is
/// that a reachable server returning an unusable/malformed lease is reported as a
/// bad response (or hard error), NOT as "server unreachable". This is a
/// diagnostic-quality gap and a new-CLI gate, not a correctness/compat break
/// (the leased key is correctly never used; see `missing_model_is_load_bearing`).
///
/// Ignored so the group stays green; recorded in `red_by_design`.
#[test]
#[ignore = "red_by_design: current CLI mislabels a reachable-server malformed 200 as 'server unreachable' (init.rs:829)"]
fn missing_field_should_not_claim_server_unreachable_red_by_design() {
    let fixture = VaultFixture::new();
    fixture.login("k-missing-field-diag");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_credit_balance(100.0);
    api.set_free_config_json(serde_json::json!({
        "api_key": "enz-no-model-diag",
        "base_url": llm.base_url(),
        // model omitted -> unusable, but the server WAS reachable
        "provider": "openrouter-free",
    }));

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("missing model diagnostic (red)", &api, &llm, &out);

    // DESIRED (fails today): a reachable server that returned a malformed lease
    // must not be reported as unreachable.
    assert!(
        !out.contains("Enzyme server unreachable"),
        "MISLEADING DIAGNOSTIC: free-config returned a reachable 200 that was \
         merely missing a load-bearing field, but the CLI claims the server was \
         unreachable. A malformed-response failure must be reported as such.{diag}"
    );
}
