use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::Embedding;

pub const TARGET_PROJECTION_NOT_READY: &str = "target projection is not ready for active epoch";

/// Structured response from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResponse {
    pub results: Vec<CatalyzeResultItem>,
    pub top_contributing_catalysts: Vec<TopCatalyst>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingBackend;
    use crate::models::Catalyst;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ActivatingBackend {
        db_path: PathBuf,
        calls: Arc<AtomicUsize>,
        change_epoch: bool,
    }

    impl EmbeddingBackend for ActivatingBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let db = Database::open(&self.db_path)?;
                let token = db.begin_continuous_projection()?;
                if self.change_epoch {
                    db.execute_write(
                        "UPDATE catalyst_epoch_state SET active_epoch_id='new-epoch' WHERE singleton=1",
                        vec![],
                    )?;
                }
                db.finish_continuous_projection(&token)?;
            }
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn dimension(&self) -> usize {
            2
        }

        fn name(&self) -> &str {
            "activating-test"
        }
    }

    #[test]
    fn catalyst_search_retries_if_epoch_activates_between_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("search.db");
        let db = Database::open(&db_path).unwrap();
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
        db.execute_write(
            "UPDATE catalyst_epoch_state SET active_epoch_id='old-epoch' WHERE singleton=1",
            vec![],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: calls.clone(),
            change_epoch: true,
        }));

        ContextSearch::new(&db, &service)
            .catalyze_search("query", 5)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn catalyst_search_retries_if_same_epoch_projection_changes_between_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("search.db");
        let db = Database::open(&db_path).unwrap();
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
        db.execute_write(
            "UPDATE catalyst_epoch_state SET active_epoch_id='same-epoch' WHERE singleton=1",
            vec![],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: calls.clone(),
            change_epoch: false,
        }));

        ContextSearch::new(&db, &service)
            .catalyze_search("query", 5)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn target_search_reports_missing_active_epoch_projection_as_retriable() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("source.db");
        let source = Database::open(&db_path).unwrap();
        let target = Database::open(temp.path().join("target.db")).unwrap();
        source
            .insert_catalysts(&[Catalyst {
                id: "cat".into(),
                text: "question".into(),
                entity: "entity".into(),
                context: "default".into(),
                generated_at: chrono::Utc::now(),
                metadata: serde_json::json!({}),
            }])
            .unwrap();
        source
            .execute_write(
                "UPDATE catalyst_epoch_state SET active_epoch_id='new-epoch' WHERE singleton=1",
                vec![],
            )
            .unwrap();
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: Arc::new(AtomicUsize::new(1)),
            change_epoch: false,
        }));

        let error = ContextSearch::with_target(&source, &target, &service)
            .catalyze_search("query", 5)
            .unwrap_err();
        assert!(error.to_string().contains(TARGET_PROJECTION_NOT_READY));
    }
}

/// A single result from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResultItem {
    pub file_path: String,
    pub content: String,
    pub similarity: f64,
    /// Catalyst bridge that contributed the strongest score for this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_catalyst_id: Option<String>,
}

/// A catalyst that contributed to search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopCatalyst {
    pub id: String,
    pub text: String,
    pub entity: String,
    pub topic_name: Option<String>,
    pub relevance_score: f64,
    pub contribution_count: usize,
}

/// A single result from direct (non-catalyst) search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectSearchResult {
    pub file_path: String,
    pub chunk_index: i64,
    pub content: String,
    pub similarity: f64,
}

/// Catalyst-based search over pre-computed similarities.
///
/// Supports an optional **target** database for split-DB mode (`enzyme catalyze --target`):
/// - Catalysts come from the **source** DB
/// - Catalyst embeddings, similarities, and chunk content come from the **target** DB
pub struct ContextSearch<'a> {
    /// Source vault: catalysts live here.
    db: &'a Database,
    /// Target DB for embeddings/similarities/content. Falls back to `db` when `None`.
    target_db: Option<&'a Database>,
    service: &'a EmbeddingService,
}

impl<'a> ContextSearch<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self {
            db,
            target_db: None,
            service,
        }
    }

    /// Create a split-DB search: catalysts from `source_db`, everything else from `target_db`.
    pub fn with_target(
        source_db: &'a Database,
        target_db: &'a Database,
        service: &'a EmbeddingService,
    ) -> Self {
        Self {
            db: source_db,
            target_db: Some(target_db),
            service,
        }
    }

    /// Catalyst-based search (used by `enzyme catalyze`).
    ///
    /// 1. Embed query
    /// 2. Find top relevant catalysts (from cached embeddings)
    /// 3. For each relevant catalyst, get pre-computed chunk similarities
    /// 4. Aggregate, boost by multi-catalyst and recency, deduplicate, sort
    /// 5. Compute top contributing catalysts with contribution counts
    pub fn catalyze_search(&self, query: &str, limit: usize) -> Result<CatalyzeResponse> {
        // Source activation spans several materialized tables. Read an epoch
        // fence before and after the autocommit reads; if activation landed in
        // between, discard the mixed attempt and retry against the new epoch.
        // If activation lands after the final fence, this is a valid old-epoch
        // snapshot (the read logically preceded activation).
        for _ in 0..4 {
            let source_projection_fence = self.db.continuous_projection_fence()?;
            let result_db = self.target_db.unwrap_or(self.db);
            let result_projection_fence = result_db.continuous_projection_fence()?;
            if source_projection_fence.dirty || result_projection_fence.dirty {
                std::thread::yield_now();
                continue;
            }
            let source_epoch = self.db.get_epoch_state()?.active_epoch_id;
            let response = self.catalyze_search_at_epoch(query, limit, source_epoch.as_deref())?;
            if self.db.get_epoch_state()?.active_epoch_id == source_epoch
                && self.db.continuous_projection_fence()? == source_projection_fence
                && result_db.continuous_projection_fence()? == result_projection_fence
            {
                return Ok(response);
            }
        }
        anyhow::bail!(
            "catalyst epoch or document projection changed during search; retry the query"
        )
    }

    fn catalyze_search_at_epoch(
        &self,
        query: &str,
        limit: usize,
        source_epoch: Option<&str>,
    ) -> Result<CatalyzeResponse> {
        let query_vec = self.service.embed_query(query)?;

        // Get all catalysts
        let catalysts = self.db.get_all_catalysts()?;
        if catalysts.is_empty() {
            return Ok(CatalyzeResponse {
                results: Vec::new(),
                top_contributing_catalysts: Vec::new(),
            });
        }

        // Load cached catalyst embeddings from DB, falling back to re-embedding
        let emb_db = self.target_db.unwrap_or(self.db);
        let cached = if self.target_db.is_some() {
            let epoch_id = source_epoch
                .ok_or_else(|| anyhow::anyhow!("source has no active catalyst epoch"))?;
            let cached = emb_db.get_epoch_catalyst_embeddings(epoch_id)?;
            anyhow::ensure!(
                !cached.is_empty(),
                "{TARGET_PROJECTION_NOT_READY}: {epoch_id}"
            );
            cached
        } else {
            emb_db.get_all_catalyst_embeddings()?
        };
        let cat_embeddings = if !cached.is_empty() {
            let cache_map: HashMap<&str, &Vec<f32>> =
                cached.iter().map(|(id, vec)| (id.as_str(), vec)).collect();

            let mut vecs = Vec::with_capacity(catalysts.len());
            let mut all_hit = true;
            for cat in &catalysts {
                if let Some(vec) = cache_map.get(cat.id.as_str()) {
                    vecs.push((*vec).clone());
                } else {
                    all_hit = false;
                    break;
                }
            }

            if all_hit {
                debug!("using cached catalyst embeddings ({} vectors)", vecs.len());
                vecs
            } else {
                debug!(
                    "catalyst cache stale, re-embedding {} catalysts",
                    catalysts.len()
                );
                let cat_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
                self.service.embed_documents(&cat_texts)?
            }
        } else {
            debug!("no catalyst cache, embedding {} catalysts", catalysts.len());
            let cat_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
            self.service.embed_documents(&cat_texts)?
        };

        // Find top relevant catalysts (top 10 — over-retrieve for better coverage)
        let max_catalysts = 10usize;
        let mut cat_sims: Vec<(usize, f32)> = cat_embeddings
            .iter()
            .enumerate()
            .map(|(i, vec)| (i, Embedding::cosine_similarity(&query_vec, vec)))
            .collect();
        cat_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cat_sims.truncate(max_catalysts);

        // Build catalyst metadata lookup for the top catalysts
        let catalyst_meta: HashMap<String, (&str, &str, f64)> = cat_sims
            .iter()
            .map(|(idx, sim)| {
                let c = &catalysts[*idx];
                (
                    c.id.clone(),
                    (c.text.as_str(), c.entity.as_str(), *sim as f64),
                )
            })
            .collect();

        // For each relevant catalyst, get pre-computed chunk similarities.
        // Weight each chunk's similarity by how relevant its catalyst is to the query.
        // Track per-chunk: (max weighted score, catalyst_count, best_catalyst_id)
        struct ChunkScore {
            max_weighted_score: f64,
            catalyst_count: usize,
            best_catalyst_id: String,
        }

        let mut chunk_scores: HashMap<String, ChunkScore> = HashMap::new();
        let context_per_catalyst = (limit * 3 / max_catalysts.max(1)).max(10);

        let sim_repo = self.target_db.unwrap_or(self.db);

        // Batch-load similarities for all top catalysts in one query
        let top_catalyst_ids: Vec<&str> = cat_sims
            .iter()
            .map(|(idx, _)| catalysts[*idx].id.as_str())
            .collect();
        let all_sims = if self.target_db.is_some() {
            sim_repo.get_epoch_similarities_for_catalysts(
                source_epoch.expect("checked active source epoch"),
                &top_catalyst_ids,
                context_per_catalyst,
            )?
        } else {
            sim_repo.get_similarities_for_catalysts(&top_catalyst_ids, context_per_catalyst)?
        };

        let mut sims_by_catalyst: HashMap<&str, Vec<&crate::models::CatalystSimilarity>> =
            HashMap::new();
        for sim in &all_sims {
            sims_by_catalyst
                .entry(sim.catalyst_id.as_str())
                .or_default()
                .push(sim);
        }

        for (cat_idx, cat_sim) in &cat_sims {
            let catalyst = &catalysts[*cat_idx];
            let relevance = *cat_sim as f64;

            if let Some(sims) = sims_by_catalyst.get(catalyst.id.as_str()) {
                for sim in sims {
                    let key = format!("{}:{}", sim.file_path, sim.chunk_index);
                    let weighted_score = sim.similarity * relevance;
                    let entry = chunk_scores.entry(key).or_insert(ChunkScore {
                        max_weighted_score: 0.0,
                        catalyst_count: 0,
                        best_catalyst_id: String::new(),
                    });
                    if weighted_score > entry.max_weighted_score {
                        entry.max_weighted_score = weighted_score;
                        entry.best_catalyst_id = catalyst.id.clone();
                    }
                    entry.catalyst_count += 1;
                }
            }
        }

        // Score, rank, and build results
        struct ScoredChunk {
            file_path: String,
            chunk_index: i64,
            score: f64,
            best_catalyst_id: String,
            content: String,
        }

        let mut scored: Vec<ScoredChunk> = Vec::new();

        // Batch-fetch modified_at for recency boost
        let unique_paths: Vec<&str> = chunk_scores
            .keys()
            .filter_map(|key| key.rsplitn(2, ':').last())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let modified_at_map = sim_repo
            .get_modified_at_batch(&unique_paths)
            .unwrap_or_default();
        let now = chrono::Utc::now().timestamp_millis();

        for (key, cs) in &chunk_scores {
            let parts: Vec<&str> = key.rsplitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let chunk_index: i64 = parts[0].parse().unwrap_or(0);
            let file_path = parts[1].to_string();

            // Multi-catalyst boost
            let multi_boost = 1.0 + (cs.catalyst_count as f64 - 1.0) * 0.2;

            // Recency boost (from document modified_at)
            let recency_boost = if let Some(&modified_at) = modified_at_map.get(&file_path) {
                let age_days = ((now - modified_at) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);
                1.0 + (-0.693147 / 30.0 * age_days).exp()
            } else {
                1.0
            };

            scored.push(ScoredChunk {
                file_path,
                chunk_index,
                score: cs.max_weighted_score * multi_boost * recency_boost,
                best_catalyst_id: cs.best_catalyst_id.clone(),
                content: String::new(), // filled below
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Fetch content for top limit*3 candidates BEFORE dedup/truncation
        let fetch_count = (limit * 3).min(scored.len());
        for s in scored.iter_mut().take(fetch_count) {
            s.content = sim_repo
                .get_chunk_content(&s.file_path, s.chunk_index)?
                .unwrap_or_default();
        }
        // Drop candidates without content (beyond fetch_count)
        scored.truncate(fetch_count);

        // Content-based deduplication: keep highest-scoring chunk per normalized content
        {
            let mut seen: HashMap<String, usize> = HashMap::new();
            let mut keep = vec![false; scored.len()];
            for (i, s) in scored.iter().enumerate() {
                let normalized: String = s.content.split_whitespace().collect::<Vec<_>>().join(" ");
                match seen.get(&normalized) {
                    Some(&existing_idx) => {
                        // Already have this content — keep whichever has higher score
                        if s.score > scored[existing_idx].score {
                            keep[existing_idx] = false;
                            keep[i] = true;
                            seen.insert(normalized, i);
                        }
                        // else: existing is better, skip this one
                    }
                    None => {
                        keep[i] = true;
                        seen.insert(normalized, i);
                    }
                }
            }
            let mut idx = 0;
            scored.retain(|_| {
                let k = keep[idx];
                idx += 1;
                k
            });
        }

        // Now truncate to requested limit
        scored.truncate(limit);

        // Count catalyst contributions across final results
        let mut contribution_counts: HashMap<&str, usize> = HashMap::new();
        for s in &scored {
            *contribution_counts.entry(&s.best_catalyst_id).or_insert(0) += 1;
        }

        // Build top contributing catalysts (top 3 by contribution count)
        let mut top_catalysts: Vec<TopCatalyst> = catalyst_meta
            .iter()
            .filter_map(|(id, (text, entity, relevance_score))| {
                let count = contribution_counts.get(id.as_str()).copied().unwrap_or(0);
                if count == 0 {
                    return None;
                }
                Some(TopCatalyst {
                    id: id.clone(),
                    text: text.to_string(),
                    entity: entity.to_string(),
                    topic_name: None,
                    relevance_score: (*relevance_score * 1000.0).round() / 1000.0,
                    contribution_count: count,
                })
            })
            .collect();
        top_catalysts.sort_by(|a, b| {
            b.contribution_count
                .cmp(&a.contribution_count)
                .then_with(|| {
                    b.relevance_score
                        .partial_cmp(&a.relevance_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        // Build result items using already-fetched content
        let results: Vec<CatalyzeResultItem> = scored
            .iter()
            .map(|s| CatalyzeResultItem {
                file_path: s.file_path.clone(),
                content: s.content.clone(),
                similarity: (s.score * 1000.0).round() / 1000.0,
                via_catalyst_id: Some(s.best_catalyst_id.clone()),
            })
            .collect();

        Ok(CatalyzeResponse {
            results,
            top_contributing_catalysts: top_catalysts,
        })
    }

    /// Direct search: pure cosine similarity between query and all chunk embeddings.
    ///
    /// Skips catalyst indirection entirely — baseline for benchmarking.
    pub fn direct_search(&self, query: &str, limit: usize) -> Result<Vec<DirectSearchResult>> {
        let query_vec = self.service.embed_query(query)?;

        let repo = self.target_db.unwrap_or(self.db);
        let all_chunks = repo.get_all_chunk_embeddings()?;

        if all_chunks.is_empty() {
            return Ok(Vec::new());
        }

        // Score all chunks by cosine similarity
        let mut scored: Vec<(usize, f32)> = all_chunks
            .iter()
            .enumerate()
            .map(|(i, (_fp, _ci, vec))| (i, Embedding::cosine_similarity(&query_vec, vec)))
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        // Fetch content for top results
        let mut results = Vec::with_capacity(scored.len());
        for (idx, sim) in &scored {
            let (ref file_path, chunk_index, _) = all_chunks[*idx];
            let content = repo
                .get_chunk_content(file_path, chunk_index as i64)?
                .unwrap_or_default();
            results.push(DirectSearchResult {
                file_path: file_path.clone(),
                chunk_index: chunk_index as i64,
                content,
                similarity: (*sim as f64 * 1000.0).round() / 1000.0,
            });
        }

        Ok(results)
    }

    /// Direct search with externally-provided embeddings (for cross-model benchmarking).
    ///
    /// Takes pre-computed chunk embeddings and a query vector, bypassing the
    /// service's backend entirely. Used when comparing ESE vs Arctic on the same corpus.
    pub fn direct_search_with_embeddings(
        query_vec: &[f32],
        chunk_embeddings: &[(String, u32, Vec<f32>)],
        db: &Database,
        limit: usize,
    ) -> Result<Vec<DirectSearchResult>> {
        if chunk_embeddings.is_empty() {
            return Ok(Vec::new());
        }

        let mut scored: Vec<(usize, f32)> = chunk_embeddings
            .iter()
            .enumerate()
            .map(|(i, (_fp, _ci, vec))| (i, Embedding::cosine_similarity(query_vec, vec)))
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let mut results = Vec::with_capacity(scored.len());
        for (idx, sim) in &scored {
            let (ref file_path, chunk_index, _) = chunk_embeddings[*idx];
            let content = db
                .get_chunk_content(file_path, chunk_index as i64)?
                .unwrap_or_default();
            results.push(DirectSearchResult {
                file_path: file_path.clone(),
                chunk_index: chunk_index as i64,
                content,
                similarity: (*sim as f64 * 1000.0).round() / 1000.0,
            });
        }

        Ok(results)
    }
}
