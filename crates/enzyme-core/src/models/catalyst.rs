use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Auditable evidence supplied to the model for a generated catalyst.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EvidenceAnchor {
    /// Request-local stable label cited by the model (for example `E1`).
    pub anchor_id: String,
    /// Canonical document/source reference used by the index.
    pub source_ref: String,
    /// Entity-occurrence rows connecting this evidence to the entity.
    pub occurrence_ids: Vec<i64>,
    /// Effective evidence timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: i64,
    /// Chunk index when the evidence came from a stored chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<u32>,
    /// Short verbatim span supplied to the generator.
    pub quote: String,
}

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
