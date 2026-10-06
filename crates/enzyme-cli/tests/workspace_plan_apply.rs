//! `enzyme workspace plan|apply` against a temporary `$ENZYME_HOME`.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Home {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("enzyme/configs")).unwrap();
        fs::create_dir_all(root.join("notes")).unwrap();
        Self { _tmp: tmp, root }
    }

    fn configs(&self) -> PathBuf {
        self.root.join("enzyme/configs")
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn program(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.configs().join(name)).ok()
    }

    fn enzyme(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_enzyme"))
            .args(args)
            .env("ENZYME_HOME", self.root.join("enzyme"))
            .env("HOME", &self.root)
            .env("ENZYME_UPDATE_API_BASE", "http://127.0.0.1:9")
            .env("ENZYME_UPDATE_RELEASE_BASE", "http://127.0.0.1:9")
            .env("ENZYME_FREE_CONFIG_URL", "http://127.0.0.1:9/free-config")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> (bool, Value) {
        let out = self.enzyme(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {stdout}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.success(), value)
    }

    /// Plan `desired` for `name`, saving the plan JSON; returns its path.
    fn plan(&self, name: &str, desired: &str, file: &str) -> (PathBuf, Value) {
        let desired = self.file(&format!("{file}.desired.enzyme"), desired);
        let (ok, plan) = self.json(&[
            "workspace",
            "plan",
            name,
            "--desired",
            desired.to_str().unwrap(),
            "--json",
        ]);
        assert!(ok, "{plan}");
        let path = self.file(&format!("{file}.plan.json"), &plan.to_string());
        (path, plan)
    }

    fn apply(&self, plan: &Path) -> (bool, Value) {
        self.json(&[
            "workspace",
            "apply",
            "--plan",
            plan.to_str().unwrap(),
            "--json",
        ])
    }
}

fn program(notes: &Path, extra: &str) -> String {
    format!(
        "workspace \"practice\" {{\n  source markdown \"notes\" {{ path {:?} }}\n\n{extra}}}\n",
        notes.to_str().unwrap()
    )
}

#[test]
fn a_new_workspace_is_planned_into_its_own_file_and_applied() {
    let home = Home::new();
    let desired = program(
        &home.root.join("notes"),
        "  leave out folders { \"archive\" }\n",
    );
    let (plan_path, plan) = home.plan("practice", &desired, "new");
    assert_eq!(plan["schema"], "enzyme.plan.v1");
    assert_eq!(plan["target"], "practice.enzyme");
    assert_eq!(plan["base_revision"], "absent");
    let summaries: Vec<&str> = plan["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["summary"].as_str().unwrap())
        .collect();
    assert_eq!(
        summaries,
        [
            "Create workspace \"practice\"",
            "Add markdown source \"notes\"",
            "Leave out folder \"archive\""
        ]
    );
    assert!(
        home.program("practice.enzyme").is_none(),
        "plan writes nothing"
    );

    let (ok, receipt) = home.apply(&plan_path);
    assert!(ok, "{receipt}");
    assert_eq!(receipt["schema"], "enzyme.receipt.v1");
    assert_eq!(receipt["replayed"], false);
    assert_eq!(receipt["plan_id"], plan["plan_id"]);
    assert_eq!(home.program("practice.enzyme").unwrap(), desired);

    // The engine still loads the config directory with apply state beside it.
    let inspect = home.enzyme(&[
        "spec",
        "compile",
        home.configs().join("practice.enzyme").to_str().unwrap(),
    ]);
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
}

#[test]
fn an_existing_workspace_is_planned_in_the_file_that_declares_it_and_replays() {
    let home = Home::new();
    let notes = home.root.join("notes");
    let current = format!(
        "settings {{\n  updates disabled\n}}\n\n{}",
        program(&notes, "")
    );
    fs::write(home.configs().join("mine.enzyme"), &current).unwrap();
    let desired = format!(
        "settings {{\n  updates disabled\n}}\n\n{}",
        program(&notes, "  leave out tags { \"private\" }\n")
    );
    let (plan_path, plan) = home.plan("practice", &desired, "edit");
    assert_eq!(plan["target"], "mine.enzyme");
    assert!(
        plan["diff"]
            .as_str()
            .unwrap()
            .contains("+  leave out tags { \"private\" }")
    );

    let (ok, first) = home.apply(&plan_path);
    assert!(ok, "{first}");
    assert_eq!(home.program("mine.enzyme").unwrap(), desired);
    let (ok, again) = home.apply(&plan_path);
    assert!(ok, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["after_revision"], first["after_revision"]);

    // Human output for the same commands.
    let out = home.enzyme(&["workspace", "apply", "--plan", plan_path.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already applied"));
}

#[test]
fn a_stale_plan_is_refused_with_both_revisions() {
    let home = Home::new();
    let notes = home.root.join("notes");
    fs::write(home.configs().join("practice.enzyme"), program(&notes, "")).unwrap();
    let (plan_path, plan) = home.plan(
        "practice",
        &program(&notes, "  leave out folders { \"a\" }\n"),
        "stale",
    );
    let edited = program(&notes, "  leave out folders { \"b\" }\n");
    fs::write(home.configs().join("practice.enzyme"), &edited).unwrap();
    let (ok, error) = home.apply(&plan_path);
    assert!(!ok);
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["kind"], "stale", "{error}");
    assert_eq!(error["error"]["expected_revision"], plan["base_revision"]);
    assert_eq!(home.program("practice.enzyme").unwrap(), edited);
}

#[test]
fn a_tampered_plan_is_refused() {
    let home = Home::new();
    let notes = home.root.join("notes");
    let current = program(&notes, "");
    fs::write(home.configs().join("practice.enzyme"), &current).unwrap();
    let (_, plan) = home.plan(
        "practice",
        &program(&notes, "  leave out folders { \"a\" }\n"),
        "tamper",
    );
    // Hidden change with a recomputed digest: the summary no longer matches.
    let mut tampered = plan.clone();
    let desired = program(&notes, "  leave out folders { \"a\" \"everything\" }\n");
    tampered["desired_sha256"] = Value::String(sha256(&desired));
    tampered["desired"] = Value::String(desired);
    let path = home.file("tampered.json", &tampered.to_string());
    let (ok, error) = home.apply(&path);
    assert!(!ok);
    assert_eq!(error["error"]["kind"], "altered", "{error}");

    // Desired text that no longer matches its digest.
    let mut tampered = plan.clone();
    tampered["desired"] = Value::String(current.replace("notes", "other"));
    let path = home.file("tampered2.json", &tampered.to_string());
    let (_, error) = home.apply(&path);
    assert_eq!(error["error"]["kind"], "altered", "{error}");
    assert_eq!(home.program("practice.enzyme").unwrap(), current);
}

#[test]
fn an_interrupted_apply_is_recovered_by_the_next_command() {
    let home = Home::new();
    let notes = home.root.join("notes");
    let current = program(&notes, "");
    fs::write(home.configs().join("practice.enzyme"), &current).unwrap();
    let desired = program(&notes, "  leave out folders { \"a\" }\n");
    let (plan_path, plan) = home.plan("practice", &desired, "crash");
    // Leave the state a crash between journal and write would leave.
    let store = enzyme_spec::plan::ConfigStore::new(home.configs(), &home.root);
    let parsed: enzyme_spec::plan::Plan = serde_json::from_value(plan.clone()).unwrap();
    store.prepare(&parsed).unwrap();
    assert_eq!(home.program("practice.enzyme").unwrap(), current);

    // Planning anything completes the interrupted apply first.
    let (_, next) = home.plan("practice", &current, "after-crash");
    assert_eq!(next["base_revision"], plan["desired_sha256"]);
    assert_eq!(home.program("practice.enzyme").unwrap(), desired);
    let (ok, receipt) = home.apply(&plan_path);
    assert!(ok, "{receipt}");
    assert_eq!(receipt["replayed"], true);
}

#[test]
fn invalid_requests_fail_without_writing() {
    let home = Home::new();
    let notes = home.root.join("notes");
    let other = home.file(
        "other.enzyme",
        &program(&notes, "").replace("practice", "other"),
    );
    let out = home.enzyme(&[
        "workspace",
        "plan",
        "practice",
        "--desired",
        other.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not declare workspace \"practice\"")
    );
    let out = home.enzyme(&[
        "workspace",
        "plan",
        "../x",
        "--desired",
        other.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let (ok, error) = home.json(&[
        "workspace",
        "plan",
        "practice",
        "--desired",
        home.file("bad.enzyme", "workspace {").to_str().unwrap(),
        "--json",
    ]);
    assert!(!ok);
    assert_eq!(error["error"]["kind"], "invalid");
    assert!(
        fs::read_dir(home.configs()).unwrap().all(|e| !e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "enzyme"))
    );
}

fn sha256(text: &str) -> String {
    enzyme_spec::plan::sha256(text.as_bytes())
}

#[test]
fn an_unrecoverable_apply_names_its_journal_and_recover_discard_clears_it() {
    let home = Home::new();
    let notes = home.root.join("notes");
    let current = program(&notes, "");
    fs::write(home.configs().join("practice.enzyme"), &current).unwrap();
    let (_, plan) = home.plan(
        "practice",
        &program(&notes, "  leave out folders { \"a\" }\n"),
        "x",
    );
    let store = enzyme_spec::plan::ConfigStore::new(home.configs(), &home.root);
    store
        .prepare(&serde_json::from_value(plan).unwrap())
        .unwrap();
    let hand = program(&notes, "  leave out folders { \"hand\" }\n");
    fs::write(home.configs().join("practice.enzyme"), &hand).unwrap();

    let (ok, error) = home.json(&["workspace", "recover", "--json"]);
    assert!(!ok);
    assert_eq!(error["error"]["kind"], "unrecoverable");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("journals/practice.enzyme.json"),
        "{message}"
    );

    let (ok, out) = home.json(&["workspace", "recover", "--discard", "--json"]);
    assert!(ok, "{out}");
    assert_eq!(out["discarded"].as_array().unwrap().len(), 1);
    assert_eq!(home.program("practice.enzyme").unwrap(), hand);
    home.plan("practice", &current, "after");
}
