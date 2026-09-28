use std::collections::HashMap;

use enzyme_core::config::{
    OneOrManyStrings, SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig,
    SqliteTimestampUnit, SqliteWhoRole,
};
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient};
use enzyme_core::models::{catalyst_embedding_text, parse_catalyst_text};
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystPipeline, DEFAULT_MIN_CONTEXT_DOCUMENTS, DEFAULT_MIN_CONTEXT_TOKENS,
};
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::epoch::{GenerationContract, now_ms};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::search::ContextSearch;
use enzyme_core::sqlite_source::run_sqlite_source;
use rusqlite::{Connection, params};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_evidence(vault: &std::path::Path, documents: usize, boilerplate: bool) {
    std::fs::create_dir_all(vault).unwrap();
    for index in 0..documents {
        let body = if boilerplate {
            "Forwarded newsletter unsubscribe view browser advertisement ".repeat(20)
        } else {
            format!(
                "#focus\n\nDocument {index}: {}",
                "The concrete migration decision links source evidence to an auditable product tradeoff. "
                    .repeat(25)
            )
        };
        std::fs::write(vault.join(format!("evidence-{index}.md")), body).unwrap();
    }
}

fn focus_entity(db: &Database) -> enzyme_core::models::Entity {
    db.get_all_entities()
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "focus")
        .expect("focus tag")
}

fn contract(service: &enzyme_core::embedding::EmbeddingService) -> GenerationContract {
    GenerationContract {
        schema_version: "catalyst-epoch.v2".into(),
        prompt_version: "catalyst-prompts.2026-08-context-sources".into(),
        catalyst_format: DEFAULT_FORMAT_KEY.into(),
        catalyst_profile: DEFAULT_PROFILE_KEY.into(),
        model: "fixture-model".into(),
        llm_mode: "embedder-pull".into(),
        llm_provider: "openai-compatible-stub".into(),
        llm_base_url: "fixture".into(),
        allocation_semantics: "volume-floor-and-top-boost.v1".into(),
        catalysts_per_entity: 1,
        min_top_catalysts: None,
        embedding_backend: service.backend_name().into(),
        embedding_dimension: service.dimension(),
        targets: vec![],
        selection_config_json: "{}".into(),
        max_context_chars: None,
        max_context_samples: 20,
        min_context_documents: DEFAULT_MIN_CONTEXT_DOCUMENTS,
        min_context_tokens: DEFAULT_MIN_CONTEXT_TOKENS,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stub_generation_persists_engine_selected_source_refs() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    write_evidence(&vault, DEFAULT_MIN_CONTEXT_DOCUMENTS, false);
    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let entity = focus_entity(&db);

    let server = MockServer::start().await;
    let content = serde_json::json!({
        "catalysts": [
            "Which concrete migration tradeoff keeps source evidence auditable?"
        ],
        "topicNames": ["auditable migration tradeoff"]
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;

    let service = get_embedding_service(&vault).unwrap();
    let epoch_id = db
        .begin_inline_catalyst_epoch(&contract(service), now_ms())
        .unwrap();
    let llm = LlmClient::with_base_url("fixture-key", &server.uri());
    let profiles = HashMap::new();
    let config = CatalystConfig {
        entity_profiles: &profiles,
        catalysts_per_entity: 1,
        model: "fixture-model",
        min_context_documents: DEFAULT_MIN_CONTEXT_DOCUMENTS,
        min_context_tokens: DEFAULT_MIN_CONTEXT_TOKENS,
        epoch_id: Some(&epoch_id),
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&[entity], &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 1);

    let catalyst = db.get_all_catalysts().unwrap().pop().unwrap();
    let canonical = parse_catalyst_text(&catalyst.text).unwrap();
    let source_refs = &canonical.source_refs;
    assert!(
        catalyst
            .metadata
            .get("era")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|era| !era.is_empty())
    );
    assert_eq!(source_refs.len(), 1);
    assert!(source_refs[0].contains("evidence-"));
    assert!(catalyst.text.contains("```enzyme-context\n"));
    assert!(!catalyst.text.contains("anchor_id"));
    assert!(!catalyst.text.contains("occurrence_ids"));
    assert!(!catalyst.text.contains("timestamp_ms"));
    assert!(!catalyst_embedding_text(&catalyst.text).contains(&source_refs[0]));

    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    SimilarityPipeline::new(&db, service)
        .run(|_, _| {})
        .unwrap();
    db.activate_inline_catalyst_epoch(&epoch_id, now_ms())
        .unwrap();
    let response = ContextSearch::new(&db, service)
        .catalyze_search("auditable migration tradeoff", 5)
        .unwrap();
    assert!(
        response.top_contributing_catalysts[0]
            .text
            .contains("```enzyme-context\n")
    );
    assert!(
        response.results[0]
            .via_catalyst_text
            .as_deref()
            .unwrap()
            .contains("```enzyme-context\n")
    );

    let epochs: i64 = db
        .query_read(
            "SELECT COUNT(*) FROM catalyst_epochs WHERE epoch_id=?1 AND status='active'",
            vec![libsql::Value::Text(epoch_id)],
            |row| row.get(0),
        )
        .unwrap()[0];
    assert_eq!(epochs, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_catalyst_carries_the_engine_selected_source_refs() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    let projects = vault.join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    for day in 1..=3 {
        std::fs::write(
            projects.join(format!("2024-01-0{day}-garden-irrigation.md")),
            "---\ntags: [focus]\n---\n\nThe garden irrigation manifold balances drip pressure across the vegetable beds. A calibrated valve controls the watering cycle. Related: [[heliograph-antenna]].",
        )
        .unwrap();
        std::fs::write(
            projects.join(format!("2025-01-0{day}-heliograph-antenna.md")),
            "---\ntags: [focus]\n---\n\nThe heliograph antenna mounts to the chimney mast. Signal telemetry determines the antenna calibration angle. Related: [[garden-irrigation]].",
        )
        .unwrap();
    }

    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let entity = focus_entity(&db);

    let server = MockServer::start().await;
    let content = serde_json::json!({
        "catalysts": [
            "How does the heliograph antenna's chimney mast constrain signal telemetry?",
            "Which garden irrigation manifold tradeoff protects drip pressure?"
        ],
        "topicNames": ["antenna signal telemetry", "irrigation pressure tradeoff"],
        "eraLabels": ["2024-Q1", "2025-Q1"]
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;

    let llm = LlmClient::with_base_url("fixture-key", &server.uri());
    let profiles = HashMap::new();
    let config = CatalystConfig {
        entity_profiles: &profiles,
        catalysts_per_entity: 2,
        model: "fixture-model",
        max_samples: 2,
        min_context_documents: 1,
        min_context_tokens: 1,
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&[entity], &config, |_| {})
        .await
        .unwrap();

    assert_eq!(stats.catalysts_generated, 2);
    let persisted = db
        .get_all_catalysts()
        .unwrap()
        .into_iter()
        .map(|catalyst| parse_catalyst_text(&catalyst.text).unwrap())
        .collect::<Vec<_>>();
    let antenna = persisted
        .iter()
        .find(|catalyst| catalyst.hypothesis.contains("heliograph antenna"))
        .unwrap();
    assert_eq!(antenna.source_refs.len(), 2);
    assert!(
        antenna
            .source_refs
            .iter()
            .any(|source| source.contains("heliograph-antenna"))
    );
    assert!(
        antenna
            .source_refs
            .iter()
            .any(|source| source.contains("garden-irrigation"))
    );

    let irrigation = persisted
        .iter()
        .find(|catalyst| catalyst.hypothesis.contains("garden irrigation"))
        .unwrap();
    assert_eq!(irrigation.source_refs, antenna.source_refs);
}

#[tokio::test(flavor = "multi_thread")]
async fn thin_context_skips_llm() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    write_evidence(&vault, 3, true);
    // Add the selected entity without adding meaningful evidence.
    for index in 0..3 {
        let path = vault.join(format!("evidence-{index}.md"));
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("\n#focus\n");
        std::fs::write(path, body).unwrap();
    }
    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let entity = focus_entity(&db);
    let server = MockServer::start().await;
    let llm = LlmClient::with_base_url("fixture-key", &server.uri());
    let profiles = HashMap::new();
    let config = CatalystConfig {
        entity_profiles: &profiles,
        catalysts_per_entity: 1,
        model: "fixture-model",
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&[entity], &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 0);
    assert_eq!(stats.entities_below_threshold, 1);
    assert!(db.get_all_catalysts().unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn whole_context_sufficiency_does_not_filter_individual_eras() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    for index in 0..3 {
        std::fs::write(
            vault.join(format!("eligible-{index}.md")),
            format!(
                "---\ntags: [focus]\ndate: 2025-01-0{}\n---\n\nELIGIBLE_ERA_MARKER {}",
                index + 1,
                "The migration decision preserves grounded evidence while recording a concrete product tradeoff for later review. "
                    .repeat(8)
            ),
        )
        .unwrap();
        std::fs::write(
            vault.join(format!("thin-{index}.md")),
            format!(
                "---\ntags: [focus]\ndate: 2026-01-0{}\n---\n\nTHIN_ERA_MARKER {}",
                index + 1,
                "Forwarded newsletter unsubscribe view browser advertisement ".repeat(12)
            ),
        )
        .unwrap();
    }

    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let entity = focus_entity(&db);
    assert_eq!(entity.trends.timeline_buckets.len(), 2);

    let server = MockServer::start().await;
    let content = serde_json::json!({
        "catalysts": ["Which migration tradeoff keeps the recorded evidence available for later review?"],
        "topicNames": ["grounded migration tradeoff"],
        "eraLabels": ["2026-Q1"]
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;

    let llm = LlmClient::with_base_url("fixture-key", &server.uri());
    let profiles = HashMap::new();
    let config = CatalystConfig {
        entity_profiles: &profiles,
        catalysts_per_entity: 2,
        model: "fixture-model",
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&[entity], &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 1);
    assert_eq!(stats.entities_below_threshold, 0);
    assert!(stats.suppressed_entities.is_empty());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8_lossy(&requests[0].body);
    assert!(request.contains("ELIGIBLE_ERA_MARKER"));
    assert!(request.contains("THIN_ERA_MARKER"));
    assert!(request.contains("Generate 2 catalyst"));

    let catalyst = db.get_all_catalysts().unwrap().pop().unwrap();
    let canonical = parse_catalyst_text(&catalyst.text).unwrap();
    assert!(catalyst.metadata.get("era").and_then(|era| era.as_str()) == Some("2026-Q1"));
    assert!(
        canonical
            .source_refs
            .iter()
            .any(|source| source.contains("eligible-"))
    );
    assert!(
        canonical
            .source_refs
            .iter()
            .any(|source| source.contains("thin-"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_document_link_entities_each_see_full_conversation() {
    enzyme_core::initialize_sqlite_runtime().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("mail.db");
    let source = Connection::open(&source_path).unwrap();
    source
        .execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                sender TEXT NOT NULL,
                sent_at INTEGER NOT NULL,
                body TEXT NOT NULL,
                thread TEXT NOT NULL
             );",
        )
        .unwrap();
    let participants = [
        ("Alice", "ALICE_PRIVATE_SPAN"),
        ("Bob", "BOB_PRIVATE_SPAN"),
        ("Carol", "CAROL_PRIVATE_SPAN"),
    ];
    for (index, (name, marker)) in participants.iter().enumerate() {
        source
            .execute(
                "INSERT INTO messages (id, sender, sent_at, body, thread)
                 VALUES (?1, ?2, ?3, ?4, 'shared-thread')",
                params![
                    index as i64 + 1,
                    name,
                    1_775_000_000_000i64 + index as i64 * 1_000,
                    format!(
                        "{marker}: {name} proposes a concrete migration boundary and records the evidence needed to audit it."
                    ),
                ],
            )
            .unwrap();
    }
    drop(source);

    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    let source_config = SqliteSourceConfig {
        db: source_path,
        query: "SELECT id, sender, sent_at, body, thread FROM messages".to_string(),
        roles: SqliteSourceRoles {
            id: vec!["id".to_string()],
            document_ref: None,
            who: SqliteWhoRole::Columns(OneOrManyStrings::Many(vec!["sender".to_string()])),
            when: "sent_at".to_string(),
            what: vec!["body".to_string()],
            r#where: vec!["thread".to_string()],
            weight: None,
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
        filter: None,
    };
    run_sqlite_source(&db, "mail", &source_config).unwrap();

    let entities = db
        .get_all_entities()
        .unwrap()
        .into_iter()
        .filter(|entity| ["Alice", "Bob"].contains(&entity.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(entities.len(), 2);

    let server = MockServer::start().await;
    for (name, _) in [participants[0], participants[1]] {
        let response = serde_json::json!({
            "catalysts": [
                format!("What does the shared migration boundary leave unresolved for {name}?")
            ],
            "topicNames": ["shared migration boundary"]
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains(format!("Focus entity: {name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": response.to_string()}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
    }

    let llm = LlmClient::with_base_url("fixture-key", &server.uri());
    let profiles = HashMap::new();
    let config = CatalystConfig {
        entity_profiles: &profiles,
        catalysts_per_entity: 1,
        model: "fixture-model",
        min_context_documents: 1,
        min_context_tokens: 1,
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&entities, &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 2);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let all_markers = [
        "ALICE_PRIVATE_SPAN",
        "BOB_PRIVATE_SPAN",
        "CAROL_PRIVATE_SPAN",
    ];
    for (name, _) in [participants[0], participants[1]] {
        let request = requests
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body))
            .find(|body| body.contains(&format!("Focus entity: {name}")))
            .expect("request for link entity");
        for marker in all_markers {
            assert!(
                request.contains(marker),
                "{name} prompt should include the full shared conversation via document/chunk association"
            );
        }
    }

    let catalysts = db.get_all_catalysts().unwrap();
    assert_eq!(catalysts.len(), 2);
    for catalyst in &catalysts {
        let canonical = parse_catalyst_text(&catalyst.text).unwrap();
        assert!(
            canonical
                .source_refs
                .iter()
                .all(|source_ref| source_ref.starts_with("sqlite:mail/"))
        );
    }

    let service = get_embedding_service(temp.path()).unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    SimilarityPipeline::new(&db, service)
        .run(|_, _| {})
        .unwrap();
    for (name, _) in [participants[0], participants[1]] {
        let response = ContextSearch::new(&db, service)
            .catalyze_search(&format!("What should I recall about {name}?"), 8)
            .unwrap();
        assert_eq!(response.top_contributing_catalysts[0].entity, name);
        let routed_id = response.results[0].via_catalyst_id.as_deref().unwrap();
        assert_eq!(
            catalysts
                .iter()
                .find(|catalyst| catalyst.id == routed_id)
                .unwrap()
                .entity,
            name
        );
    }
}
