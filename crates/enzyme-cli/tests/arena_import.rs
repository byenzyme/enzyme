use std::process::Command;

use enzyme_core::db::Database;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

fn block(id: i64) -> Value {
    json!({
        "id": id, "type": "Text", "base_type": "Block", "title": format!("Block {id}"),
        "created_at": "2024-01-01T00:00:00Z", "updated_at": "2024-01-02T00:00:00Z",
        "content": {"markdown": "Research context decisions alternatives relationships outcomes"},
        "connection": {"id": id + 100, "connected_at": "2025-02-03T04:05:06Z", "pinned": false}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_arena_program_indexes_through_init_and_refresh() {
    let server = MockServer::start().await;
    let channel =
        json!({"id": 9, "slug": "research", "title": "Research", "counts": {"contents": 2}});
    Mock::given(method("GET"))
        .and(path("/v3/channels/research"))
        .respond_with(ResponseTemplate::new(200).set_body_json(channel.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/channels/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(channel))
        .expect(2)
        .mount(&server)
        .await;
    let mut image = block(77);
    image["type"] = json!("Image");
    image["content"] = Value::Null;
    Mock::given(method("GET"))
        .and(path("/v3/channels/9/contents"))
        .and(query_param("page", "1"))
        .and(query_param("per", "24"))
        .and(query_param("sort", "created_at_desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "meta": {"total_count": 2}, "data": [image, block(42)]
        })))
        .expect(2)
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[update]\nauto = false\n").unwrap();
    // Serve the hosted LLM lease from the same mock so init and refresh never
    // reach api.enzyme.garden; the lease points at an unroutable LLM.
    Mock::given(method("GET"))
        .and(path("/llm/free-config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_key": "enz-free-test",
            "base_url": "http://127.0.0.1:1/v1",
            "model": "test-model",
        })))
        .mount(&server)
        .await;
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_enzyme"));
        command
            .args(["--collection", "arena-test"])
            .env("ENZYME_HOME", &home)
            .env("ENZYME_API_URL", server.uri())
            .env(
                "ENZYME_FREE_CONFIG_URL",
                format!("{}/llm/free-config", server.uri()),
            )
            .env("ENZYME_ARENA_API_BASE", format!("{}/v3/", server.uri()))
            .env_remove("ARENA_ACCESS_TOKEN");
        command
    };
    let compiled = command()
        .args([
            "compile",
            "--preset",
            "arena",
            "research",
            "--arena-api-base",
        ])
        .arg(format!("{}/v3/", server.uri()))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let program_path = home.join("configs/arena-arena-test.enzyme");
    assert!(program_path.exists());
    assert!(!home.join("workspaces/arena-test/enzyme.db").exists());

    let indexed = command()
        .args(["init", "--stage", "index"])
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let db_path = home.join("workspaces/arena-test/enzyme.db");
    let db = Database::open(&db_path).unwrap();
    assert_eq!(db.get_all_source_refs().unwrap(), vec!["arena:block:42"]);
    drop(db);
    let receipt_path = home.join("workspaces/arena-test/arena-coverage.json");
    let receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["channels"][0]["excluded_images"], 1);
    assert_eq!(receipt["channels"][0]["indexed_documents"], 1);

    let edited = std::fs::read_to_string(&program_path)
        .unwrap()
        .replace("sample by recency", "sample by time");
    std::fs::write(&program_path, edited).unwrap();
    let refreshed = command().args(["refresh", "--quiet"]).output().unwrap();
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["catalyst_evidence"]["policy"], "sample by time");
    assert!(
        std::fs::read_to_string(&program_path)
            .unwrap()
            .contains("sample by time")
    );
    assert_eq!(
        Database::open(&db_path)
            .unwrap()
            .get_all_source_refs()
            .unwrap(),
        vec!["arena:block:42"]
    );
}

#[test]
fn standalone_arena_command_is_removed() {
    let output = Command::new(env!("CARGO_BIN_EXE_enzyme"))
        .args(["arena", "research"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}
