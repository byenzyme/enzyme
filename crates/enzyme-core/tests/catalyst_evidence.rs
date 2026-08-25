use std::collections::HashMap;

use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient};
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystPipeline, DEFAULT_MIN_EVIDENCE_DOCUMENTS, DEFAULT_MIN_EVIDENCE_TOKENS,
};
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::epoch::{GenerationContract, now_ms};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;
use enzyme_core::search::ContextSearch;
use wiremock::matchers::{method, path};
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
        prompt_version: "catalyst-prompts.2026-08-anchors".into(),
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
        min_evidence_documents: DEFAULT_MIN_EVIDENCE_DOCUMENTS,
        min_evidence_tokens: DEFAULT_MIN_EVIDENCE_TOKENS,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stub_generation_persists_and_returns_anchors_and_drops_ungrounded_claims() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    write_evidence(&vault, DEFAULT_MIN_EVIDENCE_DOCUMENTS, false);
    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let entity = focus_entity(&db);

    let server = MockServer::start().await;
    let content = serde_json::json!({
        "catalysts": [
            "Which concrete migration tradeoff keeps source evidence auditable?",
            "What unnamed needs are accelerating engagement?"
        ],
        "topicNames": ["auditable migration tradeoff", "generic engagement prose"],
        "anchorIds": [["E1"], ["NOT_SUPPLIED"]]
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
        min_evidence_documents: DEFAULT_MIN_EVIDENCE_DOCUMENTS,
        min_evidence_tokens: DEFAULT_MIN_EVIDENCE_TOKENS,
        epoch_id: Some(&epoch_id),
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&[entity], &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 1);
    assert_eq!(stats.hypotheses_rejected, 1);

    let catalyst = db.get_all_catalysts().unwrap().pop().unwrap();
    let anchors = catalyst.metadata["evidence_anchors"].as_array().unwrap();
    assert!(
        !catalyst.metadata["era"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    assert_eq!(
        catalyst.metadata["epoch_id"].as_str(),
        Some(epoch_id.as_str())
    );
    assert_eq!(anchors.len(), 1);
    assert!(
        anchors[0]["source_ref"]
            .as_str()
            .unwrap()
            .contains("evidence-")
    );
    assert!(!anchors[0]["occurrence_ids"].as_array().unwrap().is_empty());
    assert!(anchors[0]["timestamp_ms"].as_i64().is_some());
    assert!(!anchors[0]["quote"].as_str().unwrap().is_empty());

    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    SimilarityPipeline::new(&db, service)
        .run(|_, _| {})
        .unwrap();
    db.activate_inline_catalyst_epoch(&epoch_id, now_ms())
        .unwrap();
    let response = ContextSearch::new(&db, service)
        .catalyze_search("auditable migration tradeoff", 5)
        .unwrap();
    assert!(!response.results[0].evidence_anchors.is_empty());
    assert!(
        !response.top_contributing_catalysts[0]
            .evidence_anchors
            .is_empty()
    );

    let (eras, epochs): (i64, i64) = (
        db.query_read(
            "SELECT COUNT(*) FROM catalyst_eras WHERE epoch_id=?1 AND occurrence_count > 0 AND catalyst_budget > 0",
            vec![libsql::Value::Text(epoch_id.clone())],
            |row| row.get(0),
        )
        .unwrap()[0],
        db.query_read(
            "SELECT COUNT(*) FROM catalyst_epochs WHERE epoch_id=?1 AND status='active'",
            vec![libsql::Value::Text(epoch_id)],
            |row| row.get(0),
        )
        .unwrap()[0],
    );
    assert!(eras > 0);
    assert_eq!(epochs, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn thin_evidence_skips_llm_and_records_reason() {
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
    let reasons = db
        .query_read(
            "SELECT reason_code, reason FROM catalyst_generation_skips",
            vec![],
            |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
        )
        .unwrap();
    assert_eq!(reasons[0].0, "thin_evidence");
    assert!(reasons[0].1.contains("distinct documents"));
}
