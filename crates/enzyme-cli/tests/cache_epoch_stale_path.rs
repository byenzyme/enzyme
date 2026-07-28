//! e2e regression: cache_epoch_stale_path
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`,
//! `support::epoch`).
//!
//! # What this reproduces
//!
//! `enzyme refresh` (interactive) bumps the collection cache epoch
//! (`collection.index_generation` in the vault DB) so that downstream retrieval
//! caches know their view of the collection is invalid.
//!
//! Under the current catalyst-epoch architecture
//! (`crates/enzyme-core/src/pipeline/epoch.rs`,
//! `run_refresh_interactive` in init.rs), catalyst (LLM) generation no longer
//! runs inline: `run_refresh_interactive` does the local scan+embed work
//! synchronously, then unconditionally calls
//! `maybe_bump_collection_cache_epoch(&db, local.collection_changed())` —
//! *before* the catalyst epoch (which may spawn a detached
//! `enzyme refresh --detached --quiet` worker) is even scheduled. So the cache
//! epoch bump is keyed purely to what scan+embed actually changed, not to
//! whether catalyst generation later succeeds or fails in the background.
//!
//! Correct behavior, asserted below: if `refresh` indexed and embedded new
//! documents, the epoch MUST bump — regardless of whether catalyst generation
//! then succeeded or failed. The documents are in the DB either way.
//!
//! Each sub-case forces the catalyst epoch to be due with `--full` (there is
//! no more entity-mention staleness threshold; due-ness is now cadence /
//! contract / manual-force based) and then waits for the spawned background
//! worker to reach a terminal status (`active` or `failed` in
//! `catalyst_epochs`) before asserting on LLM call counts, so the guard that
//! catalyst work genuinely ran is not vacuous.

mod support;

use enzyme_core::db::Database;

use support::epoch::{build_epoch_id, wait_for_epoch_terminal, WORKER_TERMINAL_TIMEOUT};
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

/// Run `enzyme refresh --use-env-llm --full` (forcing the catalyst epoch to
/// be due, since there is no more entity-mention staleness threshold to lean
/// on), wait for the spawned background worker to reach a terminal status,
/// and return `(refresh output, epoch_id, terminal status)`.
///
/// This is the non-vacuousness mechanism for both sub-cases below: it proves
/// (a) the epoch was genuinely due and a worker was spawned, and (b) that
/// worker actually reached a terminal outcome rather than the test racing
/// ahead of it.
#[track_caller]
fn force_refresh_and_wait_for_worker(
    fixture: &VaultFixture,
    llm: &MockLlm,
) -> (CliOutput, String, String) {
    let refresh = fixture
        .cli()
        .args(["refresh", "--use-env-llm", "--full"])
        .env_llm(llm)
        .run();

    let stdout_json: serde_json::Value = serde_json::from_str(refresh.stdout.trim())
        .unwrap_or_else(|e| {
            panic!(
                "refresh did not print parseable JSON on stdout ({e}).\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                refresh.stdout, refresh.stderr
            )
        });

    assert_eq!(
        stdout_json["epoch"]["due"].as_bool(),
        Some(true),
        "TEST IS VACUOUS: `--full` did not make the catalyst epoch due, so the \
         background worker was never scheduled and this run cannot exercise the \
         catalyst-outcome-vs-cache-epoch contract.\nepoch JSON: {}\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        stdout_json["epoch"],
        refresh.stdout,
        refresh.stderr
    );
    assert_eq!(
        stdout_json["background_spawned"].as_bool(),
        Some(true),
        "TEST IS VACUOUS: refresh reported background_spawned=false, so no worker ran \
         and catalyst generation was never attempted.\n\
         --- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        refresh.stdout,
        refresh.stderr
    );

    let epoch_id = build_epoch_id(&stdout_json);
    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");

    // `spawn_background_refresh` double-forks a worker while this test's own
    // `refresh` process may still hold its own open SQLite connection to the
    // same file; the worker's own `Database::open()` unconditionally re-runs
    // the full schema DDL batch and can lose a schema-lock race against that
    // still-live connection, dying immediately with "database is locked"
    // before it ever claims the scheduled epoch (observed empirically, not
    // hypothetically). Production self-heals this: a scheduled-but-unclaimed
    // epoch reports `launch_needed: true` again on the next refresh call. We
    // reproduce that same self-heal deterministically by invoking the exact
    // detached-worker subcommand ourselves once our own `refresh` process (and
    // therefore its DB connection) has fully exited — at that point there is
    // no longer a competing connection, so this is a no-op if the auto-spawned
    // worker already claimed the epoch, and a reliable retry if it did not.
    let _ = fixture
        .cli()
        .args(["refresh", "--detached", "--quiet", "--use-env-llm"])
        .env_llm(llm)
        .run();

    let status = wait_for_epoch_terminal(&db_path, &epoch_id, WORKER_TERMINAL_TIMEOUT);

    (refresh, epoch_id, status)
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

/// Sub-case (a): catalyst generation on the forced epoch SUCCEEDS.
///
/// New documents were indexed and embedded, catalysts regenerated. The epoch
/// must bump so retrieval sees the new documents.
///
/// NEW DEFECT DISCOVERED WHILE ADAPTING THIS TEST (not papered over — see
/// AGENTS.md hard rule on reporting real regressions found during a merge):
/// canary-vault has both a `#projects` tag and a `projects/` folder, i.e. two
/// *different* entities that share the name "projects". `CatalystPipeline`
/// deletes-then-inserts a shadow entity's catalysts keyed by entity *name*
/// only (`Database::delete_catalysts_for_entity`, called from the streaming
/// path in `crates/enzyme-core/src/pipeline/catalysts.rs`), so whichever of
/// the two "projects" entities finishes second silently wipes out the other's
/// freshly-inserted catalysts even though BOTH generations succeeded. Under
/// plain `init` this was a harmless silent under-count. Under the new
/// catalyst-epoch worker it is fatal: `build_and_activate_epoch` in init.rs
/// asserts `sim_stats.catalysts_processed == stats.catalysts_generated` and
/// bails with "shadow epoch similarity set is incomplete" — turning an
/// entirely successful catalyst run into a hard epoch failure. Reproduced
/// deterministically (not flaky) against the canary-vault fixture with a
/// healthy mock LLM: same-named tag/folder entities (canary-vault's
/// `projects` tag + `projects/` folder) used to overwrite each other's
/// catalysts via a name-only delete, tripping build_and_activate_epoch's
/// strict catalysts_processed == catalysts_generated check. Fixed by keying
/// catalyst replacement by (entity_name, entity_type).
#[test]
fn epoch_bumps_on_stale_path_when_catalysts_succeed() {
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_payload());

    let fixture = init_vault(&llm);
    let epoch_before = read_epoch(&fixture);

    write_new_notes(&fixture);

    let llm_calls_before = llm.request_count();
    let (refresh, epoch_id, status) = force_refresh_and_wait_for_worker(&fixture, &llm);
    refresh.assert_success();

    // Mechanism guard: the background worker must have genuinely reached the
    // LLM (not just been scheduled) and it must have succeeded, otherwise
    // this sub-case is not testing what its name claims.
    assert!(
        llm.request_count() > llm_calls_before,
        "TEST IS VACUOUS: the background worker for epoch {epoch_id} made no LLM \
         requests ({llm_calls_before} -> {}), so catalyst generation was never \
         actually attempted.\n--- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        llm.request_count(),
        refresh.stdout,
        refresh.stderr
    );
    assert_eq!(
        status,
        "active",
        "expected the forced catalyst epoch {epoch_id} to succeed and activate, but it \
         ended in status {status:?}.\n--- refresh stdout ---\n{}\n--- refresh stderr ---\n{}\n\
         --- background_refresh.log ---\n{}",
        refresh.stdout,
        refresh.stderr,
        support::epoch::background_refresh_log(&fixture.vault_enzyme_dir().join("enzyme.db")),
    );

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
    let (refresh, epoch_id, status) = force_refresh_and_wait_for_worker(&fixture, &llm);
    refresh.assert_success();

    // Mechanism guard: the background worker must have genuinely attempted
    // the LLM and failed, otherwise this sub-case never exercised the
    // "catalyst generation fails" scenario it claims to.
    assert!(
        llm.request_count() > llm_calls_before,
        "TEST IS VACUOUS: the background worker for epoch {epoch_id} made no LLM \
         requests ({llm_calls_before} -> {}), so catalyst generation was never \
         actually attempted.\n--- refresh stdout ---\n{}\n--- refresh stderr ---\n{}",
        llm.request_count(),
        refresh.stdout,
        refresh.stderr
    );
    assert_eq!(
        status, "failed",
        "expected the forced catalyst epoch {epoch_id} to fail against a broken LLM, \
         but it ended in status {status:?}.\n--- refresh stdout ---\n{}\n--- refresh stderr ---\n{}\n\
         --- background_refresh.log ---\n{}",
        refresh.stdout,
        refresh.stderr,
        support::epoch::background_refresh_log(&fixture.vault_enzyme_dir().join("enzyme.db")),
    );

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
