//! Catalyst vectors: one embedding per catalyst question, stored packed in
//! `catalyst_embeddings_packed`. Search scores catalysts against notes from
//! these vectors on demand (see [`crate::search::catalyst_links`]); nothing
//! else about catalyst-to-note links is stored.

use std::collections::HashSet;

use anyhow::Result;
use tracing::info;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::{Catalyst, catalyst_embedding_text};

/// Embed `catalysts` and replace the stored catalyst vectors with them.
/// Returns how many vectors were stored.
///
/// Only the question prose is embedded: the source references rendered into a
/// catalyst's text stay with the catalyst but do not move its vector.
pub fn embed_catalysts(
    db: &Database,
    service: &EmbeddingService,
    catalysts: &[Catalyst],
) -> Result<usize> {
    if catalysts.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = catalysts
        .iter()
        .map(|c| catalyst_embedding_text(&c.text))
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let vectors = service.embed_documents(&refs)?;
    let entries: Vec<(&str, &[f32])> = catalysts
        .iter()
        .zip(vectors.iter())
        .map(|(c, v)| (c.id.as_str(), v.as_slice()))
        .collect();
    db.upsert_catalyst_embeddings(&entries)?;
    info!(catalysts = entries.len(), "catalyst vectors stored");
    Ok(entries.len())
}

/// Embed every catalyst in `db` unless the stored vectors already cover
/// exactly the current catalyst set. Returns how many vectors were stored
/// (zero when they were already current).
///
/// Stored vectors go missing when the index is rebuilt or the embedding
/// backend changes; both keep catalysts but drop their vectors.
pub fn ensure_catalyst_vectors(db: &Database, service: &EmbeddingService) -> Result<usize> {
    let catalysts = db.get_all_catalysts()?;
    let current: HashSet<&str> = catalysts.iter().map(|c| c.id.as_str()).collect();
    let stored = db.get_all_catalyst_embeddings()?;
    let stored_ids: HashSet<&str> = stored.iter().map(|(id, _)| id.as_str()).collect();
    if current == stored_ids {
        return Ok(0);
    }
    embed_catalysts(db, service, &catalysts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingBackend;
    use crate::models::render_catalyst_text;
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    struct RecordingBackend(Arc<Mutex<Vec<String>>>);

    impl EmbeddingBackend for RecordingBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.0
                .lock()
                .unwrap()
                .extend(texts.iter().map(|text| (*text).to_string()));
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn dimension(&self) -> usize {
            2
        }

        fn name(&self) -> &str {
            "recording-test"
        }
    }

    fn catalyst(id: &str, text: &str) -> Catalyst {
        Catalyst {
            id: id.to_string(),
            text: text.to_string(),
            entity: String::new(),
            context: String::new(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    fn recording() -> (EmbeddingService, Arc<Mutex<Vec<String>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        (
            EmbeddingService::new(Box::new(RecordingBackend(recorded.clone()))),
            recorded,
        )
    }

    #[test]
    fn catalyst_embedding_excludes_source_refs() {
        let db = Database::open_in_memory().unwrap();
        let (service, recorded) = recording();
        let text =
            render_catalyst_text("Hypothesis prose.", &["SECRET_SOURCE_REF".into()]).unwrap();
        assert_eq!(
            embed_catalysts(&db, &service, &[catalyst("canonical", &text)]).unwrap(),
            1
        );
        assert_eq!(recorded.lock().unwrap().as_slice(), ["Hypothesis prose."]);
        assert_eq!(db.get_all_catalyst_embeddings().unwrap()[0].0, "canonical");
    }

    #[test]
    fn ensure_embeds_only_when_stored_vectors_do_not_match_the_catalysts() {
        let db = Database::open_in_memory().unwrap();
        let (service, recorded) = recording();
        assert_eq!(ensure_catalyst_vectors(&db, &service).unwrap(), 0);

        db.insert_catalysts(&[catalyst("a", "First?"), catalyst("b", "Second?")])
            .unwrap();
        assert_eq!(ensure_catalyst_vectors(&db, &service).unwrap(), 2);
        assert_eq!(ensure_catalyst_vectors(&db, &service).unwrap(), 0);
        assert_eq!(recorded.lock().unwrap().len(), 2);

        // A rebuilt index or a new embedding backend drops the vectors but
        // keeps the catalysts; the next call restores them.
        db.execute_write("DELETE FROM catalyst_embeddings_packed", vec![])
            .unwrap();
        assert_eq!(ensure_catalyst_vectors(&db, &service).unwrap(), 2);

        // A changed catalyst set re-embeds the whole set.
        db.execute_write("DELETE FROM catalysts", vec![]).unwrap();
        db.insert_catalysts(&[catalyst("c", "Third?")]).unwrap();
        assert_eq!(ensure_catalyst_vectors(&db, &service).unwrap(), 1);
        let stored: Vec<String> = db
            .get_all_catalyst_embeddings()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(stored, vec!["c"]);
    }
}
