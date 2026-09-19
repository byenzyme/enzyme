//! Exercise the actual CLI with a local OpenRouter Decisions server; no real credentials or
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
        .env("OPENROUTER_JEV_MODEL", "typesafe/jev-test")
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
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| {
        request.url.path() == "/api/alpha/decisions"
            && request.headers.get("authorization").unwrap() == "Bearer test-key"
            && serde_json::from_slice::<Value>(&request.body).unwrap()["model"]
                == "typesafe/jev-test"
    }));
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
    let mut extra_links = Vec::new();
    for i in 0..12 {
        std::fs::write(vault.join(format!("people/Person{i:02}.md")), "").unwrap();
        extra_links.push(format!("[[Person{i:02}]]"));
    }
    std::fs::write(vault.join("people/Alice.md"), "").unwrap();
    std::fs::write(vault.join("people/Bob.md"), "").unwrap();
    for i in 0..3 {
        std::fs::write(vault.join(format!("inbox/{i}.md")), format!("Met [[Alice]] and [[Bob]] with {} to discuss how our collaboration has changed and what each of us needs from the relationship.", extra_links.join(" "))).unwrap();
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
    assert_eq!(people["page_entities"].as_array().unwrap().len(), 10);
    assert!(
        body["state"]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| !c["spec"].as_str().unwrap().starts_with("[["))
    );
    let scan = command(&vault, &home, &server.uri()).output().unwrap();
    assert!(scan.status.success());
    let scan: Value = serde_json::from_slice(&scan.stdout).unwrap();
    let group = scan["folder_page_entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["folder"] == "people")
        .unwrap();
    assert_eq!(group["page_entities"].as_array().unwrap().len(), 10);
    assert!(
        scan["entities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s.as_str().unwrap().starts_with("[["))
    );

    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let proposed_path = home.join("proposal.toml");
    std::fs::write(
        &proposed_path,
        format!(
            "[vaults.{}]\n{}",
            serde_json::to_string(vault.to_str().unwrap()).unwrap(),
            result["suggested_entities_toml"].as_str().unwrap()
        ),
    )
    .unwrap();
    let proposed =
        enzyme_core::config::enzyme_config::EnzymeConfig::load_at(&vault, &proposed_path)
            .unwrap()
            .unwrap();
    let people_options = proposed
        .catalyzed_entities
        .iter()
        .find_map(|entity| match entity {
            enzyme_core::config::enzyme_config::CatalyzedEntity::WithOptions(options) => {
                options.get("folder:people")
            }
            _ => None,
        })
        .unwrap();
    assert!(people_options.expandable);
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
        .env("OPENROUTER_API_KEY", "test-key")
        .env("OPENROUTER_BASE_URL", base);
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

#[tokio::test(flavor = "multi_thread")]
async fn jev_scan_includes_tags_beyond_display_limit_and_supplies_hierarchy() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[update]\nauto = false\n[vaults.{}]\nentities = [\"#private-reference-only\"]\n",
            serde_json::to_string(vault.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let tags = (0..25)
        .map(|i| format!("#theme{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    for i in 0..3 {
        std::fs::write(vault.join(format!("projects/{i}.md")), format!("---\ncreated: 2020-01-01\n---\n{tags} #relationships #relationships/dating\nWe are learning how to communicate our expectations and support each other through changes.")).unwrap();
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
    assert!(
        requests
            .iter()
            .all(|r| !String::from_utf8_lossy(&r.body).contains("private-reference-only"))
    );
    let candidates: Vec<Value> = requests
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["state"]["candidates"].as_array().unwrap().clone()
        })
        .collect();
    assert_eq!(
        candidates
            .iter()
            .filter(|c| c["spec"].as_str().unwrap().starts_with('#'))
            .count(),
        27
    );
    let child = candidates
        .iter()
        .find(|c| c["spec"] == "#relationships/dating")
        .unwrap();
    assert_eq!(child["context"]["parent_tag"], "#relationships");
    assert_eq!(child["file_count"], 3);
    assert_eq!(child["context"]["dated_files"], 3);
    assert_eq!(child["context"]["recent_90d_files"], 0);
    assert_eq!(child["context"]["recent_365d_files"], 0);
    assert!(!child["samples"].as_array().unwrap().is_empty());
    let parent = candidates
        .iter()
        .find(|c| c["spec"] == "#relationships")
        .unwrap();
    assert!(
        parent["context"]["child_tags"]
            .as_array()
            .unwrap()
            .contains(&json!("#relationships/dating"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jev_samples_use_two_library_dated_notes_with_titles_and_paths() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let files = [
        (
            "projects/2019-01-01.md",
            "---\ntitle: A library dated note\ncreated: 2024-06-01\nmodified: 2025-01-01\n---\n",
        ),
        ("projects/2024/05/06/note.md", "# Hierarchical date\n"),
        ("projects/2024-04-01.md", "# April\n"),
        ("projects/2024-03-01.md", "# March\n"),
        ("projects/2024-02-01.md", "# February\n"),
        ("projects/2024-01-01.md", "# Copied old note\n"),
        ("projects/2023-01-01.md", "# Older\n"),
    ];
    for (path, header) in files {
        let path = vault.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{header}#research\nWe compared the evidence from several sources and recorded the reasons behind the conclusion.")).unwrap();
    }
    // A recent filesystem timestamp must not outrank explicit document dates.
    std::fs::File::open(vault.join("projects/2024-01-01.md"))
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() + std::time::Duration::from_secs(86_400),
            ),
        )
        .unwrap();
    std::fs::write(vault.join("projects/undated.md"), "# Undated\n#research #undated\nThis note has useful evidence but no explicit date and must not inherit import recency.").unwrap();
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
    let candidates: Vec<Value> = requests
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["state"]["candidates"].as_array().unwrap().clone()
        })
        .collect();
    let research = candidates
        .iter()
        .find(|c| c["spec"] == "#research")
        .unwrap();
    let samples = research["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0]["source_ref"], "projects/2019-01-01.md");
    assert_eq!(samples[0]["title"], "A library dated note");
    assert_eq!(samples[0]["date"], "2025-01-01T00:00:00+00:00");
    assert_eq!(samples[0]["date_source"], "frontmatter");
    assert_eq!(samples[1]["source_ref"], "projects/2024/05/06/note.md");
    assert_eq!(samples[1]["date_source"], "filename");
    assert_eq!(samples[1]["date"], "2024-05-06T00:00:00+00:00");
    assert_eq!(
        research["context"]["representative_files"][0],
        samples[0]["source_ref"]
    );
    assert!(
        samples
            .iter()
            .all(|s| s["source_ref"] != "projects/2024-01-01.md"
                && s["source_ref"] != "projects/undated.md")
    );
    let undated = candidates.iter().find(|c| c["spec"] == "#undated").unwrap();
    assert!(undated["samples"][0]["date"].is_null());
    assert!(undated["samples"][0]["date_source"].is_null());
}
