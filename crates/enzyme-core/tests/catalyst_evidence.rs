use std::collections::HashMap;

use enzyme_core::config::{
    SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit,
};
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient};
use enzyme_core::models::{catalyst_embedding_text, parse_catalyst_text};
use enzyme_core::pipeline::catalysts::{
    CatalystConfig, CatalystPipeline, DEFAULT_MIN_EVIDENCE_DOCUMENTS, DEFAULT_MIN_EVIDENCE_TOKENS,
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
        prompt_version: "catalyst-prompts.2026-08-canonical-receipts".into(),
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
    let canonical = parse_catalyst_text(&catalyst.text).unwrap();
    let anchors = &canonical.receipts;
    assert!(!canonical.header.era.is_empty());
    assert_eq!(
        canonical.header.epoch_id.as_deref(),
        Some(epoch_id.as_str())
    );
    assert_eq!(anchors.len(), 1);
    assert!(anchors[0].source_ref.contains("evidence-"));
    assert!(!anchors[0].occurrence_ids.is_empty());
    assert!(!anchors[0].quote.is_empty());
    assert!(catalyst.text.contains("```receipts\n"));
    assert!(!catalyst_embedding_text(&catalyst.text).contains(&anchors[0].source_ref));

    let derived_snapshot = || {
        let receipts = db
            .query_read(
                "SELECT catalyst_id,anchor_id,occurrence_ordinal,source_ref,occurrence_id,
                        timestamp_ms,chunk_index,quote FROM catalyst_receipts
                 ORDER BY catalyst_id,anchor_id,occurrence_ordinal",
                vec![],
                |row| {
                    Ok((
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<i64>(2)?,
                        row.get::<String>(3)?,
                        row.get::<Option<i64>>(4)?,
                        row.get::<i64>(5)?,
                        row.get::<Option<i64>>(6)?,
                        row.get::<String>(7)?,
                    ))
                },
            )
            .unwrap();
        let eras = db
            .query_read(
                "SELECT generation_run_id,epoch_id,entity,entity_type,era_label,start_month,
                        end_month,distinct_documents,substantive_tokens,occurrence_count,
                        catalyst_budget,status,reason,created_at_ms FROM catalyst_eras
                 ORDER BY generation_run_id,entity,entity_type,era_label",
                vec![],
                |row| {
                    Ok((0..14)
                        .map(|index| format!("{:?}", row.get_value(index).unwrap()))
                        .collect::<Vec<_>>())
                },
            )
            .unwrap();
        (receipts, eras)
    };
    let before_rebuild = derived_snapshot();
    db.execute_write("DELETE FROM catalyst_receipts", vec![])
        .unwrap();
    db.execute_write("DELETE FROM catalyst_eras", vec![])
        .unwrap();
    let rebuilt = db.rebuild_catalyst_derived_indexes().unwrap();
    assert!(rebuilt.0 > 0 && rebuilt.1 > 0);
    assert_eq!(derived_snapshot(), before_rebuild);

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
    assert!(
        response.top_contributing_catalysts[0]
            .text
            .contains("```receipts\n")
    );
    assert!(
        response.results[0]
            .via_catalyst_text
            .as_deref()
            .unwrap()
            .contains("```receipts\n")
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

#[tokio::test(flavor = "multi_thread")]
async fn shared_thread_generation_and_person_retrieval_stay_participant_scoped() {
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
        ("Alice", "ALICE_PRIVATE_SPAN", "Bob"),
        ("Bob", "BOB_PRIVATE_SPAN", "Carol"),
        ("Carol", "CAROL_PRIVATE_SPAN", "Alice"),
    ];
    for (index, (name, marker, _)) in participants.iter().enumerate() {
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
            who: vec!["sender".to_string()],
            when: "sent_at".to_string(),
            what: vec!["body".to_string()],
            r#where: vec!["thread".to_string()],
        },
        timestamp: SqliteTimestampConfig {
            unit: SqliteTimestampUnit::Ms,
            epoch: None,
        },
    };
    run_sqlite_source(&db, "mail", &source_config).unwrap();

    let entities = db
        .get_all_entities()
        .unwrap()
        .into_iter()
        .filter(|entity| participants.iter().any(|(name, _, _)| entity.name == *name))
        .collect::<Vec<_>>();
    assert_eq!(entities.len(), 3);

    let server = MockServer::start().await;
    for (name, _, wrong_subject) in participants {
        let response = serde_json::json!({
            "catalysts": [
                format!("What does {name}'s concrete migration boundary leave unresolved?"),
                format!("What does {wrong_subject}'s role tell {name} about the migration?")
            ],
            "topicNames": ["owned migration boundary", "wrong participant"],
            "anchorIds": [["E1"], ["E1"]]
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
        min_evidence_documents: 1,
        min_evidence_tokens: 1,
        ..Default::default()
    };
    let stats = CatalystPipeline::new(&db, &llm, None)
        .run(&entities, &config, |_| {})
        .await
        .unwrap();
    assert_eq!(stats.catalysts_generated, 3);
    assert_eq!(stats.hypotheses_rejected, 3);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for (name, own_marker, _) in participants {
        let request = requests
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body))
            .find(|body| body.contains(&format!("Focus entity: {name}")))
            .expect("request for participant");
        assert!(request.contains(own_marker));
        for (_, other_marker, _) in participants {
            if other_marker != own_marker {
                assert!(!request.contains(other_marker));
            }
        }
    }

    let catalysts = db.get_all_catalysts().unwrap();
    assert_eq!(catalysts.len(), 3);
    for catalyst in &catalysts {
        assert!(catalyst.text.contains(&catalyst.entity));
        let canonical = parse_catalyst_text(&catalyst.text).unwrap();
        assert!(canonical.header.participant_scoped);
        assert!(
            canonical
                .receipts
                .iter()
                .all(|receipt| receipt.source_ref.starts_with("sqlite:mail/"))
        );
    }

    let service = get_embedding_service(temp.path()).unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    SimilarityPipeline::new(&db, service)
        .run(|_, _| {})
        .unwrap();
    for (name, _, _) in participants {
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
