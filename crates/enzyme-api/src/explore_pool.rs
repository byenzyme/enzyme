//! In-memory pool for fast explore queries.
//!
//! Holds the HNSW index over catalyst embeddings, pre-loaded catalyst metadata,
//! and pre-computed similarities for rapid KNN + aggregation without DB I/O.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::Result;
use tracing::info;

use enzyme_core::db::Database;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::search::hnsw::HnswIndex;

/// Metadata about a single catalyst in the pool.
#[derive(Debug, Clone)]
pub struct CatalystMeta {
    pub id: String,
    pub text: String,
    pub entity: String,
    pub topic_name: Option<String>,
}

/// A document similar to a catalyst (pre-computed).
#[derive(Debug, Clone)]
pub struct SimilarDoc {
    pub file_path: String,
    pub chunk_index: i64,
    pub similarity: f64,
}

pub struct ExplorePool {
    pub vault_path: PathBuf,
    pub hnsw_index: HnswIndex,
    pub catalyst_metadata: Vec<CatalystMeta>,
    pub similarities: HashMap<String, Vec<SimilarDoc>>,
    pub pool_context: String,
    pub warmed_prefix: Option<String>,
    pub warmed_catalyst_ids: Option<Vec<String>>,
    pub warmed_doc_index_map: Option<HashMap<String, String>>,
    pub built_at: SystemTime,
}

impl ExplorePool {
    /// Load the pool from disk: read catalyst embeddings, build HNSW, load metadata + similarities.
    pub fn load(vault_path: &Path) -> Result<Self> {
        let start = Instant::now();
        let paths = EnzymePaths::new(vault_path)?;

        // Load catalyst metadata and similarities from DB
        let db = Database::open(&paths.vault_db)?;

        // Load catalyst embeddings from DB
        let cached = db.get_all_catalyst_embeddings()?;
        if cached.is_empty() {
            anyhow::bail!("No cached catalyst embeddings found. Run `enzyme init` first.");
        }

        // Build HNSW index over catalyst embeddings
        let mut hnsw_index = HnswIndex::new();
        let points: Vec<(Vec<f32>, String)> = cached
            .iter()
            .map(|(id, vec)| (vec.clone(), id.clone()))
            .collect();
        hnsw_index.build(points)?;

        let all_catalysts = db.get_all_catalysts()?;
        let mut catalyst_metadata = Vec::with_capacity(all_catalysts.len());
        let mut similarities: HashMap<String, Vec<SimilarDoc>> = HashMap::new();

        for cat in &all_catalysts {
            catalyst_metadata.push(CatalystMeta {
                id: cat.id.clone(),
                text: cat.text.clone(),
                entity: cat.entity.clone(),
                topic_name: None, // Could be enriched later
            });

            // Load pre-computed similarities for this catalyst
            let sims = db.get_similarities_for_catalyst(&cat.id, 50)?;
            let sim_docs: Vec<SimilarDoc> = sims
                .into_iter()
                .map(|s| SimilarDoc {
                    file_path: s.file_path,
                    chunk_index: s.chunk_index,
                    similarity: s.similarity,
                })
                .collect();
            similarities.insert(cat.id.clone(), sim_docs);
        }

        // Pre-render pool context markdown
        let pool_context = Self::render_pool_context(&catalyst_metadata);


        let elapsed = start.elapsed();
        info!(
            catalysts = catalyst_metadata.len(),
            similarities = similarities.len(),
            elapsed_ms = elapsed.as_millis(),
            "ExplorePool loaded"
        );

        Ok(Self {
            vault_path: vault_path.to_path_buf(),
            hnsw_index,
            catalyst_metadata,
            similarities,
            pool_context,
            warmed_prefix: None,
            warmed_catalyst_ids: None,
            warmed_doc_index_map: None,
            built_at: SystemTime::now(),
        })
    }

    /// Check if the pool is stale (always returns false; staleness is managed by the caller).
    pub fn is_stale(&self) -> bool {
        false
    }

    /// Search the HNSW index for catalysts most similar to a query embedding.
    pub fn search_catalysts(&self, query_embedding: &[f32], top_k: usize) -> Result<Vec<(String, f32)>> {
        let results = self.hnsw_index.search(query_embedding, top_k)?;
        Ok(results.into_iter().map(|r| (r.id, r.similarity)).collect())
    }

    /// Get similarities for a catalyst ID.
    pub fn get_similarities(&self, catalyst_id: &str) -> Option<&Vec<SimilarDoc>> {
        self.similarities.get(catalyst_id)
    }

    /// Get catalyst metadata by ID.
    pub fn get_catalyst_meta(&self, catalyst_id: &str) -> Option<&CatalystMeta> {
        self.catalyst_metadata.iter().find(|c| c.id == catalyst_id)
    }

    fn render_pool_context(catalysts: &[CatalystMeta]) -> String {
        let mut lines = Vec::new();
        let mut by_entity: HashMap<&str, Vec<&str>> = HashMap::new();

        for cat in catalysts {
            by_entity
                .entry(&cat.entity)
                .or_default()
                .push(&cat.text);
        }

        for (entity, texts) in &by_entity {
            lines.push(format!("- {}: {}", entity, texts.join("; ")));
        }

        lines.join("\n")
    }
}
