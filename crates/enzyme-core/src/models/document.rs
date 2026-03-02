use serde::{Deserialize, Serialize};

use super::Timestamp;

/// How a timestamp was determined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateSource {
    Frontmatter,
    Filename,
    Filesystem,
    Content,
}

impl DateSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Frontmatter => "frontmatter",
            Self::Filename => "filename",
            Self::Filesystem => "filesystem",
            Self::Content => "content",
        }
    }
}

/// A markdown document indexed from the vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: Option<i64>,
    /// Relative path within the vault, e.g. `notes/my-note.md`.
    pub source_ref: String,
    pub title: Option<String>,
    pub content: Option<String>,
    /// SHA-256 hash of content for change detection.
    pub content_hash: String,
    pub word_count: i64,
    pub char_count: i64,
    pub created_at: Timestamp,
    pub created_source: DateSource,
    pub modified_at: Timestamp,
    pub modified_source: DateSource,
    pub indexed_at: Timestamp,
    pub embedded_at: Option<Timestamp>,
    /// Content hash at embedding time — if different from `content_hash`, embeddings are stale.
    pub embedded_content_hash: Option<String>,
    pub metadata: serde_json::Value,
}

/// A chunk of a document, used for embedding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Option<i64>,
    pub doc_id: i64,
    pub chunk_index: i64,
    pub content: String,
    pub word_count: i64,
    pub created_at: Timestamp,
}
