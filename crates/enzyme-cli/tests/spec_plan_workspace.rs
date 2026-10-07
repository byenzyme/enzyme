//! `enzyme spec plan --workspace <name>`: a named workspace's program planned
//! against its home-store index, read-only, with the `entities`/`skipped`/
//! `counts` explain view and the `--limit` bound.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const SCRUBBED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "ENZYME_HOME",
    "ENZYME_API_URL",
    "ENZYME_VAULT_ROOT",
    "ENZYME_LOCAL_MODEL",
];

const BODY: &str = "The team chose a reversible migration with auditable source evidence. ";

struct Home {
    root: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("home/configs")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn db(&self, name: &str) -> PathBuf {
        self.home().join("workspaces").join(name).join("enzyme.db")
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.path().join("notes").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Dated meeting notes tagged `focus` that link two people pages.
    fn notes(&self) -> PathBuf {
        let date = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%d");
        for page in ["alice", "bob"] {
            self.write(&format!("people/{page}.md"), &format!("# {page}\n"));
        }
        for i in 0..4 {
            self.write(
                &format!("meetings/m-{i}.md"),
                &format!(
                    "---\ncreated: {date}\ntags: [focus]\n---\n# Meeting {i}\n[[alice]]\n[[bob]]\n{}",
                    BODY.repeat(12)
                ),
            );
        }
        self.root.path().join("notes").canonicalize().unwrap()
    }

    fn program(&self, file: &str, body: &str) {
        fs::write(self.home().join("configs").join(file), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
        for var in SCRUBBED_ENV {
            command.env_remove(var);
        }
        command
            .args(args)
            .env("ENZYME_HOME", self.home())
            .env("ENZYME_API_CACHE_PATH", self.root.path().join("api-cache"))
            .env("ENZYME_FREE_CONFIG_URL", "http://127.0.0.1:9/free-config")
            .env("ENZYME_UPDATE_API_BASE", "http://127.0.0.1:9")
            .env("ENZYME_UPDATE_RELEASE_BASE", "http://127.0.0.1:9")
            .output()
            .unwrap()
    }

    /// Every file under the home, without SQLite's reader sidecars: a
    /// read-only WAL reader maps `-shm` and may create an empty `-wal`.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.to_string_lossy();
                if path.is_dir() {
                    walk(&path, out);
                } else if name.ends_with("-shm")
                    || (name.ends_with("-wal") && fs::metadata(&path).unwrap().len() == 0)
                {
                    continue;
                } else {
                    out.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.home(), &mut files);
        files.sort();
        files
    }
}

fn text(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[track_caller]
fn assert_code(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{}", text(output));
}

fn plan(output: &Output) -> Value {
    assert_code(output, 0);
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["schema"], "enzyme.spec-plan.v1");
    assert_eq!(plan["vaults"].as_array().unwrap().len(), 1);
    plan["vaults"][0].clone()
}

fn indexed_workspace() -> Home {
    let h = Home::new();
    let notes = h.notes();
    h.program(
        "work.enzyme",
        &format!(
            r#"settings {{ updates disabled }}
workspace "work" {{
  source markdown "notes" {{ path "{}" }}
  learn questions from tag "focus"
  learn questions from folder "people" including linked pages
  learn questions from tag "absent"
  learn questions from tags matching "zz-*"
}}
"#,
            notes.display()
        ),
    );
    let init = h.run(&[
        "--workspace",
        "work",
        "init",
        "--stage",
        "index",
        "--llm",
        "none",
        "--quiet",
    ]);
    assert_code(&init, 0);
    assert!(h.db("work").is_file());
    h
}

fn entity<'a>(v: &'a Value, kind: &str, name: &str) -> &'a Value {
    v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == kind && e["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} {name}: {:#}", v["entities"]))
}

fn skip<'a>(v: &'a Value, code: &str) -> &'a Value {
    v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["code"] == code)
        .unwrap_or_else(|| panic!("no {code} skip: {:#}", v["skipped"]))
}

#[test]
fn plans_a_named_workspace_read_only_with_origins_and_reason_codes() {
    let h = indexed_workspace();
    let before = h.snapshot();

    let v = plan(&h.run(&["--workspace", "work", "spec", "plan", "--json"]));
    assert_eq!(h.snapshot(), before, "spec plan wrote to the home");
    // `--workspace` is global: after the subcommand it plans the same.
    let after = plan(&h.run(&["spec", "plan", "--json", "--workspace", "work"]));
    assert_eq!(after["entities"], v["entities"]);
    assert_eq!(v["workspace"], "work");
    assert_eq!(v["selection"], "readings");

    let focus = entity(&v, "tag", "focus");
    assert_eq!(focus["origin"], "reading");
    assert_eq!(focus["reading"], 0);
    assert_eq!(focus["status"], "planned");
    assert!(focus["questions"].as_u64().unwrap() > 0);
    let alice = entity(&v, "link", "alice");
    assert_eq!(alice["origin"], "expanded");
    assert_eq!(alice["expanded_from"], "people");
    assert_eq!(alice["reading"], 1);

    let absent = skip(&v, "not_indexed");
    assert_eq!(
        (
            &absent["scope"],
            &absent["kind"],
            &absent["name"],
            &absent["reading"]
        ),
        (
            &"reading".into(),
            &"tag".into(),
            &"absent".into(),
            &2.into()
        )
    );
    assert!(!absent["reason"].as_str().unwrap().is_empty());
    let pattern = skip(&v, "no_matches");
    assert_eq!(pattern["reading"], 3);
    for s in v["skipped"].as_array().unwrap() {
        assert!(s["reason"].as_str().is_some_and(|r| !r.is_empty()), "{s}");
    }

    let counts = &v["counts"];
    let entities = v["entities"].as_array().unwrap();
    assert_eq!(counts["selected"], entities.len());
    assert_eq!(
        counts["planned"].as_u64().unwrap() + counts["skipped"].as_u64().unwrap(),
        counts["selected"].as_u64().unwrap()
    );
    assert_eq!(counts["planned"], v["totals"]["jobs"]);
    assert_eq!(counts["by_origin"]["expanded"], 2);
    assert!(v.get("truncated").is_none());

    // The text view says the same.
    let out = h.run(&["--workspace", "work", "spec", "plan"]);
    assert_code(&out, 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("entities  "), "{stdout}");
    assert!(stdout.contains("absent is not in the index"), "{stdout}");
}

#[test]
fn limit_bounds_every_entity_list_and_keeps_totals() {
    let h = indexed_workspace();
    let full = plan(&h.run(&["--workspace", "work", "spec", "plan", "--json"]));
    let v = plan(&h.run(&[
        "--workspace",
        "work",
        "spec",
        "plan",
        "--json",
        "--limit",
        "1",
    ]));
    assert_eq!(v["entities"].as_array().unwrap().len(), 1);
    assert_eq!(v["skipped"].as_array().unwrap().len(), 1);
    for r in v["readings"].as_array().unwrap() {
        assert!(r["jobs"].as_array().unwrap().len() <= 1);
    }
    assert_eq!(v["counts"], full["counts"]);
    assert_eq!(v["totals"], full["totals"]);
    let t = &v["truncated"];
    assert_eq!(t["limit"], 1);
    assert_eq!(
        t["entities"].as_u64().unwrap() + 1,
        full["counts"]["selected"].as_u64().unwrap()
    );
    assert!(t["skipped"].as_u64().unwrap() >= 1);
    assert!(t["jobs"].as_u64().unwrap() >= 1, "{t}");
}

#[test]
fn a_desired_file_previews_against_the_workspace_index() {
    let h = indexed_workspace();
    let before = h.snapshot();
    let desired = h.root.path().join("desired.enzyme");
    fs::write(
        &desired,
        format!(
            r#"workspace "work" {{
  source markdown "notes" {{ path "{}" }}
  learn questions from tag "focus"
}}
"#,
            h.root
                .path()
                .join("notes")
                .canonicalize()
                .unwrap()
                .display()
        ),
    )
    .unwrap();
    let v = plan(&h.run(&[
        "--workspace",
        "work",
        "spec",
        "plan",
        "--json",
        desired.to_str().unwrap(),
    ]));
    assert_eq!(v["readings"].as_array().unwrap().len(), 1);
    assert_eq!(v["counts"]["selected"], 1);
    assert_eq!(h.snapshot(), before, "previewing a desired program wrote");
}

#[test]
fn an_older_index_exits_3_and_a_locked_one_exits_4() {
    let h = indexed_workspace();
    let db = h.db("work");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA journal_mode = DELETE;").unwrap();
        // A held write lock makes a rollback-journal reader see SQLITE_BUSY.
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let start = std::time::Instant::now();
        let out = h.run(&["--workspace", "work", "spec", "plan", "--json"]);
        assert_code(&out, 4);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        conn.execute_batch("ROLLBACK;").unwrap();
    }
    // An index from an older enzyme lacks a table this binary expects.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE source_refreshes;")
        .unwrap();
    let out = h.run(&["--workspace", "work", "spec", "plan", "--json"]);
    assert_code(&out, 3);
    assert!(text(&out).contains("enzyme refresh"), "{}", text(&out));
}

#[test]
fn failures_use_the_exit_code_contract() {
    let h = Home::new();
    let notes = h.notes();
    h.program(
        "fresh.enzyme",
        &format!(
            "workspace \"fresh\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  learn questions from tag \"focus\"\n}}\n",
            notes.display()
        ),
    );
    // Declared but never indexed: configuration, with the init command named.
    let out = h.run(&["--workspace", "fresh", "spec", "plan", "--json"]);
    assert_code(&out, 3);
    assert!(
        text(&out).contains("--workspace fresh init"),
        "{}",
        text(&out)
    );
    assert!(out.stdout.is_empty());
    // A missing program file is a configuration error too.
    assert_code(
        &h.run(&[
            "--workspace",
            "fresh",
            "spec",
            "plan",
            "/nonexistent/x.enzyme",
        ]),
        3,
    );
    // Not declared anywhere.
    assert_code(
        &h.run(&["--workspace", "nope", "spec", "plan", "--json"]),
        3,
    );
    // A path is not a workspace name; other spec commands take no workspace.
    assert_code(&h.run(&["--workspace", "a/b", "spec", "plan"]), 2);
    assert_code(&h.run(&["--workspace", "fresh", "spec", "profiles"]), 2);
    // Neither a file nor a workspace.
    assert_code(&h.run(&["spec", "plan"]), 2);
    assert!(!h.db("fresh").exists(), "a failed plan created an index");
}
