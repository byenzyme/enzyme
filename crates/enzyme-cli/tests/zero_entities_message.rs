//! e2e regression: zero_entities_message
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! # Part 1 — the reported bug
//!
//! `init.rs:702` destructures `api_config`, and the `else` arm at `init.rs:821`
//! unconditionally reports:
//!
//! > "No API key found and Enzyme server unreachable — skipping catalyst
//! >  generation. Set OPENAI_API_KEY or ensure network access."
//!
//! But `api_config` is `None` for two unrelated reasons (`init.rs:567`):
//!   1. LLM credential resolution failed, OR
//!   2. `catalyst_generation_planned(&sel_result.entities) == false`, i.e. the
//!      selection pipeline picked ZERO entities.
//!
//! Under `--use-env-llm`, cause (1) is impossible at this point: a genuine
//! resolution failure returns `Err` early at `init.rs:580-583`. So reaching
//! `init.rs:821` with `--use-env-llm` set *proves* the zero-entities path — yet
//! the user is told their API key is missing and their network is down. Both
//! claims are false, and both send the user chasing the wrong fix.
//!
//! # Part 2 — why a large vault selects zero entities
//!
//! See `generic_folder_shape_selects_zero_entities` and
//! `curated_entities_restore_selection_without_a_new_binary` below, plus the
//! findings write-up at the bottom of this file.

mod support;

use support::fixture::VaultFixture;
use support::mock::MockLlm;

/// Seed a vault with `2 * n` real markdown notes laid out the way a large
/// daily-notes vault actually is: every file lives under a root folder whose
/// name is in `entity_map.rs:108 GENERIC_FOLDERS`, no `#tags`, and no
/// `[[wikilinks]]`. This is the shape that selects zero entities.
fn seed_generic_shape_vault(f: &VaultFixture, n: usize) {
    for i in 0..n {
        f.write_note(
            &format!("daily/2024-01-{i:05}.md"),
            &format!(
                "# Day {i}\n\nMeeting notes and a retrospective for day {i}. \
                 Plain prose, no tags, no wikilinks.\n"
            ),
        );
        f.write_note(
            &format!("notes/note-{i:05}.md"),
            &format!("# Note {i}\n\nA standalone note about topic {i}.\n"),
        );
    }
}

/// Seed a vault with an equivalent amount of content in a shape the shortlist
/// *does* accept: a non-generic root folder, real tags, repeated wikilinks.
fn seed_structured_vault(f: &VaultFixture, n: usize) {
    for i in 0..n {
        f.write_note(
            &format!("research/paper-{i:05}.md"),
            &format!("# Paper {i}\n\n#biology #genomics\n\nSee [[Alice]] and [[Bob]].\n"),
        );
    }
}

/// The exact string `init.rs:821` emits. Asserting on the substring keeps the
/// test honest if the sentence is reworded but the misdiagnosis survives.
const WRONG_CAUSE: &str = "No API key found and Enzyme server unreachable";

/// The lines of CLI output that announce catalyst generation was skipped.
fn skip_lines(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.contains("skipping catalyst generation"))
        .map(|l| l.trim().to_string())
        .collect()
}

/// The `entities = [...]` list persisted into `$ENZYME_HOME/config.toml`, as a
/// raw string. Independent proof of how many entities selection actually
/// picked, without depending on any progress-line wording.
fn persisted_entities(f: &VaultFixture) -> String {
    let cfg = std::fs::read_to_string(f.enzyme_home().join("config.toml")).unwrap_or_default();
    let Some(vault_section) = cfg.split(&format!("[vaults.\"{}\"]", f.vault_path().display())).nth(1)
    else {
        return "<no vault section in config.toml>".to_string();
    };
    let Some(rest) = vault_section.split("entities = ").nth(1) else {
        return "<no entities key>".to_string();
    };
    match rest.find(']') {
        Some(end) => rest[..=end].replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" "),
        None => "<unterminated entities list>".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 1: the message must name the real cause.
// ─────────────────────────────────────────────────────────────────────────────

/// A vault that selects ZERO entities, run with a fully valid env LLM.
///
/// The API key is present, well-formed, and points at a live mock server that
/// answers. Nothing about credentials or the network is wrong. The only reason
/// catalyst generation is skipped is that selection returned no entities.
///
/// EXPECTED (asserted): the skip message names entity selection.
/// ACTUAL (the bug): it blames a missing API key and an unreachable server.
#[test]
#[ignore = "known defect: zero-entity selection is misreported as an API-key problem"]
fn zero_entity_selection_must_not_blame_the_api_key() {
    let llm = MockLlm::start();
    llm.set_ok_content("- a catalyst");

    let fixture = VaultFixture::empty();
    seed_generic_shape_vault(&fixture, 40);

    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    let combined = out.combined();
    let entities = persisted_entities(&fixture);
    let skips = skip_lines(&combined);

    // Preconditions — establish that we are genuinely on the zero-entities
    // path and NOT on a credential-failure path. If any of these trip, the
    // scenario drifted and the Part 1 assertion below would be meaningless.
    assert_eq!(
        entities, "[]",
        "PRECONDITION: expected selection to pick zero entities, but \
         $ENZYME_HOME/config.toml persisted entities = {entities}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert_eq!(
        llm.request_count(),
        0,
        "PRECONDITION: expected no catalyst LLM calls on the zero-entity path, \
         got {} request(s)\n--- stdout ---\n{}\n--- stderr ---\n{}",
        llm.request_count(),
        out.stdout,
        out.stderr
    );
    assert!(
        !skips.is_empty(),
        "PRECONDITION: expected a 'skipping catalyst generation' message\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );

    // THE BUG: the CLI blames credentials/network for what is a selection outcome.
    assert!(
        !combined.contains(WRONG_CAUSE),
        "MISDIAGNOSIS at init.rs:821.\n\
         Zero entities were selected (config.toml persisted entities = []), and\n\
         --use-env-llm was set with a VALID, reachable env LLM \
         (OPENAI_API_KEY=sk-test-env-key, OPENAI_BASE_URL={base}).\n\
         Under --use-env-llm a real resolution failure returns Err at \
         init.rs:580-583 and can never reach init.rs:821, so this message can \
         ONLY mean zero entities.\n\
         EXPECTED: a message naming entity selection as the cause.\n\
         ACTUAL:   {skips:?}\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        base = llm.base_url(),
        stdout = out.stdout,
        stderr = out.stderr,
    );

    // ...and it must positively say what really happened. Matched only against
    // the skip lines so the unrelated "Extracting entities..." scan progress
    // line cannot satisfy this by accident.
    let names_real_cause = skips.iter().any(|line| {
        let l = line.to_lowercase();
        l.contains("entit") || l.contains("select")
    });
    assert!(
        names_real_cause,
        "EXPECTED the skip message to name entity selection (e.g. \"no entities \
         were selected for catalyst generation\").\n\
         ACTUAL skip lines: {skips:?}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
}

/// Control for the test above: same binary, same env LLM wiring, same flags —
/// only the vault shape differs. Proves the credentials in
/// `zero_entity_selection_must_not_blame_the_api_key` were never the problem,
/// so the "No API key found" message there is unambiguously a misdiagnosis.
///
/// Expected to PASS today.
#[test]
fn control_same_env_llm_reaches_the_llm_when_entities_exist() {
    let llm = MockLlm::start();
    llm.set_ok_content("- a catalyst");

    let fixture = VaultFixture::empty();
    seed_structured_vault(&fixture, 40);

    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    assert!(
        llm.request_count() > 0,
        "CONTROL: identical --use-env-llm credentials must reach the mock LLM \
         when entities are selected, but got 0 requests. Selection persisted \
         entities = {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        persisted_entities(&fixture),
        out.stdout,
        out.stderr
    );
    out.assert_not_contains(WRONG_CAUSE);
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 2: why does a large vault select zero entities?
// ─────────────────────────────────────────────────────────────────────────────

/// Repro for the open investigation.
///
/// `SelectionPipeline::run` (selection.rs:52) with no curated config falls
/// through to `select_shortlist`, which calls
/// `selection_steps::build_shortlist_selection(db, 20)` — note the limit is the
/// HARDCODED literal `20` at selection.rs:117, NOT `total_limit`.
///
/// That bottoms out in `EntityMap::select_shortlist` (entity_map.rs:373), whose
/// greedy loop can only return fewer than `limit` entities when the `candidates`
/// vector (entity_map.rs:410-430) is empty. `candidates` is built with three
/// HARDCODED filters:
///   * Folder — root-level only, and not in `EXCLUDED_FOLDERS` (entity_map.rs:95)
///     or `GENERIC_FOLDERS` (entity_map.rs:108 = daily/notes/convos/journal/journals)
///   * Tag    — root tags only, not in `EXCLUDED_TAGS` (entity_map.rs:111)
///   * Link   — `frequency >= 3`
///
/// A vault whose root folders are all "generic" and whose notes carry no tags
/// and no repeated wikilinks therefore yields an EMPTY candidate set and zero
/// entities — at ANY size. There is no user-facing knob that relaxes any of
/// these three filters.
///
/// EXPECTED (asserted): a vault of several hundred real markdown notes selects
/// at least one entity to catalyze.
/// PREDICTED: RED.
#[test]
#[ignore = "known defect: selection fallback drops generic/daily-notes vault shapes to zero entities"]
fn generic_folder_shape_selects_zero_entities() {
    let llm = MockLlm::start();
    llm.set_ok_content("- a catalyst");

    let fixture = VaultFixture::empty();
    seed_generic_shape_vault(&fixture, 250); // 500 notes across daily/ and notes/

    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    let combined = out.combined();
    assert!(
        combined.contains("500 discovered"),
        "PRECONDITION: expected all 500 notes to be indexed\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );

    let entities = persisted_entities(&fixture);
    assert_ne!(
        entities, "[]",
        "SCALING BUG: a 500-file vault indexed cleanly but selection chose ZERO \
         entities, so init silently generated nothing.\n\
         Cause: EntityMap::select_shortlist (entity_map.rs:373) builds its \
         candidate set with hardcoded filters — root folders in GENERIC_FOLDERS \
         (entity_map.rs:108: daily, notes, convos, journal, journals) are \
         dropped, subfolders are dropped for containing '/', and links need \
         frequency >= 3. This vault's folders are all generic and its notes have \
         no tags or repeated wikilinks, so `candidates` is empty and the greedy \
         loop breaks on its first iteration.\n\
         Note `total_limit = 30` is NOT the gate: it is trending-era config that \
         the init selection path never reads (the old Database::get_trending_entities \
         consumer was removed); selection passes the literal 20 into \
         build_shortlist_selection.\n\
         EXPECTED: at least one entity selected.\n\
         ACTUAL:   entities = {entities}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
}

/// Companion evidence for Part 2: does a user have a workaround TODAY, without
/// a new binary?
///
/// Yes. `SelectionPipeline::run` short-circuits to `select_from_config` when
/// `EnzymeConfig::has_curated_entities()` is true (selection.rs:55-59), and that
/// path looks entities up by name in the DB — it never applies `GENERIC_FOLDERS`,
/// `EXCLUDED_TAGS`, or the `frequency >= 3` link filter. So hand-writing
/// `entities = ["folder:daily", "folder:notes"]` into `$ENZYME_HOME/config.toml`
/// rescues the exact vault that `generic_folder_shape_selects_zero_entities`
/// finds unselectable.
///
/// Expected to PASS today. It exists to pin the workaround down as a real
/// escape hatch (and to catch a regression that would close it off), and to
/// prove the zero-entity result is a shortlist-policy artifact rather than a
/// property of the vault content.
#[test]
fn curated_entities_restore_selection_without_a_new_binary() {
    let llm = MockLlm::start();
    llm.set_ok_content("- a catalyst");

    let fixture = VaultFixture::empty();
    seed_generic_shape_vault(&fixture, 40);

    // First init: populates the DB with folder entities, selects zero.
    let first = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();
    assert_eq!(
        persisted_entities(&fixture),
        "[]",
        "PRECONDITION: first init should select zero entities\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        first.stdout,
        first.stderr
    );
    assert_eq!(llm.request_count(), 0, "PRECONDITION: no LLM calls on first init");

    // The workaround: curate the entities the shortlist refuses to consider.
    std::fs::write(
        fixture.enzyme_home().join("config.toml"),
        format!(
            "[defaults]\n\
             minimum_tags = 8\nminimum_links = 6\nminimum_folders = 6\ntotal_limit = 30\n\n\
             [vaults.\"{}\"]\n\
             entities = [\"folder:daily\", \"folder:notes\"]\n",
            fixture.vault_path().display()
        ),
    )
    .expect("write curated config.toml");

    let second = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    assert!(
        llm.request_count() > 0,
        "WORKAROUND BROKEN: after curating entities = [\"folder:daily\", \
         \"folder:notes\"] in config.toml, init still made 0 LLM calls, meaning \
         the curated-config escape hatch no longer rescues a vault the shortlist \
         rejects.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        second.stdout,
        second.stderr
    );
    second.assert_not_contains(WRONG_CAUSE);
}

/// Counter-evidence for Part 2: rules out raw scale as the cause.
///
/// 2,000 notes in an acceptable shape select entities just fine, so the
/// zero-entity outcome on the reporter's ~19,238-file vault is driven by vault
/// SHAPE (generic root folders / no tags / no repeated links), not by file
/// count. Nothing in `select_shortlist` is size-sensitive: there is no cap that
/// degrades as the candidate pool grows.
///
/// Expected to PASS today.
#[test]
fn scale_alone_does_not_cause_zero_entity_selection() {
    let llm = MockLlm::start();
    llm.set_ok_content("- a catalyst");

    let fixture = VaultFixture::empty();
    seed_structured_vault(&fixture, 2000);

    let out = fixture
        .cli()
        .arg("init")
        .arg("--use-env-llm")
        .env_llm(&llm)
        .run();

    let entities = persisted_entities(&fixture);
    assert_ne!(
        entities, "[]",
        "A 2000-file vault in an acceptable shape selected zero entities — that \
         would mean selection IS size-sensitive, contradicting the shape-based \
         diagnosis.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert!(
        llm.request_count() > 0,
        "expected catalyst LLM calls at 2000 files, got 0. entities = {entities}\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout,
        out.stderr
    );
}
