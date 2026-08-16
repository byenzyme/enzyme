use sha2::{Digest, Sha256};

use crate::models::DateSource;

use super::{DEFAULT_CHUNK_SIZE_CHARS, ProcessedDocument, chunk_document};

/// Normalized structured input shared by CLI ingestion and database sources.
#[derive(Debug, Clone)]
pub struct StructuredDocumentInput {
    /// Exact stable identity stored in `docs.source_ref`.
    pub source_ref: String,
    pub title: String,
    pub content: Option<String>,
    pub notes: Option<String>,
    pub source: Option<String>,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub folders: Vec<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
}

/// Map an externally structured record into the same document representation
/// used by Markdown indexing.
pub fn structured_document_to_processed_document(
    input: StructuredDocumentInput,
) -> ProcessedDocument {
    let mut parts = Vec::new();
    if let Some(ref content) = input.content {
        parts.push(content.clone());
    }
    if let Some(ref notes) = input.notes
        && !notes.is_empty()
    {
        parts.push(format!("\n{notes}"));
    }
    if let Some(ref source) = input.source
        && !source.is_empty()
    {
        parts.push(format!("\nSource: {source}"));
    }

    let full_content = if parts.is_empty() {
        input.title.clone()
    } else {
        format!("# {}\n\n{}", input.title, parts.join("\n"))
    };

    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(full_content.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let chunks = chunk_document(&full_content, DEFAULT_CHUNK_SIZE_CHARS);

    ProcessedDocument {
        source_ref: input.source_ref,
        title: Some(input.title),
        word_count: full_content.split_whitespace().count() as i64,
        char_count: full_content.chars().count() as i64,
        content: full_content,
        content_hash,
        created_at: input.created_at,
        created_source: DateSource::Content,
        modified_at: input.created_at,
        modified_source: DateSource::Content,
        tags: input.tags,
        links: input.links,
        folders: input.folders,
        chunk_entry_dates: vec![None; chunks.len()],
        chunks,
        frontmatter: None,
        is_log: false,
        append_log: None,
    }
}
