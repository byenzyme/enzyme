use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::models::DateSource;

use super::DiscoveredFile;
use super::appendlog::{
    AppendLog, MIN_LOG_ENTRIES, chunk_append_log_dated, detect_append_log, entries_with_body,
    segment_declared_log,
};
use super::chunking::{DEFAULT_CHUNK_SIZE_CHARS, chunk_document};
use super::datepath::extract_date_from_path;
use super::extract::{ExtractedEntities, extract_entities};
use super::frontmatter::{Frontmatter, parse_frontmatter, strip_frontmatter};
use super::readwise::{chunk_readwise_highlights, extract_highlights, is_readwise_content};

/// Word floor for auto-flagging a body as an append-only log.
///
/// The primary stub guard is structural: at least [`MIN_LOG_ENTRIES`] entries
/// must carry a non-empty body (see [`entries_with_body`]) — that is what
/// actually separates a journal from a note that merely lists dated backlinks
/// or a bare dated scaffold, whatever its word count. The floor survives only
/// as a small secondary bound so a three-line scaffold with a word or two
/// under each date does not flip a note's handling; below it, the dominant
/// vault shape is "many short dated files" (topology A), which never hits the
/// oversized-context paths this flag guards.
pub const LOG_AUTODETECT_MIN_WORDS: i64 = 100;

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
    /// Per-chunk log-entry dates (millis), aligned index-for-index with
    /// `chunks`. `Some` only for chunks cut from a dated append-log entry;
    /// `None` everywhere else. Always the same length as `chunks`.
    pub chunk_entry_dates: Vec<Option<i64>>,
    pub frontmatter: Option<Frontmatter>,
    /// True when this document is a single-file append-only log (topology B):
    /// an internal timeline of dated entries under one document date. Drives
    /// chronological chunking and recency-biased context. Declared by a config
    /// `log "name"` line naming this document, else auto-detected structurally.
    pub is_log: bool,
    /// The resolved log structure when `is_log` is true — detection runs
    /// exactly once, at process time, and every downstream consumer (chunking
    /// here, the indexing pipeline's recency context) reads this field so all
    /// of them agree on the same entry boundaries. Never re-detect downstream.
    ///
    /// `is_log == true` with `append_log == None` happens only for a declared
    /// log with an empty body; [`segment_declared_log`] otherwise always finds
    /// entries, down to paragraphs. When `is_log` is false this is always
    /// `None`.
    pub append_log: Option<AppendLog>,
}

/// Orchestrates the full document-processing pipeline:
/// read -> frontmatter -> extract entities -> chunk -> hash.
pub struct DocumentProcessor {
    vault_path: PathBuf,
    chunk_size: usize,
    frontmatter_link_fields: HashSet<String>,
    declared_logs: HashSet<String>,
}

impl DocumentProcessor {
    /// Create a processor for the given vault directory.
    pub fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path,
            chunk_size: DEFAULT_CHUNK_SIZE_CHARS,
            frontmatter_link_fields: HashSet::new(),
            declared_logs: HashSet::new(),
        }
    }

    /// Override the chunk size (in characters).
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Promote plain string values from these frontmatter fields as generic
    /// link entities. Literal wikilinks remain recognized in every field.
    pub fn with_frontmatter_link_fields(
        mut self,
        fields: impl IntoIterator<Item = String>,
    ) -> Self {
        self.frontmatter_link_fields = fields
            .into_iter()
            .map(|field| field.trim().to_lowercase())
            .filter(|field| !field.is_empty())
            .collect();
        self
    }

    /// Treat documents whose log entity name (see [`log_entity_name`]) is in
    /// `names` as declared logs: always flagged, and segmented structurally
    /// when their dates don't auto-detect. Names are config `log` names,
    /// already lowercased.
    pub fn with_declared_logs(mut self, names: &HashSet<String>) -> Self {
        self.declared_logs = names.clone();
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
        let mut frontmatter = parse_frontmatter(&content);
        if let Some(frontmatter) = frontmatter.as_mut() {
            frontmatter.promote_link_fields(&self.frontmatter_link_fields);
        }

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
        let frontmatter_links = frontmatter
            .as_ref()
            .map(|fm| fm.links.clone())
            .unwrap_or_default();
        let ExtractedEntities {
            tags,
            mut links,
            folders,
        } = extract_entities(body, path, &frontmatter_tags);

        // Merge frontmatter links (people, author, related, etc.) into the link list
        for fm_link in &frontmatter_links {
            if !links.contains(fm_link) {
                links.push(fm_link.clone());
            }
        }
        links.sort();

        // Readwise detection — used for both chunking and date resolution
        let is_readwise = is_readwise_content(body);

        // Word and char counts (from body, not frontmatter YAML)
        let word_count = body.split_whitespace().count() as i64;
        let char_count = body.chars().count() as i64;

        // Append-only-log detection (topology B), computed exactly once here
        // and threaded downstream via `ProcessedDocument::append_log`. A
        // config-declared log is always a log: strict date detection first,
        // then structural segmentation. Otherwise auto-detect strictly,
        // requiring MIN_LOG_ENTRIES entries with real bodies plus a modest
        // word floor so stub notes that merely list dated lines never flag.
        let declared = !self.declared_logs.is_empty()
            && log_name(title.as_deref(), &source_ref)
                .is_some_and(|name| self.declared_logs.contains(&name));
        let detected = detect_append_log(body);
        let (is_log, append_log) = if declared {
            (true, detected.or_else(|| segment_declared_log(body)))
        } else {
            let is_log = detected.as_ref().is_some_and(|log| {
                entries_with_body(body, log) >= MIN_LOG_ENTRIES
                    && word_count >= LOG_AUTODETECT_MIN_WORDS
            });
            (is_log, if is_log { detected } else { None })
        };

        // Chunks — Readwise highlights and append-only logs get structure-aware
        // chunking; everything else uses the generic paragraph chunker. Log
        // chunks carry their entry's date so downstream era logic can see the
        // file's internal timeline instead of one document date. A declared
        // log wins over Readwise highlight grouping: the user asked for entries.
        let (chunks, chunk_entry_dates) = if is_readwise && !declared {
            let highlights = extract_highlights(body);
            let chunks = if highlights.is_empty() {
                chunk_document(body, self.chunk_size)
            } else {
                chunk_readwise_highlights(&highlights, 5, 15)
            };
            let dates = vec![None; chunks.len()];
            (chunks, dates)
        } else if let Some(log) = append_log.as_ref() {
            let dated = chunk_append_log_dated(body, log, self.chunk_size);
            let dates = dated.iter().map(|c| c.entry_date).collect();
            let chunks = dated.into_iter().map(|c| c.content).collect();
            (chunks, dates)
        } else {
            let chunks = chunk_document(body, self.chunk_size);
            let dates = vec![None; chunks.len()];
            (chunks, dates)
        };

        // Dates
        let (created_at, created_source) =
            resolve_created_date(&frontmatter, path, &abs_path, body, is_readwise)?;
        let (modified_at, modified_source) =
            resolve_modified_date(&frontmatter, &abs_path, body, is_readwise)?;

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
            chunk_entry_dates,
            frontmatter,
            is_log,
            append_log,
        })
    }

    /// Process multiple files in parallel using rayon.
    pub fn process_all(&self, paths: &[PathBuf]) -> Vec<Result<ProcessedDocument>> {
        paths
            .par_iter()
            .map(|path| self.process_file(path))
            .collect()
    }

    /// Process named-root discovery results in parallel while preserving their
    /// discovery order in the returned vector.
    pub fn process_all_discovered_with_link_fields(
        files: &[DiscoveredFile],
        fields: &[String],
        declared_logs: &HashSet<String>,
    ) -> Vec<Result<ProcessedDocument>> {
        files
            .par_iter()
            .map(|file| {
                Self::process_discovered_with_link_fields(
                    file,
                    fields.iter().cloned(),
                    declared_logs,
                )
            })
            .collect()
    }

    /// Process a file returned by named-root discovery. The configured root
    /// name becomes the leading textual folder only when source refs are
    /// namespaced (multi-root mode); single-root behavior stays byte-compatible.
    pub fn process_discovered(
        file: &DiscoveredFile,
        declared_logs: &HashSet<String>,
    ) -> Result<ProcessedDocument> {
        Self::process_discovered_with_link_fields(file, std::iter::empty::<String>(), declared_logs)
    }

    /// Process a discovered file using configured plain frontmatter link
    /// fields. This static form keeps named-root callers easy to parallelize.
    pub fn process_discovered_with_link_fields(
        file: &DiscoveredFile,
        fields: impl IntoIterator<Item = String>,
        declared_logs: &HashSet<String>,
    ) -> Result<ProcessedDocument> {
        let mut document = Self::new(file.root_path.clone())
            .with_frontmatter_link_fields(fields)
            .with_declared_logs(declared_logs)
            .process_file(&file.relative_path)?;
        let relative = file.relative_path.to_string_lossy().replace('\\', "/");
        if document.source_ref != file.source_ref {
            let prefix = file
                .source_ref
                .strip_suffix(&relative)
                .unwrap_or("")
                .trim_end_matches('/');
            if !prefix.is_empty() {
                document.folders = if document.folders.is_empty() {
                    vec![prefix.to_string()]
                } else {
                    document
                        .folders
                        .into_iter()
                        .map(|folder| format!("{prefix}/{folder}"))
                        .collect()
                };
            }
            document.source_ref = file.source_ref.clone();
        }
        Ok(document)
    }
}

/// The entity name a log document is known by: its title (or filename stem),
/// lowercased. Both the config `log:` grammar and scan's write-back use this,
/// so a `log:` entry always matches what indexing would promote.
pub fn log_entity_name(doc: &ProcessedDocument) -> Option<String> {
    log_name(doc.title.as_deref(), &doc.source_ref)
}

fn log_name(title: Option<&str>, source_ref: &str) -> Option<String> {
    let name = title
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(source_ref)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })?;
    let normalized = name.trim().to_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

/// Resolve the created-at timestamp.
/// Priority: frontmatter → filename/path date → content date (Readwise) → filesystem.
fn resolve_created_date(
    frontmatter: &Option<Frontmatter>,
    rel_path: &Path,
    abs_path: &Path,
    body: &str,
    is_readwise: bool,
) -> Result<(i64, DateSource)> {
    if let Some(fm) = frontmatter {
        if let Some(dt) = fm.created {
            return Ok((datetime_to_millis(dt), DateSource::Frontmatter));
        }
    }

    // Try extracting a date from the file path/name.
    if let Some(dt) = extract_date_from_path(rel_path) {
        return Ok((datetime_to_millis(dt), DateSource::Filename));
    }

    // For Readwise-like content, use the earliest content date as creation date
    // rather than the filesystem timestamp (which reflects import time).
    if is_readwise {
        if let Some(dt) = extract_earliest_content_date(body) {
            return Ok((datetime_to_millis(dt), DateSource::Content));
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

/// Resolve the modified-at timestamp.
/// Priority: frontmatter → content date (Readwise) → filesystem.
fn resolve_modified_date(
    frontmatter: &Option<Frontmatter>,
    abs_path: &Path,
    body: &str,
    is_readwise: bool,
) -> Result<(i64, DateSource)> {
    if let Some(fm) = frontmatter {
        if let Some(dt) = fm.modified {
            return Ok((datetime_to_millis(dt), DateSource::Frontmatter));
        }
    }

    // For Readwise-like content, use the most recent content date.
    if is_readwise {
        if let Some(dt) = extract_latest_content_date(body) {
            return Ok((datetime_to_millis(dt), DateSource::Content));
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

// ---------------------------------------------------------------------------
// Content date extraction (Readwise / highlight-based files)
// ---------------------------------------------------------------------------

/// Collect all dates from content: `>Last-Highlighted-Date:: [[YYYY-MM-DD]]`
/// and generic `[[YYYY-MM-DD]]` wikilinks.
fn collect_content_dates(body: &str) -> Vec<DateTime<Utc>> {
    let mut dates = Vec::new();

    // Scan for [[YYYY-MM-DD]] patterns (covers both standalone date links
    // and Last-Highlighted-Date:: [[YYYY-MM-DD]]).
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 13 <= bytes.len() {
        // Look for `[[` followed by a 10-char date and `]]`
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if i + 14 <= bytes.len() && bytes[i + 12] == b']' && bytes[i + 13] == b']' {
                let date_str = &body[i + 2..i + 12];
                if let Some(dt) = parse_ymd(date_str) {
                    dates.push(dt);
                }
                i += 14;
                continue;
            }
        }
        i += 1;
    }

    dates
}

/// Most recent content date — used for modified timestamp.
fn extract_latest_content_date(body: &str) -> Option<DateTime<Utc>> {
    collect_content_dates(body).into_iter().max()
}

/// Earliest content date — used for created timestamp on Readwise files
/// where filesystem date reflects import time, not highlight time.
fn extract_earliest_content_date(body: &str) -> Option<DateTime<Utc>> {
    collect_content_dates(body).into_iter().min()
}

/// Parse a `YYYY-MM-DD` string into a UTC datetime at midnight.
fn parse_ymd(s: &str) -> Option<DateTime<Utc>> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|ndt| DateTime::from_naive_utc_and_offset(ndt, Utc))
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
    fn test_process_configured_plain_frontmatter_link_field() {
        let (_dir, vault) = create_test_vault();
        fs::write(
            vault.join("meeting.md"),
            "---\npeople: [Jay Vidyarthi, Stefan Leßle]\n---\nMeeting notes.\n",
        )
        .unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_frontmatter_link_fields(["people".to_string()])
            .process_file(Path::new("meeting.md"))
            .unwrap();

        assert_eq!(
            doc.links,
            vec!["jay vidyarthi".to_string(), "stefan leßle".to_string()]
        );
        assert_eq!(doc.frontmatter.unwrap().links, doc.links);
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
        // Date should be extracted from the filename
        assert_eq!(doc.created_source, DateSource::Filename);
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

        let source_refs: Vec<String> = results.into_iter().map(|r| r.unwrap().source_ref).collect();
        assert!(source_refs.contains(&"a.md".to_string()));
        assert!(source_refs.contains(&"b.md".to_string()));
        assert!(source_refs.contains(&"c.md".to_string()));
    }

    #[test]
    fn test_process_all_discovered_preserves_order_and_link_fields() {
        let (_dir, vault) = create_test_vault();
        fs::write(vault.join("a.md"), "---\npeople: [Ada]\n---\n# A").unwrap();
        fs::write(vault.join("b.md"), "---\npeople: [Grace]\n---\n# B").unwrap();
        let discovered = ["b.md", "a.md"].map(|name| DiscoveredFile {
            root_name: "notes".to_string(),
            root_path: vault.clone(),
            relative_path: PathBuf::from(name),
            source_ref: name.to_string(),
            writable: true,
        });

        let results = DocumentProcessor::process_all_discovered_with_link_fields(
            &discovered,
            &["people".to_string()],
            &HashSet::new(),
        );
        let docs: Vec<_> = results.into_iter().collect::<Result<_>>().unwrap();

        assert_eq!(
            docs.iter()
                .map(|doc| doc.source_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["b.md", "a.md"]
        );
        assert_eq!(docs[0].links, vec!["grace"]);
        assert_eq!(docs[1].links, vec!["ada"]);
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
    fn test_readwise_content_dates() {
        let (_dir, vault) = create_test_vault();
        let content = "\
---
title: Some Article
---
>Title:: Some Article
>Author:: John Doe
>Last-Highlighted-Date:: [[2024-01-15]]

## Highlights
First highlight from [[2023-11-02]]
^rw100
Second highlight from [[2024-01-15]]
^rw200
";
        fs::write(vault.join("article.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("article.md")).unwrap();

        // Created should use earliest content date (2023-11-02)
        assert_eq!(doc.created_source, DateSource::Content);
        let created = DateTime::from_timestamp_millis(doc.created_at).unwrap();
        assert_eq!(created.format("%Y-%m-%d").to_string(), "2023-11-02");

        // Modified should use latest content date (2024-01-15)
        assert_eq!(doc.modified_source, DateSource::Content);
        let modified = DateTime::from_timestamp_millis(doc.modified_at).unwrap();
        assert_eq!(modified.format("%Y-%m-%d").to_string(), "2024-01-15");
    }

    #[test]
    fn test_readwise_frontmatter_takes_priority() {
        let (_dir, vault) = create_test_vault();
        let content = "\
---
title: Article With Dates
created: 2023-06-01
modified: 2024-02-01
---
>Last-Highlighted-Date:: [[2024-01-15]]

## Highlights
Some highlight [[2023-11-02]]
^rw100
Another highlight [[2024-01-15]]
^rw200
";
        fs::write(vault.join("dated.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("dated.md")).unwrap();

        // Frontmatter should win over content dates
        assert_eq!(doc.created_source, DateSource::Frontmatter);
        assert_eq!(doc.modified_source, DateSource::Frontmatter);
    }

    #[test]
    fn test_non_readwise_ignores_content_dates() {
        let (_dir, vault) = create_test_vault();
        // Not Readwise content (no ^rw markers)
        let content = "# Note\n\nReferencing [[2023-05-01]] and [[2024-01-15]] dates.\n";
        fs::write(vault.join("normal.md"), content).unwrap();

        let processor = DocumentProcessor::new(vault);
        let doc = processor.process_file(Path::new("normal.md")).unwrap();

        // Should fall through to filesystem, not content
        assert_eq!(doc.created_source, DateSource::Filesystem);
        assert_eq!(doc.modified_source, DateSource::Filesystem);
    }

    #[test]
    fn test_collect_content_dates_parsing() {
        let body = "Some text [[2024-03-15]] more [[not-a-date]] and [[2023-01-01]]";
        let dates = collect_content_dates(body);
        assert_eq!(dates.len(), 2);

        let earliest = dates.iter().min().unwrap();
        let latest = dates.iter().max().unwrap();
        assert_eq!(earliest.format("%Y-%m-%d").to_string(), "2023-01-01");
        assert_eq!(latest.format("%Y-%m-%d").to_string(), "2024-03-15");
    }

    // --- Append-only-log resolution (flag + threaded structure) ---

    #[test]
    fn test_autodetects_log_with_prose_entries() {
        let (_dir, vault) = create_test_vault();
        // Past the word floor, and each entry past MIN_LOG_CHUNK_CHARS.
        let filler = "word ".repeat(220);
        let content = ["2026-07-25", "2026-07-26", "2026-07-27"]
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>();
        fs::write(vault.join("journal.md"), &content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("journal.md"))
            .unwrap();
        assert!(doc.is_log);
        let log = doc
            .append_log
            .as_ref()
            .expect("structure is threaded through");
        assert_eq!(log.entries.len(), 3);
        // Entry-aligned chunking used the threaded structure.
        assert_eq!(doc.chunks.len(), 3);
    }

    #[test]
    fn test_autodetects_log_with_month_name_headers() {
        let (_dir, vault) = create_test_vault();
        let filler = "word ".repeat(40);
        let content = ["July 25, 2026", "Jul 26 2026", "27 July 2026"]
            .iter()
            .map(|d| format!("## {d}\n{filler}\n"))
            .collect::<String>();
        fs::write(vault.join("journal.md"), &content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("journal.md"))
            .unwrap();
        assert!(doc.is_log, "month-name headers should auto-detect");
        assert!(doc.append_log.is_some());
    }

    #[test]
    fn test_dated_wikilink_list_does_not_flag() {
        // Regression: a stub note listing dated backlinks has ≥3 monotonic
        // dated lines and can exceed the word floor, but no entry bodies.
        let (_dir, vault) = create_test_vault();
        let mut content = String::from("# Days I worked on this\n\n");
        for day in 10..40 {
            content.push_str(&format!(
                "- [[2026-06-{day}]] touched the parser again that day for a while\n"
            ));
        }
        assert!(content.split_whitespace().count() > 100);
        fs::write(vault.join("stub.md"), &content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("stub.md"))
            .unwrap();
        assert!(!doc.is_log, "dated backlink list must not auto-flag");
        assert!(doc.append_log.is_none());
    }

    #[test]
    fn test_short_dated_checklist_does_not_flag() {
        // Regression: a tiny dated checklist (real bodies, but far under the
        // word floor) stays a plain note.
        let (_dir, vault) = create_test_vault();
        let content = "\
## 2026-07-25
- [ ] call the bank

## 2026-07-26
- [ ] renew passport

## 2026-07-27
- [ ] water plants
";
        fs::write(vault.join("todos.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("todos.md"))
            .unwrap();
        assert!(!doc.is_log, "short dated checklist must not auto-flag");
        assert!(doc.append_log.is_none());
    }

    fn declared(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn test_declared_log_on_detectable_body_bypasses_floor() {
        // A config-declared log is honoured even below the auto-detect word
        // floor, and the structure is threaded through for entry-aligned
        // chunking.
        let (_dir, vault) = create_test_vault();
        let content = "\
## 2026-07-25
short one

## 2026-07-26
short two

## 2026-07-27
short three
";
        fs::write(vault.join("tiny-log.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_declared_logs(&declared(&["tiny-log"]))
            .process_file(Path::new("tiny-log.md"))
            .unwrap();
        assert!(doc.is_log);
        assert_eq!(doc.append_log.as_ref().unwrap().entries.len(), 3);
        // Short entries accumulate into one chunk dated by the newest entry.
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(
            doc.chunk_entry_dates[0],
            Some(
                chrono::NaiveDate::from_ymd_opt(2026, 7, 27)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis()
            )
        );
    }

    #[test]
    fn test_frontmatter_type_no_longer_declares_a_log() {
        let (_dir, vault) = create_test_vault();
        let content = "---\ntype: log\n---\n## 2026-07-25\nshort one\n\n## 2026-07-26\nshort two\n\n## 2026-07-27\nshort three\n";
        fs::write(vault.join("tiny-log.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("tiny-log.md"))
            .unwrap();
        assert!(!doc.is_log, "only config declarations bypass auto-detect");
    }

    #[test]
    fn test_declared_yymmdd_journal_segments_with_dates() {
        let (_dir, vault) = create_test_vault();
        let content = "\
# Journal 2026

261010 slow morning, wrote about the move.

261011 long walk, thinking about reflection as a habit.

261012 call with mom.
";
        fs::write(vault.join("journal-2026.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_declared_logs(&declared(&["journal-2026"]))
            .process_file(Path::new("journal-2026.md"))
            .unwrap();
        assert!(doc.is_log);
        let log = doc.append_log.as_ref().expect("segmented");
        assert_eq!(log.entries.len(), 3);
        assert_eq!(log.dated_entries(), 3);
        assert_eq!(
            log.entries[2].date,
            chrono::NaiveDate::from_ymd_opt(2026, 10, 12)
        );
        assert_eq!(doc.chunks.len(), 1);
        assert!(
            !doc.chunks[0].contains("# Journal"),
            "title preamble is dropped"
        );
    }

    #[test]
    fn test_declared_nested_heading_journal_segments_by_day() {
        let (_dir, vault) = create_test_vault();
        let content = "\
# 25
## 12
### 30
end of year.
### 31
new year's eve.
# 26
## 1
### 1
first day.
### 2
second day.
";
        fs::write(vault.join("journal-2024.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_declared_logs(&declared(&["journal-2024"]))
            .process_file(Path::new("journal-2024.md"))
            .unwrap();
        let log = doc.append_log.as_ref().expect("segmented");
        let dates: Vec<_> = log
            .entries
            .iter()
            .map(|e| e.date.unwrap().to_string())
            .collect();
        assert_eq!(
            dates,
            ["2025-12-30", "2025-12-31", "2026-01-01", "2026-01-02"]
        );
    }

    #[test]
    fn test_declared_readwise_export_segments_by_highlight() {
        let (_dir, vault) = create_test_vault();
        let content = "\
# Some Book

![rw-book-cover](https://example.com/cover.jpg)
>[!note]- Readwise Information
>Title:: Some Book
>Readwise-Source:: #Readwise/source/kindle

## Highlights
- First highlight. [Location 1](https://readwise.io/x?location=1) [[2023-02-21]]
  
  **Note**: my first note ^rw480419750

- Second highlight. [Location 2](https://readwise.io/x?location=2) [[2023-02-22]]
   ^rw480419751

- Third highlight, undated.
   ^rw480419752
";
        fs::write(vault.join("Some Book Highlights.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_declared_logs(&declared(&["some book highlights"]))
            .process_file(Path::new("Some Book Highlights.md"))
            .unwrap();
        let log = doc.append_log.as_ref().expect("segmented");
        assert_eq!(log.entries.len(), 3);
        assert_eq!(log.dated_entries(), 2);
        assert!(log.entries[2].date_inferred, "undated entry borrows a date");
        assert!(doc.chunks[0].contains("**Note**: my first note"));
        assert_eq!(doc.chunk_entry_dates.len(), doc.chunks.len());
    }

    #[test]
    fn test_non_log_chunk_entry_dates_align_as_none() {
        let (_dir, vault) = create_test_vault();
        let content = "Just a normal note.\n\nWith two paragraphs of prose.";
        fs::write(vault.join("plain.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .process_file(Path::new("plain.md"))
            .unwrap();
        assert!(!doc.is_log);
        assert_eq!(doc.chunk_entry_dates.len(), doc.chunks.len());
        assert!(doc.chunk_entry_dates.iter().all(Option::is_none));
    }

    #[test]
    fn test_declared_log_without_structure_uses_paragraphs() {
        // Declared, but no headings, list items or dates repeat: paragraphs
        // become undated entries, accumulated into one chunk.
        let (_dir, vault) = create_test_vault();
        let content = "\
Monday — planning session, long discussion.

Wednesday — shipped the thing.

Friday — retro notes without any parseable dates.
";
        fs::write(vault.join("undated-log.md"), content).unwrap();

        let doc = DocumentProcessor::new(vault)
            .with_declared_logs(&declared(&["undated-log"]))
            .process_file(Path::new("undated-log.md"))
            .unwrap();
        assert!(doc.is_log, "the declared flag is honoured");
        let log = doc.append_log.as_ref().expect("paragraph entries");
        assert_eq!(log.entries.len(), 3);
        assert_eq!(log.dated_entries(), 0);
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunk_entry_dates, vec![None]);
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
