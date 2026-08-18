use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::models::DateSource;

use super::{DEFAULT_CHUNK_SIZE_CHARS, ProcessedDocument, chunk_document};

/// A gap this large starts a new SQLite conversation window even when the
/// current window still fits within the character budget.
pub const STRUCTURED_LOG_GAP_MS: i64 = 6 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredLogEntry {
    pub who: Vec<String>,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct StructuredLogDocumentInput {
    pub source_ref: String,
    pub title: String,
    pub source: Option<String>,
    pub folders: Vec<String>,
    pub entries: Vec<StructuredLogEntry>,
    /// The source-role fingerprint, intentionally independent of rendering.
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct StructuredLogDocumentOutput {
    pub document: ProcessedDocument,
    /// Conversation-window context aligned one-for-one with `entries`.
    pub entry_contexts: Vec<String>,
}

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

/// Render a structured conversation log without ever splitting a message.
/// Windows break at long idle gaps or before adding an entry that would push
/// the window over the normal embedding character budget. An individually
/// oversized message remains intact in its own chunk.
pub fn structured_log_document_to_processed_document(
    input: StructuredLogDocumentInput,
) -> StructuredLogDocumentOutput {
    let rendered_entries: Vec<String> = input.entries.iter().map(render_log_entry).collect();
    let mut chunk_entry_ranges = Vec::new();
    let mut range_start = 0usize;
    let mut range_len = 0usize;
    let chunk_prefix_len = format!("# {}\n\n", input.title).chars().count();

    for index in 0..input.entries.len() {
        let entry_len = rendered_entries[index].chars().count();
        let separator_len = usize::from(range_len > 0) * 2;
        let gap_split = index > range_start
            && input.entries[index]
                .created_at
                .saturating_sub(input.entries[index - 1].created_at)
                > STRUCTURED_LOG_GAP_MS;
        let budget_split = range_len > 0
            && chunk_prefix_len + range_len + separator_len + entry_len > DEFAULT_CHUNK_SIZE_CHARS;
        if gap_split || budget_split {
            chunk_entry_ranges.push(range_start..index);
            range_start = index;
            range_len = 0;
        }
        if range_len > 0 {
            range_len += 2;
        }
        range_len += entry_len;
    }
    if range_start < input.entries.len() {
        chunk_entry_ranges.push(range_start..input.entries.len());
    }

    let mut chunks = Vec::with_capacity(chunk_entry_ranges.len());
    let mut chunk_entry_dates = Vec::with_capacity(chunk_entry_ranges.len());
    let mut entry_contexts = vec![String::new(); input.entries.len()];
    for range in chunk_entry_ranges {
        let window = rendered_entries[range.clone()].join("\n\n");
        let chunk = format!("# {}\n\n{window}", input.title);
        let date = input.entries[range.clone()]
            .iter()
            .map(|entry| entry.created_at)
            .max();
        for index in range {
            entry_contexts[index] = chunk.clone();
        }
        chunks.push(chunk);
        chunk_entry_dates.push(date);
    }

    let mut full_content = format!("# {}", input.title);
    if !rendered_entries.is_empty() {
        full_content.push_str("\n\n");
        full_content.push_str(&rendered_entries.join("\n\n"));
    }
    if let Some(source) = input.source.as_deref().filter(|source| !source.is_empty()) {
        full_content.push_str("\n\nSource: ");
        full_content.push_str(source);
    }
    let created_at = input
        .entries
        .first()
        .map(|entry| entry.created_at)
        .unwrap_or_default();
    let modified_at = input
        .entries
        .last()
        .map(|entry| entry.created_at)
        .unwrap_or(created_at);
    let mut links: Vec<String> = input
        .entries
        .iter()
        .flat_map(|entry| entry.who.iter().cloned())
        .collect();
    links.sort();
    links.dedup();

    StructuredLogDocumentOutput {
        document: ProcessedDocument {
            source_ref: input.source_ref,
            title: Some(input.title),
            word_count: full_content.split_whitespace().count() as i64,
            char_count: full_content.chars().count() as i64,
            content: full_content,
            content_hash: input.fingerprint,
            created_at,
            created_source: DateSource::Content,
            modified_at,
            modified_source: DateSource::Content,
            tags: Vec::new(),
            links,
            folders: input.folders,
            chunks,
            chunk_entry_dates,
            frontmatter: None,
            is_log: true,
            append_log: None,
        },
        entry_contexts,
    }
}

fn render_log_entry(entry: &StructuredLogEntry) -> String {
    let timestamp = DateTime::<Utc>::from_timestamp_millis(entry.created_at)
        .map(|date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{} ms", entry.created_at));
    let who = if entry.who.is_empty() {
        "Unknown".to_string()
    } else {
        entry.who.join(", ")
    };
    format!("## {timestamp} — {who}\n\n{}", entry.content)
}

#[cfg(test)]
mod log_tests {
    use super::*;

    fn input(entries: Vec<StructuredLogEntry>) -> StructuredLogDocumentInput {
        StructuredLogDocumentInput {
            source_ref: "sqlite:messages/thread/2025-01".to_string(),
            title: "Thread".to_string(),
            source: Some("messages".to_string()),
            folders: vec!["Thread".to_string()],
            entries,
            fingerprint: "fingerprint".to_string(),
        }
    }

    #[test]
    fn structured_log_splits_on_long_gaps_and_preserves_message_boundaries() {
        let first = StructuredLogEntry {
            who: vec!["Alice".to_string()],
            content: "before gap".to_string(),
            created_at: 1_700_000_000_000,
        };
        let second = StructuredLogEntry {
            who: vec!["Bob".to_string()],
            content: "after gap".to_string(),
            created_at: first.created_at + STRUCTURED_LOG_GAP_MS + 1,
        };
        let output = structured_log_document_to_processed_document(input(vec![first, second]));

        assert_eq!(output.document.chunks.len(), 2);
        assert!(output.document.chunks[0].contains("before gap"));
        assert!(!output.document.chunks[0].contains("after gap"));
        assert!(output.document.chunks[1].contains("after gap"));
        assert_eq!(output.document.chunk_entry_dates.len(), 2);
        assert!(output.document.is_log);
    }

    #[test]
    fn structured_log_keeps_an_oversized_message_whole() {
        let body = "x".repeat(DEFAULT_CHUNK_SIZE_CHARS + 100);
        let output =
            structured_log_document_to_processed_document(input(vec![StructuredLogEntry {
                who: vec!["Alice".to_string()],
                content: body.clone(),
                created_at: 1_700_000_000_000,
            }]));

        assert_eq!(output.document.chunks.len(), 1);
        assert!(output.document.chunks[0].contains(&body));
    }

    #[test]
    fn structured_log_splits_before_a_window_exceeds_the_character_budget() {
        let first_body = "a".repeat(DEFAULT_CHUNK_SIZE_CHARS / 2 + 50);
        let second_body = "b".repeat(DEFAULT_CHUNK_SIZE_CHARS / 2 + 50);
        let output = structured_log_document_to_processed_document(input(vec![
            StructuredLogEntry {
                who: vec!["Alice".to_string()],
                content: first_body.clone(),
                created_at: 1_700_000_000_000,
            },
            StructuredLogEntry {
                who: vec!["Bob".to_string()],
                content: second_body.clone(),
                created_at: 1_700_000_001_000,
            },
        ]));

        assert_eq!(output.document.chunks.len(), 2);
        assert!(output.document.chunks[0].contains(&first_body));
        assert!(!output.document.chunks[0].contains(&second_body));
        assert!(output.document.chunks[1].contains(&second_body));
        assert!(
            output
                .document
                .chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= DEFAULT_CHUNK_SIZE_CHARS)
        );
    }
}
