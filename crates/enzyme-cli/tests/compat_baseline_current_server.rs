//! e2e compat group: `compat_baseline_current_server`
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! SKEW AXIS: **OLD CLI x OLD SERVER** (baseline).
//!
//! Every test here drives the *current* `enzyme` binary
//! (`env!("CARGO_BIN_EXE_enzyme")`, i.e. the fixed "old CLI" artifact) as a
//! subprocess against the mock Enzyme API + mock LLM configured to emulate the
//! *currently deployed* server. The point is to document the correct, current
//! baseline behavior for matrix configs 1, 2, 3, 6, 7 so the other skew groups
//! (old-CLI x new-server, new-CLI contracts) have a green reference to diff
//! against. Any RED in this file is a harness bug, not a compat finding — the
//! current CLI against the current server is, by construction, compatible.
//!
//! Matrix cells covered:
//!   1  free account, credits>0, pool funded   -> $0.20/day free lease, init proceeds
//!   2  free account, credits=0                 -> free-config 402, no LLM call, BYOK guidance
//!   3  pro account active, pool funded         -> $10/mo pro lease, init proceeds
//!   6  env-llm / BYOK                          -> no hosted call at all, no deduct
//!   7  anonymous bootstrap (unauth first init) -> X-Enzyme-Bootstrap-Id sent, key handed out
//!
//! NOTE on the credit deduct: the CLI deducts credits AFTER successful LLM
//! work (deduct-after-success, gated on `ConfigProvider::ServerFallback` and
//! `catalysts_generated > 0`). The healthy hosted cells therefore still
//! observe a deduct — it just fires post-generation. Failed runs never deduct;
//! that contract is pinned in `compat_client_classification_contract`.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A well-formed catalyst response body in the object format
/// `parse_catalyst_response` prefers. A trivial `{}` stub parses as zero
/// catalysts and makes `init` bail before the run is meaningful, so the fake
/// LLM must answer with something real-shaped for the catalyst pipeline to run
/// to completion.
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
         free-config bootstrap ids: {:?}\n\
         deduct calls (POST /v1/credits/deduct): {}\n\
         deduct request bodies: {:?}\n\
         LLM requests: {}\n\
         LLM Authorization headers: {:?}\n\
         refund calls: {} | account calls: {}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
        api.free_config_auth_headers(),
        api.free_config_bootstrap_ids(),
        api.deduct_calls(),
        api.deduct_requests(),
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

/// Assert the current CLI never spoke to any endpoint that only the *new* CLI
/// contract introduces (R2, matrix config 9). Every baseline run must leave
/// these at zero; if not, the old CLI already depends on a not-yet-deployed
/// endpoint and the rollout story is wrong.
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

// ---------------------------------------------------------------------------
// Cell 1 — free account, credits>0, pool funded.
// ---------------------------------------------------------------------------

/// A logged-in FREE account with credits and a funded pool gets a usable
/// $0.20/day lease from free-config, the leased key reaches the LLM, catalyst
/// generation runs, and the current server contract deducts credits. init
/// succeeds and no hosted-billing error is shown.
#[test]
fn cell1_free_with_credits_gets_free_lease_and_proceeds() {
    const LEASED_KEY: &str = "enz-free-020";

    let fixture = VaultFixture::new();
    // Logged in: hosted path (ServerFallback), and the account credit preflight
    // has a token to charge.
    fixture.login("k-free-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    // Deployed FREE-tier success body (provider: openrouter-free, $0.20/day),
    // pointing at the mock LLM. Plenty of account credits so the preflight
    // deduct succeeds.
    api.set_credit_balance(100.0);
    api.set_free_config_free_tier(LEASED_KEY, &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("cell1 free w/ credits", &api, &llm, &out);

    assert!(
        out.success,
        "expected `init` to succeed for a funded free account.{diag}"
    );
    assert!(
        api.free_config_calls() >= 1,
        "expected the hosted free-config endpoint to be consulted.{diag}"
    );
    // The $0.20 lease must actually be used to talk to the LLM.
    assert!(
        llm.request_count() > 0,
        "expected catalyst generation to call the leased LLM.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(LEASED_KEY)),
        "expected the leased free-tier key {LEASED_KEY:?} to be presented to the LLM.{diag}"
    );
    // The hosted free-tier init deducts account credits after the successful
    // generation (deduct-after-success).
    assert!(
        api.deduct_calls() > 0,
        "expected the hosted free-tier init to deduct account credits after a \
         successful generation.{diag}"
    );
    // The account is healthy: no billing failure should surface.
    assert!(
        !out.contains("Insufficient credits"),
        "a funded free account must not see an insufficient-credits error.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ---------------------------------------------------------------------------
// Cell 2 — free account, credits=0.
// ---------------------------------------------------------------------------

/// A logged-in FREE account with zero credits: free-config answers the deployed
/// 402 contract. The current CLI must NOT call the LLM (there is no key), must
/// NOT deduct, must not crash, and must guide the user toward `--use-env-llm`.
#[test]
fn cell2_free_credits_zero_refuses_cleanly_and_suggests_env_llm() {
    let fixture = VaultFixture::new();
    fixture.login("k-broke-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content()); // healthy, but must never be reached

    // Deployed free-config 402: { error, credits:0, message }.
    api.set_free_config_402_full(0.0);

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("cell2 free credits=0", &api, &llm, &out);

    // The hosted key fetch was attempted...
    assert!(
        api.free_config_calls() >= 1,
        "expected the CLI to attempt a hosted free-config fetch.{diag}"
    );
    // ...and cleanly refused: no leased key means no LLM call.
    assert_eq!(
        llm.request_count(),
        0,
        "a 402 from free-config must abort before any LLM call — there is no key \
         to present.{diag}"
    );
    // With no ServerFallback config resolved (and nothing generated), the
    // deduct must not fire.
    assert_eq!(
        api.deduct_calls(),
        0,
        "no credits should be deducted when free-config itself 402s.{diag}"
    );
    // The user must be told how to proceed under their own key.
    assert!(
        out.contains("--use-env-llm"),
        "a credits-exhausted hosted refusal must guide the user toward \
         `--use-env-llm` (BYOK).{diag}"
    );
    // Must not have panicked / corrupted state.
    assert!(
        !out.contains("panicked"),
        "the CLI must not panic on a free-config 402.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ---------------------------------------------------------------------------
// Cell 3 — pro account active, pool funded.
// ---------------------------------------------------------------------------

/// A logged-in active PRO account gets the $10/mo pro-included lease. The old
/// CLI's `FreeConfigResponse` ignores the `provider` discriminator entirely, so
/// behaviorally this is identical to cell 1 except for the key that is leased —
/// which is exactly the point: the tier distinction lives server-side only.
#[test]
fn cell3_pro_active_gets_pro_lease_and_proceeds() {
    const PRO_KEY: &str = "enz-pro-10";

    let fixture = VaultFixture::new();
    fixture.login("k-pro-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    api.set_credit_balance(100.0);
    // The mock chooses the lease from the request identity: only the expected
    // Bearer token receives the pro lease; a bootstrap request would receive a
    // distinct free key. This proves the CLI exercises server-side tier routing.
    api.set_free_config_by_auth(
        "k-pro-user",
        PRO_KEY,
        "enz-anonymous-wrong-branch",
        &llm.base_url(),
        "test-model",
    );

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("cell3 pro active", &api, &llm, &out);

    assert!(
        out.success,
        "expected `init` to succeed for an active pro account.{diag}"
    );
    assert!(
        api.free_config_calls() >= 1,
        "expected the hosted free-config endpoint to be consulted.{diag}"
    );
    assert!(
        api.free_config_auth_headers()
            .iter()
            .any(|header| header == "Bearer k-pro-user"),
        "logged-in hosted resolution must send the account bearer token.{diag}"
    );
    assert!(
        api.free_config_bootstrap_ids().iter().all(String::is_empty),
        "logged-in hosted resolution must not also send a bootstrap identity.{diag}"
    );
    assert!(
        llm.request_count() > 0,
        "expected catalyst generation to call the leased LLM.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(PRO_KEY)),
        "expected the leased pro key {PRO_KEY:?} to be presented to the LLM \
         (old CLI ignores the provider discriminator, so the key is the only \
         observable difference from the free tier).{diag}"
    );
    assert!(
        !out.contains("Insufficient credits"),
        "an active pro account must not see an insufficient-credits error.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ---------------------------------------------------------------------------
// Cell 6 — env-llm / BYOK.
// ---------------------------------------------------------------------------

/// `init --use-env-llm` resolves credentials from the environment
/// (`ConfigProvider::Env`) and must never touch the hosted service: no
/// free-config fetch, no credit deduct, no hosted-billing messaging — even
/// though the account IS logged in (so the provider fence is the only thing
/// keeping the deduct from firing).
#[test]
fn cell6_env_llm_makes_no_hosted_call_and_never_deducts() {
    let fixture = VaultFixture::new();
    // Logged in on purpose: without this the deduct would be skipped for lack of
    // a token and the assertion would be vacuous.
    fixture.login("k-hosted-test");

    let api = MockEnzymeApi::start();
    // Any hosted billing attempt would be fatal; makes a leak loud.
    api.set_deduct_always_402(0.0);
    api.set_credit_balance(0.0);

    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--quiet"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics("cell6 env-llm/BYOK", &api, &llm, &out);

    assert!(
        llm.request_count() > 0,
        "TEST IS VACUOUS: the env LLM was never called, so the credit gate was \
         never exercised.{diag}"
    );
    assert_eq!(
        api.free_config_calls(),
        0,
        "`--use-env-llm` must not fetch hosted LLM credentials.{diag}"
    );
    assert_eq!(
        api.deduct_calls(),
        0,
        "`--use-env-llm` (ConfigProvider::Env) must never deduct hosted credits.{diag}"
    );
    assert!(
        !out.contains("app.enzyme.garden"),
        "no hosted-billing message may surface during a BYOK run.{diag}"
    );
    assert!(
        out.success,
        "expected `init --use-env-llm` to succeed against a healthy env LLM.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ---------------------------------------------------------------------------
// Cell 7 — anonymous bootstrap (unauthenticated first init).
// ---------------------------------------------------------------------------

/// A fresh machine with no login: first init is allowed loginless. The CLI must
/// send an `X-Enzyme-Bootstrap-Id` (never a Bearer token), the server hands out
/// a bootstrap lease, and catalyst generation proceeds on it. Because there is
/// no account, nothing is deducted.
#[test]
fn cell7_anonymous_bootstrap_sends_bootstrap_id_and_gets_key() {
    const BOOTSTRAP_KEY: &str = "enz-bootstrap-lease";

    // NOT logged in. Fresh isolated ENZYME_HOME => this is a legitimate first
    // init, so `require_login_for_hosted_init_if_needed` permits it.
    let fixture = VaultFixture::new();
    assert!(
        !fixture.is_logged_in(),
        "cell7 must run without login for the bootstrap path to be exercised."
    );

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    // Emulate the deployed unauthenticated contract: refuse (400) if neither a
    // Bearer token nor a bootstrap id is present. The CLI sending the bootstrap
    // id is what earns it the free lease.
    api.set_free_config_by_auth(
        "unused-account-token",
        "unused-account-lease",
        BOOTSTRAP_KEY,
        &llm.base_url(),
        "test-model",
    );

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("cell7 anonymous bootstrap", &api, &llm, &out);

    assert!(
        api.free_config_calls() >= 1,
        "expected the anonymous CLI to hit free-config.{diag}"
    );
    // A bootstrap id was sent on at least one free-config request...
    assert!(
        api.free_config_bootstrap_ids()
            .iter()
            .any(|id| !id.trim().is_empty()),
        "expected the unauthenticated CLI to send a non-empty \
         X-Enzyme-Bootstrap-Id.{diag}"
    );
    // ...and NO Bearer token was ever presented (there is no account).
    assert!(
        api.free_config_auth_headers()
            .iter()
            .all(|h| !h.to_lowercase().contains("bearer")),
        "an unauthenticated CLI must not send a Bearer token to free-config.{diag}"
    );
    // The server never refused for a missing bootstrap id...
    assert!(
        !out.contains("Missing bootstrap id"),
        "the CLI sent a bootstrap id, so the server must not have refused it.{diag}"
    );
    // ...the bootstrap lease was actually used against the LLM...
    assert!(
        llm.request_count() > 0
            && llm
                .auth_headers()
                .iter()
                .any(|h| h == &bearer(BOOTSTRAP_KEY)),
        "expected the handed-out bootstrap key {BOOTSTRAP_KEY:?} to reach the LLM.{diag}"
    );
    // ...and nothing was deducted, since there is no account to charge.
    assert_eq!(
        api.deduct_calls(),
        0,
        "an anonymous bootstrap init has no account and must not deduct credits.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ---------------------------------------------------------------------------
// Cell 2 diagnostic — a reachable-server 402 names the real cause.
// ---------------------------------------------------------------------------

/// When free-config answers a *clean* 402 (credits=0), the server was plainly
/// REACHABLE and gave a precise, correct reason. The CLI's visible skip line
/// must report the credit refusal (with top-up + BYOK guidance) rather than
/// the catch-all "Enzyme server unreachable" string, which is reserved for
/// genuine network / missing-key failures.
#[test]
fn cell2_credits_zero_message_should_not_claim_unreachable_red_by_design() {
    let fixture = VaultFixture::new();
    fixture.login("k-broke-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config_402_full(0.0);

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("cell2 credits=0 diagnostic (red)", &api, &llm, &out);

    // DESIRED behavior (fails today): a clean 402 from a reachable server must
    // not be reported as the server being unreachable.
    assert!(
        !out.contains("Enzyme server unreachable"),
        "MISLEADING DIAGNOSTIC: free-config returned a clean HTTP 402 \
         (credits=0), so the server was reachable and told us exactly why, but \
         the CLI's visible line claims 'Enzyme server unreachable'. A \
         credits-exhausted refusal must be reported as such, not as a network \
         failure.{diag}"
    );
}
