//! `enzyme spec plan` previews selection and job construction on an indexed
//! vault without model calls, and never changes the index.
mod support;
use std::fs;
use support::fixture::VaultFixture;

const BODY: &str = "The team chose a reversible migration with auditable source evidence. Rejected alternatives traded traceability for speed. ";

fn note(f: &VaultFixture, path: &str, date: &str, tags: &str, links: &[&str]) {
    let links: String = links.iter().map(|l| format!("[[{l}]]\n")).collect();
    f.write_note(
        path,
        &format!(
            "---\ncreated: {date}\ntags: [{tags}]\n---\n# Note\n{links}{}",
            BODY.repeat(12)
        ),
    );
}

/// Name, size and SHA-256 of the database and a nonempty WAL. A read-only
/// SQLite reader of a WAL database may create empty `-wal`/`-shm` sidecars;
/// those carry no content.
fn snapshot(dir: &std::path::Path) -> Vec<(String, usize, String)> {
    use sha2::{Digest, Sha256};
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            // SQLite's shared-memory index is not content.
            name.starts_with("enzyme.db") && !name.ends_with("-shm")
        })
        .map(|p| {
            let bytes = fs::read(&p).unwrap();
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                bytes.len(),
                format!("{:x}", Sha256::digest(&bytes)),
            )
        })
        .filter(|(name, len, _)| !(name.ends_with("-wal") && *len == 0))
        .collect();
    files.sort();
    files
}

#[test]
fn plan_reports_jobs_periods_patterns_and_drop_reasons_without_writing() {
    let f = VaultFixture::empty();
    let now = chrono::Utc::now();
    let day = |ago: i64| {
        (now - chrono::Duration::days(ago))
            .format("%Y-%m-%d")
            .to_string()
    };
    for page in ["alice", "bob", "carol"] {
        f.write_note(&format!("people/{page}.md"), &format!("# {page}\n"));
    }
    for page in ["old-one", "old-two"] {
        f.write_note(&format!("archive/{page}.md"), &format!("# {page}\n"));
    }
    // alice: six documents across three months; bob four; carol three.
    for (i, ago) in [100, 100, 50, 50, 5, 5].into_iter().enumerate() {
        note(
            &f,
            &format!("notes/a-{i}.md"),
            &day(ago),
            "misc",
            &["alice"],
        );
    }
    for i in 0..4 {
        note(&f, &format!("notes/b-{i}.md"), &day(20), "misc", &["bob"]);
    }
    for i in 0..3 {
        note(&f, &format!("notes/c-{i}.md"), &day(20), "misc", &["carol"]);
    }
    // Archive pages are linked often enough, but only two years ago.
    for i in 0..3 {
        note(
            &f,
            &format!("old/o-{i}.md"),
            &day(800),
            "misc",
            &["old-one", "old-two"],
        );
    }
    for i in 0..3 {
        note(&f, &format!("projects/pa-{i}.md"), &day(30), "proj-a", &[]);
    }
    note(&f, "projects/pb-0.md", &day(30), "proj-b", &[]);

    let index = f.cli().args(["init", "--stage", "index"]).run();
    index.assert_success();

    let file = f.root().join("plan.enzyme");
    fs::write(
        &file,
        format!(
            r#"
learning {{ question budget 4 }}
vault {:?} {{
  learn questions from folder "people" including linked pages {{
    select 2 by frequency
    sample by time
    periods monthly
    question budget 6
  }}
  learn questions from folder "archive" including linked pages
  learn questions from tags matching "proj-*" {{
    select 1 by frequency
  }}
}}
"#,
            f.vault_path().to_string_lossy()
        ),
    )
    .unwrap();

    let before = snapshot(&f.vault_enzyme_dir());
    assert!(!before.is_empty());
    let prompts = f.root().join("prompts");
    let out = f
        .cli()
        .args(["spec", "plan", "--json", "--prompts"])
        .arg(prompts.to_string_lossy())
        .arg(file.to_string_lossy())
        .run();
    out.assert_success();
    assert_eq!(
        before,
        snapshot(&f.vault_enzyme_dir()),
        "spec plan changed the index"
    );

    let value: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let vault = &value["vaults"][0];
    assert_eq!(vault["reference"]["source"], "wall clock");
    assert_eq!(vault["prompts"], "hosted");
    let readings = vault["readings"].as_array().unwrap();
    assert_eq!(readings.len(), 3);

    // Folder expansion: two of three eligible pages, ranked by documents.
    let people = &readings[0];
    assert_eq!(
        people["source"],
        r#"folder "people" including linked pages"#
    );
    assert_eq!(people["learning"]["sampling"]["value"], "by time");
    assert_eq!(people["learning"]["sampling"]["from"], "reading");
    assert_eq!(people["learning"]["budget"]["value"], "6");
    let selected: Vec<_> = people["expansion"]["selected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap(),
                e["documents"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(selected, [("alice", 6), ("bob", 4)]);
    assert_eq!(people["expansion"]["eligible"], 3);
    let alice = people["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["entity_name"] == "alice")
        .unwrap_or_else(|| panic!("no alice job: {people}"));
    assert_eq!(alice["requested_questions"], 6);
    assert_eq!(alice["sampling"], "time");
    assert_eq!(alice["period_granularity"], "monthly");
    let periods = alice["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 3, "{alice}");
    for period in periods {
        assert_eq!(period["questions"], 2, "{alice}");
        assert!(period["excerpts"].as_u64().unwrap() >= 1);
        assert!(!period["label"].as_str().unwrap().contains("-Q"), "{alice}");
    }
    let labels: std::collections::HashSet<_> = periods
        .iter()
        .map(|p| p["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels.len(), 3, "one period per month");
    assert_eq!(alice["sources"], 6);
    assert!(
        people["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["entity_name"] == "bob")
    );
    assert!(
        !people["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["entity_name"] == "carol")
    );

    // Empty expansion explains the recent-year gate and suggests `as of`.
    let archive = &readings[1];
    assert_eq!(archive["expansion"]["linked_pages"], 2);
    assert_eq!(archive["expansion"]["inactive"], 2);
    assert!(
        archive["expansion"]["selected"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let notes = archive["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| {
            let n = n.as_str().unwrap();
            n.contains("365 days") && n.contains("as of")
        }),
        "{notes:?}"
    );

    // Pattern: both tags match, the more frequent one is selected.
    let pattern = &readings[2];
    assert_eq!(pattern["matches"]["matched"], 2);
    assert_eq!(pattern["matches"]["eligible"], 2);
    let chosen: Vec<_> = pattern["matches"]["selected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(chosen, ["proj-a"]);
    let jobs = pattern["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["entity_name"], "proj-a");
    // The inherited global budget is the vault's allocation base (floor 3).
    assert_eq!(jobs[0]["requested_questions"], 3);
    assert_eq!(
        pattern["learning"]["budget"]["value"],
        "volume allocation, base 4"
    );
    assert_eq!(pattern["learning"]["budget"]["from"], "global");

    let totals = &vault["totals"];
    let total_jobs = totals["jobs"].as_u64().unwrap();
    assert!(total_jobs >= 3);
    let files = fs::read_dir(&prompts).unwrap().count() as u64;
    assert_eq!(files, total_jobs);

    // Text output renders the same plan.
    let text = f
        .cli()
        .args(["spec", "plan"])
        .arg(file.to_string_lossy())
        .run();
    text.assert_success();
    assert!(
        text.stdout
            .contains(r#"1. folder "people" including linked pages"#)
    );
    assert!(text.stdout.contains("by time · periods"), "{}", text.stdout);
    assert!(text.stdout.contains("365 days"));
    assert_eq!(before, snapshot(&f.vault_enzyme_dir()));
    println!("{}", text.stdout);
}

#[test]
fn plan_requires_an_indexed_vault() {
    let f = VaultFixture::empty();
    let file = f.root().join("plan.enzyme");
    fs::write(
        &file,
        format!(
            r#"vault {:?} {{ learn questions from tag "focus" }}"#,
            f.vault_path().to_string_lossy()
        ),
    )
    .unwrap();
    let out = f
        .cli()
        .args(["spec", "plan"])
        .arg(file.to_string_lossy())
        .run();
    assert!(!out.success);
    assert!(
        out.combined().contains("is not indexed"),
        "{}",
        out.combined()
    );
    assert!(!f.vault_enzyme_dir().exists());
}
