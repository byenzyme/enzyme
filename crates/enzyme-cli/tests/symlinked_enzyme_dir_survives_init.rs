//! e2e regression: symlinked_enzyme_dir_survives_init (issue #6)
//!
//! Harness: `crates/enzyme-cli/tests/support/` (see `support::mock`, `support::fixture`).
//!
//! CLAIM UNDER TEST
//! ----------------
//! A `<vault>/.enzyme` that is a SYMLINK to a directory outside the vault is a
//! supported layout (users do this to keep the multi-hundred-MB index out of
//! iCloud Drive sync). Enzyme must be symlink-preserving: it may resolve
//! through the link for all I/O, but it must never delete or replace the link,
//! and it must never remove the link's target directory. `init` re-runs reset
//! DB *contents* (`reset_vault_database` in
//! `crates/enzyme-cli/src/commands/init.rs` removes only the `enzyme.db` file),
//! never the directory.
//!
//! Additionally, when the link's target has been wiped by an external actor
//! (issue #6's actual destroyer was the pre-v0.7.0 `plugin/install.sh` running
//! `rm -rf "$HOME/.enzyme"`, leaving the vault's `.enzyme` symlink dangling),
//! `init` must repair the layout by recreating the target directory THROUGH
//! the link (`ensure_dir_symlink_aware` in
//! `crates/enzyme-core/src/config/paths.rs`) — not fail with "File exists"
//! and not replace the link with a plain directory.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

use support::fixture::{CliOutput, VaultFixture};
use support::mock::{MockEnzymeApi, MockLlm};

/// A well-formed catalyst response body in the object format
/// `parse_catalyst_response` prefers (`crates/enzyme-core/src/llm/catalyst_prompt.rs`),
/// so `init` genuinely completes the LLM-spending part of the pipeline.
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

fn diagnostics(label: &str, llm: &MockLlm, out: &CliOutput) -> String {
    format!(
        "\n=== {label} ===\n\
         mock LLM requests: {}\n\
         exit code: {:?} (success={})\n\
         --- stdout ---\n{}\n--- stderr ---\n{}\n",
        llm.request_count(),
        out.code,
        out.success,
        out.stdout,
        out.stderr,
    )
}

/// Replace `<vault>/.enzyme` with a symlink to `target` (outside the vault),
/// mirroring `ln -s ~/.enzyme/<name> "<vault>/.enzyme"` from the issue report.
fn symlink_enzyme_dir(fixture: &VaultFixture, target: &Path) -> PathBuf {
    std::fs::create_dir_all(target).expect("create symlink target");
    let link = fixture.vault_enzyme_dir();
    assert!(
        !link.exists() && link.symlink_metadata().is_err(),
        "fixture unexpectedly pre-created {link:?}"
    );
    std::os::unix::fs::symlink(target, &link).expect("create .enzyme symlink");
    link
}

/// Assert every invariant of the symlink-preserving contract at once, with a
/// message that pinpoints which part broke.
#[track_caller]
fn assert_symlink_layout_intact(label: &str, link: &Path, target: &Path, diag: &str) {
    let meta = link.symlink_metadata().unwrap_or_else(|e| {
        panic!(
            "DATA LOSS ({label}): {link:?} is GONE ({e}). \
             Something deleted the vault's .enzyme symlink.{diag}"
        )
    });
    assert!(
        meta.file_type().is_symlink(),
        "REGRESSION ({label}): {link:?} is no longer a symlink — something \
         replaced the link with a real file/directory, orphaning the target.{diag}"
    );
    assert_eq!(
        std::fs::read_link(link).expect("read .enzyme link"),
        target,
        "REGRESSION ({label}): {link:?} was re-pointed away from {target:?}.{diag}"
    );
    assert!(
        target.is_dir(),
        "DATA LOSS ({label}): symlink target {target:?} was deleted — this is \
         exactly issue #6 (broken symlink, orphaned index, wasted LLM spend).{diag}"
    );
    let db = target.join("enzyme.db");
    assert!(
        db.is_file(),
        "DATA LOSS ({label}): {db:?} missing — the index DB did not survive.{diag}"
    );
    assert!(
        std::fs::metadata(&db).expect("db metadata").len() > 0,
        "DATA LOSS ({label}): {db:?} exists but is empty.{diag}"
    );
}

/// Two consecutive full `init` runs against a symlinked `.enzyme`: the link,
/// its target directory, and the DB inside the target must all survive both.
#[test]
fn symlinked_enzyme_dir_survives_two_full_inits() {
    let fixture = VaultFixture::new();
    fixture.login("k-hosted-test");

    // Outside-the-vault data dir, as in the issue's `ln -s ~/.enzyme/<name> ...`.
    let target = fixture.root().join("enzyme-data");
    let link = symlink_enzyme_dir(&fixture, &target);

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    for run in 1..=2 {
        let out = fixture
            .cli()
            .args(["init", "--use-env-llm", "--quiet"])
            .enzyme_api(&api)
            .env_llm(&llm)
            .run();
        let diag = diagnostics(&format!("init run {run}"), &llm, &out);

        assert!(
            out.success,
            "expected `init` run {run} to succeed against a symlinked .enzyme.{diag}"
        );
        assert_symlink_layout_intact(&format!("after init run {run}"), &link, &target, &diag);
    }

    // Non-vacuity: the runs must actually have reached (mock) LLM spend — the
    // issue is about losing the index AFTER paying for generation.
    assert!(
        llm.request_count() > 0,
        "TEST IS VACUOUS: init never reached catalyst generation, so surviving \
         it proves nothing about the post-spend data-loss scenario."
    );
}

/// The recovery path for users already bitten: the symlink target was wiped
/// externally (the pre-v0.7.0 installer's `rm -rf "$HOME/.enzyme"`), leaving a
/// dangling `.enzyme` link. `init` must recreate the target directory through
/// the link and complete — never error with "File exists" and never swap the
/// link out for a plain directory.
#[test]
fn dangling_enzyme_symlink_is_repaired_not_replaced() {
    let fixture = VaultFixture::new();
    fixture.login("k-hosted-test");

    let target = fixture.root().join("enzyme-data");
    let link = symlink_enzyme_dir(&fixture, &target);

    let api = MockEnzymeApi::start();
    let llm = MockLlm::start();
    llm.set_ok_content(&catalyst_content());

    let first = fixture
        .cli()
        .args(["init", "--use-env-llm", "--quiet"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("seed init", &llm, &first);
    assert!(first.success, "seed `init` failed.{diag}");
    assert_symlink_layout_intact("after seed init", &link, &target, &diag);

    // External wipe of the target (what the old installer did to ~/.enzyme).
    std::fs::remove_dir_all(&target).expect("simulate external wipe of target");
    assert!(
        link.symlink_metadata()
            .expect(".enzyme link should remain")
            .file_type()
            .is_symlink()
            && !target.exists(),
        "test setup: .enzyme should now be a dangling symlink"
    );

    let second = fixture
        .cli()
        .args(["init", "--use-env-llm", "--quiet"])
        .enzyme_api(&api)
        .env_llm(&llm)
        .run();
    let diag = diagnostics("init after external wipe", &llm, &second);
    assert!(
        second.success,
        "REGRESSION: `init` could not recover from a dangling .enzyme symlink \
         (expected it to recreate the target directory through the link).{diag}"
    );
    assert_symlink_layout_intact("after recovery init", &link, &target, &diag);
}
