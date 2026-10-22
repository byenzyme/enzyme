use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::config::NotesRootConfig;
use crate::db::Database;
use crate::document::{
    DiscoveredFile, DocumentProcessor, FileDiscovery, ProcessedDocument, most_recent_entry,
};
use crate::models::{
    EntityOccurrence, EntityTrends, EntityType, INLINE_CONTEXT_MAX_WORDS, ROOT_FOLDER_ENTITY,
    representative_excerpt,
};
use crate::pipeline::entity_map::{
    coverage_recency_score, coverage_recency_weight, is_recent_coverage_occurrence,
};

/// Orchestrates the full indexing pipeline:
/// discover files -> process -> diff against DB -> upsert new/changed ->
/// delete removed -> extract entities -> compute trending scores.
pub struct IndexingPipeline<'a> {
    db: &'a Database,
    vault_path: PathBuf,
    /// User "leave it alone" folders that must not be indexed (the index gate).
    /// `None` means "resolve from the vault config at run time"; `Some` is an
    /// explicit override (used by tests and callers that already hold the list).
    excluded_folders: Option<Vec<String>>,
    /// Config-declared `log:` entity names. `None` means "resolve from the
    /// vault config at run time"; `Some` is an explicit override (tests).
    configured_logs: Option<std::collections::HashSet<String>>,
    /// Plain frontmatter fields promoted to generic link entities. `None`
    /// resolves the active vault/workspace config at run time.
    frontmatter_link_fields: Option<std::collections::HashSet<String>>,
    notes_roots: Option<Vec<NotesRootConfig>>,
    /// The vault's `as of` clause for trend computation. `None` resolves the
    /// vault config at run time; `Some(None)` explicitly uses the wall clock.
    as_of: Option<Option<enzyme_spec::AsOf>>,
}

/// Cap on entity occurrences emitted per config-declared log. Keeps a
/// years-long daily journal from flooding the occurrence table; the newest
/// entries are kept, and the drop is logged.
const MAX_LOG_OCCURRENCES: usize = 1000;
const FRONTMATTER_LINK_FIELDS_STATE_KEY: &str = "index.frontmatter_link_fields.v1";
fn is_markdown_source_ref(source_ref: &str) -> bool {
    !source_ref.starts_with("sqlite:")
        && std::path::Path::new(source_ref)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Summary statistics from an indexing run.
pub struct IndexingStats {
    pub files_discovered: usize,
    pub files_new: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub entities_extracted: usize,
    /// True when unchanged Markdown was re-read to apply a changed
    /// `frontmatter_link_fields` interpretation without rebuilding embeddings.
    pub entity_config_changed: bool,
    pub duration_ms: u64,
}

/// Read-only freshness of the Markdown sources against the index: what an
/// incremental index run would pick up, judged by file presence and mtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MarkdownFreshness {
    pub files_discovered: usize,
    /// On disk but not in the index.
    pub files_new: usize,
    /// Modified on disk after the index last wrote them.
    pub files_modified: usize,
    /// In the index but no longer on disk (or now excluded).
    pub files_deleted: usize,
    pub stale: bool,
}

impl<'a> IndexingPipeline<'a> {
    pub fn new(db: &'a Database, vault_path: PathBuf) -> Self {
        Self {
            db,
            vault_path,
            excluded_folders: None,
            configured_logs: None,
            frontmatter_link_fields: None,
            notes_roots: None,
            as_of: None,
        }
    }

    /// Explicitly set the reading's `as of` clause, bypassing config resolution.
    pub fn with_as_of(mut self, as_of: Option<enzyme_spec::AsOf>) -> Self {
        self.as_of = Some(as_of);
        self
    }

    /// Trend scores decay from this vault's reference time. Absent or unreadable
    /// config keeps the wall clock, like the other config-derived settings.
    fn resolve_as_of(&self) -> Option<enzyme_spec::AsOf> {
        match &self.as_of {
            Some(as_of) => as_of.clone(),
            None => crate::config::enzyme_config::EnzymeConfig::load(&self.vault_path)
                .ok()
                .flatten()
                .and_then(|config| config.reading_spec)
                .and_then(|spec| spec.learning.as_of),
        }
    }

    /// Use explicit named Markdown roots. A one-root list preserves historical
    /// relative source refs; two or more roots are namespaced by root name.
    pub fn with_notes_roots(mut self, roots: Vec<NotesRootConfig>) -> Self {
        self.notes_roots = Some(roots);
        self
    }

    /// Explicitly set the config-declared log names, bypassing config
    /// resolution. Mainly for tests.
    pub fn with_configured_logs(mut self, names: std::collections::HashSet<String>) -> Self {
        self.configured_logs = Some(names);
        self
    }

    /// Explicitly select plain frontmatter fields that become link entities.
    /// Mainly for tests and callers that already loaded an explicit config.
    pub fn with_frontmatter_link_fields(
        mut self,
        fields: impl IntoIterator<Item = String>,
    ) -> Self {
        self.frontmatter_link_fields = Some(
            fields
                .into_iter()
                .map(|field| field.trim().to_lowercase())
                .filter(|field| !field.is_empty())
                .collect(),
        );
        self
    }

    /// Explicitly set the "leave it alone" folders to exclude from indexing,
    /// bypassing config resolution. Mainly for tests and callers that already
    /// hold the list.
    pub fn with_excluded_folders(mut self, names: Vec<String>) -> Self {
        self.excluded_folders = Some(names);
        self
    }

    /// The folders to exclude from the index gate: the explicit override if set,
    /// otherwise the vault's persisted `config.excluded_folders`. Any config-load
    /// error resolves to "no extra excludes" so indexing never fails on a bad or
    /// absent config.
    fn resolve_excluded_folders(&self) -> Vec<String> {
        if let Some(names) = &self.excluded_folders {
            return names.clone();
        }
        crate::config::enzyme_config::EnzymeConfig::excluded_folders_at(&self.vault_path)
    }

    fn resolve_frontmatter_link_fields(&self) -> std::collections::HashSet<String> {
        if let Some(fields) = &self.frontmatter_link_fields {
            return fields.clone();
        }
        crate::config::enzyme_config::EnzymeConfig::load(&self.vault_path)
            .ok()
            .flatten()
            .map(|config| config.frontmatter_link_fields.into_iter().collect())
            .unwrap_or_default()
    }

    fn frontmatter_link_fields_state(
        &self,
        fields: &std::collections::HashSet<String>,
    ) -> Result<(String, bool)> {
        let mut fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        fields.sort_unstable();
        let current = serde_json::to_string(&fields)?;
        let stored = self.db.get_config(FRONTMATTER_LINK_FIELDS_STATE_KEY)?;
        let changed = match stored {
            Some(stored) => stored != current,
            None => !fields.is_empty(),
        };
        Ok((current, changed))
    }

    /// Compare the Markdown sources with the index without reading file
    /// contents or writing anything (cheap enough for `status`). Uses file
    /// mtime against the document's `indexed_at`, so an edit that preserves
    /// an older mtime is not reported here; `refresh` still hashes every file.
    pub fn freshness(&self) -> Result<MarkdownFreshness> {
        let discovery = match &self.notes_roots {
            Some(roots) => FileDiscovery::from_notes_roots(roots.clone()),
            None => FileDiscovery::new(self.vault_path.clone()),
        }
        .with_excluded_folders(self.resolve_excluded_folders());
        let files = discovery.discover_named()?;
        let indexed: HashMap<String, i64> = self
            .db
            .query_read("SELECT source_ref, indexed_at FROM docs", vec![], |row| {
                Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
            })?
            .into_iter()
            .filter(|(source_ref, _)| is_markdown_source_ref(source_ref))
            .collect();
        let mut freshness = MarkdownFreshness {
            files_discovered: files.len(),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        for file in &files {
            seen.insert(file.source_ref.as_str());
            let Some(indexed_at) = indexed.get(&file.source_ref) else {
                freshness.files_new += 1;
                continue;
            };
            let modified_ms = std::fs::metadata(file.root_path.join(&file.relative_path))
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_millis() as i64);
            if modified_ms.is_some_and(|modified| modified > *indexed_at) {
                freshness.files_modified += 1;
            }
        }
        freshness.files_deleted = indexed
            .keys()
            .filter(|source_ref| !seen.contains(source_ref.as_str()))
            .count();
        freshness.stale =
            freshness.files_new + freshness.files_modified + freshness.files_deleted > 0;
        Ok(freshness)
    }

    /// Execute the indexing pipeline.
    ///
    /// When `full` is `true`, every file is re-processed regardless of whether
    /// it has changed. When `false`, only new and changed files are processed.
    ///
    /// The `on_progress` callback is invoked with human-readable status
    /// messages at each major stage.
    pub fn run(&self, full: bool, on_progress: impl Fn(&str)) -> Result<IndexingStats> {
        let start = Instant::now();
        self.db.ensure_chunk_fts()?;

        // Step 1: Discover files. The user's `exclude` list feeds the index gate
        // here, so excluded folders are never read. Built-in structural folders
        // (templates, excalidraw, …) are deliberately NOT excluded here — they
        // stay indexed and are only suppressed at the surface gate.
        on_progress("Discovering files...");
        let discovery = match &self.notes_roots {
            Some(roots) => FileDiscovery::from_notes_roots(roots.clone()),
            None => FileDiscovery::new(self.vault_path.clone()),
        }
        .with_excluded_folders(self.resolve_excluded_folders());
        let all_files = discovery.discover_named()?;
        let existing_hashes = self.db.get_all_document_hashes()?;
        let db_doc_count = existing_hashes.len();
        let db_markdown_count = existing_hashes
            .keys()
            .filter(|source_ref| is_markdown_source_ref(source_ref))
            .count();
        if all_files.is_empty() && db_doc_count > 0 && db_markdown_count == 0 {
            on_progress("Using DB-ingested documents...");
            self.compute_trending_scores()?;
            let duration_ms = start.elapsed().as_millis() as u64;
            info!(
                db_doc_count,
                duration_ms, "no files discovered; preserving DB-ingested corpus"
            );
            return Ok(IndexingStats {
                files_discovered: 0,
                files_new: 0,
                files_updated: 0,
                files_deleted: 0,
                entities_extracted: 0,
                entity_config_changed: false,
                duration_ms,
            });
        }
        let files_discovered = all_files.len();
        info!(files_discovered, "files discovered");
        let frontmatter_link_fields = self.resolve_frontmatter_link_fields();
        let (frontmatter_link_fields_state, entity_config_changed) =
            self.frontmatter_link_fields_state(&frontmatter_link_fields)?;

        // Step 3: Build a lookup from source_ref -> PathBuf for all discovered files
        let all_source_refs: HashMap<String, DiscoveredFile> = all_files
            .iter()
            .map(|file| (file.source_ref.clone(), file.clone()))
            .collect();

        // Step 4: Diff — determine which files are new, changed, or deleted
        let mut paths_to_process: Vec<DiscoveredFile> = Vec::new();
        let files_new;
        let mut files_updated = 0usize;

        if full {
            // Full mode: process everything
            paths_to_process = all_files.clone();
            files_new = paths_to_process.len();
        } else {
            // Incremental mode: hash every previously indexed file. Filesystems,
            // sync tools, and backup restores can preserve mtimes while replacing
            // bytes; using mtime as a gate makes those edits permanently stale.
            // Hashing is parallel and unchanged files still avoid the expensive
            // parse/chunk/entity/upsert stages below.
            let mut new_paths = Vec::new();
            let mut candidates_to_check: Vec<(DiscoveredFile, &str)> = Vec::new();

            for file in &all_files {
                match existing_hashes.get(file.source_ref.as_str()) {
                    None => {
                        new_paths.push(file.clone());
                    }
                    Some((old_hash, _)) => {
                        candidates_to_check.push((file.clone(), old_hash.as_str()));
                    }
                }
            }

            files_new = new_paths.len();
            paths_to_process.extend(new_paths);

            // Read and hash in parallel to confirm byte-level changes.
            let changed: Vec<DiscoveredFile> = candidates_to_check
                .par_iter()
                .filter_map(|(file, old_hash)| {
                    let abs_path = file.root_path.join(&file.relative_path);
                    let content = std::fs::read_to_string(&abs_path).ok()?;
                    let mut hasher = Sha256::new();
                    hasher.update(content.as_bytes());
                    let current_hash = format!("{:x}", hasher.finalize());
                    if current_hash != *old_hash {
                        Some(file.clone())
                    } else {
                        None
                    }
                })
                .collect();

            files_updated = changed.len();
            paths_to_process.extend(changed);
        }

        // Find deleted files (present in DB but no longer on disk)
        let deleted_paths: Vec<String> = existing_hashes
            .keys()
            .filter(|source_ref| is_markdown_source_ref(source_ref))
            .filter(|source_ref| !all_source_refs.contains_key(*source_ref))
            .cloned()
            .collect();
        let files_deleted = deleted_paths.len();

        // Step 5: Process new + changed files. A frontmatter field-mapping
        // change re-reads every Markdown document for entity extraction only;
        // unchanged documents are not upserted, so their chunks and embeddings
        // stay intact.
        let normal_source_refs: std::collections::HashSet<&str> = paths_to_process
            .iter()
            .map(|file| file.source_ref.as_str())
            .collect();
        let entity_paths = if entity_config_changed && !full {
            &all_files
        } else {
            &paths_to_process
        };
        on_progress(&format!("Processing {} files...", entity_paths.len()));
        let configured_logs = self.resolve_configured_logs();
        let results: Vec<_> = entity_paths
            .par_iter()
            .map(|file| {
                DocumentProcessor::process_discovered_with_link_fields(
                    file,
                    frontmatter_link_fields.iter().cloned(),
                    &configured_logs,
                )
            })
            .collect();

        let mut processed_docs: Vec<ProcessedDocument> = Vec::new();
        let mut processing_failures = 0usize;
        for result in results {
            match result {
                Ok(doc) => processed_docs.push(doc),
                Err(e) => {
                    processing_failures += 1;
                    warn!("Failed to process file: {}", e);
                }
            }
        }
        let docs_to_upsert: Vec<ProcessedDocument> = if entity_config_changed && !full {
            processed_docs
                .iter()
                .filter(|doc| normal_source_refs.contains(doc.source_ref.as_str()))
                .cloned()
                .collect()
        } else {
            processed_docs.clone()
        };

        // Step 6: Upsert processed documents into the database (single transaction)
        on_progress("Upserting documents...");
        self.db.upsert_documents_batch(&docs_to_upsert)?;

        // Step 7: Delete removed files from the database
        if !deleted_paths.is_empty() {
            on_progress(&format!(
                "Deleting {} removed files...",
                deleted_paths.len()
            ));
            self.db.delete_documents_by_paths(&deleted_paths)?;
        }

        // Step 8: Extract entities from processed documents (single transaction)
        on_progress("Extracting entities...");
        let entity_entries = Self::collect_entity_entries(&processed_docs);
        let entities_extracted = entity_entries.len();
        {
            let refs: Vec<(&str, &EntityType, &EntityOccurrence)> = entity_entries
                .iter()
                .map(|(name, etype, occ)| (name.as_str(), etype, occ))
                .collect();
            self.db.upsert_entities_batch(&refs)?;
        }

        // Step 8b: Reconcile config-declared `log:` entities (replace
        // semantics, promotion of newly declared logs, removal of undeclared).
        on_progress("Reconciling log entities...");
        self.reconcile_log_entities(&processed_docs, &all_files, &on_progress)?;

        // Step 8c: Drop entities whose last occurrence is gone (their notes
        // were deleted, or an edit removed the tag/link). They have no
        // material to catalyze and would otherwise linger in selection.
        let pruned_entities = self.db.prune_entities_without_occurrences()?;
        if pruned_entities > 0 {
            debug!(
                pruned_entities,
                "pruned entities with no remaining occurrences"
            );
        }

        // Step 9: Compute trending scores for all entities (bulk fetch)
        on_progress("Computing trending scores...");
        self.compute_trending_scores()?;

        // Persist only after entity reconciliation succeeds. If a config-led
        // full entity pass could not read every file, leave the old state in
        // place so the next refresh retries instead of silently accepting a
        // partial interpretation.
        if !entity_config_changed || processing_failures == 0 {
            self.db.set_config(
                FRONTMATTER_LINK_FIELDS_STATE_KEY,
                &frontmatter_link_fields_state,
            )?;
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        info!(
            files_discovered,
            files_new,
            files_updated,
            files_deleted,
            entities_extracted,
            entity_config_changed,
            duration_ms,
            "indexing complete"
        );

        Ok(IndexingStats {
            files_discovered,
            files_new,
            files_updated,
            files_deleted,
            entities_extracted,
            entity_config_changed,
            duration_ms,
        })
    }

    /// Collect all entity entries from processed documents into a flat list
    /// suitable for `upsert_entities_batch`.
    ///
    /// Context hierarchy (matching enzyme-python):
    /// - Tag in frontmatter → full file body
    /// - Link in any frontmatter field → full file body
    /// - Folder entity → full file body (the file belongs to that folder)
    /// - Inline-only tag/link → 250-word neighborhood around the mention
    pub fn collect_entity_entries(
        docs: &[ProcessedDocument],
    ) -> Vec<(String, EntityType, EntityOccurrence)> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut entries = Vec::new();
        let flat_vault = !docs.is_empty() && docs.iter().all(|doc| doc.folders.is_empty());

        for doc in docs {
            // Pre-compute frontmatter sets for fast lookup
            let fm_tags: std::collections::HashSet<&str> = doc
                .frontmatter
                .as_ref()
                .map(|fm| fm.tags.iter().map(|t| t.as_str()).collect())
                .unwrap_or_default();

            let fm_links: std::collections::HashSet<&str> = doc
                .frontmatter
                .as_ref()
                .map(|fm| fm.links.iter().map(|l| l.as_str()).collect())
                .unwrap_or_default();

            // Context (frontmatter stripped) for document-level entities
            // (frontmatter tags/links, folders, root):
            //   - short body → stored whole (passes the inline-context filter);
            //   - append-only log → the most-recent entry, bounded — its
            //     highest-value, chronologically-current slice, which keeps the
            //     log in the query centroid without dumping the whole timeline;
            //   - other long note → `None`: the chunk-extractive reader (Tier-1)
            //     handles it, and a body excerpt would only pollute the link
            //     centroid, which wants mention-neighborhoods, not arbitrary
            //     passages.
            let full_body: Option<String> = doc.chunks.first().and_then(|_| {
                let body = crate::document::strip_frontmatter(&doc.content);
                if body.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS {
                    Some(body.to_string())
                } else {
                    // Log structure was resolved once at process time; a
                    // forced-on log with no detectable entries stays `None`
                    // here by design (see `ProcessedDocument::append_log`).
                    doc.append_log
                        .as_ref()
                        .map(|log| representative_excerpt(&most_recent_entry(body, log)))
                }
            });

            for tag in &doc.tags {
                let context = if fm_tags.contains(tag.as_str()) {
                    // Frontmatter tag → full file body
                    full_body.clone()
                } else {
                    // Inline-only tag → neighborhood context
                    Self::lasso_context_for_tag(&doc.content, tag)
                };

                entries.push((
                    tag.clone(),
                    EntityType::Tag,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context,
                        created_at: now,
                        effective_date: Some(doc.created_at),
                        date_source: doc.created_source.as_str().to_string(),
                        weight: 1.0,
                    },
                ));
            }

            for link in &doc.links {
                let context = if fm_links.contains(link.as_str()) {
                    // Frontmatter link → full file body
                    full_body.clone()
                } else {
                    // Inline-only link → neighborhood context
                    Self::lasso_context_for_wikilink(&doc.content, link)
                };

                entries.push((
                    link.clone(),
                    EntityType::Link,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context,
                        created_at: now,
                        effective_date: Some(doc.created_at),
                        date_source: doc.created_source.as_str().to_string(),
                        weight: 1.0,
                    },
                ));
            }

            for folder in &doc.folders {
                // Folder entity → always full file body
                entries.push((
                    folder.clone(),
                    EntityType::Folder,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context: full_body.clone(),
                        created_at: now,
                        effective_date: Some(doc.created_at),
                        date_source: doc.created_source.as_str().to_string(),
                        weight: 1.0,
                    },
                ));
            }

            if flat_vault {
                // Flat vaults have no path-derived folder entities. Treat the
                // vault root as the structural entity so selection has a
                // corpus-level target.
                entries.push((
                    ROOT_FOLDER_ENTITY.to_string(),
                    EntityType::Folder,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context: full_body.clone(),
                        created_at: now,
                        effective_date: Some(doc.created_at),
                        date_source: doc.created_source.as_str().to_string(),
                        weight: 1.0,
                    },
                ));
            }
        }

        entries
    }

    /// The occurrences a config-declared log contributes: one per dated entry,
    /// newest `MAX_LOG_OCCURRENCES` kept. Per-entry effective dates are what
    /// let the era machinery see the file's internal timeline, and frequency
    /// (= entry count, set by the replace call) makes a busy log rank by its
    /// real activity.
    ///
    /// An entry without a date (a declared log where nothing parsed) falls
    /// back to the document's own date.
    fn log_occurrences_for_doc(doc: &ProcessedDocument) -> Option<(String, Vec<EntityOccurrence>)> {
        let log = doc.append_log.as_ref()?;
        let name = crate::document::log_entity_name(doc)?;
        let now = chrono::Utc::now().timestamp_millis();
        let body = crate::document::strip_frontmatter(&doc.content);
        let mut log_entries = crate::document::log_entries(body, log);
        if log_entries.len() > MAX_LOG_OCCURRENCES {
            let dropped = log_entries.len() - MAX_LOG_OCCURRENCES;
            // Keep the newest entries, honouring append direction.
            match log.direction {
                crate::document::LogDirection::OldestFirst => {
                    log_entries.drain(..dropped);
                }
                crate::document::LogDirection::NewestFirst => {
                    log_entries.truncate(MAX_LOG_OCCURRENCES);
                }
            }
            info!(
                log = name.as_str(),
                dropped, "capped log entries at newest MAX_LOG_OCCURRENCES"
            );
        }
        let occurrences = log_entries
            .into_iter()
            .map(|(entry_date, text)| EntityOccurrence {
                id: None,
                entity_id: 0,
                file_path: doc.source_ref.clone(),
                context: Some(representative_excerpt(&text)),
                created_at: now,
                effective_date: Some(entry_date.unwrap_or(doc.created_at)),
                date_source: if entry_date.is_some() {
                    "content".to_string()
                } else {
                    doc.created_source.as_str().to_string()
                },
                weight: 1.0,
            })
            .collect();
        Some((name, occurrences))
    }

    /// Reconcile config-declared `log:` entities against the DB. Runs every
    /// indexing pass:
    /// - a removed `log:` line deletes its entity and occurrences;
    /// - a reprocessed log doc replaces its occurrences (never accumulates);
    /// - a newly added `log:` line promotes its (unchanged, already-indexed)
    ///   file by processing it directly.
    fn reconcile_log_entities(
        &self,
        processed_docs: &[ProcessedDocument],
        all_paths: &[DiscoveredFile],
        on_progress: &impl Fn(&str),
    ) -> Result<()> {
        let configured = self.resolve_configured_logs();
        let removed = self.db.delete_append_log_entities_not_in(&configured)?;
        if removed > 0 {
            info!(removed, "removed append-log entities no longer in config");
        }
        if configured.is_empty() {
            return Ok(());
        }

        let mut handled: std::collections::HashSet<String> = Default::default();
        for doc in processed_docs {
            if let Some((name, occurrences)) = Self::log_occurrences_for_doc(doc) {
                if configured.contains(&name) {
                    Self::report_log(doc, &name, on_progress);
                    self.db.replace_append_log_entity(&name, &occurrences)?;
                    handled.insert(name);
                }
            }
        }

        // Configured logs whose files did not change this run: promote only if
        // the entity does not exist yet (the `log:` line was just added).
        let existing: std::collections::HashSet<String> = self
            .db
            .query_read(
                "SELECT name FROM entities WHERE type = 'append_log'",
                vec![],
                |row| row.get::<String>(0),
            )?
            .into_iter()
            .collect();
        for name in &configured {
            if handled.contains(name) || existing.contains(name) {
                continue;
            }
            let candidate = all_paths.iter().find(|file| {
                file.relative_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.trim().to_lowercase() == *name)
            });
            let Some(path) = candidate else {
                warn!(
                    log = name.as_str(),
                    "configured log matches no vault file by name; leaving unpromoted"
                );
                continue;
            };
            match DocumentProcessor::process_discovered(path, &configured) {
                Ok(doc) => {
                    if let Some((doc_name, occurrences)) = Self::log_occurrences_for_doc(&doc) {
                        if doc_name == *name {
                            Self::report_log(&doc, name, on_progress);
                            self.db.replace_append_log_entity(name, &occurrences)?;
                        } else {
                            warn!(
                                log = name.as_str(),
                                resolves_to = doc_name.as_str(),
                                "configured log name does not match the file's title; leaving unpromoted"
                            );
                        }
                    } else {
                        let message = format!(
                            "Log \"{name}\": the file has no text to split into entries; leaving unpromoted"
                        );
                        warn!(log = name.as_str(), "{message}");
                        on_progress(&message);
                    }
                }
                Err(e) => {
                    warn!(log = name.as_str(), error = %e, "failed to process configured log")
                }
            }
        }
        Ok(())
    }

    /// Say how a declared log was read, so a low dated count is visible
    /// instead of silently flattening the file's timeline.
    fn report_log(doc: &ProcessedDocument, name: &str, on_progress: &impl Fn(&str)) {
        let Some(log) = doc.append_log.as_ref() else {
            return;
        };
        let message = format!(
            "Log \"{name}\": {} entries, {} dated",
            log.entries.len(),
            log.dated_entries()
        );
        info!(
            log = name,
            entries = log.entries.len(),
            dated = log.dated_entries(),
            "{message}"
        );
        on_progress(&message);
    }

    /// Config-declared `log:` entity names, lowercased. Any config-load error
    /// resolves to "no logs declared" so indexing never fails on a bad or
    /// absent config.
    fn resolve_configured_logs(&self) -> std::collections::HashSet<String> {
        if let Some(names) = &self.configured_logs {
            return names.clone();
        }
        crate::config::enzyme_config::EnzymeConfig::load(&self.vault_path)
            .ok()
            .flatten()
            .map(|cfg| {
                cfg.parse_preferences()
                    .entities
                    .into_iter()
                    .filter(|e| e.entity_type == EntityType::AppendLog)
                    .map(|e| e.name)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Extract ~250 words of context around each `#tag` occurrence, returning the
    /// first match. Falls back to the first 200 chars if the tag isn't found.
    fn lasso_context_for_tag(content: &str, tag: &str) -> Option<String> {
        let needle = format!("#{tag}");
        Self::lasso_around(content, &needle)
    }

    /// Extract ~250 words of context around a `[[wikilink]]` occurrence.
    fn lasso_context_for_wikilink(content: &str, target: &str) -> Option<String> {
        // Try [[target]] and [[target|...]] patterns
        let needle = format!("[[{target}");
        Self::lasso_around(content, &needle)
    }

    /// Find `needle` (case-insensitive) in `content` and return ~250 words around it.
    fn lasso_around(content: &str, needle: &str) -> Option<String> {
        let content_lower = content.to_lowercase();
        let needle_lower = needle.to_lowercase();

        let Some(byte_pos) = content_lower.find(&needle_lower) else {
            // Fallback: first 200 chars
            return Some(content.chars().take(200).collect());
        };

        let words: Vec<&str> = content.split_whitespace().collect();
        if words.is_empty() {
            return None;
        }

        // Find which word index the match falls in
        let mut char_offset = 0;
        let mut match_word_idx = 0;
        for (i, word) in words.iter().enumerate() {
            // Find this word's start in the original content
            if let Some(pos) = content[char_offset..].find(word) {
                let word_start = char_offset + pos;
                if word_start + word.len() > byte_pos {
                    match_word_idx = i;
                    break;
                }
                char_offset = word_start + word.len();
            }
        }

        let start = match_word_idx.saturating_sub(125);
        let end = (match_word_idx + 125).min(words.len());
        Some(words[start..end].join(" "))
    }

    /// Recompute trending scores for every entity in the database.
    ///
    /// Fetches all occurrence dates in a **single query**, groups by entity_id
    /// in memory, then batch-updates all trends in one transaction.
    fn compute_trending_scores(&self) -> Result<()> {
        let entities = self.db.get_all_entities()?;
        let as_of_ms = crate::pipeline::as_of::resolve(self.db, self.resolve_as_of().as_ref())?;
        let now_millis = as_of_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        // Single bulk fetch of all occurrence dates
        let all_occ_dates = self.db.get_all_occurrence_dates()?;

        // Group by entity_id and compute scores in one pass
        // (rows are ordered by entity_id from the query)
        struct Accum {
            weighted_sum: f64,
            frequency_recent: i64,
            timeline_buckets: HashMap<String, i64>,
            timeline_weights: HashMap<String, f64>,
        }

        let mut accums: HashMap<i64, Accum> = HashMap::new();

        for (entity_id, effective_date_opt, created_at, occurrence_weight) in &all_occ_dates {
            let effective_date = effective_date_opt.unwrap_or(*created_at);
            // Occurrences after a declared reference time do not exist yet;
            // without one, future dates keep today's weight.
            if as_of_ms.is_some_and(|reference| effective_date > reference) {
                continue;
            }
            let age_days = (now_millis.saturating_sub(effective_date).max(0) as f64)
                / (1000.0 * 60.0 * 60.0 * 24.0);

            let acc = accums.entry(*entity_id).or_insert_with(|| Accum {
                weighted_sum: 0.0,
                frequency_recent: 0,
                timeline_buckets: HashMap::new(),
                timeline_weights: HashMap::new(),
            });

            let weight = coverage_recency_weight(age_days);
            acc.weighted_sum += weight * occurrence_weight;

            if is_recent_coverage_occurrence(age_days) {
                acc.frequency_recent += 1;
            }

            if let Some(dt) = chrono::DateTime::from_timestamp_millis(effective_date) {
                let bucket = dt.format("%Y-%m").to_string();
                *acc.timeline_buckets.entry(bucket.clone()).or_insert(0) += 1;
                *acc.timeline_weights.entry(bucket).or_insert(0.0) += occurrence_weight;
            }
        }

        // Build batch update
        let updates: Vec<(i64, EntityTrends)> = entities
            .iter()
            .filter_map(|e| {
                let entity_id = e.id?;

                let acc = accums.remove(&entity_id);
                let (weighted_sum, frequency_recent, timeline_buckets, timeline_weights) = match acc
                {
                    Some(a) => (
                        a.weighted_sum,
                        a.frequency_recent,
                        a.timeline_buckets,
                        a.timeline_weights,
                    ),
                    None => (0.0, 0, HashMap::new(), HashMap::new()),
                };

                let recency_score = coverage_recency_score(weighted_sum, e.ranking_weight);

                Some((
                    entity_id,
                    EntityTrends {
                        frequency_recent,
                        recency_score,
                        timeline_buckets,
                        timeline_weights,
                    },
                ))
            })
            .collect();

        self.db.update_entity_trends_batch(&updates)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Catalyst;
    use crate::models::EntityType;
    use crate::pipeline::staleness::{
        StaleReason, StalenessResult, check_catalyst_staleness, store_staleness_bookkeeping,
    };
    use std::path::Path;

    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
    }

    /// Create a vault directory with 5 sample markdown files containing tags,
    /// links, and frontmatter.
    fn create_test_vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_md(
            dir.path(),
            "note-a.md",
            "---\ntitle: Note A\ntags:\n  - alpha\n  - shared\n---\n# Note A\nSome content about [[Topic One]].\n",
        );
        write_md(
            dir.path(),
            "note-b.md",
            "---\ntitle: Note B\n---\n# Note B\nReferences [[Topic Two]] and #beta.\n",
        );
        write_md(
            dir.path(),
            "note-c.md",
            "# Note C\nPlain file with #gamma and [[Topic One]].\n",
        );
        write_md(
            dir.path(),
            "note-d.md",
            "---\ntitle: Note D\ntags:\n  - delta\n---\n# Note D\nAnother document with #shared tag.\n",
        );
        write_md(
            dir.path(),
            "note-e.md",
            "# Note E\nFinal file with [[Topic Three]] and #alpha.\n",
        );
        dir
    }

    fn write_md(vault: &Path, name: &str, content: &str) {
        let path = vault.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    // -----------------------------------------------------------------------
    // 1. Full first-run indexes everything
    // -----------------------------------------------------------------------
    #[test]
    fn full_first_run_indexes_everything() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        let stats = pipeline.run(true, |_| {}).unwrap();

        assert_eq!(stats.files_discovered, 5);
        assert_eq!(stats.files_new, 5);

        assert_eq!(db.get_document_count().unwrap(), 5);

        let entities = db.get_all_entities().unwrap();
        assert!(!entities.is_empty(), "entities should have been extracted");

        // Trending scores should be non-zero for at least some entities
        let has_nonzero = entities.iter().any(|e| e.trends.recency_score > 0.0);
        assert!(
            has_nonzero,
            "at least one entity should have a non-zero recency score"
        );
    }

    #[test]
    fn named_multi_roots_are_indexed_without_materialization() {
        let db = test_db();
        let sessions = tempfile::tempdir().unwrap();
        let references = tempfile::tempdir().unwrap();
        write_md(sessions.path(), "shared.md", "# Session\n#session\n");
        write_md(references.path(), "shared.md", "# Reference\n#reference\n");
        let roots = vec![
            NotesRootConfig {
                name: "sessions".into(),
                path: sessions.path().to_path_buf(),
                document_ref_prefix: None,
                exclusions: Vec::new(),
                writable: true,
            },
            NotesRootConfig {
                name: "references".into(),
                path: references.path().to_path_buf(),
                document_ref_prefix: None,
                exclusions: Vec::new(),
                writable: false,
            },
        ];
        IndexingPipeline::new(&db, sessions.path().to_path_buf())
            .with_notes_roots(roots)
            .run(true, |_| {})
            .unwrap();
        let refs = db.get_all_source_refs().unwrap();
        assert!(refs.contains(&"sessions/shared.md".to_string()));
        assert!(refs.contains(&"references/shared.md".to_string()));
    }

    #[test]
    fn full_first_run_indexes_root_entity_for_flat_vault() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();
        write_md(vault.path(), "note-a.md", "# Note A\nPlain root note.\n");
        write_md(vault.path(), "note-b.md", "# Note B\nAnother root note.\n");

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        let stats = pipeline.run(true, |_| {}).unwrap();

        assert_eq!(stats.files_discovered, 2);

        let entities = db.get_all_entities().unwrap();
        let root = entities
            .iter()
            .find(|entity| {
                entity.entity_type == EntityType::Folder
                    && entity.name == crate::models::ROOT_FOLDER_ENTITY
            })
            .expect("flat vault should index the root folder entity");

        assert_eq!(root.frequency, 2);
    }

    // -----------------------------------------------------------------------
    // 2. Incremental no-op when nothing changed
    // -----------------------------------------------------------------------
    #[test]
    fn incremental_noop_when_nothing_changed() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let stats = pipeline.run(false, |_| {}).unwrap();

        assert_eq!(stats.files_discovered, 5);
        assert_eq!(stats.files_new, 0);
        assert_eq!(stats.files_updated, 0);
        assert_eq!(stats.entities_extracted, 0);
        assert!(!stats.entity_config_changed);
    }

    #[test]
    fn frontmatter_link_field_change_reconciles_unchanged_docs_without_reembedding() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();
        write_md(
            vault.path(),
            "meetings/one.md",
            "---\npeople: [Jay Vidyarthi, Stefan Leßle]\n---\nMeeting notes.\n",
        );

        let initial = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_frontmatter_link_fields(Vec::<String>::new());
        initial.run(true, |_| {}).unwrap();
        assert!(
            !db.get_all_entities()
                .unwrap()
                .iter()
                .any(|entity| entity.name == "jay vidyarthi")
        );

        let document = db.get_document_by_path("meetings/one.md").unwrap().unwrap();
        db.mark_document_embedded(
            document.id.unwrap(),
            &document.content_hash,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();

        let configured = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_frontmatter_link_fields(["people".to_string()]);
        let stats = configured.run(false, |_| {}).unwrap();
        assert_eq!(stats.files_new + stats.files_updated, 0);
        assert!(stats.entity_config_changed);
        let entities = db.get_all_entities().unwrap();
        assert!(entities.iter().any(|entity| {
            entity.name == "jay vidyarthi" && entity.entity_type == EntityType::Link
        }));
        assert!(entities.iter().any(|entity| {
            entity.name == "stefan leßle" && entity.entity_type == EntityType::Link
        }));
        let document = db.get_document_by_path("meetings/one.md").unwrap().unwrap();
        assert_eq!(
            document.embedded_content_hash.as_deref(),
            Some(document.content_hash.as_str()),
            "entity-only reconciliation must preserve the existing embedding receipt"
        );

        let removed = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_frontmatter_link_fields(Vec::<String>::new());
        let stats = removed.run(false, |_| {}).unwrap();
        assert!(stats.entity_config_changed);
        assert!(
            !db.get_all_entities()
                .unwrap()
                .iter()
                .any(|entity| entity.name == "jay vidyarthi")
        );
    }

    #[test]
    fn living_knowledge_base_reconciles_edit_delete_and_schema_change() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();
        write_md(
            vault.path(),
            "living.md",
            "---\ntitle: Living\npeople: [Ada Lovelace]\n---\nThe obsolete answer. #living\n",
        );
        write_md(
            vault.path(),
            "deleted.md",
            "# Temporary\nThis source must disappear. #temporary\n",
        );

        let initial = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_frontmatter_link_fields(Vec::<String>::new());
        initial.run(true, |_| {}).unwrap();
        let living = db.get_document_by_path("living.md").unwrap().unwrap();
        let deleted = db.get_document_by_path("deleted.md").unwrap().unwrap();
        let original_mtime = std::fs::metadata(vault.path().join("living.md"))
            .unwrap()
            .modified()
            .unwrap();
        db.mark_document_embedded(
            living.id.unwrap(),
            &living.content_hash,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
        db.mark_document_embedded(
            deleted.id.unwrap(),
            &deleted.content_hash,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
        db.upsert_chunk_embeddings(&[
            ("living.md".into(), 0, vec![1.0, 0.0]),
            ("deleted.md".into(), 0, vec![0.0, 1.0]),
        ])
        .unwrap();

        // A sync/restore tool may replace bytes without moving mtime. The
        // incremental pass must still compare content, not trust metadata.
        write_md(
            vault.path(),
            "living.md",
            "---\ntitle: Living\npeople: [Ada Lovelace]\n---\nThe current answer. #living\n",
        );
        std::fs::File::open(vault.path().join("living.md"))
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();
        let edited = initial.run(false, |_| {}).unwrap();
        assert_eq!(edited.files_updated, 1);
        let living = db.get_document_by_path("living.md").unwrap().unwrap();
        let living_content = living.content.as_deref().unwrap();
        assert!(living_content.contains("current answer"));
        assert!(!living_content.contains("obsolete answer"));
        assert_eq!(living.embedded_content_hash, None);

        // Model the synchronous embedding stage that follows indexing.
        db.delete_chunk_embeddings_for_file("living.md").unwrap();
        db.upsert_chunk_embeddings(&[("living.md".into(), 0, vec![0.5, 0.5])])
            .unwrap();
        db.mark_document_embedded(
            living.id.unwrap(),
            &living.content_hash,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();

        std::fs::remove_file(vault.path().join("deleted.md")).unwrap();
        let pruned = initial.run(false, |_| {}).unwrap();
        assert_eq!(pruned.files_deleted, 1);
        assert!(db.get_document_by_path("deleted.md").unwrap().is_none());
        assert_eq!(db.prune_stale_chunk_embeddings().unwrap(), 1);
        assert!(
            db.get_all_chunk_embeddings()
                .unwrap()
                .iter()
                .all(|(path, _, _)| path != "deleted.md")
        );
        let deleted_occurrences = db
            .query_read(
                "SELECT COUNT(*) FROM entity_occurrences WHERE file_path = 'deleted.md'",
                Vec::<libsql::Value>::new(),
                |row| row.get::<i64>(0),
            )
            .unwrap();
        assert_eq!(deleted_occurrences, vec![0]);

        // Changing the generated program's frontmatter interpretation should
        // reconcile unchanged notes without invalidating their embeddings.
        let schema_changed = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_frontmatter_link_fields(["people".to_string()]);
        let schema = schema_changed.run(false, |_| {}).unwrap();
        assert_eq!(schema.files_new + schema.files_updated, 0);
        assert!(schema.entity_config_changed);
        assert!(db.get_all_entities().unwrap().iter().any(|entity| {
            entity.name == "ada lovelace" && entity.entity_type == EntityType::Link
        }));
        let living_after_schema = db.get_document_by_path("living.md").unwrap().unwrap();
        assert_eq!(
            living_after_schema.embedded_content_hash.as_deref(),
            Some(living_after_schema.content_hash.as_str())
        );
    }

    // -----------------------------------------------------------------------
    // 3. Incremental detects new files
    // -----------------------------------------------------------------------
    #[test]
    fn incremental_detects_new_files() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let count_before = db.get_document_count().unwrap();

        write_md(
            vault.path(),
            "new-one.md",
            "# New One\nContent with #newtag.\n",
        );
        write_md(
            vault.path(),
            "new-two.md",
            "# New Two\nContent referencing [[New Link]].\n",
        );

        let stats = pipeline.run(false, |_| {}).unwrap();

        assert_eq!(stats.files_new, 2);
        assert_eq!(stats.files_updated, 0);
        assert_eq!(db.get_document_count().unwrap(), count_before + 2);

        // New entities from the new files should exist
        let entities = db.get_all_entities().unwrap();
        let entity_names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            entity_names.contains(&"newtag"),
            "newtag entity should exist after indexing new file"
        );
    }

    // -----------------------------------------------------------------------
    // 4. Incremental detects changed files
    // -----------------------------------------------------------------------
    #[test]
    fn incremental_detects_changed_files() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let hash_before = db
            .get_document_by_path("note-a.md")
            .unwrap()
            .unwrap()
            .content_hash
            .clone();

        // Modify the file content so the hash changes
        write_md(
            vault.path(),
            "note-a.md",
            "---\ntitle: Note A Modified\ntags:\n  - alpha\n  - shared\n---\n# Note A\nThis content is completely different now.\n",
        );

        let stats = pipeline.run(false, |_| {}).unwrap();

        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_new, 0);

        let hash_after = db
            .get_document_by_path("note-a.md")
            .unwrap()
            .unwrap()
            .content_hash;
        assert_ne!(
            hash_before, hash_after,
            "content hash should change after modification"
        );
    }

    /// Reverting a repair restores older content *and* an older mtime. A
    /// forward-only comparison treated that as unchanged, so the vault held the
    /// original file while the index kept serving the repaired one.
    #[test]
    fn incremental_detects_content_restored_with_an_older_mtime() {
        let db = test_db();
        let vault = create_test_vault();
        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let original = std::fs::read(vault.path().join("note-a.md")).unwrap();
        let original_mtime = std::fs::metadata(vault.path().join("note-a.md"))
            .unwrap()
            .modified()
            .unwrap();
        let hash_before = db
            .get_document_by_path("note-a.md")
            .unwrap()
            .unwrap()
            .content_hash
            .clone();

        // Repair: change content, and let mtime move forward normally.
        write_md(
            vault.path(),
            "note-a.md",
            "---\ntitle: Note A Repaired\ncreated: 2020-01-01\n---\n# Note A\nRepaired body.\n",
        );
        pipeline.run(false, |_| {}).unwrap();
        let repaired_hash = db
            .get_document_by_path("note-a.md")
            .unwrap()
            .unwrap()
            .content_hash
            .clone();
        assert_ne!(hash_before, repaired_hash);

        // Revert: restore the original bytes and the original, older mtime.
        std::fs::write(vault.path().join("note-a.md"), &original).unwrap();
        std::fs::File::open(vault.path().join("note-a.md"))
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();

        let stats = pipeline.run(false, |_| {}).unwrap();
        assert_eq!(
            stats.files_updated, 1,
            "a restored file must be re-indexed even though its mtime moved backward"
        );
        assert_eq!(
            db.get_document_by_path("note-a.md")
                .unwrap()
                .unwrap()
                .content_hash,
            hash_before,
            "index must serve the restored content, not the repaired content"
        );
    }

    // -----------------------------------------------------------------------
    // 5. Incremental detects deleted files
    // -----------------------------------------------------------------------
    #[test]
    fn incremental_detects_deleted_files() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let count_before = db.get_document_count().unwrap();

        std::fs::remove_file(vault.path().join("note-e.md")).unwrap();

        let stats = pipeline.run(false, |_| {}).unwrap();

        assert_eq!(stats.files_deleted, 1);
        assert_eq!(db.get_document_count().unwrap(), count_before - 1);
    }

    // -----------------------------------------------------------------------
    // 6. Entity occurrences accumulate across incremental runs
    // -----------------------------------------------------------------------
    #[test]
    fn configured_log_promotes_replaces_and_removes() {
        let db = test_db();
        let vault = create_test_vault();
        let filler = "word ".repeat(40);
        write_md(
            vault.path(),
            "journal.md",
            &format!("## 2026-01-01\n{filler}\n## 2026-02-01\n{filler}\n## 2026-03-01\n{filler}\n"),
        );

        let configured: std::collections::HashSet<String> =
            ["journal".to_string()].into_iter().collect();

        // Promotion: one occurrence per dated entry, frequency = entry count.
        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_configured_logs(configured.clone());
        pipeline.run(true, |_| {}).unwrap();

        let entity = db
            .get_all_entities()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "journal" && e.entity_type == EntityType::AppendLog)
            .expect("configured log should be promoted");
        assert_eq!(entity.frequency, 3);
        let occs = db.get_entity_occurrences(entity.id.unwrap()).unwrap();
        assert_eq!(occs.len(), 3);
        let dates: std::collections::HashSet<i64> =
            occs.iter().filter_map(|o| o.effective_date).collect();
        assert_eq!(dates.len(), 3, "each occurrence carries its entry date");
        assert!(occs.iter().all(|o| o.date_source == "content"));

        // Replace semantics: a full re-run must not accumulate.
        let pipeline =
            IndexingPipeline::new(&db, vault.path().to_path_buf()).with_configured_logs(configured);
        pipeline.run(true, |_| {}).unwrap();
        let entity = db
            .get_all_entities()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "journal" && e.entity_type == EntityType::AppendLog)
            .unwrap();
        assert_eq!(entity.frequency, 3, "re-run must not inflate frequency");
        assert_eq!(
            db.get_entity_occurrences(entity.id.unwrap()).unwrap().len(),
            3
        );

        // Removal: dropping the log: line deletes the entity.
        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_configured_logs(Default::default());
        pipeline.run(false, |_| {}).unwrap();
        assert!(
            !db.get_all_entities()
                .unwrap()
                .iter()
                .any(|e| e.entity_type == EntityType::AppendLog),
            "removed log: line must remove the entity"
        );
    }

    #[test]
    fn newly_configured_log_promotes_without_file_change() {
        let db = test_db();
        let vault = create_test_vault();
        let filler = "word ".repeat(40);
        write_md(
            vault.path(),
            "journal.md",
            &format!("## 2026-01-01\n{filler}\n## 2026-02-01\n{filler}\n## 2026-03-01\n{filler}\n"),
        );

        // Index once with no logs configured.
        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf())
            .with_configured_logs(Default::default());
        pipeline.run(true, |_| {}).unwrap();
        assert!(
            !db.get_all_entities()
                .unwrap()
                .iter()
                .any(|e| e.entity_type == EntityType::AppendLog)
        );

        // Add the log: line; the file itself is unchanged, so an incremental
        // run processes nothing — promotion must still happen.
        let configured: std::collections::HashSet<String> =
            ["journal".to_string()].into_iter().collect();
        let pipeline =
            IndexingPipeline::new(&db, vault.path().to_path_buf()).with_configured_logs(configured);
        let stats = pipeline.run(false, |_| {}).unwrap();
        assert_eq!(stats.files_new + stats.files_updated, 0);
        let entity = db
            .get_all_entities()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "journal" && e.entity_type == EntityType::AppendLog)
            .expect("adding a log: line must promote the unchanged file");
        assert_eq!(entity.frequency, 3);
    }

    #[test]
    fn entity_occurrences_accumulate() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        // Find the "alpha" tag entity and record its frequency
        let entities_before = db.get_all_entities().unwrap();
        let alpha_before = entities_before
            .iter()
            .find(|e| e.name == "alpha" && e.entity_type == EntityType::Tag)
            .expect("alpha tag should exist");
        let freq_before = alpha_before.frequency;
        let alpha_id = alpha_before.id.unwrap();
        let occs_before = db.get_entity_occurrences(alpha_id).unwrap().len();

        // Add a new file that also uses #alpha
        write_md(
            vault.path(),
            "extra.md",
            "# Extra\nThis file also uses #alpha.\n",
        );
        pipeline.run(false, |_| {}).unwrap();

        let entities_after = db.get_all_entities().unwrap();
        let alpha_after = entities_after
            .iter()
            .find(|e| e.name == "alpha" && e.entity_type == EntityType::Tag)
            .unwrap();

        assert!(
            alpha_after.frequency > freq_before,
            "alpha frequency should increase: {} -> {}",
            freq_before,
            alpha_after.frequency
        );

        let occs_after = db.get_entity_occurrences(alpha_id).unwrap().len();
        assert!(
            occs_after > occs_before,
            "alpha occurrences should increase: {} -> {}",
            occs_before,
            occs_after
        );
    }

    // -----------------------------------------------------------------------
    // 7. Staleness: new content triggers stale
    // -----------------------------------------------------------------------
    #[test]
    fn staleness_new_content_triggers_stale() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        // Insert a fake catalyst so staleness check doesn't short-circuit with NoCatalysts
        let fake_catalyst = Catalyst {
            id: uuid::Uuid::new_v4().to_string(),
            text: "test catalyst".to_string(),
            entity: "alpha".to_string(),
            context: "test".to_string(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        };
        db.insert_catalysts(&[fake_catalyst]).unwrap();

        // Store bookkeeping with current entity doc counts
        let entity_names = vec!["alpha".to_string()];
        store_staleness_bookkeeping(&db, vault.path(), &entity_names).unwrap();

        // Add 3+ files that mention alpha (the threshold is 3 new mentions)
        for i in 0..4 {
            write_md(
                vault.path(),
                &format!("stale-{}.md", i),
                &format!("# Stale {}\nContent with #alpha.\n", i),
            );
        }

        // Re-index so the DB counts update
        pipeline.run(false, |_| {}).unwrap();

        let result = check_catalyst_staleness(&db, vault.path()).unwrap();
        assert!(
            result.is_stale(),
            "should be stale after adding 4 new files mentioning alpha"
        );
        match result {
            StalenessResult::Stale(StaleReason::EntitiesStale { stale_entities }) => {
                assert!(
                    stale_entities.iter().any(|e| e.name == "alpha"),
                    "alpha should be stale"
                );
                let alpha = stale_entities.iter().find(|e| e.name == "alpha").unwrap();
                assert!(
                    alpha.delta >= 3,
                    "delta should be >= 3, got {}",
                    alpha.delta
                );
            }
            other => panic!("expected EntitiesStale, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // 8. Staleness: fresh when nothing changed
    // -----------------------------------------------------------------------
    #[test]
    fn staleness_fresh_when_nothing_changed() {
        let db = test_db();
        let vault = create_test_vault();

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        // Insert a fake catalyst
        let fake_catalyst = Catalyst {
            id: uuid::Uuid::new_v4().to_string(),
            text: "test catalyst".to_string(),
            entity: "alpha".to_string(),
            context: "test".to_string(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        };
        db.insert_catalysts(&[fake_catalyst]).unwrap();

        // Store bookkeeping
        let entity_names = vec!["alpha".to_string()];
        store_staleness_bookkeeping(&db, vault.path(), &entity_names).unwrap();

        // Check staleness without making any changes
        let result = check_catalyst_staleness(&db, vault.path()).unwrap();
        assert!(
            !result.is_stale(),
            "should be fresh when nothing changed, got {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // 9. Incremental is faster than full re-index
    // -----------------------------------------------------------------------
    #[test]
    fn incremental_faster_than_full() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Create ~200 files
        for i in 0..200 {
            write_md(
                vault.path(),
                &format!("doc-{:04}.md", i),
                &format!(
                    "---\ntitle: Document {}\ntags:\n  - bulk\n---\n# Document {}\nSome content for document number {}. References [[Ref {}]].\n",
                    i,
                    i,
                    i,
                    i % 10
                ),
            );
        }

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        let full_stats = pipeline.run(true, |_| {}).unwrap();

        // Add 1 file, run incremental
        write_md(
            vault.path(),
            "doc-new.md",
            "# New Doc\nOne new file with #newtag.\n",
        );

        let inc_stats = pipeline.run(false, |_| {}).unwrap();

        assert_eq!(inc_stats.files_new, 1);
        assert!(
            inc_stats.duration_ms < full_stats.duration_ms,
            "incremental ({} ms) should be faster than full ({} ms)",
            inc_stats.duration_ms,
            full_stats.duration_ms
        );
    }

    // -----------------------------------------------------------------------
    // 10. Folder entities get context snippets
    // -----------------------------------------------------------------------
    #[test]
    fn folder_entities_get_context() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Create files inside a subfolder so folder entities are extracted
        write_md(
            vault.path(),
            "people/alice.md",
            "# Alice\nAlice is a researcher working on distributed systems.\n",
        );
        write_md(
            vault.path(),
            "people/bob.md",
            "# Bob\nBob collaborates with Alice on networking protocols.\n",
        );

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        let folder_entity = entities
            .iter()
            .find(|e| e.name == "people" && e.entity_type == EntityType::Folder)
            .expect("people folder entity should exist");

        let occs = db
            .get_entity_occurrences(folder_entity.id.unwrap())
            .unwrap();
        assert_eq!(occs.len(), 2, "should have 2 occurrences (one per file)");

        for occ in &occs {
            assert!(
                occ.context.is_some(),
                "folder occurrence for {} should have context",
                occ.file_path
            );
            let ctx = occ.context.as_ref().unwrap();
            assert!(
                !ctx.is_empty(),
                "folder occurrence context should not be empty"
            );
            // Folder entities should get full file body, not just 200 chars
            assert!(
                ctx.contains("researcher") || ctx.contains("collaborates"),
                "folder context should contain substantive file content"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 11. Frontmatter tag → full file context
    // -----------------------------------------------------------------------
    #[test]
    fn frontmatter_tag_gets_full_context() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Long content so we can distinguish full-body from a snippet
        let long_body = "Paragraph one about productivity.\n\n".to_string()
            + &"More content. ".repeat(50)
            + "\n\nFinal paragraph about productivity workflow.";
        let content = format!("---\ntags: [productivity]\n---\n{long_body}");
        write_md(vault.path(), "note.md", &content);

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        let tag_entity = entities
            .iter()
            .find(|e| e.name == "productivity" && e.entity_type == EntityType::Tag)
            .expect("productivity tag should exist");

        let occs = db.get_entity_occurrences(tag_entity.id.unwrap()).unwrap();
        assert_eq!(occs.len(), 1);

        let ctx = occs[0].context.as_ref().expect("should have context");
        // Full body should contain both first and last paragraph
        assert!(
            ctx.contains("Paragraph one"),
            "should contain start of body"
        );
        assert!(
            ctx.contains("Final paragraph"),
            "should contain end of body"
        );
    }

    // -----------------------------------------------------------------------
    // 12. Inline-only tag → neighborhood context (not full file)
    // -----------------------------------------------------------------------
    #[test]
    fn inline_tag_gets_neighborhood_context() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Tag only appears inline, not in frontmatter. Make content much longer
        // than the 250-word neighborhood window.
        let intro = "Introduction paragraph with various words. ".repeat(150);
        let content = format!(
            "---\ntitle: Test\n---\n{intro}\n\nMiddle section with #inline-tag here.\n\n{}",
            "Conclusion paragraph with even more words. ".repeat(150)
        );
        write_md(vault.path(), "note.md", &content);

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        let tag_entity = entities
            .iter()
            .find(|e| e.name == "inline-tag" && e.entity_type == EntityType::Tag)
            .expect("inline-tag should exist");

        let occs = db.get_entity_occurrences(tag_entity.id.unwrap()).unwrap();
        assert_eq!(occs.len(), 1);

        let ctx = occs[0].context.as_ref().expect("should have context");
        // Neighborhood should include the tag mention
        assert!(
            ctx.contains("inline-tag") || ctx.contains("#inline-tag"),
            "neighborhood context should contain the tag mention"
        );
        // But should NOT be the entire file
        let full_len = content.len();
        assert!(
            ctx.len() < full_len / 2,
            "neighborhood context ({} chars) should be much smaller than full file ({} chars)",
            ctx.len(),
            full_len
        );
    }

    // -----------------------------------------------------------------------
    // 13. Frontmatter wikilink → full file context
    // -----------------------------------------------------------------------
    #[test]
    fn frontmatter_wikilink_gets_full_context() {
        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        let long_body = "First paragraph about a meeting with Alice.\n\n".to_string()
            + &"More discussion notes. ".repeat(50)
            + "\n\nFinal notes about action items.";
        let content = format!("---\npeople: \"[[Alice]]\"\n---\n{long_body}");
        write_md(vault.path(), "note.md", &content);

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        let link_entity = entities
            .iter()
            .find(|e| e.name == "alice" && e.entity_type == EntityType::Link)
            .expect("alice link should exist");

        let occs = db.get_entity_occurrences(link_entity.id.unwrap()).unwrap();
        assert_eq!(occs.len(), 1);

        let ctx = occs[0].context.as_ref().expect("should have context");
        assert!(
            ctx.contains("First paragraph"),
            "should contain start of body"
        );
        assert!(ctx.contains("Final notes"), "should contain end of body");
    }

    // -----------------------------------------------------------------------
    // 14. A coherent (non-log) oversized body stores NO document-level context:
    //     Tier-1 chunk selection handles it, and a body excerpt would only
    //     pollute the link centroid, which wants mention-neighborhoods.
    // -----------------------------------------------------------------------
    #[test]
    fn coherent_oversized_body_stores_no_context() {
        use crate::models::INLINE_CONTEXT_MAX_WORDS;

        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Long, but NOT a log: no internal date headers.
        let big_body = "word ".repeat(600);
        assert!(big_body.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS);

        write_md(
            vault.path(),
            "people/big.md",
            &format!("---\ntags: [oversized]\npeople: \"[[BigLink]]\"\n---\n{big_body}"),
        );

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        for name_type in [
            ("oversized", EntityType::Tag),
            ("biglink", EntityType::Link),
            ("people", EntityType::Folder),
        ] {
            let entity = entities
                .iter()
                .find(|e| e.name == name_type.0 && e.entity_type == name_type.1)
                .unwrap_or_else(|| panic!("entity {} should exist", name_type.0));
            let occs = db.get_entity_occurrences(entity.id.unwrap()).unwrap();
            let big_occ = occs
                .iter()
                .find(|o| o.file_path.ends_with("big.md"))
                .expect("big.md occurrence should exist");
            assert!(
                big_occ.context.is_none(),
                "coherent oversized body for entity {} should store no context",
                entity.name
            );
        }
    }

    // -----------------------------------------------------------------------
    // 15. An append-only log (topology B) stores its most-recent entry as a
    //     bounded, chunk-scorable context — not None, not the whole timeline.
    // -----------------------------------------------------------------------
    #[test]
    fn append_log_stores_recent_entry_context() {
        use crate::models::INLINE_CONTEXT_MAX_WORDS;

        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // Five bottom-appended dated entries → auto-detected as a log.
        let mut body = String::new();
        for m in 1..=5 {
            body.push_str(&format!(
                "## 2026-0{m}-01\nentry {m} {}\n",
                "word ".repeat(120)
            ));
        }
        assert!(body.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS);

        write_md(
            vault.path(),
            "journal/log.md",
            &format!("---\ntags: [logtag]\npeople: \"[[LogLink]]\"\n---\n{body}"),
        );

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();
        for name_type in [
            ("logtag", EntityType::Tag),
            ("loglink", EntityType::Link),
            ("journal", EntityType::Folder),
        ] {
            let entity = entities
                .iter()
                .find(|e| e.name == name_type.0 && e.entity_type == name_type.1)
                .unwrap_or_else(|| panic!("entity {} should exist", name_type.0));
            let occs = db.get_entity_occurrences(entity.id.unwrap()).unwrap();
            let occ = occs
                .iter()
                .find(|o| o.file_path.ends_with("log.md"))
                .expect("log.md occurrence should exist");
            let ctx = occ.context.as_ref().unwrap_or_else(|| {
                panic!("log context for entity {} should be present", entity.name)
            });
            // Bounded, so it stays in the centroid.
            assert!(
                ctx.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS,
                "log context for entity {} must stay under the filter",
                entity.name
            );
            // Most recent entry (2026-05-01), not the oldest (2026-01-01).
            assert!(
                ctx.contains("2026-05-01"),
                "log context for entity {} should hold the most recent entry",
                entity.name
            );
            assert!(
                !ctx.contains("2026-01-01"),
                "log context for entity {} should drop the oldest entry",
                entity.name
            );
        }
    }
}
