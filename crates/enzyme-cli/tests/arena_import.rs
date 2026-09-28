use std::process::Command;

use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

#[tokio::test(flavor = "multi_thread")]
async fn arena_dry_run_fetches_one_bounded_page_and_explains_sampling() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/research"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9,
            "slug": "research",
            "title": "Research",
            "counts": {"contents": 8}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/9/contents"))
        .and(query_param("page", "1"))
        .and(query_param("per", "2"))
        .and(query_param("sort", "created_at_desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "meta": {"total_count": 8},
            "data": [
                block(42, "2025-02-03T04:05:06Z"),
                block(41, "2025-01-02T03:04:05Z")
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[update]\nauto = false\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .arg("--collection")
        .arg("arena-test")
        .arg("arena")
        .arg("research")
        .arg("--limit")
        .arg("2")
        .arg("--dry-run")
        .arg("--json")
        .arg("--api-base")
        .arg(format!("{}/v3/", server.uri()))
        .env("ENZYME_HOME", &home)
        .env_remove("ARENA_ACCESS_TOKEN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let program = result["program"].as_str().unwrap();
    assert!(program.contains("sample by recency"), "{program}");
    assert!(program.contains("workspace \"arena-test\""), "{program}");
    assert!(program.contains("source arena \"channels\""), "{program}");
    assert!(program.contains("channel 9 as \"Research\""), "{program}");
    assert!(program.contains("from channel 9"), "{program}");
    let parsed = enzyme_spec::parse(program).unwrap();
    let resolved = enzyme_spec::resolve(vec![parsed], root.path()).unwrap();
    assert_eq!(
        resolved.vaults[0].readings[0].entity,
        "collection:arena:channels/channel/9"
    );

    let coverage = &result["coverage"];
    assert_eq!(coverage["api_fetch"]["sort"], "created_at_desc");
    assert_eq!(coverage["catalyst_evidence"]["policy"], "sample by recency");
    let plan_argv = coverage["catalyst_evidence"]["plan_argv"]
        .as_array()
        .unwrap();
    assert_eq!(plan_argv[0], "enzyme");
    assert_eq!(plan_argv[1], "-p");
    let workspace = home.join("workspaces/arena-test");
    assert_eq!(plan_argv[2], workspace.to_string_lossy().as_ref());
    assert_eq!(plan_argv[3], "spec");
    assert_eq!(plan_argv[4], "plan");
    assert_eq!(plan_argv[5], result["program_path"]);
    assert_eq!(plan_argv[6], "--json");
    assert!(
        coverage["catalyst_evidence"]["planned_distribution"]
            .as_str()
            .unwrap()
            .contains("planned excerpts")
    );
    assert!(
        coverage["catalyst_evidence"]["completed_epoch_evidence"]
            .as_str()
            .unwrap()
            .contains("not captured")
    );
    assert_eq!(coverage["channels"][0]["reported_connections"], 8);
    assert_eq!(coverage["channels"][0]["slug"], "research");
    assert_eq!(coverage["channels"][0]["title"], "Research");
    assert_eq!(coverage["channels"][0]["fetched_connections"], 2);
    assert_eq!(coverage["channels"][0]["partial"], true);
    assert_eq!(
        coverage["channels"][0]["oldest_connected_at"],
        "2025-01-02T03:04:05Z"
    );
    assert_eq!(coverage["channels"][0]["connected_by_month"]["2025-01"], 1);
    assert_eq!(coverage["channels"][0]["connected_by_month"]["2025-02"], 1);

    let omitted_first_run = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .args(["--collection", "new-arena", "arena", "--dry-run"])
        .arg("--api-base")
        .arg(format!("{}/v3/", server.uri()))
        .env("ENZYME_HOME", &home)
        .env_remove("ARENA_ACCESS_TOKEN")
        .output()
        .unwrap();
    assert!(!omitted_first_run.status.success());
    assert!(String::from_utf8_lossy(&omitted_first_run.stderr).contains("first import"));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("authorization").is_none())
    );
    assert!(!workspace.join(".enzyme").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn arena_index_program_drives_collection_plan_and_preserves_edits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/research"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9,
            "slug": "research",
            "title": "Research Display",
            "counts": {"contents": 2}
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9,
            "slug": "research",
            "title": "Research Display",
            "counts": {"contents": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/9/contents"))
        .and(query_param("page", "1"))
        .and(query_param("per", "2"))
        .and(query_param("sort", "created_at_desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "meta": {"total_count": 2},
            "data": [
                block(42, "2025-02-03T04:05:06Z"),
                block(41, "2024-01-02T03:04:05Z")
            ]
        })))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/outside"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 10,
            "slug": "outside",
            "title": "Outside",
            "counts": {"contents": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[update]\nauto = false\n").unwrap();
    let run_import = || {
        Command::new(env!("CARGO_BIN_EXE_enzyme"))
            .args([
                "--collection",
                "arena-test",
                "arena",
                "research",
                "--limit",
                "2",
                "--index-only",
                "--json",
                "--api-base",
            ])
            .arg(format!("{}/v3/", server.uri()))
            .env("ENZYME_HOME", &home)
            .env_remove("ARENA_ACCESS_TOKEN")
            .output()
            .unwrap()
    };
    let run_declared_scope = || {
        Command::new(env!("CARGO_BIN_EXE_enzyme"))
            .args([
                "--collection",
                "arena-test",
                "arena",
                "--limit",
                "2",
                "--index-only",
                "--json",
                "--api-base",
            ])
            .arg(format!("{}/v3/", server.uri()))
            .env("ENZYME_HOME", &home)
            .env_remove("ARENA_ACCESS_TOKEN")
            .output()
            .unwrap()
    };

    let first = run_import();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let imported: Value = serde_json::from_slice(&first.stdout).unwrap();
    let program_path = std::path::PathBuf::from(imported["program"].as_str().unwrap());
    let generated_program = std::fs::read_to_string(&program_path).unwrap();
    assert!(generated_program.contains("channel 9 as \"Research Display\""));
    assert!(generated_program.contains("learn questions from channel 9"));
    let workspace = home.join("workspaces/arena-test");
    let plan = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .arg("-p")
        .arg(&workspace)
        .arg("spec")
        .arg("plan")
        .arg(&program_path)
        .arg("--json")
        .env("ENZYME_HOME", &home)
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_json: Value = serde_json::from_slice(&plan.stdout).unwrap();
    let plan_text = plan_json.to_string();
    assert!(
        plan_text.contains("arena:channels/channel/9"),
        "{plan_text}"
    );
    assert!(plan_text.contains("collection"), "{plan_text}");
    assert!(plan_text.contains("2024-Q1"), "{plan_text}");
    assert!(plan_text.contains("2025-Q1"), "{plan_text}");

    let edited = std::fs::read_to_string(&program_path)
        .unwrap()
        .replace("sample by recency", "sample by time");
    std::fs::write(&program_path, &edited).unwrap();
    let second = run_import();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let refreshed: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(refreshed["program_preserved"], true);
    assert!(
        std::fs::read_to_string(&program_path)
            .unwrap()
            .contains("sample by time")
    );
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(refreshed["coverage_receipt"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["catalyst_evidence"]["policy"], "sample by time");

    let source_only = std::fs::read_to_string(&program_path)
        .unwrap()
        .replace("  learn questions from channel 9\n", "");
    std::fs::write(&program_path, source_only).unwrap();
    let third = run_declared_scope();
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    let source_only_result: Value = serde_json::from_slice(&third.stdout).unwrap();
    let source_only_receipt: Value = serde_json::from_slice(
        &std::fs::read(source_only_result["coverage_receipt"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        source_only_receipt["catalyst_evidence"]["policy"],
        "not sampled (no reading)"
    );

    let rejected = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .args([
            "--collection",
            "arena-test",
            "arena",
            "outside",
            "--dry-run",
        ])
        .arg("--api-base")
        .arg(format!("{}/v3/", server.uri()))
        .env("ENZYME_HOME", &home)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("outside the source arena scope"));
}

fn block(id: i64, connected_at: &str) -> Value {
    json!({
        "id": id,
        "type": "Text",
        "base_type": "Block",
        "title": format!("Block {id}"),
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-02T00:00:00Z",
        "content": {"markdown": format!(
            "Evidence {id}: {}",
            "research context decisions alternatives relationships outcomes ".repeat(12)
        )},
        "connection": {
            "id": id + 100,
            "connected_at": connected_at,
            "pinned": false
        }
    })
}
