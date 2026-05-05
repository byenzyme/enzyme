use std::io::Read;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::document::{DEFAULT_CHUNK_SIZE_CHARS, ProcessedDocument, chunk_document};
use enzyme_core::models::{DateSource, EntityType};
use enzyme_core::pipeline::indexing::IndexingPipeline;
use serde::Deserialize;
use sha2::{Digest, Sha256};

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
        std::fs::create_dir_all(parent).context("failed to create .enzyme directory")?;
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
    db.upsert_documents_batch(&processed)?;

    let entity_entries = IndexingPipeline::collect_entity_entries(&processed);
    let refs: Vec<(&str, &EntityType, &enzyme_core::models::EntityOccurrence)> = entity_entries
        .iter()
        .map(|(name, etype, occurrence)| (name.as_str(), etype, occurrence))
        .collect();
    if let Err(e) = db.upsert_entities_batch(&refs) {
        tracing::warn!("entity extraction failed: {}", e);
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    if quiet {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "status": "ok",
                "documents_ingested": count,
                "duration_ms": duration_ms,
            }))?
        );
    } else {
        eprintln!("Ingested {} documents in {}ms", count, duration_ms);
    }

    Ok(())
}

fn entry_to_processed_document(entry: IngestEntry, now_ms: i64) -> Result<ProcessedDocument> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref content) = entry.content {
        parts.push(content.clone());
    }
    if let Some(ref notes) = entry.notes {
        if !notes.is_empty() {
            parts.push(format!("\n{}", notes));
        }
    }
    if let Some(ref source) = entry.source {
        if !source.is_empty() {
            parts.push(format!("\nSource: {}", source));
        }
    }

    let full_content = if parts.is_empty() {
        entry.title.clone()
    } else {
        format!("# {}\n\n{}", entry.title, parts.join("\n"))
    };

    let source_ref = entry.id.unwrap_or_else(|| slugify(&entry.title));
    let source_ref = if let Some(ref folder) = entry.folder {
        if folder.is_empty() {
            source_ref
        } else {
            format!("{}/{}", folder, source_ref)
        }
    } else {
        source_ref
    };

    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(full_content.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let chunks = chunk_document(&full_content, DEFAULT_CHUNK_SIZE_CHARS);
    let word_count = full_content.split_whitespace().count() as i64;
    let char_count = full_content.chars().count() as i64;
    let created_at = parse_timestamp(&entry.created_at, now_ms);
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

    if let Some(metadata) = entry.metadata {
        tracing::debug!(
            ?metadata,
            "ingest metadata is accepted but not persisted yet"
        );
    }

    Ok(ProcessedDocument {
        source_ref,
        title: Some(entry.title),
        content: full_content,
        content_hash,
        word_count,
        char_count,
        created_at,
        created_source: DateSource::Content,
        modified_at: created_at,
        modified_source: DateSource::Content,
        tags: entry.tags,
        links: entry.links,
        folders,
        chunks,
        frontmatter: None,
    })
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
}
