use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::models::DateSource;

use super::chunking::{chunk_document, DEFAULT_CHUNK_SIZE_CHARS};
use super::extract::{extract_entities, ExtractedEntities};
use super::frontmatter::{parse_frontmatter, strip_frontmatter, Frontmatter};
use super::readwise::{chunk_readwise_highlights, extract_highlights, is_readwise_content};

/// A fully processed document ready for indexing.
#[derive(Debug, Clone)]
pub struct ProcessedDocument {
    /// Relative path within the vault (e.g., `notes/my-note.md`).
    pub source_ref: String,
    pub title: Option<String>,
    /// Full raw content of the file.
    pub content: String,
    /// SHA-256 hex digest of the raw content.
    pub content_hash: String,
    pub word_count: i64,
    pub char_count: i64,
    /// Milliseconds since Unix epoch.
    pub created_at: i64,
    pub created_source: DateSource,
    /// Milliseconds since Unix epoch.
    pub modified_at: i64,
    pub modified_source: DateSource,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub folders: Vec<String>,
    /// Content chunks for embedding.
    pub chunks: Vec<String>,
    pub frontmatter: Option<Frontmatter>,
}

/// Orchestrates the full document-processing pipeline:
/// read -> frontmatter -> extract entities -> chunk -> hash.
pub struct DocumentProcessor {
    vault_path: PathBuf,
    chunk_size: usize,
}

impl DocumentProcessor {
    /// Create a processor for the given vault directory.
    pub fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path,
            chunk_size: DEFAULT_CHUNK_SIZE_CHARS,
        }
    }

    /// Override the chunk size (in characters).
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Process a single file. `path` should be relative to the vault root.
    pub fn process_file(&self, path: &Path) -> Result<ProcessedDocument> {
        let abs_path = self.vault_path.join(path);
        let content = fs::read_to_string(&abs_path)
            .with_context(|| format!("failed to read file: {:?}", abs_path))?;

        let source_ref = path.to_string_lossy().to_string();

        // SHA-256 content hash
        let content_hash = {
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        // Frontmatter
        let frontmatter = parse_frontmatter(&content);

        // Title: from frontmatter, or derive from filename
        let title = frontmatter
            .as_ref()
            .and_then(|fm| fm.title.clone())
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            });

        // Body (without frontmatter) for entity extraction and chunking
        let body = strip_frontmatter(&content);

        // Entities
        let frontmatter_tags = frontmatter
            .as_ref()
            .map(|fm| fm.tags.clone())
            .unwrap_or_default();
        let ExtractedEntities {
            tags,
            links,
            folders,
        } = extract_entities(body, path, &frontmatter_tags);

        // Chunks — use Readwise-aware chunking for highlight-based content
        let chunks = if is_readwise_content(body) {
            let highlights = extract_highlights(body);
            if highlights.is_empty() {
                chunk_document(body, self.chunk_size)
            } else {
                chunk_readwise_highlights(&highlights, 5, 15)
            }
        } else {
            chunk_document(body, self.chunk_size)
        };

        // Word and char counts (from body, not frontmatter YAML)
        let word_count = body.split_whitespace().count() as i64;
        let char_count = body.chars().count() as i64;

        // Dates
        let (created_at, created_source) = resolve_created_date(&frontmatter, &abs_path)?;
        let (modified_at, modified_source) = resolve_modified_date(&frontmatter, &abs_path)?;

        Ok(ProcessedDocument {
            source_ref,
            title,
            content,
            content_hash,
            word_count,
            char_count,
            created_at,
            created_source,
            modified_at,
            modified_source,
            tags,
            links,
            folders,
            chunks,
            frontmatter,
        })
    }

    /// Process multiple files in parallel using rayon.
    pub fn process_all(&self, paths: &[PathBuf]) -> Vec<Result<ProcessedDocument>> {
        paths
            .par_iter()
            .map(|path| self.process_file(path))
            .collect()
    }
}

/// Resolve the created-at timestamp. Prefer frontmatter, fall back to filesystem.
fn resolve_created_date(
    frontmatter: &Option<Frontmatter>,
    abs_path: &Path,
) -> Result<(i64, DateSource)> {
    if let Some(fm) = frontmatter {
        if let Some(dt) = fm.created {
            return Ok((datetime_to_millis(dt), DateSource::Frontmatter));
        }
    }

    // Fall back to filesystem creation time (or modified time on Linux).
    let metadata = fs::metadata(abs_path)
        .with_context(|| format!("failed to read metadata: {:?}", abs_path))?;

    let system_time = metadata
        .created()
        .or_else(|_| metadata.modified())
        .context("no creation or modification time available")?;

    let dt: DateTime<Utc> = system_time.into();
    Ok((datetime_to_millis(dt), DateSource::Filesystem))
}

/// Resolve the modified-at timestamp. Prefer frontmatter, fall back to filesystem.
fn resolve_modified_date(
    frontmatter: &Option<Frontmatter>,
    abs_path: &Path,
) -> Result<(i64, DateSource)> {
    if let Some(fm) = frontmatter {
        if let Some(dt) = fm.modified {
            return Ok((datetime_to_millis(dt), DateSource::Frontmatter));
        }
    }

    let metadata = fs::metadata(abs_path)
        .with_context(|| format!("failed to read metadata: {:?}", abs_path))?;

    let system_time = metadata
        .modified()
        .context("no modification time available")?;

    let dt: DateTime<Utc> = system_time.into();
    Ok((datetime_to_millis(dt), DateSource::Filesystem))
}

/// Convert a `DateTime<Utc>` to milliseconds since Unix epoch.
fn datetime_to_millis(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_vault() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let vault = dir.path().to_path_buf();
        (dir, vault)
    }

    #[test]
    fn test_process_simple_file() {
        let (_dir, vault) = create_test_vault();
        let content = "\
---
title: Test Note
tags: [rust, testing]
created: 2024-06-15
---
# Test Note

This is a test document with a [[link]] and a #hashtag.

Second paragraph here.
";
        fs::write(vault.join("test.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("test.md")).unwrap();

        assert_eq!(doc.source_ref, "test.md");
        assert_eq!(doc.title, Some("Test Note".to_string()));
        assert!(!doc.content_hash.is_empty());
        assert!(doc.word_count > 0);
        assert!(doc.char_count > 0);
        assert!(doc.tags.contains(&"rust".to_string()));
        assert!(doc.tags.contains(&"testing".to_string()));
        assert!(doc.tags.contains(&"hashtag".to_string()));
        assert!(doc.links.contains(&"link".to_string()));
        assert!(!doc.chunks.is_empty());
        assert!(doc.frontmatter.is_some());

        // Created date should come from frontmatter
        assert_eq!(doc.created_source, DateSource::Frontmatter);
    }

    #[test]
    fn test_process_file_without_frontmatter() {
        let (_dir, vault) = create_test_vault();
        let content = "# Just a Heading\n\nSome content here.\n";
        fs::write(vault.join("simple.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("simple.md")).unwrap();

        assert_eq!(doc.title, Some("simple".to_string())); // derived from filename
        assert!(doc.frontmatter.is_none());
        assert_eq!(doc.created_source, DateSource::Filesystem);
        assert_eq!(doc.modified_source, DateSource::Filesystem);
    }

    #[test]
    fn test_process_file_in_subfolder() {
        let (_dir, vault) = create_test_vault();
        fs::create_dir_all(vault.join("notes/daily")).unwrap();
        let content = "# Daily Note\n\nToday's content.\n";
        fs::write(vault.join("notes/daily/2024-01-15.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor
            .process_file(Path::new("notes/daily/2024-01-15.md"))
            .unwrap();

        assert_eq!(doc.source_ref, "notes/daily/2024-01-15.md");
        assert_eq!(doc.folders, vec!["notes", "notes/daily"]);
    }

    #[test]
    fn test_content_hash_deterministic() {
        let (_dir, vault) = create_test_vault();
        let content = "# Deterministic\n\nSame content always.\n";
        fs::write(vault.join("det.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc1 = processor.process_file(Path::new("det.md")).unwrap();
        let doc2 = processor.process_file(Path::new("det.md")).unwrap();

        assert_eq!(doc1.content_hash, doc2.content_hash);
    }

    #[test]
    fn test_content_hash_changes_with_content() {
        let (_dir, vault) = create_test_vault();
        fs::write(vault.join("a.md"), "Content A").unwrap();
        fs::write(vault.join("b.md"), "Content B").unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc_a = processor.process_file(Path::new("a.md")).unwrap();
        let doc_b = processor.process_file(Path::new("b.md")).unwrap();

        assert_ne!(doc_a.content_hash, doc_b.content_hash);
    }

    #[test]
    fn test_process_nonexistent_file() {
        let (_dir, vault) = create_test_vault();
        let processor = DocumentProcessor::new(vault);
        let result = processor.process_file(Path::new("nonexistent.md"));

        assert!(result.is_err());
    }

    #[test]
    fn test_process_all_parallel() {
        let (_dir, vault) = create_test_vault();
        fs::write(vault.join("a.md"), "# A\nContent A").unwrap();
        fs::write(vault.join("b.md"), "# B\nContent B").unwrap();
        fs::write(vault.join("c.md"), "# C\nContent C").unwrap();

        let paths: Vec<PathBuf> = vec!["a.md".into(), "b.md".into(), "c.md".into()];

        let processor = DocumentProcessor::new(vault);
        let results = processor.process_all(&paths);

        assert_eq!(results.len(), 3);
        for result in &results {
            assert!(result.is_ok(), "all files should process successfully");
        }

        let source_refs: Vec<String> = results
            .into_iter()
            .map(|r| r.unwrap().source_ref)
            .collect();
        assert!(source_refs.contains(&"a.md".to_string()));
        assert!(source_refs.contains(&"b.md".to_string()));
        assert!(source_refs.contains(&"c.md".to_string()));
    }

    #[test]
    fn test_process_all_with_errors() {
        let (_dir, vault) = create_test_vault();
        fs::write(vault.join("good.md"), "# Good").unwrap();

        let paths: Vec<PathBuf> = vec!["good.md".into(), "missing.md".into()];

        let processor = DocumentProcessor::new(vault);
        let results = processor.process_all(&paths);

        assert_eq!(results.len(), 2);
        // One should succeed, one should fail
        let successes = results.iter().filter(|r| r.is_ok()).count();
        let failures = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(successes, 1);
        assert_eq!(failures, 1);
    }

    #[test]
    fn test_word_and_char_count() {
        let (_dir, vault) = create_test_vault();
        let body = "Hello world. This is a test.";
        let content = format!("---\ntitle: Test\n---\n{}", body);
        fs::write(vault.join("count.md"), &content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("count.md")).unwrap();

        assert_eq!(doc.word_count, 6); // "Hello world. This is a test."
        assert_eq!(doc.char_count, body.len() as i64);
    }

    #[test]
    fn test_custom_chunk_size() {
        let (_dir, vault) = create_test_vault();
        // Create content that will definitely be split at a small chunk size
        let paragraph = "This is a sentence. ".repeat(20);
        let content = format!("{}\n\n{}", paragraph.trim(), paragraph.trim());
        fs::write(vault.join("long.md"), &content).unwrap();

        let processor = DocumentProcessor::new(vault).with_chunk_size(100);
        let doc = processor.process_file(Path::new("long.md")).unwrap();

        assert!(
            doc.chunks.len() > 1,
            "content should be split into multiple chunks"
        );
    }
}
