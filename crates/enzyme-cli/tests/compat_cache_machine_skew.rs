//! e2e compat group: `compat_cache_machine_skew`
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! SKEW AXIS: **CO-LOCATED CLI skew** (R3) — matrix cell 10.
//!
//! Two `enzyme` CLIs can share one machine's free-config cache
//! (`ENZYME_API_CACHE_PATH`, default `/tmp/enzyme-api-cache`). A newer co-located
//! CLI writing that cache and an OLDER CLI reading it must stay compatible. The
//! old CLI is the FIXED artifact driven here (`env!("CARGO_BIN_EXE_enzyme")`); a
//! "newer CLI" is EMULATED by seeding the shared cache file with the exact shape
//! a future CLI would persist.
//!
//! The read path under test is `resolve_hosted_api_config` (client.rs:1020):
//!   * cache exists + parses + `api_key` non-empty + not expired  -> REUSE it,
//!     do NOT contact the server;
//!   * cache exists but is expired (age > 300s TTL, or `cached_at == 0`)
//!     -> re-fetch from the server and rewrite the cache;
//!   * cache exists but FAILS to parse (`CachedFreeConfig` has no
//!     `deny_unknown_fields`, but every core field is required)
//!     -> `serde_json::from_str::<CachedFreeConfig>(..)?` PROPAGATES the error
//!     (client.rs:1027) — it does NOT gracefully drop the cache and re-fetch.
//!
//! What we pin here:
//!
//!   (a) ADDITIVE tolerance (must be GREEN): a cache carrying the 3 core fields
//!       PLUS unknown/future fields a newer co-located CLI persisted must still
//!       be read and REUSED by the current CLI, with `api_key`/`base_url`/`model`
//!       threaded through unchanged and NO re-fetch. This is the compliant-new-CLI
//!       rollout path (R3: additive-only cache changes are safe).
//!
//!   (b) MISSING a core field (documenting + gate): a cache dropping `model`
//!       makes the current CLI's non-lenient parse propagate. We DOCUMENT that the
//!       current CLI neither crashes nor self-heals: it does NOT re-fetch, and
//!       silently skips catalyst generation (mislabeled "server unreachable"). A
//!       companion `*_red_by_design` (ignored) asserts the SAFE behavior a robust
//!       CLI should have — gracefully discard the unparseable cache and re-fetch.
//!       This pins the R3 constraint: a new CLI MUST only ADD `serde(default)`
//!       fields and MUST NEVER drop/rename a core field, because the old CLI has
//!       no recovery path if it does.
//!
//!   (c) TTL (must be GREEN): a fresh parseable cache (age < 300s) is honored
//!       (reused, no server call); a stale parseable cache (age > 300s) is
//!       re-fetched and its stale values are NOT used.

mod support;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A well-formed catalyst response body so catalyst generation genuinely runs to
/// completion when the LLM is actually reached (a trivial `{}` parses as zero
/// catalysts and short-circuits the pipeline).
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

/// Render every observable fact about a run so a failure explains itself.
fn diagnostics(
    label: &str,
    shared_cache: &VaultFixture,
    api: &MockEnzymeApi,
    llm: &MockLlm,
    out: &CliOutput,
) -> String {
    format!(
        "\n=== {label} ===\n\
         free-config calls (GET {}): {}\n\
         deduct calls: {}\n\
         LLM requests: {}\n\
         LLM Authorization headers: {:?}\n\
         cache exists: {} | cache cached_at: {:?}\n\
         cache raw: {:?}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        MockEnzymeApi::FREE_CONFIG_PATH,
        api.free_config_calls(),
        api.deduct_calls(),
        llm.request_count(),
        llm.auth_headers(),
        shared_cache.api_cache_exists(),
        shared_cache.api_cache_cached_at(),
        shared_cache.read_api_cache(),
        out.code,
        out.success,
        out.stdout,
        out.stderr,
    )
}

#[track_caller]
fn assert_no_panic(out: &CliOutput, diag: &str) {
    assert!(
        !out.contains("panicked") && !out.contains("RUST_BACKTRACE"),
        "the CLI must not panic reading a co-located cache.{diag}"
    );
}

/// Drive the CURRENT ("old") CLI's `init --quiet` against a cache file that a
/// (possibly newer) co-located CLI has already written. `shared_cache` OWNS the
/// cache file; `old_cli` is a *separate* vault + home whose
/// `ENZYME_API_CACHE_PATH` is redirected at that same file, modeling two CLIs
/// sharing one machine's `/tmp` cache.
fn run_old_cli_against_shared_cache(
    old_cli: &VaultFixture,
    shared_cache: &VaultFixture,
    api: &MockEnzymeApi,
) -> CliOutput {
    old_cli.login("k-colocated-old-cli");
    old_cli
        .cli()
        .args(["init", "--quiet"])
        .enzyme_api(api)
        .api_cache_path(shared_cache.api_cache_path())
        .run()
}

// ===========================================================================
// (a) ADDITIVE tolerance — a newer CLI's cache with extra fields is REUSED.
//     Must be GREEN. This is the compliant-new-CLI rollout path (R3).
// ===========================================================================

/// A newer co-located CLI wrote a cache carrying the 3 core fields PLUS future
/// fields the current CLI has never seen (`provider`, `expires_at`,
/// `pool_status`). The current CLI must:
///   * PARSE it (no `deny_unknown_fields` on `CachedFreeConfig`),
///   * REUSE it without contacting the server (fresh, age < 300s TTL),
///   * thread the cached `api_key`/`base_url`/`model` straight through to the LLM.
#[test]
fn additive_cache_fields_are_reused_by_old_cli() {
    const CACHED_KEY: &str = "enz-colocated-additive-key";
    const CACHED_MODEL: &str = "openrouter/colocated-additive-probe";

    let old_cli = VaultFixture::new();
    let shared_cache = VaultFixture::empty();

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    // If the CLI ever ignored the cache and re-fetched, the server would hand
    // back a DIFFERENT key/base_url, so a re-fetch would be observable.
    api.set_free_config("enz-server-would-mint", "http://127.0.0.1:1/v1", "server-model");

    // Newer co-located CLI's cache: 3 core fields (base_url -> our mock LLM) +
    // additive future fields.
    shared_cache.seed_api_cache_with_extra(CACHED_KEY, &llm.base_url(), CACHED_MODEL, 30);
    let cached_at_before = shared_cache.api_cache_cached_at();

    let out = run_old_cli_against_shared_cache(&old_cli, &shared_cache, &api);
    let diag = diagnostics("additive cache reused", &shared_cache, &api, &llm, &out);

    assert_no_panic(&out, &diag);
    out.assert_success();

    // REUSE, not re-fetch: the server free-config endpoint was never consulted.
    assert_eq!(
        api.free_config_calls(),
        0,
        "a fresh additive cache must be REUSED, not trigger a server re-fetch \
         (additive tolerance for a co-located cache write).{diag}"
    );
    // The cached core three were threaded to the LLM verbatim.
    assert!(
        llm.request_count() > 0,
        "the cached base_url must have been used to reach the LLM.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(CACHED_KEY)),
        "the cached api_key {CACHED_KEY:?} must be the Bearer presented to the LLM, \
         proving api_key survived the extra fields.{diag}"
    );
    assert!(
        llm.requests().iter().any(|b| b.contains(CACHED_MODEL)),
        "the cached model {CACHED_MODEL:?} must appear in an LLM request body, \
         proving model survived the extra fields.{diag}"
    );
    // The server-minted key must never appear — proves no silent re-fetch.
    assert!(
        !llm.auth_headers()
            .iter()
            .any(|h| h == &bearer("enz-server-would-mint")),
        "the server-minted key must NOT reach the LLM; the cache was supposed to \
         be reused, not refreshed.{diag}"
    );
    // Cache must not have been rewritten (reuse leaves cached_at untouched).
    assert_eq!(
        shared_cache.api_cache_cached_at(),
        cached_at_before,
        "reusing a fresh cache must not rewrite it (cached_at changed => refetch).{diag}"
    );
    // A reused, well-formed cache must not surface a diagnostic error.
    assert!(
        !out.contains("Enzyme server unreachable") && !out.contains("Insufficient credits"),
        "reusing a valid additive cache must not surface any error banner.{diag}"
    );
}

// ===========================================================================
// (c) TTL — fresh cache reused; stale cache re-fetched (values not leaked).
//     Must be GREEN.
// ===========================================================================

/// A plain (non-additive) cache within the 300s TTL is honored: reused, no
/// server call, cached values used. Isolates the TTL/reuse mechanic from the
/// additive-field mechanic.
#[test]
fn fresh_cache_within_ttl_is_reused() {
    const CACHED_KEY: &str = "enz-fresh-ttl-key";
    const CACHED_MODEL: &str = "openrouter/fresh-ttl-model";

    let old_cli = VaultFixture::new();
    let shared_cache = VaultFixture::empty();

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config("enz-server-would-mint", "http://127.0.0.1:1/v1", "server-model");

    // 60s old: comfortably inside the 300s TTL.
    shared_cache.seed_api_cache(CACHED_KEY, &llm.base_url(), CACHED_MODEL, 60);
    let cached_at_before = shared_cache.api_cache_cached_at();

    let out = run_old_cli_against_shared_cache(&old_cli, &shared_cache, &api);
    let diag = diagnostics("fresh cache within TTL", &shared_cache, &api, &llm, &out);

    assert_no_panic(&out, &diag);
    out.assert_success();
    assert_eq!(
        api.free_config_calls(),
        0,
        "a cache within the 300s TTL must be honored (reused) without a server \
         call.{diag}"
    );
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(CACHED_KEY)),
        "the fresh cached key must reach the LLM.{diag}"
    );
    assert_eq!(
        shared_cache.api_cache_cached_at(),
        cached_at_before,
        "reusing a fresh cache must not rewrite it.{diag}"
    );
}

/// A parseable cache OLDER than the 300s TTL must NOT be reused: the current CLI
/// re-fetches from the server and the stale key/base_url/model must never be
/// used. Pins that the TTL is honored and that stale values do not leak into a
/// run.
#[test]
fn stale_cache_beyond_ttl_is_refetched_not_reused() {
    const STALE_KEY: &str = "enz-STALE-must-not-use";
    const STALE_MODEL: &str = "stale/model-must-not-use";
    const FRESH_KEY: &str = "enz-fresh-from-server";
    const FRESH_MODEL: &str = "openrouter/fresh-from-server";

    let old_cli = VaultFixture::new();
    let shared_cache = VaultFixture::empty();

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    // The server will mint a FRESH, usable lease pointing at the mock LLM.
    api.set_free_config(FRESH_KEY, &llm.base_url(), FRESH_MODEL);

    // Stale cache: 600s old (> 300s TTL) and pointing at an UNROUTABLE base_url,
    // so if it were wrongly reused the run could not reach any LLM.
    shared_cache.seed_api_cache(STALE_KEY, "http://127.0.0.1:1/v1", STALE_MODEL, 600);
    let stale_cached_at = shared_cache.api_cache_cached_at();

    let out = run_old_cli_against_shared_cache(&old_cli, &shared_cache, &api);
    let diag = diagnostics("stale cache beyond TTL", &shared_cache, &api, &llm, &out);

    assert_no_panic(&out, &diag);
    out.assert_success();

    // TTL honored: the stale cache was discarded and the server was re-consulted.
    assert!(
        api.free_config_calls() >= 1,
        "a cache older than the 300s TTL must trigger a server re-fetch.{diag}"
    );
    // The freshly minted lease reached the LLM.
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(FRESH_KEY)),
        "the freshly re-fetched key {FRESH_KEY:?} must reach the LLM.{diag}"
    );
    // The stale key/model must NEVER be used.
    assert!(
        !llm.auth_headers().iter().any(|h| h == &bearer(STALE_KEY)),
        "the STALE cached key must never be presented to the LLM.{diag}"
    );
    assert!(
        !llm.requests().iter().any(|b| b.contains(STALE_MODEL)),
        "the STALE cached model must never appear in an LLM request.{diag}"
    );
    // The cache was rewritten with the fresh lease (cached_at moved forward).
    let new_cached_at = shared_cache.api_cache_cached_at();
    assert!(
        new_cached_at > stale_cached_at,
        "a re-fetch must rewrite the cache with a newer cached_at \
         (was {stale_cached_at:?}, now {new_cached_at:?}).{diag}"
    );
    assert!(
        shared_cache
            .read_api_cache()
            .map_or(false, |c| c.contains(FRESH_KEY)),
        "the rewritten cache must contain the fresh server-minted key.{diag}"
    );
}

// ===========================================================================
// (b) MISSING a core field — DOCUMENTING the current fragility (GREEN).
//     Pins WHY R3 forbids a new CLI from dropping/renaming a core field.
// ===========================================================================

/// A newer co-located CLI wrote a cache MISSING a load-bearing core field
/// (`model`). The current CLI's read (`serde_json::from_str::<CachedFreeConfig>`,
/// client.rs:1027) is non-lenient and propagates the parse error rather than
/// discarding the cache and re-fetching.
///
/// DOCUMENTED current behavior (this test is GREEN and pins it):
///   * NO recovery: the server free-config endpoint is NEVER re-fetched even
///     though the server is fully reachable and would hand back a usable lease;
///   * catalyst generation is silently SKIPPED (no LLM call);
///   * the run does not crash — it exits "successfully" but generated nothing,
///     mislabeling the corrupt co-located cache as "server unreachable".
///
/// This is the R3 pin: because the old CLI cannot self-heal from an unparseable
/// cache, a co-located NEW CLI must never drop/rename a core field — additive
/// `serde(default)` fields only. The SAFE behavior the new CLI should adopt is
/// asserted in `missing_core_field_cache_should_refetch_red_by_design` below.
#[test]
fn missing_core_field_cache_aborts_without_refetch() {
    let old_cli = VaultFixture::new();
    let shared_cache = VaultFixture::empty();

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    // Healthy LLM + a valid server lease: if the CLI DID re-fetch (self-heal),
    // it would reach the LLM. A zero LLM count is therefore meaningful.
    llm.set_ok_content(&catalyst_content());
    api.set_free_config("enz-server-would-mint", &llm.base_url(), "server-model");

    // Newer co-located CLI's cache dropped `model`; age is fresh so the TTL is
    // NOT the reason for any refetch — the parse failure is.
    shared_cache.seed_api_cache_missing_field("enz-missing-model-key", &llm.base_url(), 30);

    let out = run_old_cli_against_shared_cache(&old_cli, &shared_cache, &api);
    let diag = diagnostics("missing core field (documenting)", &shared_cache, &api, &llm, &out);

    assert_no_panic(&out, &diag);

    // THE FRAGILITY: the unparseable cache blocks recovery — no re-fetch happens
    // even though the server is reachable and would mint a usable lease.
    assert_eq!(
        api.free_config_calls(),
        0,
        "current CLI does NOT re-fetch after an unparseable co-located cache — the \
         parse error propagates and blocks recovery (client.rs:1027). This is the \
         R3 fragility the new CLI must not trigger.{diag}"
    );
    // Consequently catalyst generation never runs.
    assert_eq!(
        llm.request_count(),
        0,
        "with the cache unparseable and no re-fetch, no leased key ever reaches the \
         LLM, so catalyst generation is skipped.{diag}"
    );
    // Nothing was charged for a run that produced nothing.
    assert_eq!(
        api.deduct_calls(),
        0,
        "an unusable/unparseable cache must not deduct credits.{diag}"
    );
    // Documented (mis)diagnostic: the corrupt co-located cache is reported as a
    // server-reachability problem.
    out.assert_contains("Enzyme server unreachable");
}

// ===========================================================================
// RED BY DESIGN — the SAFE behavior a robust CLI should have when it finds an
// unparseable co-located cache: discard it and re-fetch. FAILS on current CLI.
// ===========================================================================

/// SAFE behavior (NOT satisfied by the current CLI): encountering a co-located
/// cache it cannot parse (e.g. a newer CLI dropped/renamed a field), the CLI
/// should treat the cache as a miss — discard it and re-fetch from the server —
/// rather than propagate the parse error and skip catalyst generation entirely.
///
/// The current CLI does NOT: `serde_json::from_str::<CachedFreeConfig>(..)?`
/// (client.rs:1027) hard-errors, so this asserts the ideal and is ignored.
/// Recorded in `red_by_design`. This is the new-CLI robustness gate that would
/// make the R3 "never drop a core field" constraint fail-safe instead of
/// fail-silent.
#[test]
#[ignore = "red_by_design: current CLI propagates the cache parse error instead of \
            discarding an unparseable co-located cache and re-fetching (client.rs:1027)"]
fn missing_core_field_cache_should_refetch_red_by_design() {
    const FRESH_KEY: &str = "enz-fresh-after-bad-cache";

    let old_cli = VaultFixture::new();
    let shared_cache = VaultFixture::empty();

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());
    api.set_free_config(FRESH_KEY, &llm.base_url(), "server-model");

    shared_cache.seed_api_cache_missing_field("enz-missing-model-key", &llm.base_url(), 30);

    let out = run_old_cli_against_shared_cache(&old_cli, &shared_cache, &api);
    let diag = diagnostics("missing core field (red_by_design)", &shared_cache, &api, &llm, &out);

    assert_no_panic(&out, &diag);
    out.assert_success();
    // DESIRED (fails today): an unparseable cache is treated as a miss and the
    // CLI re-fetches a fresh lease from the reachable server...
    assert!(
        api.free_config_calls() >= 1,
        "DESIRED: an unparseable co-located cache should be discarded and the \
         server re-consulted.{diag}"
    );
    // ...and that fresh lease is actually used to generate catalysts.
    assert!(
        llm.auth_headers().iter().any(|h| h == &bearer(FRESH_KEY)),
        "DESIRED: after discarding the bad cache the freshly re-fetched key should \
         reach the LLM.{diag}"
    );
    // ...and the corrupt cache must never be reported as a server outage.
    assert!(
        !out.contains("Enzyme server unreachable"),
        "DESIRED: a parse failure of a co-located cache is not a server outage.{diag}"
    );
}
