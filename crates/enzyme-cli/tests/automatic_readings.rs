//! `learn questions automatically`: declared readings plus automatic
//! (coverage) selection, without duplicates, honoring leave-outs, shown by
//! `spec plan` and `status --json`, and never written back by `init`.
mod support;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use support::fixture::{CliOutput, VaultFixture};
use support::mock::MockLlm;

const CATALYSTS: &str = r#"{"catalysts":["Which migration tradeoff preserves source evidence?","What rejected alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#;

const BODY: &str = "The team chose a reversible migration with auditable source evidence. Rejected alternatives traded traceability for speed. ";

fn note(f: &VaultFixture, path: &str, tags: &str, links: &[&str]) {
    let date = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%d");
    let links: String = links.iter().map(|l| format!("[[{l}]]\n")).collect();
    f.write_note(
        path,
        &format!(
            "---\ncreated: {date}\ntags: [{tags}]\n---\n# Note\n{links}{}",
            BODY.repeat(12)
        ),
    );
}

fn json(out: &CliOutput) -> Value {
    assert!(out.success, "command failed:\n{}", out.combined());
    serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{}", out.stdout))
}

fn vault(f: &VaultFixture) -> String {
    f.vault_path().canonicalize().unwrap().display().to_string()
}

/// A vault where automatic selection has more to find than the readings name.
fn fixture() -> VaultFixture {
    let f = VaultFixture::empty();
    for page in ["alice", "bob"] {
        f.write_note(&format!("people/{page}.md"), &format!("# {page}\n"));
    }
    for i in 0..4 {
        note(
            &f,
            &format!("meetings/m-{i}.md"),
            "standup",
            &["alice", "bob"],
        );
    }
    for i in 0..4 {
        note(&f, &format!("research/r-{i}.md"), "ideas", &["alice"]);
    }
    for i in 0..3 {
        note(&f, &format!("projects/p-{i}.md"), "roadmap", &["bob"]);
    }
    // The most frequent tag in the vault; only `leave out` keeps it out.
    for i in 0..6 {
        note(&f, &format!("inbox/d-{i}.md"), "draft", &[]);
    }
    f
}

fn program(f: &VaultFixture, automatic: &str) -> String {
    format!(
        r#"vault "{}" {{
  learn questions from folder "meetings"
  learn questions from folder "people" including linked pages
  {automatic}
  leave out tags {{ "draft" }}
}}
"#,
        vault(f)
    )
}

fn keys(entities: &[Value], name: &str, kind: &str) -> Vec<(String, String)> {
    entities
        .iter()
        .map(|e| {
            (
                e[kind].as_str().unwrap().to_string(),
                e[name].as_str().unwrap().to_lowercase(),
            )
        })
        .collect()
}

#[track_caller]
fn assert_unique(keys: &[(String, String)]) {
    let set: BTreeSet<_> = keys.iter().collect();
    assert_eq!(set.len(), keys.len(), "duplicate entities: {keys:?}");
}

#[test]
fn readings_plus_automatic_selection_is_a_union_without_duplicates() {
    let f = fixture();
    f.cli()
        .args(["init", "--stage", "index", "--quiet"])
        .run()
        .assert_success();

    let file = f.root().join("plan.enzyme");
    fs::write(&file, program(&f, "learn questions automatically")).unwrap();
    let plan = json(
        &f.cli()
            .args(["spec", "plan", "--json"])
            .arg(file.to_string_lossy())
            .run(),
    );
    let v = &plan["vaults"][0];
    assert_eq!(v["selection"], "readings and automatic", "{v:#}");
    let automatic = &v["automatic"];
    // Two declared folders take two of the twenty automatic slots.
    assert_eq!(automatic["limit"], 20, "{automatic:#}");
    assert_eq!(automatic["declared"], 2, "{automatic:#}");
    assert_eq!(automatic["slots"], 18, "{automatic:#}");
    let picked = keys(
        automatic["picked"].as_array().unwrap(),
        "name",
        "entity_type",
    );
    assert!(!picked.is_empty(), "{automatic:#}");
    // Automatic picks never repeat a reading's entity or a left-out tag.
    for excluded in [
        ("folder", "meetings"),
        ("folder", "people"),
        ("link", "alice"),
        ("link", "bob"),
        ("tag", "draft"),
    ] {
        assert!(
            !picked.contains(&(excluded.0.into(), excluded.1.into())),
            "{excluded:?} picked automatically: {picked:?}"
        );
    }
    assert!(
        picked.contains(&("folder".into(), "research".into()))
            || picked.contains(&("tag".into(), "ideas".into())),
        "automatic selection missed what the readings do not cover: {picked:?}"
    );

    // Every job appears once: under its reading or under automatic selection.
    let mut jobs: Vec<Value> = v["other_jobs"].as_array().unwrap().clone();
    for reading in v["readings"].as_array().unwrap() {
        jobs.extend(reading["jobs"].as_array().unwrap().iter().cloned());
    }
    let job_keys = keys(&jobs, "entity_name", "entity_type");
    assert_unique(&job_keys);
    assert_eq!(job_keys.len() as u64, v["totals"]["jobs"].as_u64().unwrap());
    assert!(
        job_keys.contains(&("folder".into(), "meetings".into())),
        "{job_keys:?}"
    );
    assert!(
        !job_keys.iter().any(|(_, name)| name == "draft"),
        "{job_keys:?}"
    );
    for job in v["other_jobs"].as_array().unwrap() {
        let key = (
            job["entity_type"].as_str().unwrap().to_string(),
            job["entity_name"].as_str().unwrap().to_lowercase(),
        );
        let expanded = keys(
            automatic["expanded"].as_array().unwrap(),
            "name",
            "entity_type",
        );
        assert!(
            picked.contains(&key) || expanded.contains(&key),
            "{key:?} is neither a reading's nor automatic"
        );
    }

    let text = f
        .cli()
        .args(["spec", "plan"])
        .arg(file.to_string_lossy())
        .run();
    text.assert_success();
    assert!(
        text.stdout
            .contains("selection    readings and automatic (learn questions automatically: 2 declared of limit 20 → 18 automatic slots)"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("\nautomatic selection  "),
        "{}",
        text.stdout
    );

    // `up to N` caps the automatic picks; readings are untouched.
    fs::write(&file, program(&f, "learn questions automatically up to 1")).unwrap();
    let capped = json(
        &f.cli()
            .args(["spec", "plan", "--json"])
            .arg(file.to_string_lossy())
            .run(),
    );
    let capped = &capped["vaults"][0];
    assert_eq!(capped["automatic"]["slots"], 1);
    assert_eq!(capped["automatic"]["picked"].as_array().unwrap().len(), 1);
    // Readings keep their entities; volume-allocated question counts are
    // shared with automatic entities, so those may differ.
    let reading_jobs = |plan: &Value| -> Vec<Vec<(String, String)>> {
        plan["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| keys(r["jobs"].as_array().unwrap(), "entity_name", "entity_type"))
            .collect()
    };
    assert_eq!(reading_jobs(capped), reading_jobs(v));

    // Without the statement the readings are the complete set, as before.
    fs::write(&file, program(&f, "")).unwrap();
    let plain = json(
        &f.cli()
            .args(["spec", "plan", "--json"])
            .arg(file.to_string_lossy())
            .run(),
    );
    let plain = &plain["vaults"][0];
    assert_eq!(plain["selection"], "readings");
    assert!(plain.get("automatic").is_none());
    assert!(
        plain["other_jobs"].as_array().unwrap().is_empty(),
        "{plain:#}"
    );
}

fn config_snapshot(home: &PathBuf) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(home.join("configs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.push(home.join("config.toml"));
    files.sort();
    files
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn readiness(f: &VaultFixture) -> Value {
    json(&f.cli().args(["status", "--json"]).run())["selection"].clone()
}

#[test]
fn init_reads_both_reports_both_and_writes_nothing_back() {
    let f = fixture();
    let configs = f.enzyme_home().join("configs");
    fs::create_dir_all(&configs).unwrap();
    fs::write(
        configs.join("practice.enzyme"),
        format!(
            "settings {{ updates disabled }}\n{}",
            program(&f, "learn questions automatically")
        ),
    )
    .unwrap();
    let before = config_snapshot(&f.enzyme_home());
    let llm = MockLlm::start();
    llm.set_ok_content(CATALYSTS);

    let first = f
        .cli()
        .env_llm(&llm)
        .args(["init", "--quiet", "--use-env-llm"])
        .run();
    first.assert_success();
    assert_eq!(
        config_snapshot(&f.enzyme_home()),
        before,
        "init wrote configuration"
    );

    let selection = readiness(&f);
    let entities = selection["entities"].as_array().unwrap().clone();
    let selected = keys(&entities, "name", "type");
    assert_unique(&selected);
    let origin = |kind: &str, name: &str| {
        entities
            .iter()
            .find(|e| e["type"] == kind && e["name"].as_str().unwrap().eq_ignore_ascii_case(name))
            .map(|e| e["origin"].as_str().unwrap().to_string())
    };
    assert_eq!(
        origin("folder", "meetings").as_deref(),
        Some("reading"),
        "{selection:#}"
    );
    assert_eq!(
        origin("link", "alice").as_deref(),
        Some("reading"),
        "{selection:#}"
    );
    assert_eq!(
        origin("tag", "draft"),
        None,
        "left-out tag selected: {selection:#}"
    );
    let automatic = selection["automatic"].as_u64().unwrap();
    assert!(automatic > 0, "{selection:#}");
    assert_eq!(
        automatic as usize,
        entities
            .iter()
            .filter(|e| e["origin"] == "automatic")
            .count()
    );
    assert!(selection["ready"].as_u64().unwrap() > 0, "{selection:#}");

    // A second init is idempotent: same selection, nothing written back.
    let second = f
        .cli()
        .env_llm(&llm)
        .args(["init", "--quiet", "--use-env-llm"])
        .run();
    second.assert_success();
    assert_eq!(
        config_snapshot(&f.enzyme_home()),
        before,
        "second init wrote configuration"
    );
    let again = readiness(&f);
    assert_eq!(
        again["entities"], selection["entities"],
        "selection changed"
    );
}

fn plan_json(f: &VaultFixture, file: &std::path::Path) -> Value {
    let out = f
        .cli()
        .args(["spec", "plan", "--json"])
        .arg(file.to_string_lossy())
        .run();
    let plan = json(&out);
    assert_eq!(plan["schema"], "enzyme.spec-plan.v1");
    plan["vaults"][0].clone()
}

fn all_job_keys(v: &Value) -> Vec<(String, String)> {
    let mut jobs: Vec<Value> = v["other_jobs"].as_array().unwrap().clone();
    for reading in v["readings"].as_array().unwrap() {
        jobs.extend(reading["jobs"].as_array().unwrap().iter().cloned());
    }
    keys(&jobs, "entity_name", "entity_type")
}

#[test]
fn automatic_picks_respect_reading_caps() {
    let f = fixture();
    f.cli()
        .args(["init", "--stage", "index", "--quiet"])
        .run()
        .assert_success();
    let file = f.root().join("caps.enzyme");
    fs::write(
        &file,
        format!(
            r#"vault "{}" {{
  learn questions from folder "people" including linked pages {{
    select 1 by frequency
  }}
  learn questions from tags matching "*a*" {{
    select 1 by frequency
  }}
  learn questions automatically
  leave out tags {{ "draft" }}
}}
"#,
            vault(&f)
        ),
    )
    .unwrap();
    let v = plan_json(&f, &file);
    let jobs = all_job_keys(&v);
    assert_unique(&jobs);
    // Each capped reading keeps exactly one child; automatic adds no other.
    let links: Vec<_> = jobs.iter().filter(|(t, _)| t == "link").collect();
    assert_eq!(links.len(), 1, "{jobs:?}");
    let tags: Vec<_> = jobs
        .iter()
        .filter(|(t, n)| t == "tag" && n.contains('a'))
        .collect();
    assert_eq!(tags.len(), 1, "{jobs:?}");
    let picked = keys(v["automatic"]["picked"].as_array().unwrap(), "name", "entity_type");
    assert!(!picked.is_empty(), "{v:#}");
    assert!(
        picked.iter().all(|(t, _)| t == "folder"),
        "automatic picked a capped entity: {picked:?}"
    );
}

#[test]
fn plain_automatic_selection_honours_tag_leave_outs_and_ignores_selection_settings() {
    let f = fixture();
    let configs = f.enzyme_home().join("configs");
    fs::create_dir_all(&configs).unwrap();
    f.cli()
        .args(["init", "--stage", "index", "--quiet"])
        .run()
        .assert_success();

    // No readings: plain automatic selection, which used to pick `draft`.
    let file = configs.join("plain.enzyme");
    fs::write(
        &file,
        format!("vault \"{}\" {{\n  leave out tags {{ \"draft\" }}\n}}\n", vault(&f)),
    )
    .unwrap();
    let v = plan_json(&f, &file);
    assert_eq!(v["selection"], "automatic");
    let jobs = all_job_keys(&v);
    assert!(!jobs.is_empty());
    assert!(
        !jobs.contains(&("tag".into(), "draft".into())),
        "left-out tag selected: {jobs:?}"
    );
    let status = readiness(&f);
    assert!(
        !keys(status["entities"].as_array().unwrap(), "name", "type")
            .contains(&("tag".into(), "draft".into())),
        "{status:#}"
    );

    // A v0.10-migrated program carries the legacy selection settings: they
    // load silently and leave the automatic budget at 20.
    let log_path = f.vault_enzyme_dir().join("enzyme.log");
    let deprecation = |out: &CliOutput| {
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        format!("{log}{}", out.combined()).contains("has no effect")
    };
    fs::write(
        configs.join("settings.enzyme"),
        "settings {\n  updates disabled\n  selection tags 8\n  selection links 6\n  selection folders 6\n  selection limit 30\n}\n",
    )
    .unwrap();
    fs::write(&file, program(&f, "learn questions automatically")).unwrap();
    let out = f
        .cli()
        .args(["spec", "plan", "--json"])
        .arg(file.to_string_lossy())
        .run();
    let v = json(&out)["vaults"][0].clone();
    assert_eq!(v["automatic"]["limit"], 20, "{v:#}");
    assert_eq!(v["automatic"]["slots"], 18, "{v:#}");
    let status = f.cli().args(["status", "--json"]).run();
    assert!(!deprecation(&status), "migrated defaults warned");

    // Any other value warns that it has no effect and changes nothing.
    fs::write(
        configs.join("settings.enzyme"),
        "settings {\n  selection limit 3\n}\n",
    )
    .unwrap();
    let out = f
        .cli()
        .args(["spec", "plan", "--json"])
        .arg(file.to_string_lossy())
        .run();
    assert_eq!(json(&out)["vaults"][0]["automatic"]["slots"], 18);
    let status = f.cli().args(["status", "--json"]).run();
    assert!(deprecation(&status), "{}", status.combined());
}
