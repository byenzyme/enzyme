//! e2e regression: logout_clears_llm_cache
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! Logout must remove both the local auth token and any cached hosted lease so
//! a signed-out account key is neither retained on disk nor reused by the next
//! account on the machine.

mod support;

use support::fixture::VaultFixture;
use support::mock::{MockEnzymeApi, MockLlm};

const ACCOUNT_A_KEY: &str = "k-account-A-secret";
const ACCOUNT_B_KEY: &str = "k-account-B-secret";

/// Number of `Authorization: Bearer <key>` headers the LLM saw carrying `key`.
fn auth_hits(llm: &MockLlm, key: &str) -> usize {
    llm.auth_headers()
        .iter()
        .filter(|h| h.contains(key))
        .count()
}

/// Populate the vault with enough prose that init has real entities to catalyze.
fn seed_notes(fixture: &VaultFixture, tag: &str) {
    for i in 0..3 {
        fixture.write_note(
            &format!("{tag}-{i}.md"),
            &format!(
                "# {tag} note {i}\n\n\
                 The migration away from the monolith kept stalling on the same \
                 question: who owns the write path. Note {i} of the {tag} batch \
                 records the argument, the counterargument, and the decision we \
                 deferred again.\n\n\
                 We keep circling the same tension between throughput and \
                 correctness, and every time we resolve it locally the global \
                 answer drifts.\n"
            ),
        );
    }
}

/// Drive account A: log in, run a hosted init so the free-config cache fills
/// with A's key, then log out. Panics with diagnostics if the preconditions
/// (cache written, cache holds A's key, auth.json removed) do not hold.
fn login_populate_cache_then_logout(
    fixture: &VaultFixture,
    api: &MockEnzymeApi,
    llm_a: &MockLlm,
) -> String {
    fixture.login("enz-auth-account-A");
    api.set_free_config(ACCOUNT_A_KEY, &llm_a.base_url(), "test-model");

    let init_a = fixture
        .cli()
        .arg("init")
        .arg("--quiet")
        .enzyme_api(api)
        .run();

    // Precondition: the cache actually got populated with A's key. If this
    // fails the rest of the test proves nothing, so report loudly.
    assert!(
        fixture.api_cache_exists(),
        "PRECONDITION FAILED: expected the hosted free-config cache to exist at {} \
         after account A's `enzyme init`, but no file was written.\n\
         free_config_calls={} deduct_calls={} llm_a_requests={}\n\
         --- account A init stdout ---\n{}\n--- account A init stderr ---\n{}",
        fixture.api_cache_path().display(),
        api.free_config_calls(),
        api.deduct_calls(),
        llm_a.request_count(),
        init_a.stdout,
        init_a.stderr,
    );
    let cache_after_a = fixture.read_api_cache().unwrap_or_default();
    assert!(
        !cache_after_a.trim().is_empty(),
        "PRECONDITION FAILED: the free-config cache at {} is empty after account A's init.\n\
         --- account A init stderr ---\n{}",
        fixture.api_cache_path().display(),
        init_a.stderr,
    );
    assert!(
        cache_after_a.contains(ACCOUNT_A_KEY),
        "PRECONDITION FAILED: expected the free-config cache at {} to contain account A's \
         key {ACCOUNT_A_KEY:?} after init, but it held:\n{}\n\
         --- account A init stderr ---\n{}",
        fixture.api_cache_path().display(),
        cache_after_a,
        init_a.stderr,
    );

    let logout = fixture.cli().arg("logout").enzyme_api(api).run();
    logout.assert_success();
    assert!(
        !fixture.is_logged_in(),
        "PRECONDITION FAILED: expected `enzyme logout` to remove $ENZYME_HOME/auth.json\n\
         --- logout stderr ---\n{}",
        logout.stderr,
    );

    logout.stderr
}

/// PRIMARY REPRO, on-disk half.
///
/// After `enzyme logout`, the signed-out account's hosted LLM API key must not
/// still be sitting in the free-config cache. In production that cache defaults
/// to `$ENZYME_HOME/api-cache` (formerly the shared `/tmp/enzyme-api-cache`).
#[test]
fn logout_invalidates_cached_hosted_llm_config() {
    let api = MockEnzymeApi::start();
    let llm_a = MockLlm::start();
    let fixture = VaultFixture::new();
    seed_notes(&fixture, "alpha");

    let logout_stderr = login_populate_cache_then_logout(&fixture, &api, &llm_a);

    // Logout must invalidate the cached hosted config.
    let cache_after_logout = fixture.read_api_cache();
    assert!(
        !cache_after_logout
            .as_deref()
            .unwrap_or_default()
            .contains(ACCOUNT_A_KEY),
        "SECURITY: `enzyme logout` left account A's hosted LLM API key {ACCOUNT_A_KEY:?} \
         in the free-config cache at {}.\n\
         expected: the cache is invalidated (removed or rewritten without the key)\n\
         actual  : {:?}\n\
         in production this path defaults to $ENZYME_HOME/api-cache \
         (formerly /tmp/enzyme-api-cache).\n\
         --- logout stderr ---\n{logout_stderr}",
        fixture.api_cache_path().display(),
        cache_after_logout,
    );
}

/// Logout remains a cache-clearing operation even if auth.json was removed by
/// another process or an earlier partial logout.
#[test]
fn logout_without_auth_file_still_invalidates_hosted_cache() {
    let api = MockEnzymeApi::start();
    let fixture = VaultFixture::empty();
    fixture.seed_api_cache("stale-signed-out-key", "http://127.0.0.1:1/v1", "stale", 0);
    assert!(!fixture.is_logged_in());
    assert!(fixture.api_cache_exists());

    let logout = fixture.cli().arg("logout").enzyme_api(&api).run();
    logout.assert_success();

    assert!(
        !fixture.api_cache_exists(),
        "logout must remove a stale hosted cache even when auth.json is already absent; stderr={}",
        logout.stderr
    );
}

/// PRIMARY REPRO, behavioral half.
///
/// Account A logs in, runs a hosted init (caching A's hosted LLM key), then logs
/// out. Account B logs in on the same machine and runs init.
///
/// Correct behavior: logout invalidated the cached hosted config, so B's run
/// re-resolves and never touches A's key or A's LLM endpoint.
///
#[test]
fn subsequent_resolve_after_logout_does_not_reuse_previous_account_key() {
    let api = MockEnzymeApi::start();
    let llm_a = MockLlm::start();
    let llm_b = MockLlm::start();
    let fixture = VaultFixture::new();
    seed_notes(&fixture, "alpha");

    login_populate_cache_then_logout(&fixture, &api, &llm_a);
    let llm_a_calls_before_logout = llm_a.request_count();

    // --- Account B: different user, same machine, same cache path.
    fixture.login("enz-auth-account-B");
    api.set_free_config(ACCOUNT_B_KEY, &llm_b.base_url(), "test-model");
    // New content so the refresh has work to do and must resolve an LLM config.
    seed_notes(&fixture, "beta");

    let init_b = fixture
        .cli()
        .arg("init")
        .arg("--quiet")
        .enzyme_api(&api)
        .run();

    // ASSERTION 2 (behavioral): account B's run must not resolve to account A's
    // credentials or endpoint.
    let llm_a_new_calls = llm_a.request_count() - llm_a_calls_before_logout;
    assert_eq!(
        llm_a_new_calls,
        0,
        "SECURITY: after logout, account B's `enzyme init` sent {llm_a_new_calls} request(s) \
         to account A's LLM endpoint ({}).\n\
         expected: 0 — the cached config from the signed-out account must not be reused\n\
         account A key seen on B's requests: {} time(s)\n\
         auth headers observed by A's LLM: {:?}\n\
         free_config_calls={} (a re-resolve would have incremented this)\n\
         --- account B init stdout ---\n{}\n--- account B init stderr ---\n{}",
        llm_a.base_url(),
        auth_hits(&llm_a, ACCOUNT_A_KEY),
        llm_a.auth_headers(),
        api.free_config_calls(),
        init_b.stdout,
        init_b.stderr,
    );

    assert_eq!(
        auth_hits(&llm_b, ACCOUNT_A_KEY),
        0,
        "SECURITY: account A's key {ACCOUNT_A_KEY:?} was presented on account B's LLM \
         requests.\n\
         auth headers observed by B's LLM: {:?}\n\
         --- account B init stderr ---\n{}",
        llm_b.auth_headers(),
        init_b.stderr,
    );

    // Sanity: B's run really did do hosted LLM work, so assertion 2 is not
    // passing merely because nothing happened.
    assert!(
        llm_b.request_count() > 0,
        "expected account B's `enzyme init` to reach its own LLM endpoint ({}), \
         but it made 0 requests — the assertions above may be vacuous.\n\
         free_config_calls={} deduct_calls={}\n\
         --- account B init stdout ---\n{}\n--- account B init stderr ---\n{}",
        llm_b.base_url(),
        api.free_config_calls(),
        api.deduct_calls(),
        init_b.stdout,
        init_b.stderr,
    );
}

/// TTL: a free-config cache entry older than `FREE_CONFIG_CACHE_TTL_SECS` (300s,
/// client.rs:524) must not be reused; the CLI must re-fetch from the server.
#[test]
fn expired_cache_entry_is_not_reused() {
    let api = MockEnzymeApi::start();
    let llm_stale = MockLlm::start();
    let llm_fresh = MockLlm::start();
    let fixture = VaultFixture::new();
    seed_notes(&fixture, "gamma");

    fixture.login("enz-auth-account-A");

    // Cache entry aged well past the 300s TTL, pointing at the stale LLM.
    fixture.seed_api_cache("k-stale-expired", &llm_stale.base_url(), "test-model", 400);
    // The server would hand back the fresh config on a re-fetch.
    api.set_free_config(ACCOUNT_B_KEY, &llm_fresh.base_url(), "test-model");

    let init = fixture
        .cli()
        .arg("init")
        .arg("--quiet")
        .enzyme_api(&api)
        .run();

    assert_eq!(
        llm_stale.request_count(),
        0,
        "expired cache entry (age 400s > TTL 300s) was reused: {} request(s) went to the \
         stale LLM endpoint {}.\n\
         expected: 0 — the entry is past FREE_CONFIG_CACHE_TTL_SECS and must be re-fetched\n\
         auth headers observed by the stale LLM: {:?}\n\
         free_config_calls={}\n\
         --- init stdout ---\n{}\n--- init stderr ---\n{}",
        llm_stale.request_count(),
        llm_stale.base_url(),
        llm_stale.auth_headers(),
        api.free_config_calls(),
        init.stdout,
        init.stderr,
    );

    assert!(
        api.free_config_calls() > 0,
        "expected the CLI to re-fetch the free-config after the cached entry expired \
         (age 400s > TTL 300s), but the free-config endpoint was never called.\n\
         --- init stdout ---\n{}\n--- init stderr ---\n{}",
        init.stdout,
        init.stderr,
    );

    // Non-vacuity: the run really did resolve an LLM and use it, so
    // `llm_stale.request_count() == 0` above means "re-fetched", not "did nothing".
    assert!(
        llm_fresh.request_count() > 0,
        "expected the re-fetched config to be used against the fresh LLM endpoint ({}), \
         but it made 0 requests — the expired-cache assertion above would be vacuous.\n\
         free_config_calls={} deduct_calls={}\n\
         --- init stdout ---\n{}\n--- init stderr ---\n{}",
        llm_fresh.base_url(),
        api.free_config_calls(),
        api.deduct_calls(),
        init.stdout,
        init.stderr,
    );
}
