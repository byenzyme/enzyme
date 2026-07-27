//! e2e regression: scan_embed_survives_llm_failure
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! CLAIM UNDER TEST
//! ----------------
//! A bug reporter claimed that when catalyst generation fails, the whole run
//! "aborts before scan and embed", leaving newly added notes unindexed and
//! unembedded.
//!
//! The counter-diagnosis is that `init.rs:1704` (`run_local_refresh_work`) runs
//! the local scan + embed stage *before* the catalyst stage at `init.rs:1722`,
//! so a total LLM outage must not be able to cost the user their index.
//!
//! WHAT THIS TEST DOES
//! -------------------
//! 1. Initialises a real vault via `enzyme init --stage index` (no LLM, no
//!    credits — a deterministic baseline that just creates `<vault>/.enzyme/`).
//! 2. Drops brand new markdown notes into the vault.
//! 3. Runs refresh against a mock LLM that answers **500 on every request**.
//! 4. Asserts, by reading the real vault database, that the new notes are
//!    present in `docs` AND have embeddings (`docs.embedded_at` set and rows in
//!    `chunk_embeddings`).
//!
//! Both refresh entry points are covered because they take different code paths:
//!   * `enzyme refresh`          -> `run_refresh_interactive` (catalysts inline)
//!   * `enzyme refresh --quiet`  -> `run_refresh_fast`        (hook hot path)
//!
//! The CLI's own exit status is deliberately NOT asserted: the LLM really is
//! down, so a non-zero exit is a legitimate outcome. What must survive is the
//! *local* work.

mod support;

use std::path::Path;

use enzyme_core::db::Database;

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// Notes added after the baseline index, whose survival is the whole point.
const NEW_NOTES: &[(&str, &str)] = &[
    (
        "daily/2026-06-11.md",
        "# 2026-06-11\n\nSpent the morning re-tensioning the heliograph antenna guy \
         wires with Marisol. The drip irrigation manifold in the north garden bed \
         keeps losing pressure whenever the antenna winch runs, so we suspect a \
         shared pump circuit. Logged the pressure readings for later.\n",
    ),
    (
        "projects/pump-circuit-audit.md",
        "# Pump circuit audit\n\nHypothesis: the heliograph antenna winch and the \
         garden irrigation pump share a breaker, and the winch inrush current \
         browns out the pump controller. Next step is to meter both loads during \
         a winch cycle and confirm before rewiring anything.\n",
    ),
];

// ---------------------------------------------------------------------------
// Database probes — these read the real vault DB the CLI just wrote.
// ---------------------------------------------------------------------------

/// One row of `docs` plus its embedding evidence.
#[derive(Debug, Clone)]
struct DocState {
    source_ref: String,
    embedded_at: Option<i64>,
    chunks: i64,
    embedded_chunks: i64,
}

/// Read every doc in the vault DB together with its chunk / embedding counts.
fn read_doc_states(db_path: &Path) -> Vec<DocState> {
    let db = Database::open(db_path).expect("open vault database");
    db.query_read(
        "SELECT d.source_ref,
                d.embedded_at,
                (SELECT COUNT(*) FROM chunks c WHERE c.doc_id = d.id),
                (SELECT COUNT(*) FROM chunk_embeddings e WHERE e.file_path = d.source_ref)
         FROM docs d
         ORDER BY d.source_ref",
        vec![],
        |row| {
            Ok(DocState {
                source_ref: row.get::<String>(0)?,
                embedded_at: row.get::<Option<i64>>(1)?,
                chunks: row.get::<i64>(2)?,
                embedded_chunks: row.get::<i64>(3)?,
            })
        },
    )
    .expect("query docs")
}

/// Locate the doc row for a vault-relative note path.
///
/// `source_ref` is the absolute on-disk path, so match on the suffix.
fn find_doc<'a>(docs: &'a [DocState], rel_path: &str) -> Option<&'a DocState> {
    docs.iter().find(|d| d.source_ref.ends_with(rel_path))
}

/// A dump of observed DB state, appended to every failure message so a red run
/// says what actually happened rather than just "assertion failed".
fn dump(docs: &[DocState]) -> String {
    if docs.is_empty() {
        return "  <docs table is EMPTY>".to_string();
    }
    docs.iter()
        .map(|d| {
            format!(
                "  {} | embedded_at={:?} chunks={} chunk_embeddings={}",
                d.source_ref, d.embedded_at, d.chunks, d.embedded_chunks
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn diagnostics(label: &str, out: &CliOutput, llm: &MockLlm, docs: &[DocState]) -> String {
    format!(
        "\n--- {label}: enzyme exit code {:?} (success={}) ---\
         \n--- stdout ---\n{}\
         \n--- stderr ---\n{}\
         \n--- mock LLM received {} request(s), all answered 500 ---\
         \n--- vault docs table ---\n{}\n",
        out.code,
        out.success,
        out.stdout.trim(),
        out.stderr.trim(),
        llm.request_count(),
        dump(docs),
    )
}

// ---------------------------------------------------------------------------
// Shared scenario
// ---------------------------------------------------------------------------

struct Scenario {
    fixture: VaultFixture,
    api: MockEnzymeApi,
    llm: MockLlm,
    baseline: Vec<DocState>,
}

impl Scenario {
    fn db_path(&self) -> std::path::PathBuf {
        self.fixture.vault_enzyme_dir().join("enzyme.db")
    }

    fn docs(&self) -> Vec<DocState> {
        read_doc_states(&self.db_path())
    }
}

/// Build a vault with a real index, then add new notes and arm the LLM to fail.
fn arrange() -> Scenario {
    let fixture = VaultFixture::new();
    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();

    // ── Baseline: a genuine indexed vault, built without touching the LLM. ──
    let init = fixture
        .cli()
        .args(["init", "--stage", "index"])
        .enzyme_api(&api)
        .run();
    init.assert_success();

    let db_path = fixture.vault_enzyme_dir().join("enzyme.db");
    assert!(
        db_path.exists(),
        "baseline `enzyme init --stage index` did not create {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        db_path.display(),
        init.stdout,
        init.stderr
    );
    let baseline = read_doc_states(&db_path);
    assert!(
        !baseline.is_empty(),
        "baseline index produced no docs — the fixture vault is not being scanned at all,\
         \nso this test could not distinguish survival from failure.\n{}",
        dump(&baseline)
    );

    // ── New material the user just wrote. ───────────────────────────────────
    for (rel, body) in NEW_NOTES {
        assert!(
            find_doc(&baseline, rel).is_none(),
            "precondition violated: {rel} was already indexed before it was written"
        );
        fixture.write_note(rel, body);
    }

    // ── Total LLM outage. ───────────────────────────────────────────────────
    llm.set_response(
        500,
        r#"{"error":{"message":"upstream exploded","type":"server_error"}}"#,
    );

    Scenario {
        fixture,
        api,
        llm,
        baseline,
    }
}

/// The assertion this whole file exists for: the new notes are indexed AND
/// embedded even though every LLM call failed.
#[track_caller]
fn assert_local_work_survived(label: &str, scenario: &Scenario, out: &CliOutput) {
    let docs = scenario.docs();
    let diag = diagnostics(label, out, &scenario.llm, &docs);

    for (rel, _) in NEW_NOTES {
        let doc = find_doc(&docs, rel).unwrap_or_else(|| {
            panic!(
                "{label}: new note {rel} was NOT indexed after refresh.\n\
                 Expected: scan runs before the catalyst/LLM stage, so a total LLM \
                 outage must still leave {rel} in the `docs` table.\n\
                 Actual: no `docs` row whose source_ref ends with {rel}.{diag}"
            )
        });

        assert!(
            doc.chunks > 0,
            "{label}: new note {rel} was indexed but produced 0 chunks.\n\
             Expected: chunks > 0. Actual: {}.{diag}",
            doc.chunks
        );

        assert!(
            doc.embedded_at.is_some(),
            "{label}: new note {rel} was indexed but NEVER EMBEDDED.\n\
             Expected: docs.embedded_at set, because embedding runs before the \
             catalyst/LLM stage. Actual: embedded_at IS NULL.{diag}"
        );

        assert!(
            doc.embedded_chunks > 0,
            "{label}: new note {rel} has no rows in `chunk_embeddings`.\n\
             Expected: >= 1 embedding vector persisted. Actual: {}.{diag}",
            doc.embedded_chunks
        );
    }

    // The LLM outage must not have destroyed pre-existing indexed material either.
    for old in &scenario.baseline {
        assert!(
            docs.iter().any(|d| d.source_ref == old.source_ref),
            "{label}: previously indexed doc {} disappeared from the index after a \
             failed LLM run.\nExpected: refresh is non-destructive on LLM failure.{diag}",
            old.source_ref
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `enzyme refresh` — interactive path (`run_refresh_interactive`), which runs
/// the catalyst pipeline inline in the same process.
#[test]
fn interactive_refresh_indexes_and_embeds_despite_total_llm_failure() {
    let scenario = arrange();

    let out = scenario
        .fixture
        .cli()
        .args(["refresh", "--use-env-llm"])
        .enzyme_api(&scenario.api)
        .env_llm(&scenario.llm)
        .run();

    // Guard against a vacuous pass: if the LLM was never called, the run never
    // reached the failure this test is about.
    assert!(
        scenario.llm.request_count() > 0,
        "interactive refresh never called the LLM, so the 'LLM failure' scenario \
         was never exercised and the result proves nothing.{}",
        diagnostics("interactive refresh", &out, &scenario.llm, &scenario.docs())
    );

    // Hosted credits must not have been touched under --use-env-llm.
    assert_eq!(
        scenario.api.deduct_calls(),
        0,
        "interactive refresh with --use-env-llm hit the hosted credit endpoint"
    );

    assert_local_work_survived("interactive refresh", &scenario, &out);
}

/// `enzyme refresh --quiet` — the hook hot path (`run_refresh_fast`). Local work
/// runs synchronously here and catalysts are handed to a detached worker, so the
/// user-visible index must be complete by the time the command returns.
#[test]
fn quiet_refresh_indexes_and_embeds_despite_total_llm_failure() {
    let scenario = arrange();

    let out = scenario
        .fixture
        .cli()
        .args(["refresh", "--quiet", "--use-env-llm"])
        .enzyme_api(&scenario.api)
        .env_llm(&scenario.llm)
        .run();

    assert_local_work_survived("quiet refresh", &scenario, &out);

    // The quiet path is a machine interface: the JSON it prints must agree with
    // the database, otherwise hooks are told the vault is up to date when it is not.
    let docs = scenario.docs();
    let diag = diagnostics("quiet refresh", &out, &scenario.llm, &docs);
    let json: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap_or_else(|e| {
        panic!("quiet refresh did not print parseable JSON on stdout ({e}).{diag}")
    });

    let files_new = json["indexing"]["files_new"].as_u64().unwrap_or_default();
    assert!(
        files_new >= NEW_NOTES.len() as u64,
        "quiet refresh reported indexing.files_new = {files_new}, expected >= {} \
         (the notes added since the baseline index).{diag}",
        NEW_NOTES.len()
    );

    assert_eq!(
        json["embedding_changed"].as_bool(),
        Some(true),
        "quiet refresh reported embedding_changed = {:?}, expected true — new \
         notes were added, so embeddings must have been written.{diag}",
        json["embedding_changed"]
    );
}
