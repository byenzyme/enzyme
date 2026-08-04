//! e2e compat group: `compat_stranded_entitlement_skew`
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! SKEW AXIS: **OLD CLI x {OLD, NEW} SERVER**. Every test drives the *current*
//! `enzyme` binary (`env!("CARGO_BIN_EXE_enzyme")`, the fixed "old CLI" artifact)
//! as a subprocess. OLD- and NEW-server behavior is EMULATED entirely at the mock
//! layer; the new binaries do not exist.
//!
//! ── Matrix cell 4 — lapsed Pro + stranded credits (account A) ────────────
//! An account whose Pro subscription has LAPSED but whose credit balance is still
//! positive (the credits are "stranded": the billing row no longer selects Pro,
//! yet the credits were never reclaimed).
//!
//!   * OLD server (deployed): `resolveAuthenticatedProfile` only grants Pro on a
//!     LIVE billing row (plan_id IN (pro_*), status active/trialing,
//!     current_period_end > now). A lapsed row falls through to the FREE lease
//!     ($0.20/day, provider `openrouter-free`) even though credits > 0. So
//!     free-config hands the old CLI a FREE key.
//!   * NEW server: entitlement reconciliation restores the account. free-config
//!     hands back either a PRO lease ($10/mo, provider `openrouter-pro-included`)
//!     — the credits are un-stranded — OR, if reconciliation finds the account
//!     genuinely un-entitled, a clean corrected 402 refusal.
//!
//! THE PROOF (per the scenario brief): the current CLI must consume EITHER
//! server's response correctly. The free-config SUCCESS shape is byte-identical
//! across tiers (`{api_key, base_url, model, provider, disclaimer, expires_at}`)
//! and the old CLI's `FreeConfigResponse` (client.rs:97) deserializes ONLY
//! `api_key`/`base_url`/`model` — it never reads `provider`, so it is
//! TIER-BLIND. It must therefore behave identically whether the server grants
//! free or pro, using whichever key/model it is handed. This makes it SAFE for
//! the new server to change WHICH tier a lapsed-pro account is granted: the old
//! CLI cannot tell and does not care.
//!
//! Green tests below prove that tier-blind absorption for both server variants
//! plus the corrected-refusal branch. One test is RED-BY-DESIGN: it pins the
//! *correct* diagnostic for a reachable-server corrected 402 (the current CLI
//! mislabels it "server unreachable"), and is `#[ignore]`d + recorded in
//! `red_by_design`.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm, Reply};

/// Account token seeded into `$ENZYME_HOME/auth.json`. Presence of a token is
/// what makes the CLI take the hosted (`ServerFallback`) path and arms the
/// credit preflight deduct — i.e. this is a logged-in (formerly Pro) account.
const ACCOUNT_TOKEN: &str = "enz-lapsed-pro-token";

/// A healthy, POSITIVE hosted-credit balance: the "stranded" credits of a lapsed
/// Pro. Large enough that the init preflight deduct always succeeds and the
/// balance stays positive, so any hosted-billing error the run emits would be a
/// misattribution rather than a true exhaustion.
const STRANDED_BALANCE: f64 = 100.0;

/// Key + model the OLD server leases for the lapsed-pro account (downgraded to
/// free because the billing row is no longer live).
const FREE_KEY: &str = "enz-free-020-stranded";
const FREE_MODEL: &str = "google/gemini-2.5-flash-lite-free";

/// Key + model the NEW server leases after entitlement reconciliation restores
/// the Pro grant. Deliberately DIFFERENT strings from the free lease so a test
/// can prove the CLI uses *whichever* it was handed, not a hard-coded tier.
const PRO_KEY: &str = "enz-pro-10-reconciled";
const PRO_MODEL: &str = "google/gemini-2.5-pro-reconciled";

/// A well-formed catalyst response so the pipeline runs to completion rather
/// than bailing early on an empty parse. (Same shape the baseline group uses.)
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

/// Sum of `amount` over every deduct body = net hosted credits billed.
fn net_credits_billed(api: &MockEnzymeApi) -> f64 {
    api.deduct_requests()
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter_map(|v| v["amount"].as_f64())
        .sum()
}

fn diagnostics(label: &str, api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "\n=== {label} ===\n\
         free-config calls (GET {}): {}\n\
         free-config auth headers: {:?}\n\
         deduct calls: {} | net billed: {}\n\
         deduct bodies: {:?}\n\
         LLM requests: {}\n\
         LLM Authorization headers: {:?}\n\
         refund calls: {} | account calls: {}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
        api.free_config_auth_headers(),
        api.deduct_calls(),
        net_credits_billed(api),
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

/// R2: the current CLI must never depend on a not-yet-deployed endpoint. Cell 4
/// is exactly where a *new* CLI would be tempted to query `GET /v1/account` for
/// entitlement reconciliation — prove the OLD CLI never does.
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
        "current CLI must never call GET /v1/account for entitlement reconciliation \
         (new-CLI contract only); the old CLI must require nothing new.{diag}"
    );
}

/// The old CLI must not be killed by a signal and must not panic on any
/// new-server response.
#[track_caller]
fn assert_no_panic(out: &CliOutput, diag: &str) {
    assert!(
        out.code.is_some(),
        "the CLI was killed by a signal (no exit code) instead of exiting cleanly.{diag}"
    );
    assert_ne!(
        out.code,
        Some(101),
        "the CLI exited 101 (Rust panic/abort code) — the new-server response \
         panicked the old CLI.{diag}"
    );
    for marker in ["panicked at", "RUST_BACKTRACE", "internal error:"] {
        assert!(
            !out.contains(marker),
            "the CLI output contains panic marker {marker:?}.{diag}"
        );
    }
}

/// Drive one hosted `init` for the lapsed-pro account, with `free_config` already
/// configured on `api` by the caller. Logs in, seeds a healthy stranded balance,
/// arms the LLM with a real catalyst body.
fn run_lapsed_pro_init(api: &MockEnzymeApi, llm: &MockLlm, vault: &VaultFixture) -> CliOutput {
    vault.login(ACCOUNT_TOKEN);
    api.set_credit_balance(STRANDED_BALANCE);
    llm.set_ok_content(&catalyst_content());
    vault
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(api)
        .run()
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 4, OLD server — lapsed Pro is downgraded to a FREE lease; old CLI proceeds.
// ─────────────────────────────────────────────────────────────────────────────

/// BASELINE (green). Against the *deployed* server a lapsed Pro with stranded
/// credits is handed a FREE lease. The current CLI takes it, presents that key to
/// the LLM, runs catalyst generation, and the current-server preflight deducts
/// the stranded credits. init succeeds and no hosted-billing error surfaces.
#[test]
fn cell4_old_server_lapsed_pro_gets_free_lease_and_proceeds() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();

    // OLD-server behavior: lapsed billing row => free lease even with credits > 0.
    api.set_free_config_lapsed_pro_old(FREE_KEY, &llm.base_url(), FREE_MODEL);

    let out = run_lapsed_pro_init(&api, &llm, &vault);
    let diag = diagnostics("cell4 OLD server (free lease)", &api, &llm, &out);

    assert!(
        out.success,
        "expected `init` to succeed for a lapsed-pro account handed a free lease.{diag}"
    );
    assert!(
        api.free_config_calls() >= 1,
        "expected the hosted free-config endpoint to be consulted.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(FREE_KEY)),
        "expected the leased FREE key {FREE_KEY:?} to be presented to the LLM.{diag}"
    );
    assert!(
        llm.requests().iter().any(|b| b.contains(FREE_MODEL)),
        "expected the leased FREE model {FREE_MODEL:?} to be used in the completion \
         request (the CLI uses whatever model the server hands it).{diag}"
    );
    assert!(
        api.deduct_calls() >= 1,
        "current-server contract: the hosted free-tier preflight deducts credits \
         (init.rs:611).{diag}"
    );
    assert!(
        !out.contains("Insufficient credits"),
        "a lapsed-pro account with stranded (positive) credits must not see an \
         insufficient-credits error on the free-lease path.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 4, NEW server — entitlement reconciled to a PRO lease; old CLI proceeds.
// THE PRIMARY OLD-CLI x NEW-SERVER PROOF.
// ─────────────────────────────────────────────────────────────────────────────

/// GREEN (compat proof). The new server reconciles the stranded entitlement and
/// hands the SAME account a PRO lease. The response shape is identical to the
/// free lease save for the (ignored) `provider` discriminator and the key/model
/// strings. The current CLI must absorb it exactly as it did the free lease:
/// present the PRO key to the LLM, run to completion, deduct, succeed. It must
/// NOT special-case, reject, or otherwise depend on the tier it was granted.
#[test]
fn cell4_new_server_reconciled_pro_lease_old_cli_proceeds() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();

    // NEW-server behavior: reconciliation restores the Pro grant.
    api.set_free_config_lapsed_pro_new(PRO_KEY, &llm.base_url(), PRO_MODEL);

    let out = run_lapsed_pro_init(&api, &llm, &vault);
    let diag = diagnostics("cell4 NEW server (reconciled pro lease)", &api, &llm, &out);

    assert!(
        out.success,
        "expected `init` to succeed when the new server hands the lapsed-pro \
         account a reconciled PRO lease — the old CLI must consume it exactly as \
         it does a free lease.{diag}"
    );
    assert!(
        api.free_config_calls() >= 1,
        "expected the hosted free-config endpoint to be consulted.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(PRO_KEY)),
        "expected the reconciled PRO key {PRO_KEY:?} to be presented to the LLM — \
         the old CLI must use whatever key it is handed, tier notwithstanding.{diag}"
    );
    assert!(
        llm.requests().iter().any(|b| b.contains(PRO_MODEL)),
        "expected the reconciled PRO model {PRO_MODEL:?} to be used in the \
         completion request.{diag}"
    );
    assert!(
        api.deduct_calls() >= 1,
        "the old CLI's hosted preflight fires on the ServerFallback path \
         regardless of which tier the server granted.{diag}"
    );
    assert!(
        !out.contains("Insufficient credits"),
        "a reconciled pro account must not see an insufficient-credits error.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 4 — the tier-blindness equivalence proof (the crux of the skew axis).
// ─────────────────────────────────────────────────────────────────────────────

/// GREEN (the central claim). Run the identical `init` twice for the SAME
/// lapsed-pro account: once against the OLD server (free lease) and once against
/// the NEW server (reconciled pro lease). Assert the two runs are observably
/// EQUIVALENT on every dimension the old CLI can affect — success, whether the
/// LLM was reached, whether credits were deducted, absence of billing errors,
/// and non-use of new endpoints — with the ONLY observable difference being
/// which key/model reached the LLM. That equivalence is the proof that the old
/// CLI "does not depend on tier", so it is safe for the new server to change
/// which tier a lapsed-pro account is granted.
#[test]
fn cell4_old_cli_is_tier_blind_free_and_pro_behave_identically() {
    // -- OLD server run: free lease --------------------------------------------
    let api_old = MockEnzymeApi::start();
    let llm_old = MockLlm::start();
    let vault_old = VaultFixture::new();
    api_old.set_free_config_lapsed_pro_old(FREE_KEY, &llm_old.base_url(), FREE_MODEL);
    let out_old = run_lapsed_pro_init(&api_old, &llm_old, &vault_old);
    let diag_old = diagnostics("tier-blind: OLD (free)", &api_old, &llm_old, &out_old);

    // -- NEW server run: reconciled pro lease ----------------------------------
    let api_new = MockEnzymeApi::start();
    let llm_new = MockLlm::start();
    let vault_new = VaultFixture::new();
    api_new.set_free_config_lapsed_pro_new(PRO_KEY, &llm_new.base_url(), PRO_MODEL);
    let out_new = run_lapsed_pro_init(&api_new, &llm_new, &vault_new);
    let diag_new = diagnostics("tier-blind: NEW (pro)", &api_new, &llm_new, &out_new);

    let both = format!("{diag_old}{diag_new}");

    // Same success outcome.
    assert_eq!(
        out_old.success, out_new.success,
        "SUCCESS OUTCOME DIVERGED across tiers — the old CLI reacted to the tier \
         it was granted, which it must not do.{both}"
    );
    assert!(
        out_old.success && out_new.success,
        "both tier variants must succeed for the equivalence to be meaningful.{both}"
    );

    // Same observable "did we reach the LLM?" outcome.
    assert!(
        llm_old.request_count() > 0 && llm_new.request_count() > 0,
        "both variants must reach the LLM with the leased key; the run under one \
         tier never generated catalysts.{both}"
    );

    // Same "did we deduct hosted credits?" outcome (both take ServerFallback).
    assert_eq!(
        api_old.deduct_calls() > 0,
        api_new.deduct_calls() > 0,
        "DEDUCT BEHAVIOR DIVERGED across tiers — the credit preflight fired on one \
         tier but not the other, so the old CLI is (wrongly) tier-sensitive.{both}"
    );

    // Neither tier surfaces a hosted-billing error (both accounts are healthy).
    for (label, out) in [("OLD/free", &out_old), ("NEW/pro", &out_new)] {
        assert!(
            !out.contains("Insufficient credits") && !out.contains("app.enzyme.garden"),
            "the {label} run surfaced a hosted-billing error for a healthy \
             account.{both}"
        );
    }

    // Neither tier touches a new-CLI-only endpoint.
    assert_no_new_cli_endpoints(&api_old, &diag_old);
    assert_no_new_cli_endpoints(&api_new, &diag_new);

    // THE ONLY permitted difference: which key/model reached the LLM.
    assert!(
        llm_old.auth_headers().iter().any(|h| h == &bearer(FREE_KEY))
            && !llm_old.auth_headers().iter().any(|h| h == &bearer(PRO_KEY)),
        "the OLD-server run should present only the FREE key.{diag_old}"
    );
    assert!(
        llm_new.auth_headers().iter().any(|h| h == &bearer(PRO_KEY))
            && !llm_new.auth_headers().iter().any(|h| h == &bearer(FREE_KEY)),
        "the NEW-server run should present only the reconciled PRO key.{diag_new}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 4, NEW server — reconciled PRO lease carrying ADDITIVE reconciliation
// fields (R1 / R5). The old CLI must still parse it.
// ─────────────────────────────────────────────────────────────────────────────

/// GREEN (R1/R5 additive tolerance). A realistic new-server reconciliation
/// response carries fields the old CLI has never seen (`entitlement_source`,
/// `reconciled`, `stranded_credits_restored`, `tier`) ALONGSIDE the 3 load-
/// bearing fields. Because `FreeConfigResponse` has no `#[serde(deny_unknown_
/// fields)]` (client.rs:97), the old CLI must ignore the extras and proceed on
/// the PRO key exactly as in the plain reconciled case.
#[test]
fn cell4_new_server_reconciled_pro_with_additive_fields_still_parses() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();

    // Deployed success shape + reconciliation-specific additive fields.
    api.set_free_config_json(serde_json::json!({
        "api_key": PRO_KEY,
        "base_url": llm.base_url(),
        "model": PRO_MODEL,
        "provider": "openrouter-pro-included",
        "disclaimer": "Pro AI.",
        "expires_at": 4102444800u64,
        // Additive fields introduced by the entitlement-reconciliation fix:
        "tier": "pro",
        "entitlement_source": "reconciled_stranded_credits",
        "reconciled": true,
        "stranded_credits_restored": STRANDED_BALANCE,
        "pool_status": "funded",
    }));

    let out = run_lapsed_pro_init(&api, &llm, &vault);
    let diag = diagnostics("cell4 NEW server (additive reconciliation)", &api, &llm, &out);

    assert!(
        out.success,
        "expected `init` to succeed on a reconciled PRO lease carrying additive \
         fields — the old CLI must ignore unknown fields, not reject the body.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(PRO_KEY)),
        "expected the reconciled PRO key to reach the LLM despite the additive \
         response fields.{diag}"
    );
    assert!(
        !out.contains("panicked") && !out.contains("missing field"),
        "the additive fields must not cause a parse error or panic in the old \
         CLI.{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 4, NEW server — the "corrected refusal" branch. Reconciliation finds the
// account genuinely un-entitled and refuses cleanly at free-config.
// ─────────────────────────────────────────────────────────────────────────────

/// GREEN (compat proof, refusal branch). The scenario's "OR a corrected refusal"
/// arm: the new server decides the lapsed account is not entitled and answers a
/// structured 402 at free-config, BEFORE minting a lease. The old CLI must absorb
/// this cleanly — no panic, no half-written index DB, no LLM call (no key was
/// minted), no hosted deduct (no lease was received), and no dependence on any
/// new endpoint. It may bail, but it must bail SANELY.
#[test]
fn cell4_new_server_corrected_refusal_old_cli_bails_sanely() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();
    vault.login(ACCOUNT_TOKEN);
    api.set_credit_balance(STRANDED_BALANCE);
    llm.set_ok_content(&catalyst_content()); // healthy, but must never be reached

    // NEW-server corrected-entitlement refusal: a structured 402 at free-config
    // carrying reconciliation-reason fields. No lease is handed out.
    let body = serde_json::json!({
        "error": "Insufficient credits",
        "credits": STRANDED_BALANCE,
        "message": "Visit app.enzyme.garden/plans to top up.",
        "entitlement_source": "reconciled_not_entitled",
        "reconciled": true,
    });
    api.set_free_config_replies(vec![Reply::new(402, body.to_string())]);

    let out = vault
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("cell4 NEW server (corrected refusal)", &api, &llm, &out);

    // The refusal was actually exercised.
    assert!(
        api.free_config_calls() >= 1,
        "expected the CLI to call free-config (where the new-server refusal \
         lives).{diag}"
    );
    // Core compat invariant: no crash.
    assert_no_panic(&out, &diag);
    // No lease was minted, so the CLI must never reach the LLM...
    assert_eq!(
        llm.request_count(),
        0,
        "a corrected 402 at free-config hands out no key, so the old CLI must make \
         ZERO LLM calls.{diag}"
    );
    // ...and it must not deduct hosted credits for a lease it never received.
    assert_eq!(
        api.deduct_calls(),
        0,
        "the old CLI must not deduct hosted credits when free-config refused before \
         minting a lease (net billed {}).{diag}",
        net_credits_billed(&api),
    );
    // It must guide the user to BYOK rather than dead-end them.
    assert!(
        out.contains("--use-env-llm"),
        "a hosted refusal must guide the user toward `--use-env-llm` (BYOK).{diag}"
    );
    assert_no_new_cli_endpoints(&api, &diag);
}

// ─────────────────────────────────────────────────────────────────────────────
// RED BY DESIGN — the corrected-refusal diagnostic is misleading (init.rs:829).
// ─────────────────────────────────────────────────────────────────────────────

/// RED-BY-DESIGN (correct behavior; currently fails). The new server's corrected
/// entitlement refusal is a *reachable-server* HTTP 402 that states a precise
/// reason. Yet the current CLI's visible quiet-mode line is the catch-all
/// `ApiSkipped` string (init.rs:829):
///
///   "No API key found and Enzyme server unreachable — skipping catalyst
///    generation. Set OPENAI_API_KEY or ensure network access."
///
/// That mislabels a reachable-server, entitlement-driven 402 as a network /
/// missing-key failure. Worse in *this* cell: the account still holds stranded
/// credits (balance > 0), so "server unreachable" is doubly wrong — the server
/// was reached AND the account has credits. A correct new CLI must report a 402
/// refusal as an entitlement/credits condition, never as an unreachable server.
///
/// Marked `#[ignore]` (the current binary fails it) and recorded in
/// `red_by_design`. This defines the new-CLI diagnostic gate for cell 4; it is
/// the same underlying init.rs:829 defect the baseline group documents for
/// cell 2, exercised here on the stranded-entitlement refusal.
#[test]
#[ignore = "red_by_design: current CLI mislabels a reachable-server corrected 402 \
            (lapsed-pro w/ stranded credits) as 'server unreachable' (init.rs:829)"]
fn cell4_corrected_refusal_message_should_not_claim_unreachable_red_by_design() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();
    vault.login(ACCOUNT_TOKEN);
    api.set_credit_balance(STRANDED_BALANCE);
    llm.set_ok_content(&catalyst_content());

    let body = serde_json::json!({
        "error": "Insufficient credits",
        "credits": STRANDED_BALANCE,
        "message": "Visit app.enzyme.garden/plans to top up.",
        "entitlement_source": "reconciled_not_entitled",
        "reconciled": true,
    });
    api.set_free_config_replies(vec![Reply::new(402, body.to_string())]);

    let out = vault
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(&api)
        .run();
    let diag = diagnostics("cell4 corrected refusal diagnostic (red)", &api, &llm, &out);

    // DESIRED (fails today): a reachable-server 402 must not be reported as the
    // server being unreachable.
    assert!(
        !out.contains("Enzyme server unreachable"),
        "MISLEADING DIAGNOSTIC: free-config returned a clean HTTP 402 for a \
         lapsed-pro account that still holds stranded credits, so the server was \
         reachable and gave a precise reason, yet the CLI's visible line claims \
         'Enzyme server unreachable'. An entitlement/credits refusal must be \
         reported as such, not as a network failure.{diag}"
    );
}
