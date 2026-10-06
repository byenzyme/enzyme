//! The named-workspace CLI contract hosts rely on: a program workspace is
//! addressable by name with its index in the home store, programs are never
//! written back, failures carry defined exit codes, and foreground
//! init/refresh are serialized by a workspace lock.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use fs2::FileExt;
use support::mock::MockLlm;

const CATALYSTS: &str = r#"{"catalysts":["Which migration tradeoff preserves source evidence?","What rejected alternative changes the audit decision?","Which constraint made the migration reversible?"],"topicNames":["migration evidence","audit alternatives","reversible migration"]}"#;

const SCRUBBED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "ENZYME_HOME",
    "ENZYME_API_URL",
    "ENZYME_VAULT_ROOT",
    "ENZYME_LOCAL_MODEL",
];

struct Home {
    root: tempfile::TempDir,
    llm: MockLlm,
}

impl Home {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("home/configs")).unwrap();
        let llm = MockLlm::start();
        llm.set_ok_content(CATALYSTS);
        Self { root, llm }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn store(&self, name: &str) -> PathBuf {
        self.home().join("workspaces").join(name)
    }

    /// A Markdown folder outside the home with `count` notes tagged `focus`.
    fn notes(&self, folder: &str, count: usize) -> PathBuf {
        let notes = self.root.path().join(folder);
        fs::create_dir_all(&notes).unwrap();
        for i in 0..count {
            fs::write(
                notes.join(format!("note-{i}.md")),
                format!(
                    "---\ntags: [focus]\n---\n# Migration evidence {i}\n{}",
                    "The team chose a reversible migration with auditable evidence. ".repeat(20)
                ),
            )
            .unwrap();
        }
        notes.canonicalize().unwrap()
    }

    fn program(&self, file: &str, body: &str) {
        fs::write(self.home().join("configs").join(file), body).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
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
            .env("OPENAI_API_KEY", "sk-test-env-key")
            .env("OPENAI_BASE_URL", self.llm.base_url())
            .env("OPENAI_MODEL", "test-model");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Every file under `configs/` plus `config.toml`, with contents.
    fn config_snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(self.home().join("configs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.push(self.home().join("config.toml"));
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

fn summary(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON summary\n{}", text(output)));
    serde_json::from_str(line).unwrap()
}

// rusqlite, not enzyme-core: libsql refuses to start once rusqlite has
// configured SQLite in this process.
fn document_refs(db: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(db).unwrap();
    let mut statement = db
        .prepare("SELECT source_ref FROM docs ORDER BY source_ref")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn catalyst_count(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM catalysts", [], |row| row.get(0))
        .unwrap()
}

fn hold(path: &Path) -> fs::File {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    file.lock_exclusive().unwrap();
    file
}

#[test]
fn lone_markdown_workspace_indexes_by_name_into_the_home_store() {
    let h = Home::new();
    let notes = h.notes("notes", 3);
    h.program(
        "lone.enzyme",
        &format!(
            "settings {{ updates disabled }}\nworkspace \"lone\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  learn questions from tag \"focus\"\n}}\n",
            notes.display()
        ),
    );
    let before = h.config_snapshot();

    let init = h.run(&["--collection", "lone", "init", "--quiet", "--use-env-llm"]);
    assert_code(&init, 0);
    let result = summary(&init);
    assert_eq!(result["status"], "completed", "{}", text(&init));
    assert_eq!(result["indexing"]["files_discovered"], 3);
    assert!(result["catalysts_generated"].as_u64().unwrap() > 0);

    // The index lives in the home store; the notes folder is never written.
    let db = h.store("lone").join("enzyme.db");
    assert!(db.is_file());
    assert!(!notes.join(".enzyme").exists());
    assert_eq!(
        document_refs(&db),
        vec!["note-0.md", "note-1.md", "note-2.md"],
        "a lone Markdown source keeps root-relative document refs"
    );
    assert_eq!(h.config_snapshot(), before, "init rewrote configuration");

    fs::write(notes.join("note-3.md"), "# Another\nMore evidence. #focus").unwrap();
    let refresh = h.run(&[
        "--collection",
        "lone",
        "refresh",
        "--quiet",
        "--use-env-llm",
    ]);
    assert_code(&refresh, 0);
    assert_eq!(summary(&refresh)["indexing"]["files_discovered"], 4);
    assert!(!notes.join(".enzyme").exists());
    assert_eq!(h.config_snapshot(), before, "refresh rewrote configuration");

    // The same folder addressed by path keeps its legacy index identity.
    let by_path = h
        .command(&["init", "--stage", "index", "--quiet"])
        .arg("-p")
        .arg(&notes)
        .output()
        .unwrap();
    assert_code(&by_path, 0);
    assert!(notes.join(".enzyme/enzyme.db").is_file());
}

#[test]
fn program_workspaces_never_get_selection_written_back() {
    for with_legacy_toml in [false, true] {
        let h = Home::new();
        let lone = h.notes("lone-notes", 3);
        let mixed = h.notes("mixed-notes", 2);
        let ledger = h.root.path().join("ledger.db");
        let connection = rusqlite::Connection::open(&ledger).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, sent_ms INTEGER, sender TEXT, body TEXT);
                 INSERT INTO messages VALUES (1, 1700000000000, 'Ada', 'Reversible migration evidence');",
            )
            .unwrap();
        drop(connection);
        // No readings: entities come from automatic (coverage) selection.
        h.program(
            "workspaces.enzyme",
            &format!(
                r#"settings {{ updates disabled selection tags 1 }}
workspace "lone" {{
  source markdown "notes" {{ path "{lone}" }}
}}
workspace "mixed" {{
  source markdown "notes" {{ path "{mixed}" }}
  source sqlite "mail" {{
    database "{ledger}"
    query "SELECT id, sent_ms, sender, body FROM messages"
    id "id"
    who "sender"
    when "sent_ms" unit ms
    what "body"
  }}
}}
"#,
                lone = lone.display(),
                mixed = mixed.display(),
                ledger = ledger.display(),
            ),
        );
        if with_legacy_toml {
            fs::write(h.home().join("config.toml"), "[update]\nauto = false\n").unwrap();
        }
        let before = h.config_snapshot();

        for name in ["lone", "mixed"] {
            for _ in 0..2 {
                let init = h.run(&["--collection", name, "init", "--quiet", "--use-env-llm"]);
                assert_code(&init, 0);
                assert!(
                    summary(&init)["catalysts_generated"].as_u64().is_some(),
                    "{}",
                    text(&init)
                );
                assert_eq!(
                    h.config_snapshot(),
                    before,
                    "init of {name:?} (legacy toml: {with_legacy_toml}) wrote configuration"
                );
            }
            assert!(
                catalyst_count(&h.store(name).join("enzyme.db")) > 0,
                "{name}: automatic selection produced no catalysts"
            );
        }
    }
}

#[test]
fn configuration_failures_exit_3_and_usage_failures_exit_2() {
    let h = Home::new();
    let notes = h.notes("notes", 1);
    let empty = h.notes("empty", 0);
    h.program(
        "ws.enzyme",
        &format!(
            r#"settings {{ updates disabled }}
workspace "real" {{ source markdown "notes" {{ path "{notes}" }} }}
workspace "gone" {{ source markdown "notes" {{ path "{missing}" }} }}
workspace "empty" {{ source markdown "notes" {{ path "{empty}" }} }}
"#,
            notes = notes.display(),
            missing = h.root.path().join("missing").display(),
            empty = empty.display(),
        ),
    );

    let undeclared = h.run(&["--collection", "ghost", "init", "--quiet"]);
    assert_code(&undeclared, 3);
    assert!(text(&undeclared).contains("is not declared"));
    assert!(!h.store("ghost").join("enzyme.db").exists());

    let missing = h.run(&["--collection", "gone", "init", "--quiet"]);
    assert_code(&missing, 3);
    assert!(text(&missing).contains("folder does not exist"));

    let refresh_missing = h.run(&["--collection", "gone", "refresh", "--quiet"]);
    assert_code(&refresh_missing, 3);

    // An empty source is not silent success.
    let empty = h.run(&["--collection", "empty", "init", "--quiet", "--use-env-llm"]);
    assert_code(&empty, 0);
    let result = summary(&empty);
    assert_eq!(result["status"], "completed_with_warnings");
    assert!(
        result["warnings"]
            .to_string()
            .contains("indexed 0 documents"),
        "{}",
        text(&empty)
    );

    let bad_name = h.run(&["--collection", "../escape", "init", "--quiet"]);
    assert_code(&bad_name, 2);
    let bad_flag = h.run(&["init", "--no-such-flag"]);
    assert_code(&bad_flag, 2);

    // Another workspace's invalid program does not fail this one…
    h.program("broken.enzyme", "workspace oops");
    let neighbour = h.run(&["--collection", "real", "init", "--stage", "index", "--quiet"]);
    assert_code(&neighbour, 0);
    // …but its own does.
    h.program("ws.enzyme", "workspace oops");
    let broken = h.run(&["--collection", "real", "init", "--quiet"]);
    assert_code(&broken, 3);

    // --json-progress callers also get a terminal error event.
    let json = h.run(&["--collection", "real", "init", "--json-progress"]);
    assert_code(&json, 3);
    let stderr = String::from_utf8_lossy(&json.stderr);
    let event = stderr
        .lines()
        .find(|line| line.contains(r#""stage":"error""#))
        .unwrap_or_else(|| panic!("no error event\n{}", text(&json)));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(event).unwrap()["exit_code"],
        3
    );
}

#[test]
fn foreground_init_waits_for_or_fails_on_a_held_workspace() {
    let h = Home::new();
    let notes = h.notes("notes", 2);
    h.program(
        "lone.enzyme",
        &format!(
            "settings {{ updates disabled }}\nworkspace \"lone\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  learn questions from tag \"focus\"\n}}\n",
            notes.display()
        ),
    );
    let store = h.store("lone");

    // Another foreground caller holds the workspace: --lock-timeout 0 fails busy.
    let held = hold(&store.join("workspace.lock"));
    let busy = h.run(&[
        "--collection",
        "lone",
        "init",
        "--quiet",
        "--use-env-llm",
        "--lock-timeout",
        "0",
    ]);
    assert_code(&busy, 4);
    assert!(text(&busy).contains("workspace busy"), "{}", text(&busy));
    assert!(!store.join("enzyme.db").exists());

    // With a timeout, the caller announces the wait and proceeds on release.
    let child = h
        .command(&[
            "--collection",
            "lone",
            "init",
            "--quiet",
            "--use-env-llm",
            "--json-progress",
            "--lock-timeout",
            "60",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    drop(held);
    let waited = child.wait_with_output().unwrap();
    assert_code(&waited, 0);
    assert!(
        String::from_utf8_lossy(&waited.stderr).contains(r#""status":"waiting""#),
        "{}",
        text(&waited)
    );

    // A detached catalyst worker (refresh.lock) blocks a foreground init, which
    // may reset or rewrite catalysts, but not a foreground document refresh.
    let worker = hold(&store.join("refresh.lock"));
    let init = h.run(&[
        "--collection",
        "lone",
        "init",
        "--quiet",
        "--use-env-llm",
        "--lock-timeout",
        "0",
    ]);
    assert_code(&init, 4);
    assert!(text(&init).contains("background catalyst worker"));
    let refresh = h.run(&[
        "--collection",
        "lone",
        "refresh",
        "--quiet",
        "--use-env-llm",
        "--lock-timeout",
        "0",
    ]);
    assert_code(&refresh, 0);
    drop(worker);
}

#[test]
fn three_concurrent_forced_inits_serialize_and_all_succeed() {
    let h = Home::new();
    // Slow generation keeps each run inside the lock long enough to contend.
    h.llm.set_delay(Duration::from_millis(300));
    let notes = h.notes("notes", 3);
    h.program(
        "lone.enzyme",
        &format!(
            "settings {{ updates disabled }}\nworkspace \"lone\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  learn questions from tag \"focus\"\n}}\n",
            notes.display()
        ),
    );

    let children: Vec<_> = (0..3)
        .map(|_| {
            h.command(&[
                "--collection",
                "lone",
                "init",
                "--force",
                "--use-env-llm",
                "--json-progress",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();

    for output in &outputs {
        assert_code(output, 0);
        assert_eq!(summary(output)["status"], "completed", "{}", text(output));
    }
    let waited = outputs
        .iter()
        .filter(|output| String::from_utf8_lossy(&output.stderr).contains(r#""stage":"lock""#))
        .count();
    assert!(waited >= 2, "expected two callers to wait, saw {waited}");
    assert_eq!(
        document_refs(&h.store("lone").join("enzyme.db")),
        vec!["note-0.md", "note-1.md", "note-2.md"]
    );
}

/// One home, many workspaces: two workspaces may read one notes folder, and a
/// workspace whose program is invalid fails only when it is addressed.
#[test]
fn workspaces_in_one_home_do_not_fail_together() {
    let h = Home::new();
    let notes = h.notes("shared", 3);
    h.program("settings.enzyme", "settings { updates disabled }\n");
    for (name, reading) in [
        ("meetings", r#"learn questions from tag "focus""#),
        ("people", r#"learn questions from tag "focus" about relationships"#),
    ] {
        h.program(
            &format!("{name}.enzyme"),
            &format!(
                "workspace \"{name}\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  {reading}\n}}\n",
                notes.display()
            ),
        );
    }
    // One program resolves to nothing usable; another does not parse.
    h.program(
        "broken.enzyme",
        "workspace \"broken\" {\n  source markdown \"notes\" { path \"relative/notes\" }\n}\n",
    );
    h.program("typo.enzyme", "workspace \"typo\" {\n  learn questions from\n");
    let before = h.config_snapshot();

    for name in ["meetings", "people"] {
        let init = h.run(&["--workspace", name, "init", "--quiet", "--use-env-llm"]);
        assert_code(&init, 0);
        let result = summary(&init);
        assert_eq!(result["indexing"]["files_discovered"], 3, "{}", text(&init));
        assert!(result["catalysts_generated"].as_u64().unwrap() > 0);
        let db = h.store(name).join("enzyme.db");
        assert_eq!(
            document_refs(&db),
            vec!["note-0.md", "note-1.md", "note-2.md"]
        );
    }
    assert!(!notes.join(".enzyme").exists());
    assert_eq!(h.config_snapshot(), before, "init rewrote configuration");

    // Each workspace keeps its own readings although they share a folder.
    let inspect = |name: &str| {
        let output = h
            .command(&["spec", "inspect"])
            .arg(h.store(name))
            .output()
            .unwrap();
        assert_code(&output, 0);
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    // `relationships` resolves to the built-in `relational` profile.
    assert!(!inspect("meetings").contains("relational"));
    assert!(inspect("people").contains("about relational"));

    // The invalid programs fail when addressed, with their own errors.
    let broken = h.run(&["--workspace", "broken", "init", "--quiet"]);
    assert_code(&broken, 3);
    assert!(text(&broken).contains("must be absolute"), "{}", text(&broken));
    let typo = h.run(&["--workspace", "typo", "init", "--quiet"]);
    assert_code(&typo, 3);
    assert!(
        text(&typo).contains("invalid reading config"),
        "{}",
        text(&typo)
    );

    // By path, the shared folder does not say which workspace is meant.
    let by_path = h
        .command(&["init", "--stage", "index", "--quiet"])
        .arg("-p")
        .arg(&notes)
        .output()
        .unwrap();
    assert_ne!(by_path.status.code(), Some(0), "{}", text(&by_path));
    assert!(
        text(&by_path).contains("address one of them with --workspace"),
        "{}",
        text(&by_path)
    );

    // Planning one workspace is not blocked by the others' invalid files.
    let desired = h.root.path().join("desired.enzyme");
    fs::write(
        &desired,
        format!(
            "workspace \"meetings\" {{\n  source markdown \"notes\" {{ path \"{}\" }}\n  learn questions from tag \"focus\" about decisions\n}}\n",
            notes.display()
        ),
    )
    .unwrap();
    let plan = h
        .command(&["workspace", "plan", "meetings", "--json", "--desired"])
        .arg(&desired)
        .output()
        .unwrap();
    assert_code(&plan, 0);
}

fn entity_hashes(db: &Path) -> Vec<(String, String, String)> {
    let db = rusqlite::Connection::open(db).unwrap();
    let mut statement = db
        .prepare(
            "SELECT entity, entity_type, context_hash FROM catalyst_entity_hashes ORDER BY 1, 2",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A second init over unchanged Markdown and SQLite data generates nothing:
/// catalyst prompts are built from the same embedded chunks every time.
#[test]
fn init_over_unchanged_data_is_idempotent_from_the_second_run() {
    let h = Home::new();
    let notes = h.notes("notes", 3);
    let ledger = h.root.path().join("ledger.db");
    let connection = rusqlite::Connection::open(&ledger).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, sent_ms INTEGER, sender TEXT, body TEXT);",
        )
        .unwrap();
    for i in 0..6 {
        connection
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    i,
                    1_700_000_000_000_i64 + i * 86_400_000,
                    if i % 2 == 0 { "ada@example.com" } else { "lin@example.com" },
                    format!(
                        "Message {i}: {}",
                        "The reversible migration keeps auditable evidence for review. ".repeat(12)
                    ),
                ],
            )
            .unwrap();
    }
    drop(connection);
    h.program(
        "mixed.enzyme",
        &format!(
            r#"settings {{ updates disabled }}
workspace "mixed" {{
  source markdown "notes" {{ path "{notes}" }}
  source sqlite "mail" {{
    database "{ledger}"
    query "SELECT id, sent_ms, sender, body FROM messages"
    id "id"
    who "sender"
    when "sent_ms" unit ms
    what "body"
  }}
  learn questions from tag "focus"
  learn questions from source "mail" including who links
}}
"#,
            notes = notes.display(),
            ledger = ledger.display(),
        ),
    );
    let db = h.store("mixed").join("enzyme.db");
    let init = || {
        let output = h.run(&["--workspace", "mixed", "init", "--quiet", "--use-env-llm"]);
        assert_code(&output, 0);
        summary(&output)
    };

    let first = init();
    assert!(
        first["catalysts_generated"].as_u64().unwrap() > 0,
        "{first}"
    );
    let hashes = entity_hashes(&db);
    assert!(hashes.len() >= 3, "expected tag, link and source entities: {hashes:?}");
    let requests = h.llm.request_count();
    for run in 2..=3 {
        let again = init();
        assert_eq!(again["catalysts_generated"], 0, "run {run}: {again}");
        assert_eq!(entity_hashes(&db), hashes, "run {run} changed entity hashes");
    }
    assert_eq!(h.llm.request_count(), requests, "unchanged data called the LLM");
}
