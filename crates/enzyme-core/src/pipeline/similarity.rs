use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use tracing::info;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::{Catalyst, CatalystSimilarity, Embedding};

pub struct SimilarityPipeline<'a> {
    db: &'a Database,
    service: &'a EmbeddingService,
}

pub struct SimilarityStats {
    pub catalysts_processed: usize,
    pub similarities_computed: usize,
    pub documents_compared: usize,
    pub duration_ms: u64,
}

impl<'a> SimilarityPipeline<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self { db, service }
    }

    /// Run similarity using catalysts from the local database.
    pub fn run(&self, on_progress: impl Fn(usize, usize)) -> Result<SimilarityStats> {
        let catalysts = self.db.get_all_catalysts()?;
        self.run_with_catalysts(&catalysts, true, on_progress)
    }

    /// Run similarity with externally-provided catalysts (e.g. from a source vault via `apply`).
    ///
    /// When `cache_catalyst_embeddings` is true, catalyst embeddings are saved to the
    /// database for reuse in `catalyze_search`.
    pub fn run_with_catalysts(
        &self,
        catalysts: &[Catalyst],
        cache_catalyst_embeddings: bool,
        on_progress: impl Fn(usize, usize),
    ) -> Result<SimilarityStats> {
        let start = Instant::now();

        if catalysts.is_empty() {
            return Ok(SimilarityStats {
                catalysts_processed: 0,
                similarities_computed: 0,
                documents_compared: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        let catalyst_ids: Vec<String> = catalysts.iter().map(|c| c.id.clone()).collect();
        self.db
            .delete_catalyst_similarities_for_catalysts(&catalyst_ids)?;
        if cache_catalyst_embeddings {
            self.db.prune_catalyst_similarities_except(&catalyst_ids)?;
        }

        // Embed all catalyst texts
        let catalyst_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
        let catalyst_vecs = self.service.embed_documents(&catalyst_texts)?;

        // Cache catalyst embeddings to DB for fast reuse in catalyze_search
        if cache_catalyst_embeddings {
            let entries: Vec<(&str, &[f32])> = catalysts
                .iter()
                .zip(catalyst_vecs.iter())
                .map(|(c, v)| (c.id.as_str(), v.as_slice()))
                .collect();
            self.db.upsert_catalyst_embeddings(&entries)?;
        }

        // Load all chunk embeddings from DB
        let chunk_embeddings = self.db.get_all_chunk_embeddings()?;
        if chunk_embeddings.is_empty() {
            return Ok(SimilarityStats {
                catalysts_processed: 0,
                similarities_computed: 0,
                documents_compared: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        // Count unique documents
        let documents_compared = {
            let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (file_path, _, _) in &chunk_embeddings {
                unique.insert(file_path.as_str());
            }
            unique.len()
        };

        let total = catalysts.len();
        let mut similarities_computed = 0usize;

        // For each catalyst, compute cosine similarity against all chunk embeddings
        for (i, (catalyst, cat_vec)) in catalysts.iter().zip(catalyst_vecs.iter()).enumerate() {
            on_progress(i + 1, total);

            let mut sims: Vec<(f64, &str, u32)> = chunk_embeddings
                .iter()
                .map(|(file_path, chunk_index, vector)| {
                    let sim = Embedding::cosine_similarity(cat_vec, vector) as f64;
                    (sim, file_path.as_str(), *chunk_index)
                })
                .collect();

            // Sort by similarity descending, keep top 50
            sims.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            sims.truncate(50);

            let similarity_records: Vec<CatalystSimilarity> = sims
                .into_iter()
                .map(|(sim, file_path, chunk_index)| CatalystSimilarity {
                    id: None,
                    catalyst_id: catalyst.id.clone(),
                    catalyst_text: catalyst.text.clone(),
                    file_path: file_path.to_string(),
                    chunk_index: chunk_index as i64,
                    similarity: sim,
                    chunk_metadata: serde_json::json!({}),
                    timeline_data: serde_json::json!({}),
                    created_at: Utc::now(),
                })
                .collect();

            similarities_computed += similarity_records.len();
            self.db.upsert_catalyst_similarities(&similarity_records)?;
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        info!(
            catalysts_processed = total,
            similarities_computed, duration_ms, "similarity computation complete"
        );

        Ok(SimilarityStats {
            catalysts_processed: total,
            similarities_computed,
            documents_compared,
            duration_ms,
        })
    }
}
