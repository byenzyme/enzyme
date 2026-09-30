//! Exercise `enzyme compile` against a local OpenRouter Decisions server; no real
//! credentials or network access required. The program it writes must be usable by
//! the existing selection path, and nothing it does may touch the user's notes.
use serde_json::{Value, json};
use std::process::Command;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

struct Decisions;

#[tokio::test(flavor = "multi_thread")]
async fn compile_batches_all_candidates_and_merges_profiles() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(2)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    for i in 0..17 {
        let dir = vault.join(format!("project-{i}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("note.md"), "We chose a small launch to learn from real users before committing to the larger project.").unwrap();
    }
    let output = command(&vault, &home, &server.uri())
        .env("ENZYME_JEV_MODEL", "typesafe/jev-test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The mock's `.expect(2)` proves the candidates were batched across two
    // requests. What matters downstream is that every decision reached the
    // program with its merged profile, so assert the program itself.
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let program = result["program"].as_str().unwrap();
    assert!(program.contains("workspace \"vault\""), "{program}");
    assert!(program.contains("source markdown \"notes\""), "{program}");
    assert!(!program.contains("\nvault \""), "{program}");
    assert_eq!(program.matches("learn questions from folder").count(), 18);
    assert_eq!(program.matches("about decision_trace").count(), 18);
    assert!(program.contains("sample by time"));
    assert!(!program.contains("sample across time") && !program.contains("favor "));
    let compiled =
        enzyme_spec::resolve(vec![enzyme_spec::parse(program).unwrap()], &vault).unwrap();
    assert!(
        compiled.vaults[0]
            .readings
            .iter()
            .all(|r| r.learning.sample.as_deref() == Some("across_time")
                && r.learning.favor.as_deref() == Some("equal"))
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
        assert!(
            body["state"]["learning_definitions"]["select"]
                .as_str()
                .unwrap()
                .contains("EACH folder")
        );
        assert!(
            body["state"]["learning_definitions"]["sampling"]
                .as_str()
                .unwrap()
                .contains("before generation")
        );
        assert!(
            body["questions"]
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k.starts_with("temporal_"))
        );
        let answers: serde_json::Map<String, Value> = body["questions"].as_object().unwrap().iter().map(|(id, question)| {
            let choice = if id.starts_with("select_") { "include" } else if id.starts_with("temporal_") { "time" } else { "decision_trace" };
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
async fn compile_uses_hosted_free_config_without_an_environment_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/llm/free-config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_key": "hosted-test-key",
            "base_url": server.uri(),
            "model": "catalyst-model"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/alpha/decisions"))
        .respond_with(Decisions)
        .expect(1)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let output = command(&vault, &home, &server.uri())
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_MODEL")
        .env(
            "ENZYME_FREE_CONFIG_URL",
            format!("{}/llm/free-config", server.uri()),
        )
        .env("ENZYME_API_CACHE_PATH", home.join("hosted-cache.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.received_requests().await.unwrap();
    let decision = requests
        .iter()
        .find(|request| request.url.path() == "/api/alpha/decisions")
        .unwrap();
    assert_eq!(
        decision.headers.get("authorization").unwrap(),
        "Bearer hosted-test-key"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_expandable_people_uses_backlinks_even_with_empty_identity_pages() {
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
    let output = command(&vault, &home, &server.uri()).output().unwrap();
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
    let scan = scan_command(&vault, &home, &server.uri()).output().unwrap();
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
    // Expandability is stated directly in the program rather than round-tripped
    // through a TOML proposal.
    let program = result["program"].as_str().unwrap();
    let people_reading = program
        .split("learn questions from ")
        .find(|block| block.starts_with("folder \"people\""))
        .expect("people folder reading");
    assert!(people_reading.contains("including linked pages"));
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
        .arg("compile")
        .arg("--json")
        .env("ENZYME_HOME", home)
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base);
    command
}

/// The hidden read-only inventory command, still used by SQLite source validation.
fn scan_command(vault: &std::path::Path, home: &std::path::Path, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
    command
        .arg("-p")
        .arg(vault)
        .arg("scan")
        .env("ENZYME_HOME", home)
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base);
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
async fn compile_writes_a_program_without_touching_notes_or_legacy_config() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(2)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let original_config = std::fs::read(home.join("config.toml")).unwrap();
    let original_note = std::fs::read(vault.join("projects/decision.md")).unwrap();

    let first = command(&vault, &home, &server.uri()).output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let result: Value = serde_json::from_slice(&first.stdout).unwrap();
    let saved = result["saved_to"].as_str().unwrap();
    assert!(std::path::Path::new(saved).exists());
    assert!(
        result["program"]
            .as_str()
            .unwrap()
            .contains("source markdown")
    );
    assert!(
        result["program"]
            .as_str()
            .unwrap()
            .contains("decision_trace")
    );

    // Writing a program touches neither the notes nor the legacy config, and
    // builds no index. Backing out is deleting the one file it wrote.
    assert_eq!(
        std::fs::read(home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        std::fs::read(vault.join("projects/decision.md")).unwrap(),
        original_note
    );
    assert!(!vault.join(".enzyme").exists());

    // The saved program is what init consumes.
    let loaded = enzyme_core::config::enzyme_config::EnzymeConfig::load_at(
        &vault,
        &home.join("config.toml"),
    )
    .unwrap()
    .unwrap()
    .parse_preferences();
    assert_eq!(loaded.entity_profiles["projects"], "decision_trace");

    // A second compile re-selects and rewrites this vault's program while
    // leaving the notes alone.
    let second = command(&vault, &home, &server.uri()).output().unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        std::fs::read(vault.join("projects/decision.md")).unwrap(),
        original_note
    );
    assert!(!vault.join(".enzyme").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_replaces_matching_legacy_vault_block_with_workspace_syntax() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    let file = home.join("configs/existing.enzyme");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "vault {:?} {{ learn questions from folder \"projects\" }}\n",
            vault.to_string_lossy()
        ),
    )
    .unwrap();

    let output = command(&vault, &home, &server.uri()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["saved_to"].as_str(), Some(file.to_str().unwrap()));
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("workspace \"vault\""), "{saved}");
    assert!(saved.contains("source markdown \"notes\""), "{saved}");
    assert!(!saved.contains("\nvault \""), "{saved}");
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_bad_response_leaves_no_config_mutation() {
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
    let output = command(&vault, &home, &server.uri()).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(home.join("config.toml")).unwrap(), before);
    // A refused selection writes no program at all, so there is nothing to undo.
    assert!(
        !home.join("configs").exists()
            || std::fs::read_dir(home.join("configs"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(!vault.join(".enzyme").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_scan_never_calls_jev_even_with_a_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Decisions)
        .expect(0)
        .mount(&server)
        .await;
    let (_root, vault, home) = fixture();
    std::fs::create_dir(vault.join(".enzyme")).unwrap();
    std::fs::write(vault.join(".enzyme/enzyme.log"), "existing log").unwrap();
    let output = scan_command(&vault, &home, &server.uri())
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
async fn compile_includes_tags_beyond_display_limit_and_supplies_hierarchy() {
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
    let output = command(&vault, &home, &server.uri()).output().unwrap();
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
async fn compile_samples_use_two_library_dated_notes_with_titles_and_paths() {
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
    let output = command(&vault, &home, &server.uri()).output().unwrap();
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
