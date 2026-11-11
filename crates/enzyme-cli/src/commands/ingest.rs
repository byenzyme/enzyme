use std::io::Read;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use enzyme_core::config::paths::{EnzymePaths, ensure_dir_symlink_aware};
use enzyme_core::db::Database;
use enzyme_core::document::{
    ProcessedDocument, StructuredDocumentInput, structured_document_to_processed_document,
};
use enzyme_core::embedding::{EmbeddingService, get_embedding_service};
use enzyme_core::models::EntityType;
use enzyme_core::pipeline::catalyst_vectors::ensure_catalyst_vectors;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::epoch::now_ms as epoch_now_ms;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct IngestEntry {
    pub id: Option<String>,
    pub title: String,
    pub content: Option<String>,
    pub notes: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    pub collection: Option<String>,
    #[serde(default)]
    pub collections: Vec<String>,
    pub folder: Option<String>,
    pub created_at: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct IngestPayload {
    pub entries: Option<Vec<IngestEntry>>,
    pub entry: Option<IngestEntry>,
}

pub async fn run_ingest(vault_path: &Path, quiet: bool) -> Result<()> {
    let start = Instant::now();
    let paths = EnzymePaths::new(vault_path)?;

    if let Some(parent) = paths.vault_db.parent() {
        ensure_dir_symlink_aware(parent).context("failed to create .enzyme directory")?;
    }

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read from stdin")?;

    let payload: IngestPayload =
        serde_json::from_str(&input).context("failed to parse ingest JSON")?;

    let entries: Vec<IngestEntry> = match (payload.entries, payload.entry) {
        (Some(entries), _) => entries,
        (None, Some(entry)) => vec![entry],
        (None, None) => anyhow::bail!("ingest payload must contain 'entries' or 'entry'"),
    };

    if entries.is_empty() {
        if quiet {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "status": "ok",
                    "documents_ingested": 0,
                    "duration_ms": start.elapsed().as_millis() as u64,
                }))?
            );
        } else {
            eprintln!("No entries to ingest.");
        }
        return Ok(());
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let processed: Vec<ProcessedDocument> = entries
        .into_iter()
        .map(|entry| entry_to_processed_document(entry, now_ms))
        .collect::<Result<Vec<_>>>()?;

    let count = processed.len();
    let db = Database::open(&paths.vault_db)?;
    let _sync_guard = paths.sync_on_drop();
    // Ingest is a document-write surface, so it participates in the same
    // serialized continuous projection as refresh: dirty before the document
    // commit, clean once embeddings are durable.
    let service = get_embedding_service(&paths.home)?;
    let (emb_stats, catalysts_embedded) = project_ingest_batch(&db, &paths, &processed, service)?;
    db.reconcile_corpus_generation(epoch_now_ms())?;
    let contract =
        super::init::generation_contract(vault_path, &paths, super::generator::LlmMode::Auto)
            .await?;
    db.bootstrap_legacy_epoch(&contract, epoch_now_ms(), false)?;
    super::init::recover_abandoned_epoch_worker(&db, &paths)?;
    let schedule = db.schedule_catalyst_epoch(epoch_now_ms(), &contract, false)?;
    let background_spawned = schedule.launch_needed
        && super::init::spawn_background_refresh(
            vault_path,
            &paths,
            super::generator::LlmMode::Auto,
        );
    let epoch_state = db.get_epoch_state()?;
    let epoch_age_ms = epoch_state
        .active_epoch_completed_at_ms
        .map(|completed| epoch_now_ms().saturating_sub(completed));

    let duration_ms = start.elapsed().as_millis() as u64;
    if quiet {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "status": "ok",
                "documents_ingested": count,
                "embedding": {
                    "documents_embedded": emb_stats.documents_embedded,
                    "chunks_embedded": emb_stats.chunks_embedded,
                    "documents_pruned": emb_stats.documents_pruned,
                    "documents_skipped": emb_stats.documents_skipped,
                    "duration_ms": emb_stats.duration_ms,
                    "encode_ms": emb_stats.encode_ms,
                },
                "catalysts_embedded": catalysts_embedded,
                "epoch": {
                    "active_epoch_id": epoch_state.active_epoch_id,
                    "age_ms": epoch_age_ms,
                    "corpus_generation": epoch_state.corpus_generation,
                    "corpus_changed_since_epoch": epoch_state.active_epoch_corpus_generation
                        != Some(epoch_state.corpus_generation),
                    "due": schedule.due,
                    "scheduled": schedule.scheduled,
                    "coalesced": schedule.coalesced,
                },
                "background_spawned": background_spawned,
                "duration_ms": duration_ms,
            }))?
        );
    } else {
        eprintln!(
            "Ingested {} documents and refreshed projection in {}ms",
            count, duration_ms
        );
    }

    Ok(())
}

pub(crate) fn project_ingest_batch(
    db: &Database,
    paths: &EnzymePaths,
    processed: &[ProcessedDocument],
    service: &EmbeddingService,
) -> Result<(enzyme_core::pipeline::embedding::EmbeddingStats, usize)> {
    let _projection_lock = super::lock::acquire_projection_lock(
        paths.vault_db.parent().context("vault DB has no parent")?,
    )?;
    let projection_token = db.begin_continuous_projection()?;
    db.upsert_documents_batch(processed)?;

    let entity_entries = IndexingPipeline::collect_entity_entries(processed);
    let refs: Vec<(&str, &EntityType, &enzyme_core::models::EntityOccurrence)> = entity_entries
        .iter()
        .map(|(name, etype, occurrence)| (name.as_str(), etype, occurrence))
        .collect();
    if let Err(e) = db.upsert_entities_batch(&refs) {
        tracing::warn!("entity extraction failed: {}", e);
    }

    let emb_stats = EmbeddingPipeline::new(db, service).run_all(|_| {})?;
    // Catalyst links are scored at query time; only the catalyst vectors
    // themselves must exist, and they go missing when the index is rebuilt.
    let catalysts_embedded = ensure_catalyst_vectors(db, service)?;
    db.finish_continuous_projection(&projection_token)?;
    Ok((emb_stats, catalysts_embedded))
}

fn entry_to_processed_document(entry: IngestEntry, now_ms: i64) -> Result<ProcessedDocument> {
    let source_ref = entry.id.clone().unwrap_or_else(|| slugify(&entry.title));
    let source_ref = if let Some(ref folder) = entry.folder {
        if folder.is_empty() {
            source_ref
        } else {
            format!("{folder}/{source_ref}")
        }
    } else {
        source_ref
    };

    let mut folders = Vec::new();
    if let Some(ref folder) = entry.folder {
        push_unique_nonempty(&mut folders, folder);
    }
    if let Some(ref collection) = entry.collection {
        push_unique_nonempty(&mut folders, collection);
    }
    for collection in &entry.collections {
        push_unique_nonempty(&mut folders, collection);
    }

    if let Some(ref metadata) = entry.metadata {
        tracing::debug!(
            ?metadata,
            "ingest metadata is accepted but not persisted yet"
        );
    }

    Ok(structured_document_to_processed_document(
        StructuredDocumentInput {
            source_ref,
            title: entry.title,
            content: entry.content,
            notes: entry.notes,
            source: entry.source,
            tags: entry.tags,
            links: entry.links,
            folders,
            created_at: parse_timestamp(&entry.created_at, now_ms),
        },
    ))
}

fn parse_timestamp(value: &Option<serde_json::Value>, default_ms: i64) -> i64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(default_ms),
        Some(serde_json::Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.timestamp_millis())
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|date| {
                    date.and_hms_opt(0, 0, 0)
                        .unwrap()
                        .and_utc()
                        .timestamp_millis()
                })
            })
            .unwrap_or(default_ms),
        _ => default_ms,
    }
}

fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let mut result = String::new();
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash && !result.is_empty() {
                result.push('-');
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }
    result.trim_end_matches('-').to_string()
}

fn push_unique_nonempty(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() || values.iter().any(|existing| existing == trimmed) {
        return;
    }
    values.push(trimmed.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_core::embedding::EmbeddingBackend;
    use enzyme_core::models::Catalyst;

    struct AxisBackend;

    impl EmbeddingBackend for AxisBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn dimension(&self) -> usize {
            2
        }

        fn name(&self) -> &str {
            "ingest-axis-test"
        }
    }

    fn entry(title: &str) -> IngestEntry {
        IngestEntry {
            id: Some("doc-1".to_string()),
            title: title.to_string(),
            content: Some("body".to_string()),
            notes: None,
            source: None,
            tags: Vec::new(),
            links: Vec::new(),
            collection: None,
            collections: Vec::new(),
            folder: None,
            created_at: None,
            metadata: None,
        }
    }

    #[test]
    fn collection_label_becomes_folder_entity_without_changing_source_ref() {
        let mut input = entry("Tagged Recipe");
        input.collection = Some("recipe/main-dishes".to_string());

        let doc = entry_to_processed_document(input, 1_700_000_000_000).unwrap();

        assert_eq!(doc.source_ref, "doc-1");
        assert_eq!(doc.folders, vec!["recipe/main-dishes"]);
    }

    #[test]
    fn multiple_collection_labels_become_deduped_folder_entities() {
        let mut input = entry("Tagged Recipe");
        input.folder = Some("recipes".to_string());
        input.collection = Some("recipe/main-dishes".to_string());
        input.collections = vec![
            "recipe/weeknight".to_string(),
            "recipe/main-dishes".to_string(),
            " ".to_string(),
        ];

        let doc = entry_to_processed_document(input, 1_700_000_000_000).unwrap();

        assert_eq!(
            doc.folders,
            vec!["recipes", "recipe/main-dishes", "recipe/weeknight"]
        );
    }

    #[test]
    fn ingest_embeds_notes_and_restores_missing_catalyst_vectors() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = EnzymePaths::with_overrides(
            temp.path(),
            Some(temp.path().join("home").to_str().unwrap()),
        )
        .unwrap();
        std::fs::create_dir_all(paths.vault_db.parent().unwrap()).unwrap();
        let db = Database::open(&paths.vault_db).unwrap();
        db.insert_catalysts(&[Catalyst {
            id: "cat".into(),
            text: "question".into(),
            entity: "entity".into(),
            context: "default".into(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
        db.upsert_catalyst_embeddings(&[("cat", &[1.0_f32, 0.0][..])])
            .unwrap();
        let service = EmbeddingService::new(Box::new(AxisBackend));
        let doc = entry_to_processed_document(entry("Projected"), 1_700_000_000_000).unwrap();

        // The first embedding run records the backend and clears vectors made
        // in any other space, so the catalyst is re-embedded once...
        let (embedding, catalysts_embedded) =
            project_ingest_batch(&db, &paths, &[doc.clone()], &service).unwrap();
        assert!(embedding.chunks_embedded > 0);
        assert_eq!(catalysts_embedded, 1);
        assert!(!db.continuous_projection_fence().unwrap().dirty);
        // ...and later ingests leave current vectors alone.
        let (_, catalysts_embedded) =
            project_ingest_batch(&db, &paths, &[doc.clone()], &service).unwrap();
        assert_eq!(catalysts_embedded, 0);

        // A rebuilt index keeps catalysts but drops their vectors; the next
        // ingest restores them so search can link catalysts again.
        db.execute_write("DELETE FROM catalyst_embeddings_packed", vec![])
            .unwrap();
        let (_, restored) = project_ingest_batch(&db, &paths, &[doc], &service).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(db.get_all_catalyst_embeddings().unwrap()[0].0, "cat");
    }
}
