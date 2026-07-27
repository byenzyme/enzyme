//! e2e regression: cache_epoch_stale_path
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! # What this reproduces
//!
//! `enzyme refresh` (interactive) bumps the collection cache epoch
//! (`collection.index_generation` in the vault DB) so that downstream retrieval
//! caches know their view of the collection is invalid.
//!
//! `run_refresh_interactive` (init.rs:1680) does the local scan+embed work
//! first, then branches on catalyst staleness:
//!
//!   * `StalenessResult::Fresh`  → early-return at init.rs:1713, which calls
//!     `maybe_bump_collection_cache_epoch(&db, _local.collection_changed())`.
//!   * `StalenessResult::Stale`  → falls through to `run_stale_catalyst_refresh`
//!     and **never consults `_local.collection_changed()` again**. The only
//!     bump on that path is `bump_collection_cache_epoch` at init.rs:1452,
//!     which is gated on `stats.catalysts_generated > 0`.
//!
//! So on the stale path the epoch is keyed to *catalyst* work rather than to
//! the documents that were actually indexed and embedded. When catalyst
//! generation fails, the freshly embedded documents stay invisible to any
//! consumer that keys its cache on the epoch.
//!
//! Correct behavior, asserted below: if `refresh` indexed and embedded new
//! documents, the epoch MUST bump — regardless of whether catalyst generation
//! then succeeded or failed. The documents are in the DB either way.
//!
//! Observed at time of writing: sub-case (a) passes via init.rs:1452;
//! sub-case (b) fails.

mod support;

use enzyme_core::db::Database;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::MockLlm;

/// Historical config key that stores the collection cache epoch
/// (see `COLLECTION_CACHE_EPOCH_CONFIG_KEY`, init.rs:189).
const EPOCH_KEY: &str = "collection.index_generation";

/// Relative vault paths of the notes added between `init` and `refresh`.
///
/// Four notes tagged `daily`, which is an entity the canary vault already has
/// catalysts for. Four > `NEW_MENTIONS_THRESHOLD` (3), so catalyst staleness
/// reports `Stale(EntitiesStale)` — the branch under test.
const NEW_NOTES: &[(&str, &str)] = &[
    (
        "daily/2026-06-01.md",
        "---\ndate: 2026-06-01\ntags: [daily]\n---\n\n# 2026-06-01\n\n- Ran conduit for the CANARY-EPOCH1 feeder line to the north bed.\n- Marisol confirmed the October retreat window in writing.\n",
    ),
    (
        "daily/2026-06-02.md",
        "---\ndate: 2026-06-02\ntags: [daily]\n---\n\n# 2026-06-02\n\n- CANARY-EPOCH2: pressure regulator reseated, drip zones hold at 12 psi.\n- Ordered a second timer for the dahlia offset.\n",
    ),
    (
        "daily/2026-06-03.md",
        "---\ndate: 2026-06-03\ntags: [daily]\n---\n\n# 2026-06-03\n\n- CANARY-EPOCH3: ground rod driven to 8ft, bonded to the antenna mast.\n- Logged SWR sweep across the 20m band.\n",
    ),
    (
        "daily/2026-06-04.md",
        "---\ndate: 2026-06-04\ntags: [daily]\n---\n\n# 2026-06-04\n\n- CANARY-EPOCH4: rebuilt the raised bed manifold after the freeze crack.\n- Retreat packing list started.\n",
    ),
];

/// A well-formed catalyst payload, so catalyst generation genuinely succeeds
/// in the happy-path case rather than silently degrading.
fn catalyst_payload() -> String {
    serde_json::json!({
        "catalysts": [
            "the moment a plan survives contact with the actual hardware",
            "keeping a running tally of what the surprise costs actually were",
            "returning to a half-finished build after the season changed",
            "scheduling around someone else's stated preference",
            "the difference between what code requires and what fits",
            "noticing a fix only worked because of where it was placed",
            "logging a measurement you do not yet know how to use",
        ],
        "topic_names": [
            "plans meeting hardware",
            "surprise costs",
            "seasonal returns",
            "scheduling around others",
            "code versus fit",
            "placement-dependent fixes",
            "measurements without a use",
        ],
    })
    .to_string()
}

fn vault_db(fixture: &VaultFixture) -> Database {
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");
    assert!(
        db_path.exists(),
        "expected vault DB at {} — the vault was never initialized",
        db_path.display()
    );
    Database::open(&db_path).expect("open vault db")
}

/// Read the persisted collection cache epoch straight out of the vault DB.
///
/// This is the value `run_refresh_interactive` is supposed to bump; reading it
/// from SQLite (rather than from CLI output) means the assertion is about
/// durable state a retrieval consumer would actually observe.
fn read_epoch(fixture: &VaultFixture) -> u64 {
    vault_db(fixture)
        .get_config(EPOCH_KEY)
        .expect("read collection cache epoch")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// `(documents, documents_still_needing_embedding)` for the whole vault.
fn doc_counts(fixture: &VaultFixture) -> (i64, usize) {
    let db = vault_db(fixture);
    let docs = db.get_document_count().expect("document count");
    let pending = db
        .get_documents_needing_embedding(None, None)
        .expect("documents needing embedding")
        .len();
    (docs, pending)
}

/// Per-note report of whether a path is in `docs` and has a non-null
/// `embedded_at`. Used to prove this test isolates the epoch bug and not an
/// indexing/embedding bug.
fn index_report(fixture: &VaultFixture) -> Vec<(String, bool, bool)> {
    let db = vault_db(fixture);
    NEW_NOTES
        .iter()
        .map(|(rel, _)| {
            let doc = db.get_document_by_path(rel).expect("lookup document");
            let indexed = doc.is_some();
            let embedded = doc.and_then(|d| d.embedded_at).is_some();
            (rel.to_string(), indexed, embedded)
        })
        .collect()
}

fn write_new_notes(fixture: &VaultFixture) {
    for (rel, body) in NEW_NOTES {
        fixture.write_note(rel, body);
    }
}

/// Assert every note added after `init` reached the DB *and* got embedded.
///
/// If this fires, the vault never got as far as the epoch question and the
/// epoch assertion below would be meaningless.
#[track_caller]
fn assert_new_notes_indexed_and_embedded(fixture: &VaultFixture, refresh: &CliOutput) {
    let report = index_report(fixture);
    let broken: Vec<&(String, bool, bool)> = report
        .iter()
        .filter(|(_, indexed, embedded)| !indexed || !embedded)
        .collect();
    assert!(
        broken.is_empty(),
        "precondition failed: notes added before `enzyme refresh` were not \
         indexed+embedded, so this run cannot isolate the cache-epoch bug.\n\
         per-note (path, indexed, embedded): {report:#?}\n\
         vault (documents, pending_embedding): {:?}\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        doc_counts(fixture),
        refresh.stdout,
        refresh.stderr
    );
}

/// The real assertion: newly embedded documents must invalidate the collection
/// cache, so the epoch has to move.
#[track_caller]
fn assert_epoch_bumped(
    case: &str,
    before: u64,
    after: u64,
    fixture: &VaultFixture,
    refresh: &CliOutput,
) {
    assert!(
        after > before,
        "[{case}] collection cache epoch did NOT bump across `enzyme refresh` on the \
         catalyst-STALE path.\n\
         expected: epoch > {before} (refresh indexed and embedded {} new documents, so \
         collection-derived caches must be invalidated)\n\
         actual:   epoch == {after} (unchanged)\n\
         The Stale path never consults _local.collection_changed(); its only bump is \
         bump_collection_cache_epoch at init.rs:1452, gated on catalysts_generated > 0. \
         So indexed+embedded documents are lost to cache consumers whenever catalyst \
         generation does not produce catalysts.\n\
         per-note (path, indexed, embedded): {:#?}\n\
         vault (documents, pending_embedding): {:?}\n\
         refresh exit code: {:?}\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        NEW_NOTES.len(),
        index_report(fixture),
        doc_counts(fixture),
        refresh.code,
        refresh.stdout,
        refresh.stderr,
    );
}

/// Prove the refresh actually took the `StalenessResult::Stale` branch.
///
/// The `Fresh` branch early-returns at init.rs:1714 without ever reaching
/// `run_stale_catalyst_refresh`, so it never talks to the LLM. If the refresh
/// issued chat-completion requests, it went down the stale path — which is the
/// branch this test is about. Without this check, sub-case (a) could pass
/// simply because it took the (already-correct) Fresh path.
#[track_caller]
fn assert_took_stale_path(llm_calls_before: usize, llm_calls_after: usize, refresh: &CliOutput) {
    assert!(
        llm_calls_after > llm_calls_before,
        "refresh made no LLM requests ({llm_calls_before} -> {llm_calls_after}), which means it \
         took the StalenessResult::Fresh early-return at init.rs:1713 rather than the Stale \
         path under test. Fix the fixture so catalysts are genuinely stale.\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        refresh.stdout,
        refresh.stderr
    );
}

/// Initialize a vault against `llm`, returning the fixture once `init` has
/// produced catalysts (so a later refresh can be *stale* rather than *absent*).
fn init_vault(llm: &MockLlm) -> VaultFixture {
    let fixture = VaultFixture::new();
    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(llm)
        .run();
    out.assert_success();
    fixture
}

/// Sub-case (a): catalyst generation on the stale path SUCCEEDS.
///
/// New documents were indexed and embedded, catalysts regenerated. The epoch
/// must bump so retrieval sees the new documents.
#[test]
fn epoch_bumps_on_stale_path_when_catalysts_succeed() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());

    let fixture = init_vault(&llm);
    let epoch_before = read_epoch(&fixture);

    write_new_notes(&fixture);

    let llm_calls_before = llm.request_count();
    let refresh = fixture
        .cli()
        .arg("refresh")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();
    refresh.assert_success();

    assert_took_stale_path(llm_calls_before, llm.request_count(), &refresh);
    assert_new_notes_indexed_and_embedded(&fixture, &refresh);

    let epoch_after = read_epoch(&fixture);
    assert_epoch_bumped(
        "catalysts succeed",
        epoch_before,
        epoch_after,
        &fixture,
        &refresh,
    );
}

/// Sub-case (b): catalyst generation on the stale path FAILS.
///
/// The LLM is healthy for `init` (so catalysts exist and the later refresh is
/// *stale*, not *uncatalyzed*) and then hard-fails for the refresh.
///
/// Scan and embed run BEFORE catalysts (init.rs:1704 vs :1722), so the new
/// documents are in the DB regardless. The epoch must still bump: retrieval's
/// cached view is stale whether or not the LLM cooperated.
#[test]
fn epoch_bumps_on_stale_path_when_catalysts_fail() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());

    let fixture = init_vault(&llm);
    let epoch_before = read_epoch(&fixture);

    write_new_notes(&fixture);

    // Break the LLM only for the refresh run.
    llm.set_response(500, r#"{"error":{"message":"upstream exploded"}}"#);

    let llm_calls_before = llm.request_count();
    let refresh = fixture
        .cli()
        .arg("refresh")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    assert_took_stale_path(llm_calls_before, llm.request_count(), &refresh);
    assert_new_notes_indexed_and_embedded(&fixture, &refresh);

    let epoch_after = read_epoch(&fixture);
    assert_epoch_bumped(
        "catalysts fail",
        epoch_before,
        epoch_after,
        &fixture,
        &refresh,
    );
}
