use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A catalyst — an LLM-generated thematic handle for approximate search.
///
/// Catalysts are the bridge between user queries and document collections.
/// They represent distilled themes or questions derived from vault entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalyst {
    pub id: String,
    pub text: String,
    /// The entity this catalyst was generated from.
    pub entity: String,
    /// The context (vault partition) this catalyst belongs to.
    pub context: String,
    pub generated_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

/// Pre-computed similarity between a catalyst and a document chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalystSimilarity {
    pub id: Option<i64>,
    pub catalyst_id: String,
    pub catalyst_text: String,
    pub file_path: String,
    pub chunk_index: i64,
    /// Cosine similarity score in [0.0, 1.0].
    pub similarity: f64,
    pub chunk_metadata: serde_json::Value,
    pub timeline_data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}
