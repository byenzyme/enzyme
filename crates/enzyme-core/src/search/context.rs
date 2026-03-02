use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::{Database, Repository};
use crate::embedding::EmbeddingService;
use crate::models::Embedding;
use crate::search::hnsw::HnswIndex;
use crate::search::bm25::Bm25Index;

/// A combined search result with content and optional catalyst provenance.
#[derive(Debug, Clone)]
pub struct ContextResult {
    pub file_path: String,
    pub chunk_index: i64,
    pub score: f64,
    pub content: String,
    pub catalyst_text: Option<String>,
}

/// Structured response from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResponse {
    pub results: Vec<CatalyzeResultItem>,
    pub top_contributing_catalysts: Vec<TopCatalyst>,
}

/// A single result from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResultItem {
    pub file_path: String,
    pub content: String,
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

/// Orchestrates hybrid search using HNSW + BM25 + catalyst pre-filtering.
pub struct ContextSearch<'a> {
    repo: Repository<'a>,
    hnsw: &'a HnswIndex,
    bm25: &'a Bm25Index,
    service: &'a EmbeddingService,
}

impl<'a> ContextSearch<'a> {
    pub fn new(
        db: &'a Database,
        hnsw: &'a HnswIndex,
        bm25: &'a Bm25Index,
        service: &'a EmbeddingService,
    ) -> Self {
        Self {
            repo: Repository::new(db),
            hnsw,
            bm25,
            service,
        }
    }

    /// Catalyst-based search (used by `enzyme catalyze`).
    ///
    /// 1. Embed query
    /// 2. Find top relevant catalysts (embed all catalysts, compare)
    /// 3. For each relevant catalyst, get pre-computed chunk similarities
    /// 4. Aggregate, boost by multi-catalyst and recency, deduplicate, sort
    /// 5. Compute top contributing catalysts with contribution counts
    pub fn catalyze_search(&self, query: &str, limit: usize) -> Result<CatalyzeResponse> {
        let query_vec = self.service.embed_query(query)?;

        // Get all catalysts and embed them
        let catalysts = self.repo.get_all_catalysts()?;
        if catalysts.is_empty() {
            return Ok(CatalyzeResponse {
                results: Vec::new(),
                top_contributing_catalysts: Vec::new(),
            });
        }

        let cat_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
        let cat_embeddings = self.service.embed_documents(&cat_texts)?;

        // Find top relevant catalysts (top 8)
        let mut cat_sims: Vec<(usize, f32)> = cat_embeddings
            .iter()
            .enumerate()
            .map(|(i, vec)| (i, Embedding::cosine_similarity(&query_vec, vec)))
            .collect();
        cat_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cat_sims.truncate(8);

        // Build catalyst metadata lookup for the top catalysts
        let catalyst_meta: HashMap<String, (&str, &str, f64)> = cat_sims
            .iter()
            .map(|(idx, sim)| {
                let c = &catalysts[*idx];
                (c.id.clone(), (c.text.as_str(), c.entity.as_str(), *sim as f64))
            })
            .collect();

        // For each relevant catalyst, get pre-computed chunk similarities.
        // Track per-chunk: (max_similarity, catalyst_count, best_catalyst_id)
        struct ChunkScore {
            max_similarity: f64,
            catalyst_count: usize,
            best_catalyst_id: String,
        }

        let mut chunk_scores: HashMap<String, ChunkScore> = HashMap::new();

        for (cat_idx, _cat_sim) in &cat_sims {
            let catalyst = &catalysts[*cat_idx];
            let sims = self.repo.get_similarities_for_catalyst(&catalyst.id, 50)?;

            for sim in &sims {
                let key = format!("{}:{}", sim.file_path, sim.chunk_index);
                let entry = chunk_scores.entry(key).or_insert(ChunkScore {
                    max_similarity: 0.0,
                    catalyst_count: 0,
                    best_catalyst_id: String::new(),
                });
                if sim.similarity > entry.max_similarity {
                    entry.max_similarity = sim.similarity;
                    entry.best_catalyst_id = catalyst.id.clone();
                }
                entry.catalyst_count += 1;
            }
        }

        // Score, rank, and build results
        struct ScoredChunk {
            file_path: String,
            chunk_index: i64,
            score: f64,
            best_catalyst_id: String,
        }

        let mut scored: Vec<ScoredChunk> = Vec::new();

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
            let mut recency_boost = 1.0;
            if let Ok(Some(doc)) = self.repo.get_document_by_path(&file_path) {
                let now = chrono::Utc::now().timestamp_millis();
                let age_days = ((now - doc.modified_at) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);
                recency_boost = 1.0 + (-0.693147 / 30.0 * age_days).exp();
            }

            scored.push(ScoredChunk {
                file_path,
                chunk_index,
                score: cs.max_similarity * multi_boost * recency_boost,
                best_catalyst_id: cs.best_catalyst_id.clone(),
            });
        }

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
        top_catalysts.sort_by(|a, b| b.contribution_count.cmp(&a.contribution_count));
        top_catalysts.truncate(3);

        // Build result items with content
        let mut results = Vec::with_capacity(scored.len());
        for s in &scored {
            let content = self.repo.get_chunk_content(&s.file_path, s.chunk_index)?
                .unwrap_or_default();
            results.push(CatalyzeResultItem {
                file_path: s.file_path.clone(),
                content,
            });
        }

        Ok(CatalyzeResponse {
            results,
            top_contributing_catalysts: top_catalysts,
        })
    }

    /// Direct hybrid search (used by `enzyme context`).
    ///
    /// 1. Embed query -> HNSW search
    /// 2. BM25 lexical search
    /// 3. Merge with weighted combination (0.7 semantic + 0.3 lexical)
    /// 4. Deduplicate, sort, return top limit
    pub fn hybrid_search(&self, query: &str, limit: usize) -> Result<Vec<ContextResult>> {
        let k = limit * 3;

        // HNSW semantic search
        let query_vec = self.service.embed_query(query)?;
        let hnsw_results = if self.hnsw.is_built() {
            self.hnsw.search(&query_vec, k)?
        } else {
            Vec::new()
        };

        // BM25 lexical search
        let bm25_results = self.bm25.search(query, k);

        // Normalize scores to [0, 1]
        let hnsw_max = hnsw_results.iter().map(|r| r.similarity).fold(0.0f32, f32::max);
        let hnsw_min = hnsw_results.iter().map(|r| r.similarity).fold(f32::MAX, f32::min);
        let hnsw_range = if (hnsw_max - hnsw_min).abs() > 1e-6 { hnsw_max - hnsw_min } else { 1.0 };

        let bm25_max = bm25_results.iter().map(|r| r.score).fold(0.0f32, f32::max);
        let bm25_min = bm25_results.iter().map(|r| r.score).fold(f32::MAX, f32::min);
        let bm25_range = if (bm25_max - bm25_min).abs() > 1e-6 { bm25_max - bm25_min } else { 1.0 };

        // Combine into a single map: key -> score
        let mut combined: HashMap<String, f64> = HashMap::new();

        for r in &hnsw_results {
            let norm = ((r.similarity - hnsw_min) / hnsw_range) as f64;
            *combined.entry(r.id.clone()).or_insert(0.0) += 0.7 * norm;
        }

        for r in &bm25_results {
            let norm = ((r.score - bm25_min) / bm25_range) as f64;
            *combined.entry(r.doc_id.clone()).or_insert(0.0) += 0.3 * norm;
        }

        // Build results
        let mut results: Vec<ContextResult> = Vec::new();

        for (key, score) in &combined {
            let parts: Vec<&str> = key.rsplitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let chunk_index: i64 = parts[0].parse().unwrap_or(0);
            let file_path = parts[1].to_string();

            let content = self.repo.get_chunk_content(&file_path, chunk_index)?
                .unwrap_or_default();

            results.push(ContextResult {
                file_path,
                chunk_index,
                score: *score,
                content,
                catalyst_text: None,
            });
        }

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);

        Ok(results)
    }
}
