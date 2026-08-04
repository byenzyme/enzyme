//! e2e compat group: compat_pool_dry_skew
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//! Skew axis: OLD CLI x {OLD, NEW} SERVER. The CLI subprocess IS the "old CLI"
//! (a fixed artifact). New-server behavior is EMULATED at the mock layer.
//!
//! These two cells are the reported-account root cause: every leased key draws from
//! ONE shared OpenRouter pool. When the pool hits $0, EVERY leased key returns
//! HTTP 402 on *completion*, regardless of the per-key ($0.20/$10) limit and
//! regardless of the account's healthy Enzyme hosted-credit balance.
//!
//! ── Cell 5a (OLD server) ────────────────────────────────────────────────────
//! The deployed server has no pool-headroom check, so free-config still mints a
//! valid 200 lease. The pool-dry 402 only surfaces later, on the LLM completion.
//! The current CLI misclassifies that provider/pool 402 as *hosted billing*
//! exhaustion (`is_credits_exhausted_error`, catalysts.rs:1492 still matches the
//! literal "Insufficient credits" / "status 402 " that OpenRouter's pool-dry
//! body carries) and aborts the whole run with a top-up message pointing at
//! app.enzyme.garden/plans — even though the account's hosted credits were fine
//! (the deduct preflight succeeded and the balance stayed positive). Topping up
//! those credits cannot refill the shared pool, so the message is actively
//! misleading. This documents the current broken behavior; the correct-behavior
//! assertion is RED-BY-DESIGN (see `pool_dry_old_server_*_red_by_design`).
//!
//! ── Cell 5b (NEW server) ────────────────────────────────────────────────────
//! The new server adds a pool-headroom check and CLEANLY REFUSES at free-config
//! (structured 402) before handing out a doomed lease. This is the OLD CLI x NEW
//! SERVER proof for the pool-dry path: the current CLI must absorb that
//! new-server response WITHOUT crashing or corrupting state. It may bail, but it
//! must bail SANELY — no panic, no partial DB corruption, sane exit code, and it
//! must never reach the LLM or deduct hosted credits for a lease it never got.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// Enzyme account token seeded into `$ENZYME_HOME/auth.json`. Makes the CLI take
/// the hosted (`ServerFallback`) path and arms the credit preflight.
const ACCOUNT_TOKEN: &str = "enz-account-token";

/// The leased key the OLD-server free-config hands out in 5a.
const LEASED_KEY: &str = "enz-pool-lease";

/// A healthy hosted-credit balance. Large enough that the init preflight deduct
/// (3 credits) always succeeds and the balance stays positive — so any
/// "insufficient credits / top up" message the run emits is provably a
/// MISATTRIBUTION of the shared-pool failure, not a real hosted-credit problem.
const HEALTHY_BALANCE: f64 = 100.0;

/// Markers that would prove the run laundered a pool/provider 402 into an
/// Enzyme HOSTED-billing error the user cannot act on.
const HOSTED_BILLING_MARKERS: &[&str] = &["app.enzyme.garden/plans", "catalyst generation aborted"];

/// Sum of the `amount` field over every `/v1/credits/deduct` body = net hosted
/// credits actually billed to the account during the run.
fn net_credits_billed(api: &MockEnzymeApi) -> f64 {
    api.deduct_requests()
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter_map(|v| v["amount"].as_f64())
        .sum()
}

fn diagnostics(api: &MockEnzymeApi, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "--- run diagnostics ---\n\
         exit code:          {:?} (success={})\n\
         free-config calls:  {}\n\
         deduct calls:       {}\n\
         deduct bodies:      {:?}\n\
         net credits billed: {}\n\
         LLM requests:       {}\n\
         refund calls:       {}\n\
         account calls:      {}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.code,
        out.success,
        api.free_config_calls(),
        api.deduct_calls(),
        api.deduct_requests(),
        net_credits_billed(api),
        llm.request_count(),
        api.refund_calls(),
        api.account_calls(),
        out.stdout,
        out.stderr,
    )
}

/// First 16 bytes of every SQLite 3 database file. Used as a lightweight,
/// dependency-free corruption smoke test on the vault index DB.
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// Assert the vault index DB (`<vault>/.enzyme/enzyme.db`), if it exists, is not
/// truncated/garbled — i.e. the run did not leave the DB half-written.
#[track_caller]
fn assert_db_not_corrupt(vault: &VaultFixture, diag: &str) {
    let db = vault.vault_enzyme_dir().join("enzyme.db");
    if !db.exists() {
        // No DB at all is acceptable for these scenarios (index may not have
        // been created); corruption is only meaningful for a DB that IS there.
        return;
    }
    let bytes = std::fs::read(&db).expect("read vault index db");
    assert!(
        bytes.len() >= SQLITE_MAGIC.len() && &bytes[..SQLITE_MAGIC.len()] == SQLITE_MAGIC,
        "vault index DB at {} is present but does not begin with the SQLite 3 \
         header — the run left it corrupt/half-written.\n{diag}",
        db.display(),
    );
}

/// Assert the process was not killed by a signal and did not panic. This is the
/// core "bail SANELY" invariant for the old-CLI x new-server proof.
#[track_caller]
fn assert_no_panic(out: &CliOutput, diag: &str) {
    assert!(
        out.code.is_some(),
        "the CLI was killed by a signal (no exit code) instead of exiting \
         cleanly — that is a crash, not a sane bail.\n{diag}",
    );
    assert_ne!(
        out.code,
        Some(101),
        "the CLI exited 101, the Rust panic/abort code — the new-server response \
         panicked the old CLI.\n{diag}",
    );
    for marker in ["panicked at", "RUST_BACKTRACE", "internal error:"] {
        assert!(
            !out.contains(marker),
            "the CLI output contains panic marker {marker:?} — the new-server \
             response panicked the old CLI.\n{diag}",
        );
    }
}

/// Shared setup for the 5a (old-server pool-dry) scenario: a logged-in hosted
/// run against a free-config that MINTS a working lease (200) but an LLM that
/// 402s every completion with the canonical OpenRouter pool-dry body.
fn run_pool_dry_old_server(llm: &MockLlm, api: &MockEnzymeApi, vault: &VaultFixture) -> CliOutput {
    vault.login(ACCOUNT_TOKEN);
    api.set_credit_balance(HEALTHY_BALANCE);
    // OLD-server behavior: no pool-headroom check, key is minted regardless.
    api.set_pool_dry_old(LEASED_KEY, &llm.base_url(), "test-model");
    // The pool is dry: every completion 402s with the OpenRouter pool-dry body.
    llm.set_pool_dry_402();

    vault
        .cli()
        .enzyme_api(api)
        .args(["init", "--json-progress"])
        .run()
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 5a — OLD server, pool dry. Documents the current misclassification.
// ─────────────────────────────────────────────────────────────────────────────

/// PREMISE (green). Establishes, with true invariants only, that in 5a the
/// account's hosted credits are healthy and the failure originates entirely at
/// the LLM/pool layer. This is what makes any hosted-billing message in the
/// companion red-by-design test a provable MISATTRIBUTION rather than a correct
/// report. Asserts nothing about the (buggy) message itself.
#[test]
fn pool_dry_old_server_hosted_credits_are_healthy_and_failure_is_at_the_llm() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();

    let out = run_pool_dry_old_server(&llm, &api, &vault);
    let diag = diagnostics(&api, &llm, &out);

    // The lease was minted (free-config returned 200, not a refusal).
    assert!(
        api.free_config_calls() >= 1,
        "expected free-config to be hit to mint a lease; it never was.\n{diag}"
    );
    // The failure actually surfaced at the LLM (the pool-dry 402), not earlier.
    assert!(
        llm.request_count() >= 1,
        "expected the run to reach the mock LLM (where the pool-dry 402 lives); \
         it never did, so the scenario under test never happened.\n{diag}"
    );
    // Hosted credits were fine: the preflight deduct fired and SUCCEEDED (the
    // mock 402s a deduct only when balance < amount; balance is HEALTHY_BALANCE).
    assert!(
        api.deduct_calls() >= 1,
        "expected the hosted preflight to deduct credits (ServerFallback path); \
         it never did.\n{diag}"
    );
    let billed = net_credits_billed(&api);
    assert!(
        billed > 0.0 && billed < HEALTHY_BALANCE,
        "expected hosted credits to be charged AND the balance to remain \
         positive (net billed {billed}, starting balance {HEALTHY_BALANCE}) — \
         proving the account had ample hosted credit and the 402 came from the \
         shared pool, not the account.\n{diag}"
    );
}

/// RED-BY-DESIGN (correct behavior; currently fails). A shared-pool 402 that
/// surfaces on the LLM completion of a freshly-minted, well-funded hosted lease
/// must NOT be reported to the user as Enzyme hosted-credit exhaustion. The
/// account's hosted credits are provably fine (see the premise test), so
/// directing the user to app.enzyme.garden/plans to "top up" is unactionable and
/// wrong: topping up hosted credits cannot refill the shared OpenRouter pool.
///
/// Planned fix D1 classifies by (credential mode, HTTP status) client-side: a
/// 402 on a hosted *lease* means "this lease is spent / pool issue" (re-mint or
/// report a pool/provider error), never "your account is out of credits".
///
/// This is marked `#[ignore]` because it asserts the POST-FIX contract and is
/// red against the current binary.
#[test]
#[ignore = "RED-BY-DESIGN (D1): a shared-pool 402 on a healthy hosted lease is \
            misreported as hosted-credit exhaustion (app.enzyme.garden/plans). \
            Correct behavior: never send a user with healthy hosted credits to \
            top up; attribute the failure to the pool/provider."]
fn pool_dry_old_server_must_not_blame_hosted_credits_red_by_design() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();

    let out = run_pool_dry_old_server(&llm, &api, &vault);
    let diag = diagnostics(&api, &llm, &out);

    // Premise (true today): credits healthy, failure originated at the LLM/pool.
    assert!(
        llm.request_count() >= 1 && api.deduct_calls() >= 1,
        "scenario premise not met (LLM never called or no deduct) — the \
         misattribution assertion below would be meaningless.\n{diag}"
    );
    let billed = net_credits_billed(&api);
    assert!(
        billed < HEALTHY_BALANCE,
        "premise: expected hosted balance to remain positive after the preflight \
         deduct.\n{diag}"
    );

    // THE CORRECT BEHAVIOR (currently violated): the run must not launder the
    // shared-pool 402 into a hosted-billing top-up directive.
    for marker in HOSTED_BILLING_MARKERS {
        assert!(
            !out.contains(marker),
            "MISATTRIBUTED POOL EXHAUSTION: the shared OpenRouter pool was dry, so \
             the leased key 402'd on completion, but the account's hosted credits \
             were fine (net billed {billed} of {HEALTHY_BALANCE}, balance still \
             positive). The CLI nonetheless emitted the hosted-billing marker \
             {marker:?}, sending a well-funded user to top up credits that were \
             never the problem — and cannot refill the pool.\n\
             Root cause: is_credits_exhausted_error (catalysts.rs:1492) still \
             matches the literal \"Insufficient credits\" / \"status 402 \" in \
             OpenRouter's pool-dry body and is not gated on (credential mode, \
             HTTP status). Planned fix D1.\n{diag}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cell 5b — NEW server, pool dry. The old-CLI x new-server compat proof.
// ─────────────────────────────────────────────────────────────────────────────

/// GREEN (compat proof). The new server refuses cleanly at free-config with a
/// structured 402 before handing out a doomed lease. The current ("old") CLI
/// must absorb this WITHOUT crashing or corrupting state.
#[test]
fn pool_dry_new_server_old_cli_bails_sanely() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();
    vault.login(ACCOUNT_TOKEN);
    api.set_credit_balance(HEALTHY_BALANCE);

    // NEW-server behavior: pool-headroom check trips, free-config itself 402s
    // (clean pre-key refusal). The reported credits are the account's real,
    // healthy balance — the refusal is about the POOL, not the account.
    api.set_pool_dry_new(HEALTHY_BALANCE);

    let out = vault
        .cli()
        .enzyme_api(&api)
        .args(["init", "--json-progress"])
        .run();
    let diag = diagnostics(&api, &llm, &out);

    // Sanity: the new-server free-config refusal was actually exercised.
    assert!(
        api.free_config_calls() >= 1,
        "expected the CLI to call free-config (where the new-server refusal \
         lives); it never did.\n{diag}"
    );

    // ── Core compat invariant: no crash, no corruption ──────────────────────
    assert_no_panic(&out, &diag);
    assert_db_not_corrupt(&vault, &diag);

    // The refusal happened BEFORE any lease was handed out, so the CLI must
    // never reach the LLM with a doomed key...
    assert_eq!(
        llm.request_count(),
        0,
        "expected the old CLI to make ZERO LLM calls when the new server refused \
         at free-config (no lease was ever minted), but it hit the LLM {} \
         time(s).\n{diag}",
        llm.request_count(),
    );
    // ...and it must not deduct hosted credits for a lease it never received.
    assert_eq!(
        api.deduct_calls(),
        0,
        "expected the old CLI to deduct ZERO hosted credits when free-config \
         refused before minting a lease, but it deducted {} time(s) (a credit \
         leak against a run that got no LLM access at all).\n{diag}",
        net_credits_billed(&api),
    );
    // R2: the current CLI must not depend on any new endpoint to handle this.
    assert_eq!(
        api.refund_calls() + api.account_calls(),
        0,
        "the old CLI hit a new-CLI-contract endpoint (refund/account) while \
         handling the new-server refusal — it must require nothing new.\n{diag}"
    );
}

/// GREEN (compat proof, anonymous variant). Same new-server pool-dry refusal,
/// but on an UNAUTHENTICATED first-init (bootstrap) CLI. A first-time user hit
/// by a dry pool must still bail sanely — this is the path least likely to have
/// been exercised in the field.
#[test]
fn pool_dry_new_server_anonymous_first_init_bails_sanely() {
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    let vault = VaultFixture::new();
    // NOTE: no login() — anonymous bootstrap.
    api.set_pool_dry_new(0.0);

    let out = vault
        .cli()
        .enzyme_api(&api)
        .args(["init", "--json-progress"])
        .run();
    let diag = diagnostics(&api, &llm, &out);

    assert_no_panic(&out, &diag);
    assert_db_not_corrupt(&vault, &diag);
    assert_eq!(
        llm.request_count(),
        0,
        "expected zero LLM calls for an anonymous CLI refused at free-config.\n{diag}"
    );
    assert_eq!(
        api.deduct_calls(),
        0,
        "an anonymous first-init must never deduct hosted credits.\n{diag}"
    );
}
