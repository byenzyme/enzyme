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
/// readers (see `pipeline::catalysts`). Rather than store a full oversized
/// body (which the reader discards) or nothing (which drops the note from the
/// query centroid entirely), writers store a bounded [`representative_excerpt`]
/// that stays under this threshold — see
/// `pipeline::indexing::collect_entity_entries` and the slim/backfill
/// migrations in `db::migrations`.
pub const INLINE_CONTEXT_MAX_WORDS: usize = 400;

/// Target word count for a long note's stored [`representative_excerpt`].
///
/// Deliberately below [`INLINE_CONTEXT_MAX_WORDS`] so the excerpt passes the
/// catalyst reader's inline-context filter and contributes to the entity query
/// centroid, while staying small enough to keep prompts bounded.
pub const EXCERPT_MAX_WORDS: usize = 240;

/// Build a bounded, representative excerpt of a (frontmatter-stripped) note
/// body: the first and last words joined by a `[…]` marker, totalling about
/// [`EXCERPT_MAX_WORDS`] words. This is the "first + last chunk" bookend of a
/// long note, computed from the body so every write site (index time, slim
/// migration, backfill migration) produces the *same* excerpt for the same
/// note — keeping query centroids reproducible regardless of code path.
///
/// Bodies already at or below the word budget are returned unchanged.
pub fn representative_excerpt(body: &str) -> String {
    let words: Vec<&str> = body.split_whitespace().collect();
    if words.len() <= EXCERPT_MAX_WORDS {
        return body.trim().to_string();
    }
    let head_n = EXCERPT_MAX_WORDS / 2;
    let tail_n = EXCERPT_MAX_WORDS - head_n;
    let head = words[..head_n].join(" ");
    let tail = words[words.len() - tail_n..].join(" ");
    format!("{head}\n[…]\n{tail}")
}

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
