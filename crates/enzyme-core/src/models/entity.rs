use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::Timestamp;

/// Synthetic folder entity used when every indexed document lives at vault root.
///
/// Config represents this as `folder:.`.
pub const ROOT_FOLDER_ENTITY: &str = ".";

/// Maximum word count for an occurrence context to be considered an inline
/// lasso extract (vs. a full note body from frontmatter). Contexts at or
/// above this threshold are filtered out of centroid/context selection by
/// readers (see `pipeline::catalysts`), so writers should not bother storing
/// them (see `pipeline::indexing::collect_entity_entries`).
pub const INLINE_CONTEXT_MAX_WORDS: usize = 400;

/// The kind of entity extracted from vault documents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Tag,
    Link,
    Folder,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Link => "link",
            Self::Folder => "folder",
        }
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Trending metrics for an entity over time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityTrends {
    pub frequency_recent: i64,
    pub recency_score: f64,
    /// Counts per month bucket, e.g. `{"2025-01": 3, "2025-02": 2}`.
    pub timeline_buckets: HashMap<String, i64>,
}

/// An entity (tag, wikilink, or folder) extracted from vault documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Option<i64>,
    /// Normalized name (lowercase for tags).
    pub name: String,
    pub entity_type: EntityType,
    pub frequency: i64,
    pub first_seen: Option<Timestamp>,
    pub last_seen: Option<Timestamp>,
    pub trends: EntityTrends,
    pub metadata: serde_json::Value,
}

/// A single occurrence of an entity in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityOccurrence {
    pub id: Option<i64>,
    pub entity_id: i64,
    pub file_path: String,
    pub context: Option<String>,
    pub created_at: Timestamp,
    /// The effective date of the document (from frontmatter, filename, or filesystem).
    pub effective_date: Option<Timestamp>,
    pub date_source: String,
}
