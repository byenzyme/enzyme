//! `source kind` declarations index through the engine: a kind defined in the
//! configs namespace, the built-in `apple-notes` kind, and `.enzyme.in`
//! presets that emit them.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::{Compression, write::GzEncoder};
use rusqlite::{Connection, params};

const MEANINGFUL_NOTE: &str = "Listening\nI have been thinking about how conversations change when I listen longer. What surprised me was that the pause created space for a better question, and I want to try that again.";

struct Homes {
    _root: tempfile::TempDir,
    root: PathBuf,
    /// `$HOME`, so `~` and the default Apple Notes store live in the fixture.
    user: PathBuf,
    enzyme: PathBuf,
}

impl Homes {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let enzyme = root.path().join("enzyme");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(enzyme.join("configs")).unwrap();
        std::fs::write(
            enzyme.join("configs/settings.enzyme"),
            "settings {\n  updates disabled\n}\n",
        )
        .unwrap();
        Self {
            root: root.path().to_path_buf(),
            _root: root,
            user,
            enzyme,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_enzyme"))
            .args(args)
            .env("HOME", &self.user)
            .env("ENZYME_HOME", &self.enzyme)
            .env("ENZYME_RUST_SCCACHE", "off")
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap()
    }

    fn config(&self, name: &str, text: &str) {
        std::fs::write(self.enzyme.join("configs").join(name), text).unwrap();
    }

    fn index(&self, workspace: &str) {
        assert_success(&self.run(&[
            "init",
            "--stage",
            "index",
            "--quiet",
            "--collection",
            workspace,
        ]));
    }

    fn documents(&self, workspace: &str) -> Vec<String> {
        let db = Connection::open(
            self.enzyme
                .join("workspaces")
                .join(workspace)
                .join("enzyme.db"),
        )
        .unwrap();
        let mut statement = db
            .prepare("SELECT content FROM docs ORDER BY content")
            .unwrap();
        statement
            .query_map((), |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn apple_notes_store(&self) -> PathBuf {
        self.user
            .join("Library/Group Containers/group.com.apple.notes/NoteStore.sqlite")
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A length-delimited protobuf field.
fn field(number: u8, bytes: &[u8]) -> Vec<u8> {
    let mut result = vec![(number << 3) | 2];
    let mut length = bytes.len();
    while length >= 0x80 {
        result.push((length as u8 & 0x7f) | 0x80);
        length >>= 7;
    }
    result.push(length as u8);
    result.extend_from_slice(bytes);
    result
}

fn note_blob(text: &str) -> Vec<u8> {
    let payload = field(2, &field(3, &field(2, text.as_bytes())));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&payload).unwrap();
    encoder.finish().unwrap()
}

/// A minimal NoteStore.sqlite: one live note and one in Recently Deleted.
fn notes_database(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE ZICCLOUDSYNCINGOBJECT (
            Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, ZIDENTIFIER TEXT,
            ZTITLE1 TEXT, ZMODIFICATIONDATE1 REAL,
            ZISPASSWORDPROTECTED INTEGER, ZFOLDER INTEGER, ZTITLE2 TEXT
        );
        CREATE TABLE ZICNOTEDATA (ZNOTE INTEGER, ZDATA BLOB);
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK,Z_ENT,ZTITLE2)
        VALUES (1,15,'Notes'), (3,15,'Recently Deleted');
        INSERT INTO ZICCLOUDSYNCINGOBJECT
            (Z_PK,Z_ENT,ZIDENTIFIER,ZTITLE1,ZMODIFICATIONDATE1,ZISPASSWORDPROTECTED,ZFOLDER)
        VALUES (2,12,'live-note','Listening',800000000.25,0,1),
               (4,12,'deleted-note','Old',800000000.25,0,3);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO ZICNOTEDATA VALUES (?1,?2)",
        params![2, note_blob(MEANINGFUL_NOTE)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO ZICNOTEDATA VALUES (?1,?2)",
        params![
            4,
            note_blob("Old deleted note that should never be indexed")
        ],
    )
    .unwrap();
}

#[test]
fn user_defined_kind_indexes_rows_selected_by_an_escaped_field_value() {
    let homes = Homes::new();
    let account = r"o'neil \ x'); DROP TABLE messages; --";
    let source = homes.enzyme.join("sources/chat.db");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let db = Connection::open(&source).unwrap();
    db.execute_batch(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, account TEXT, sent_at INTEGER, body TEXT);",
    )
    .unwrap();
    for (id, owner, body) in [
        (1, account, "kept first message"),
        (2, "someone else", "message from another account"),
        (3, account, "kept second message"),
    ] {
        db.execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4)",
            params![id, owner, 1_700_000_000_000_i64 + id, body],
        )
        .unwrap();
    }
    drop(db);

    homes.config(
        "kinds.enzyme",
        r#"source kind chat-log {
  needs account
  accepts label
  database "{home}/sources/{workspace}.db"
  query "SELECT id, sent_at, body FROM messages WHERE account = {account} ORDER BY id"
  id "id"
  when "sent_at" unit ms
  what "body"
}
"#,
    );
    homes.config(
        "chat.enzyme",
        &format!(
            "workspace \"chat\" {{\n  source chat-log \"messages\" {{\n    account {}\n    label \"host only\"\n  }}\n\n  learn questions from source \"messages\"\n}}\n",
            serde_json::to_string(account).unwrap()
        ),
    );

    homes.index("chat");
    let documents = homes.documents("chat");
    assert_eq!(documents.len(), 2, "{documents:?}");
    assert!(
        documents.iter().all(|doc| doc.contains("kept")),
        "{documents:?}"
    );
    // The value was a literal, not SQL: the source table is intact.
    let rows: i64 = Connection::open(&source)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM messages", (), |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 3);

    // A field the kind neither needs nor accepts fails before indexing.
    homes.config(
        "chat.enzyme",
        "workspace \"chat\" {\n  source chat-log \"messages\" { account \"a\" lable \"x\" }\n}\n",
    );
    let typo = homes.run(&[
        "init",
        "--stage",
        "index",
        "--quiet",
        "--collection",
        "chat",
    ]);
    assert!(!typo.status.success());
    assert!(
        stderr(&typo).contains("unknown field \"lable\""),
        "{}",
        stderr(&typo)
    );
}

#[test]
fn apple_notes_preset_emits_the_builtin_kind_and_indexes_the_default_store() {
    let homes = Homes::new();
    notes_database(&homes.apple_notes_store());

    let saved = homes.run(&[
        "--collection",
        "notes",
        "compile",
        "--preset",
        "apple-notes",
        "--json",
    ]);
    assert_success(&saved);
    let receipt: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(receipt["preset"], "apple-notes");
    assert_eq!(receipt["inspection"]["rows"], 1);
    let program = std::fs::read_to_string(homes.enzyme.join("configs/notes.enzyme")).unwrap();
    assert!(
        program.contains("source apple-notes \"notes\" {}"),
        "{program}"
    );
    assert!(!program.contains("SELECT"), "{program}");

    homes.index("notes");
    let documents = homes.documents("notes");
    assert_eq!(documents.len(), 1, "{documents:?}");
    assert!(
        documents[0].contains("the pause created space"),
        "{documents:?}"
    );

    // An explicit store path becomes a database override.
    let copy = homes.root.join("Copy of NoteStore.sqlite");
    std::fs::copy(homes.apple_notes_store(), &copy).unwrap();
    let dry = homes.run(&[
        "--collection",
        "notes-copy",
        "compile",
        "--preset",
        "apple-notes",
        "--dry-run",
        copy.to_str().unwrap(),
    ]);
    assert_success(&dry);
    let canonical = copy.canonicalize().unwrap();
    assert!(
        String::from_utf8_lossy(&dry.stdout).contains(&format!(
            "source apple-notes \"notes\" {{ database {} }}",
            serde_json::to_string(&canonical).unwrap()
        )),
        "{}",
        String::from_utf8_lossy(&dry.stdout)
    );
}

#[test]
fn apple_notes_preset_without_a_store_names_the_default_path() {
    let homes = Homes::new();
    let output = homes.run(&[
        "--collection",
        "notes",
        "compile",
        "--preset",
        "apple-notes",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Apple Notes database not found")
            && stderr(&output).contains("group.com.apple.notes/NoteStore.sqlite"),
        "{}",
        stderr(&output)
    );
    assert!(!homes.enzyme.join("configs/notes.enzyme").exists());
}

#[test]
fn saved_programs_with_pasted_apple_notes_sql_keep_indexing() {
    let homes = Homes::new();
    let store = homes.root.join("NoteStore.sqlite");
    notes_database(&store);
    homes.config(
        "notes.enzyme",
        &format!(
            r#"workspace "notes" {{
  source sqlite "notes" {{
    database {}
    query """
SELECT n.ZIDENTIFIER AS note_id,
       n.ZTITLE1 AS title,
       n.ZMODIFICATIONDATE1 AS modified,
       gzip_pb_text(d.ZDATA, '2.3.2') AS body
FROM ZICCLOUDSYNCINGOBJECT AS n
JOIN ZICNOTEDATA AS d ON d.ZNOTE = n.Z_PK
JOIN ZICCLOUDSYNCINGOBJECT AS f ON f.Z_PK = n.ZFOLDER AND f.Z_ENT = 15
WHERE n.Z_ENT = 12
  AND n.ZISPASSWORDPROTECTED = 0
  AND f.ZTITLE2 = 'Notes'
  AND n.ZMODIFICATIONDATE1 IS NOT NULL
  AND d.ZDATA IS NOT NULL
"""
    id "note_id"
    when "modified" unit s epoch "2001-01-01"
    what ["title", "body"]
    filter "apple-notes-content-v1"
  }}

  learn questions from source "notes"
}}
"#,
            serde_json::to_string(&store).unwrap()
        ),
    );
    homes.index("notes");
    let documents = homes.documents("notes");
    assert_eq!(documents.len(), 1, "{documents:?}");
    assert!(documents[0].contains("the pause created space"));
}

#[test]
fn preset_paths_fill_external_templates() {
    let homes = Homes::new();
    let folder = homes.root.join("Meeting Notes");
    std::fs::create_dir_all(folder.join("Meetings")).unwrap();
    let template = homes.root.join("margins-meetings.enzyme.in");
    std::fs::write(
        &template,
        r#"// margins-meetings preset v1
workspace __WORKSPACE__ {
  source markdown "home" { path __SOURCE__ }

  remember in folder "Meetings" create note
  learn questions from folder "Meetings"
}
"#,
    )
    .unwrap();
    let template_arg = template.to_str().unwrap();

    let missing = homes.run(&["--collection", "m", "compile", "--preset", template_arg]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("needs exactly one source argument"));

    let saved = homes.run(&[
        "--collection",
        "m",
        "compile",
        "--preset",
        template_arg,
        "--json",
        folder.to_str().unwrap(),
    ]);
    assert_success(&saved);
    let receipt: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(receipt["preset"], template_arg);
    let program = std::fs::read_to_string(homes.enzyme.join("configs/m.enzyme")).unwrap();
    assert!(program.starts_with("// margins-meetings preset v1\nworkspace \"m\" {"));
    assert!(program.contains(&format!(
        "source markdown \"home\" {{ path {} }}",
        serde_json::to_string(&folder).unwrap()
    )));

    let unknown = homes.run(&["--collection", "m", "compile", "--preset", "nope"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("unknown preset \"nope\""));
}
