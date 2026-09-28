use std::io::Write;
use std::path::Path;
use std::process::Command;

use flate2::{Compression, write::GzEncoder};
use rusqlite::{Connection, params};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn field(number: u8, bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() < 128);
    let mut result = vec![(number << 3) | 2, bytes.len() as u8];
    result.extend_from_slice(bytes);
    result
}

fn note_blob(text: &str) -> Vec<u8> {
    let note = field(2, text.as_bytes());
    let document = field(3, &note);
    let payload = field(2, &document);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&payload).unwrap();
    encoder.finish().unwrap()
}

fn notes_database(path: &Path) {
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
        VALUES (2,12,'stable-note','Groceries',800000000.25,0,1),
               (4,12,'deleted-note','Old',800000000.25,0,3);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO ZICNOTEDATA VALUES (?1,?2)",
        params![
            2,
            note_blob("Groceries\nRatatouille and much more than a preview")
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO ZICNOTEDATA VALUES (?1,?2)",
        params![4, note_blob("Old deleted note")],
    )
    .unwrap();
}

fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
    command
        .env("ENZYME_HOME", home)
        .env("ENZYME_RUST_SCCACHE", "off");
    command
}

#[test]
fn apple_notes_preset_compiles_full_body_query_without_indexing() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let source = temp.path().join("NoteStore.sqlite");
    notes_database(&source);
    let source_arg = source.to_string_lossy().to_string();

    let dry = cli(&home)
        .args([
            "--collection",
            "apple-notes",
            "compile",
            "--preset",
            "apple-notes",
            "--dry-run",
        ])
        .arg(&source_arg)
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(String::from_utf8_lossy(&dry.stdout).contains("gzip_pb_text(d.ZDATA, '2.3.2')"));
    assert!(!home.exists(), "dry-run wrote to Enzyme home");

    let saved = cli(&home)
        .args([
            "--collection",
            "apple-notes",
            "compile",
            "--preset",
            "apple-notes",
            "--json",
        ])
        .arg(&source_arg)
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(receipt["inspection"]["rows"], 1);
    let program = std::fs::read_to_string(home.join("configs/apple-notes.enzyme")).unwrap();
    assert!(program.contains("filter \"apple-notes-content-v1\""));
    assert!(!home.join("workspaces/apple-notes/enzyme.db").exists());

    let scan = cli(&home)
        .args(["--collection", "apple-notes", "scan"])
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let scan: serde_json::Value = serde_json::from_slice(&scan.stdout).unwrap();
    assert_eq!(scan["source_checks"][0]["rows"], 1);
}

#[test]
fn apple_notes_preset_rejects_wrong_layout_before_saving() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let source = temp.path().join("unrelated.sqlite");
    Connection::open(&source).unwrap();
    let output = cli(&home)
        .args([
            "--collection",
            "notes",
            "compile",
            "--preset",
            "apple-notes",
        ])
        .arg(source)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("layout does not match"));
    assert!(!home.join("configs/notes.enzyme").exists());
}

#[tokio::test]
async fn arena_preset_resolves_ids_without_fetching_contents_or_indexing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/research"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 9,
            "slug": "research",
            "title": "Research",
            "counts": {"contents": 52}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let output = cli(&home)
        .args([
            "--collection",
            "research",
            "compile",
            "--preset",
            "arena",
            "research",
            "--json",
            "--arena-api-base",
        ])
        .arg(format!("{}/v3/", server.uri()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["inspection"]["channels"][0]["id"], 9);
    let program = std::fs::read_to_string(home.join("configs/arena-research.enzyme")).unwrap();
    assert!(program.contains("channel 9 as \"Research\""));
    assert!(!home.join("workspaces/research/enzyme.db").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
