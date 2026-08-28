use std::path::PathBuf;
use std::process::{Command, Output};

use rusqlite::Connection;

struct Fixture {
    _root: tempfile::TempDir,
    enzyme_home: PathBuf,
    source_db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let enzyme_home = root.path().join("home");
        let source_db = root.path().join("messages.db");
        std::fs::create_dir_all(&enzyme_home).unwrap();

        let source = Connection::open(&source_db).unwrap();
        source
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    sent_at INTEGER NOT NULL,
                    body TEXT NOT NULL,
                    thread TEXT NOT NULL
                );
                INSERT INTO messages VALUES (1, 1700000000000, 'first', 'Thread A');",
            )
            .unwrap();
        drop(source);

        std::fs::write(
            enzyme_home.join("config.toml"),
            format!(
                r#"
[update]
auto = false

[defaults]
minimum_folders = 1

[workspaces.imessage]

[workspaces.imessage.sources.messages]
db = "{}"
query = "SELECT id, sent_at, body, thread FROM messages ORDER BY id"
roles = {{ id = "id", when = "sent_at", what = "body", where = "thread" }}
timestamp = {{ unit = "ms" }}
"#,
                source_db.display()
            ),
        )
        .unwrap();

        Self {
            _root: root,
            enzyme_home,
            source_db,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
        command
            .args(args)
            .env("ENZYME_HOME", &self.enzyme_home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn store(&self) -> PathBuf {
        self.enzyme_home.join("workspaces/imessage")
    }

    fn enzyme_db(&self) -> PathBuf {
        self.store().join("enzyme.db")
    }

    fn insert_message(&self, id: i64, body: &str, thread: &str) {
        let source = Connection::open(&self.source_db).unwrap();
        source
            .execute(
                "INSERT INTO messages (id, sent_at, body, thread) VALUES (?1, ?2, ?3, ?4)",
                (id, 1_700_000_000_000_i64 + id, body, thread),
            )
            .unwrap();
    }

    fn update_message(&self, id: i64, body: &str) {
        Connection::open(&self.source_db)
            .unwrap()
            .execute("UPDATE messages SET body = ?1 WHERE id = ?2", (body, id))
            .unwrap();
    }

    fn delete_message(&self, id: i64) {
        Connection::open(&self.source_db)
            .unwrap()
            .execute("DELETE FROM messages WHERE id = ?1", [id])
            .unwrap();
    }

    fn indexed_document_count(&self) -> i64 {
        Connection::open(self.enzyme_db())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM docs", (), |row| row.get(0))
            .unwrap()
    }

    fn indexed_content(&self, needle: &str) -> Option<String> {
        Connection::open(self.enzyme_db())
            .unwrap()
            .query_row(
                "SELECT content FROM docs WHERE content LIKE '%' || ?1 || '%'",
                [needle],
                |row| row.get(0),
            )
            .ok()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn named_source_workspace_refreshes_incrementally_in_its_home_rooted_store() {
    let fixture = Fixture::new();

    let init = fixture.run(&[
        "init",
        "--stage",
        "index",
        "--quiet",
        "--collection",
        "imessage",
    ]);
    assert_success(&init);
    assert!(fixture.enzyme_db().exists());
    assert!(!fixture.store().join(".enzyme").exists());
    assert!(!fixture.store().join("workspace").exists());
    assert_eq!(fixture.indexed_document_count(), 1);

    fixture.insert_message(2, "second", "Thread B");
    let refresh = fixture.run(&["refresh", "--quiet", "--collection", "imessage"]);
    assert_success(&refresh);
    let json: serde_json::Value = serde_json::from_slice(&refresh.stdout).unwrap();
    assert_eq!(json["sources"]["sources_queried"], 1);
    assert_eq!(json["sources"]["rows_seen"], 2);
    assert_eq!(json["sources"]["documents_rebuilt"], 1);
    assert_eq!(json["sources"]["documents_skipped"], 1);
    assert_eq!(fixture.indexed_document_count(), 2);

    fixture.update_message(1, "first edited in place");
    let edit = fixture.run(&["refresh", "--quiet", "--collection", "imessage"]);
    assert_success(&edit);
    let json: serde_json::Value = serde_json::from_slice(&edit.stdout).unwrap();
    assert_eq!(json["sources"]["documents_rebuilt"], 1);
    assert_eq!(json["sources"]["documents_skipped"], 1);
    assert!(fixture.indexed_content("first edited in place").is_some());
    assert!(fixture.indexed_content("# Thread A\n\n").is_some());

    fixture.delete_message(2);
    let delete = fixture.run(&["refresh", "--quiet", "--collection", "imessage"]);
    assert_success(&delete);
    let json: serde_json::Value = serde_json::from_slice(&delete.stdout).unwrap();
    assert_eq!(json["sources"]["documents_rebuilt"], 0);
    assert_eq!(json["sources"]["documents_skipped"], 1);
    assert_eq!(json["sources"]["documents_pruned"], 1);
    assert_eq!(fixture.indexed_document_count(), 1);
    assert!(fixture.indexed_content("second").is_none());

    let status = fixture.run(&["status", "--collection", "imessage"]);
    assert_success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("Workspace: imessage"));
}

#[test]
fn collection_and_vault_addressing_are_mutually_exclusive() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--collection", "imessage", "--vault", "unused"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

#[test]
fn named_sqlite_scan_validates_without_authoring_config() {
    let fixture = Fixture::new();
    let config_path = fixture.enzyme_home.join("config.toml");
    let before = std::fs::read(&config_path).unwrap();

    let scan = fixture.run(&["scan", "--write-config", "--collection", "imessage"]);
    assert_success(&scan);
    let json: serde_json::Value = serde_json::from_slice(&scan.stdout).unwrap();
    assert_eq!(json["source_checks"][0]["valid"], true);
    assert_eq!(json["source_checks"][0]["rows"], 1);
    assert!(
        json["message"]
            .as_str()
            .unwrap()
            .contains("Markdown source")
    );
    assert_eq!(std::fs::read(config_path).unwrap(), before);
    assert!(!fixture.store().exists());
}

#[test]
fn failed_catalyst_attempt_does_not_gate_source_reexecution() {
    let fixture = Fixture::new();
    let init = fixture.run(&[
        "init",
        "--stage",
        "index",
        "--quiet",
        "--collection",
        "imessage",
    ]);
    assert_success(&init);
    let config_path = fixture.enzyme_home.join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap().replace(
        "[workspaces.imessage]\n",
        "[workspaces.imessage]\nentities = [\"folder:Thread A\"]\n",
    );
    std::fs::write(&config_path, config).unwrap();
    fixture.update_message(1, "updated before failed catalysts");
    let retry = fixture.run_with_env(
        &[
            "init",
            "--quiet",
            "--use-env-llm",
            "--collection",
            "imessage",
        ],
        &[
            ("OPENAI_API_KEY", "test-key"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:1/v1"),
            ("OPENAI_MODEL", "test-model"),
            // Keep this test non-vacuous after the evidence gates added in #13:
            // the single-row fixture must reach the deliberately failing LLM.
            ("ENZYME_MIN_CONTEXT_DOCUMENTS", "1"),
            ("ENZYME_MIN_CONTEXT_TOKENS", "1"),
        ],
    );
    assert!(
        !retry.status.success(),
        "the deliberately unreachable LLM must make the catalyst attempt fail"
    );
    assert!(
        String::from_utf8_lossy(&retry.stderr).contains("Catalyst generation produced 0 catalysts")
    );
    assert!(
        fixture
            .indexed_content("updated before failed catalysts")
            .is_some(),
        "source reconciliation must run before catalyst credential failure"
    );
    let config = std::fs::read_to_string(config_path).unwrap();
    assert!(config.contains("catalyst_format = \"question\""));
    assert!(config.contains("max_embedding_files = 1024"));
    assert!(config.contains("folder:Thread A"));
}

#[test]
fn named_markdown_workspace_indexes_notes_without_writing_into_the_notes_directory() {
    let root = tempfile::tempdir().unwrap();
    let enzyme_home = root.path().join("home");
    let notes = root.path().join("notes");
    std::fs::create_dir_all(&enzyme_home).unwrap();
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("hello.md"), "# Hello\n\nNamed workspace note.").unwrap();
    std::fs::write(
        enzyme_home.join("config.toml"),
        format!(
            r#"
[workspaces.research]

[workspaces.research.sources.notes]
path = "{}"
"#,
            notes.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .args([
            "init",
            "--stage",
            "index",
            "--quiet",
            "--collection",
            "research",
        ])
        .env("ENZYME_HOME", &enzyme_home)
        .output()
        .unwrap();
    assert_success(&output);

    let db_path = enzyme_home.join("workspaces/research/enzyme.db");
    let count: i64 = Connection::open(db_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM docs", (), |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(!notes.join(".enzyme").exists());
}
