//! e2e compat group: `compat_new_endpoint_404`
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! SKEW AXIS: **NEW CLI x OLD SERVER** (compat rule R2), matrix cell 9.
//!
//! The planned release may grow the server with additive endpoints the new CLI
//! opportunistically consults — e.g. `POST /v1/credits/refund` (compensate a
//! pre-work deduct when generation fails) and `GET /v1/account` (entitlement
//! reconciliation for a lapsed-but-credited Pro). During the rollout window,
//! and forever on rollback, a NEW CLI can find itself talking to an OLD server
//! that has never heard of those routes and answers `404`. R2 requires that:
//!
//!   * the OLD (current) CLI never depends on any of these new endpoints, and
//!   * the NEW CLI degrades gracefully when they 404 (falls back to legacy
//!     behavior; no crash, no user-visible 404).
//!
//! This file has TWO parts.
//!
//! PART (a) — TESTABLE NOW (green). Drive the *current* `enzyme` binary
//! (`env!("CARGO_BIN_EXE_enzyme")`, the fixed "old CLI" artifact) as a
//! subprocess through every flow that could plausibly want a refund or an
//! account probe — a healthy hosted init, a deduct-then-LLM-failure, a
//! completion 401, and a BYOK run — and assert that `POST /v1/credits/refund`
//! and `GET /v1/account` are hit exactly ZERO times. This is a clean OLD-CLI x
//! NEW-SERVER guarantee: because the old CLI never calls these routes, ADDING
//! them to the new server cannot possibly break it. The mock mounts both routes
//! as `404` so we are proving the old CLI ignores them even when present.
//!
//! PART (b) — CONTRACT (pending, `#[ignore]`d, RED against the current CLI).
//! The NEW CLI does not exist yet, so these encode the exact gate it must pass:
//! when it calls a new endpoint against an OLD server that 404s, it must fall
//! back to legacy behavior without surfacing the 404 or crashing. Run against
//! the current binary they fail (the current CLI never makes the call), which is
//! precisely why they are ignored and recorded in `red_by_design`.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm, Reply};

/// A well-formed catalyst response so the pipeline runs to completion on the
/// happy path (a bare `{}` parses as zero catalysts and short-circuits init).
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
         free-config calls: {}\n\
         deduct calls: {}\n\
         deduct request bodies: {:?}\n\
         GET /v1/credits calls: {}\n\
         LLM requests: {}\n\
         LLM Authorization headers: {:?}\n\
         >>> refund calls (POST /v1/credits/refund): {} <<<\n\
         >>> account calls (GET /v1/account): {} <<<\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        api.free_config_calls(),
        api.deduct_calls(),
        api.deduct_requests(),
        api.credits_get_calls(),
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

/// The core R2 assertion: the current CLI must NEVER touch a new-CLI-only
/// endpoint. If either counter is non-zero the old CLI already depends on a
/// route the deployed server does not serve, and the rollout story is broken.
#[track_caller]
fn assert_no_new_cli_endpoints(api: &MockEnzymeApi, diag: &str) {
    assert_eq!(
        api.refund_calls(),
        0,
        "R2 VIOLATION: the current CLI called POST /v1/credits/refund. That route \
         is a new-CLI-only contract endpoint; an old CLI must never depend on it, \
         or adding it to the new server would not be a safe additive change.{diag}"
    );
    assert_eq!(
        api.account_calls(),
        0,
        "R2 VIOLATION: the current CLI called GET /v1/account. That route is a \
         new-CLI-only contract endpoint; an old CLI must never depend on it.{diag}"
    );
}

// ===========================================================================
// PART (a) — TESTABLE NOW. Current CLI never calls the new endpoints.
// ===========================================================================

/// Healthy hosted free-tier init: full pipeline runs, credits are deducted, the
/// leased key reaches the LLM. Even on this completely successful path the
/// current CLI must not consult `/v1/credits/refund` or `/v1/account`.
#[test]
fn healthy_hosted_init_never_calls_new_endpoints() {
    const LEASED_KEY: &str = "enz-free-020";

    let fixture = VaultFixture::new();
    fixture.login("k-free-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    api.set_credit_balance(100.0);
    api.set_free_config_free_tier(LEASED_KEY, &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("healthy hosted init", &api, &llm, &out);

    // Sanity: the run actually exercised the hosted path (otherwise the
    // zero-count assertions below would be vacuous).
    assert!(
        api.free_config_calls() >= 1 && llm.request_count() > 0,
        "TEST IS VACUOUS: the hosted free-tier path was never exercised, so the \
         'no new endpoints' guarantee proves nothing.{diag}"
    );
    assert!(
        api.deduct_calls() >= 1,
        "TEST IS VACUOUS: no credits were deducted, so there was nothing a refund \
         endpoint could have compensated.{diag}"
    );

    assert_no_new_cli_endpoints(&api, &diag);
}

/// The scenario a refund endpoint would most plausibly exist FOR: a hosted run
/// whose LLM 402s every completion. Under deduct-after-success there is no
/// up-front charge to compensate — the failed run never touches
/// `/v1/credits/deduct` at all — so no refund endpoint is needed, and the CLI
/// must not call one. This is the strongest R2 proof: the CLI requires nothing
/// new even in the exact situation that once motivated a refund route.
#[test]
fn deduct_then_llm_failure_never_refunds() {
    const LEASED_KEY: &str = "enz-lease-dry";

    let fixture = VaultFixture::new();
    fixture.login("k-hosted-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    // Reachable, authenticates fine, but every completion 402s (pool dry, the
    // exact failure the planned refund would compensate).
    llm.set_pool_dry_402();

    api.set_credit_balance(100.0);
    api.set_free_config_free_tier(LEASED_KEY, &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("deduct-then-LLM-402", &api, &llm, &out);

    // Premise 1: the work actually failed at the LLM.
    assert!(
        llm.request_count() > 0,
        "TEST IS VACUOUS: the LLM was never reached, so the failure under test \
         never happened.{diag}"
    );
    // Premise 2 (deduct-after-success): the failed run was never charged, so
    // there is nothing a refund endpoint could compensate.
    assert_eq!(
        api.deduct_calls(),
        0,
        "deduct-after-success: a hosted run that produced nothing must never call \
         POST /v1/credits/deduct.{diag}"
    );

    // The R2 guarantee: no refund call — the failed run was never billed.
    assert_no_new_cli_endpoints(&api, &diag);

    // And the CLI must not have crashed/panicked on the failure.
    assert!(
        !out.contains("panicked"),
        "the CLI must not panic when a hosted lease 402s during generation.{diag}"
    );
}

/// A completion 401 must not re-enter credential setup or call account endpoints.
/// The initially resolved lease is the only credential generation may use.
#[test]
fn completion_401_never_reresolves_or_calls_new_endpoints() {
    const KEY_INITIAL: &str = "enz-lease-initial";
    const KEY_FORBIDDEN: &str = "enz-lease-after-setup";

    let fixture = VaultFixture::new();
    fixture.login("enz-account-token");

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
    api.set_free_config_replies(vec![
        free_config_reply(KEY_INITIAL),
        free_config_reply(KEY_FORBIDDEN),
    ]);

    // First request 401s (dead lease); everything after succeeds.
    llm.set_response_sequence(vec![
        Reply::new(
            401,
            serde_json::json!({
                "error": { "message": "Invalid or expired credentials", "code": 401 }
            })
            .to_string(),
        ),
        Reply::ok_json(MockLlm::completion_body(&catalyst_content())),
    ]);

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("completion-401", &api, &llm, &out);

    assert_eq!(
        api.free_config_calls(),
        1,
        "completion re-entered setup and fetched free-config again.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(KEY_INITIAL)),
        "TEST IS VACUOUS: the configured lease never reached the LLM.{diag}"
    );
    assert!(
        llm.auth_headers()
            .iter()
            .all(|h| h != &bearer(KEY_FORBIDDEN)),
        "completion replayed with a lease minted after setup.{diag}"
    );

    assert_no_new_cli_endpoints(&api, &diag);
}

/// BYOK / env-llm: `init --use-env-llm` never touches the hosted service at all.
/// A fortiori it cannot call the new endpoints. Included so the guarantee holds
/// across the credential mode that has no hosted account in play.
#[test]
fn byok_env_llm_never_calls_new_endpoints() {
    let fixture = VaultFixture::new();
    fixture.login("k-hosted-test");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    let out = fixture
        .cli()
        .args(["init", "--use-env-llm", "--quiet"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();

    let diag = diagnostics("byok env-llm", &api, &llm, &out);

    assert!(
        llm.request_count() > 0,
        "TEST IS VACUOUS: the env LLM was never called, so no code path was \
         exercised.{diag}"
    );
    // BYOK must not touch the hosted service at all...
    assert_eq!(
        api.free_config_calls(),
        0,
        "`--use-env-llm` must not fetch hosted LLM credentials.{diag}"
    );
    // ...and therefore certainly never the new endpoints.
    assert_no_new_cli_endpoints(&api, &diag);
}

// ===========================================================================
// PART (b) — CONTRACT (pending). The NEW CLI must tolerate a 404 from a new
// endpoint served by an OLD server, degrading to legacy behavior.
//
// These are #[ignore]'d and RED against the current binary on purpose: the
// current CLI never makes the call, so the "endpoint was consulted AND the 404
// was tolerated" gate cannot be satisfied by it. They document the exact
// behavior the new CLI must ship. Recorded in `red_by_design`.
// ===========================================================================

/// CONTRACT — refund-on-failure 404 tolerance.
///
/// A refund-capable NEW CLI, after a hosted deduct followed by a failed
/// generation, POSTs `/v1/credits/refund` to compensate the charge. Against an
/// OLD server that route 404s. The new CLI MUST:
///   1. actually attempt the refund (`refund_calls() >= 1`), and
///   2. treat the 404 as "this server can't refund; leave it to server-side
///      reconciliation" — i.e. NOT surface a raw 404 / "Not found" to the user,
///      NOT panic, and NOT turn the (already-failed) run into a different,
///      refund-shaped failure.
///
/// The original generation failure may still be reported; what must NOT happen
/// is the 404 compounding it. Against the current CLI this is RED because the
/// current CLI never issues the refund (`refund_calls()` stays 0).
#[test]
#[ignore = "contract (new CLI unbuilt): new CLI must attempt POST /v1/credits/refund and tolerate an OLD-server 404 by falling back to legacy behavior; current CLI never calls it (red_by_design)"]
fn new_cli_must_tolerate_refund_404_red_by_design() {
    const LEASED_KEY: &str = "enz-lease-dry";

    let fixture = VaultFixture::new();
    fixture.login("k-hosted-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_pool_dry_402();

    api.set_credit_balance(100.0);
    api.set_free_config_free_tier(LEASED_KEY, &llm.base_url(), "test-model");
    // OLD server: the refund route is not served. The mock already mounts
    // POST /v1/credits/refund as 404 by default; this is that OLD server.

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("CONTRACT refund-404", &api, &llm, &out);

    // GATE 1 — the new CLI must actually attempt the refund after the failed,
    // already-billed run. (RED against the current CLI: it has no refund path.)
    assert!(
        api.refund_calls() >= 1,
        "NEW-CLI CONTRACT: after deducting credits (init.rs:611) and then failing \
         generation, the new CLI must attempt POST /v1/credits/refund to \
         compensate the charge. The current CLI never does — this is the pending \
         gate the new CLI must satisfy.{diag}"
    );

    // GATE 2 — the OLD server's 404 on that route must degrade gracefully: it
    // must never surface to the user as a raw 404 / "Not found", and must never
    // crash the CLI.
    assert!(
        !out.contains("404") && !out.contains("Not found"),
        "NEW-CLI CONTRACT: a 404 from an OLD server on the (new) refund route \
         must be swallowed and handled as legacy behavior, never surfaced to the \
         user.{diag}"
    );
    assert!(
        !out.contains("panicked"),
        "NEW-CLI CONTRACT: a 404 on the new refund route must not crash the \
         CLI.{diag}"
    );
}

/// CONTRACT — entitlement-reconciliation (`GET /v1/account`) 404 tolerance.
///
/// A NEW CLI may probe `GET /v1/account` to reconcile entitlement (e.g. a
/// lapsed-but-credited Pro; config 4). Against an OLD server that route 404s.
/// The new CLI MUST:
///   1. actually probe the account endpoint (`account_calls() >= 1`), and
///   2. on the 404, fall back to the LEGACY free-config-only lease path — still
///      obtain a working lease, still run generation to success, and never
///      surface the 404. The absence of `/v1/account` must be indistinguishable
///      from today's behavior for the end result.
///
/// Against the current CLI this is RED because the current CLI never probes
/// `/v1/account` (`account_calls()` stays 0).
#[test]
#[ignore = "contract (new CLI unbuilt): new CLI must probe GET /v1/account and, on an OLD-server 404, fall back to the legacy free-config lease and still succeed; current CLI never calls it (red_by_design)"]
fn new_cli_must_tolerate_account_404_red_by_design() {
    const LEASED_KEY: &str = "enz-free-020";

    let fixture = VaultFixture::new();
    fixture.login("k-lapsed-pro-user");

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    api.set_credit_balance(100.0);
    // OLD server: no entitlement endpoint, so it hands out the legacy FREE lease
    // (config 4 old-server behavior) and 404s GET /v1/account (default mount).
    api.set_free_config_lapsed_pro_old(LEASED_KEY, &llm.base_url(), "test-model");

    let out = fixture
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();

    let diag = diagnostics("CONTRACT account-404", &api, &llm, &out);

    // GATE 1 — the new CLI must actually probe the account endpoint. (RED
    // against the current CLI: it never queries /v1/account.)
    assert!(
        api.account_calls() >= 1,
        "NEW-CLI CONTRACT: the new CLI must probe GET /v1/account for entitlement \
         reconciliation. The current CLI never does — this is the pending gate \
         the new CLI must satisfy.{diag}"
    );

    // GATE 2 — the OLD server's 404 must trigger a clean fall-back to the legacy
    // free-config lease: the run must still reach the LLM and succeed, and the
    // 404 must never surface.
    assert!(
        out.success && llm.request_count() > 0,
        "NEW-CLI CONTRACT: a 404 on GET /v1/account (old server) must fall back to \
         the legacy free-config lease and let init succeed exactly as today.{diag}"
    );
    assert!(
        !out.contains("404") && !out.contains("Not found"),
        "NEW-CLI CONTRACT: a 404 on the new account route must never surface to \
         the user.{diag}"
    );
}
