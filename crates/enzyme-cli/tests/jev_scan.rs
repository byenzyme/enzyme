//! Exercise the actual CLI with a local TypeSafe server; no real credentials or
//! network access required. Config must be usable by the existing selection path.
use serde_json::{Value, json};
use std::process::Command;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

struct Decisions;

#[tokio::test(flavor = "multi_thread")]
async fn jev_scan_batches_all_candidates_and_merges_profiles() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(3)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    for i in 0..17 {
        let dir = vault.join(format!("project-{i}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("note.md"), "We chose a small launch to learn from real users before committing to the larger project.").unwrap();
    }
    let output = command(&vault, &home, &server.uri())
        .arg("--jev")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["selection"]["decisions"].as_array().unwrap().len(),
        18
    );
    assert_eq!(
        result["selection"]["batch_request_ms"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        result["selection"]["unevaluated_entities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        result["selection"]["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["profile"] == "decision_trace")
    );
    assert!(
        !result["suggested_entities_toml"]
            .as_str()
            .unwrap()
            .contains("expandable = false")
    );
}
impl Respond for Decisions {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let answers: serde_json::Map<String, Value> = body["questions"].as_object().unwrap().iter().map(|(id, question)| {
            let choice = if id.starts_with("select_") { "include" } else { "decision_trace" };
            let probabilities: serde_json::Map<String, Value> = question["criteria"].as_object().unwrap().keys()
                .map(|k| (k.clone(), json!(if k == choice { 1.0 } else { 0.0 }))).collect();
            (id.clone(), json!({"type":"choice", "choice":choice, "probabilities":probabilities, "confidence":0.95}))
        }).collect();
        ResponseTemplate::new(200).set_body_json(
            json!({"model":"jev-1.13.0","answers":answers,"usage":{"input_tokens":123}}),
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jev_expandable_people_uses_backlinks_even_with_empty_identity_pages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(1)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    std::fs::create_dir(vault.join("people")).unwrap();
    std::fs::create_dir(vault.join("inbox")).unwrap();
    std::fs::write(vault.join("people/Alice.md"), "").unwrap();
    std::fs::write(vault.join("people/Bob.md"), "").unwrap();
    for i in 0..3 {
        std::fs::write(vault.join(format!("inbox/{i}.md")), "Met [[Alice]] and [[Bob]] to discuss how our collaboration has changed and what each of us needs from the relationship.").unwrap();
    }
    let output = command(&vault, &home, &server.uri())
        .arg("--jev")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let people = body["state"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["spec"] == "folder:people")
        .unwrap();
    assert_eq!(people["expandable"], true);
    assert!(
        people["page_entities"]
            .as_array()
            .unwrap()
            .contains(&json!("[[alice]]"))
    );
    assert!(
        people["samples"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["via_entity"] == "[[alice]]"
                && s["source_ref"].as_str().unwrap().starts_with("inbox/"))
    );
    assert!(
        people["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s["excerpt"].as_str().unwrap().is_empty())
    );
    assert!(!vault.join(".enzyme").exists());
}

fn command(vault: &std::path::Path, home: &std::path::Path, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
    command
        .arg("-p")
        .arg(vault)
        .arg("scan")
        .env("ENZYME_HOME", home)
        .env("TYPESAFE_API_KEY", "test-key")
        .env("TYPESAFE_BASE_URL", base);
    command
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    let home = root.path().join("home");
    std::fs::create_dir_all(vault.join("projects")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(vault.join("projects/decision.md"), "# Decision\nWe chose a smaller launch because we need to validate the workflow before expanding.").unwrap();
    std::fs::write(home.join("config.toml"), "[update]\nauto = false\n").unwrap();
    (root, vault, home)
}

#[tokio::test(flavor = "multi_thread")]
async fn jev_scan_preview_then_write_preserves_notes_and_existing_config() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(2)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let original_config = std::fs::read(home.join("config.toml")).unwrap();
    let original_note = std::fs::read(vault.join("projects/decision.md")).unwrap();
    let preview = command(&vault, &home, &server.uri())
        .arg("--jev")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let result: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(result["status"], "suggested");
    assert_eq!(
        result["selection"]["decisions"][0]["profile"],
        "decision_trace"
    );
    assert_eq!(
        std::fs::read(home.join("config.toml")).unwrap(),
        original_config
    );
    assert!(!vault.join(".enzyme").exists());

    let written = command(&vault, &home, &server.uri())
        .args(["--jev", "--write-config"])
        .output()
        .unwrap();
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(config.contains("decision_trace"));
    assert!(config.contains("auto = false"));
    let parsed = enzyme_core::config::enzyme_config::EnzymeConfig::load_at(
        &vault,
        &home.join("config.toml"),
    )
    .unwrap()
    .unwrap()
    .parse_preferences();
    assert_eq!(parsed.entity_profiles["projects"], "decision_trace");
    assert_eq!(
        std::fs::read(vault.join("projects/decision.md")).unwrap(),
        original_note
    );
    assert!(!vault.join(".enzyme").exists());

    let refused = command(&vault, &home, &server.uri())
        .args(["--jev", "--write-config"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already has entities"));
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        config
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jev_scan_bad_response_leaves_no_config_mutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"model":"jev", "answers":{}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let before = std::fs::read(home.join("config.toml")).unwrap();
    let output = command(&vault, &home, &server.uri())
        .args(["--jev", "--write-config"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(home.join("config.toml")).unwrap(), before);
    assert!(!vault.join(".enzyme").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_scan_never_calls_jev_even_with_a_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(0)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    std::fs::create_dir(vault.join(".enzyme")).unwrap();
    std::fs::write(vault.join(".enzyme/enzyme.log"), "existing log").unwrap();
    let output = command(&vault, &home, &server.uri())
        .arg("-v")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["schema_version"],
        "scan.v2"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join(".enzyme/enzyme.log")).unwrap(),
        "existing log"
    );
}
