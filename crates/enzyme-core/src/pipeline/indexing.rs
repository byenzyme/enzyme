use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::db::Database;
use crate::document::{DocumentProcessor, FileDiscovery, ProcessedDocument};
use crate::models::{
    EntityOccurrence, EntityTrends, EntityType, INLINE_CONTEXT_MAX_WORDS, ROOT_FOLDER_ENTITY,
};

/// Orchestrates the full indexing pipeline:
/// discover files -> process -> diff against DB -> upsert new/changed ->
/// delete removed -> extract entities -> compute trending scores.
pub struct IndexingPipeline<'a> {
    db: &'a Database,
    vault_path: PathBuf,
}

/// Summary statistics from an indexing run.
pub struct IndexingStats {
    pub files_discovered: usize,
    pub files_new: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub entities_extracted: usize,
    pub duration_ms: u64,
}

impl<'a> IndexingPipeline<'a> {
    pub fn new(db: &'a Database, vault_path: PathBuf) -> Self {
        Self { db, vault_path }
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

        // Step 1: Discover files
        on_progress("Discovering files...");
        let discovery = FileDiscovery::new(self.vault_path.clone());
        let all_paths = discovery.discover()?;
        let db_doc_count = self.db.get_document_count().unwrap_or(0);
        if all_paths.is_empty() && db_doc_count > 0 {
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
                duration_ms,
            });
        }
        let files_discovered = all_paths.len();
        info!(files_discovered, "files discovered");

        // Step 2: Get existing hashes from DB
        let existing_hashes = self.db.get_all_document_hashes()?;

        // Step 3: Build a lookup from source_ref -> PathBuf for all discovered files
        let all_source_refs: HashMap<String, PathBuf> = all_paths
            .iter()
            .map(|p| (p.to_string_lossy().to_string(), p.clone()))
            .collect();

        // Step 4: Diff — determine which files are new, changed, or deleted
        let mut paths_to_process: Vec<PathBuf> = Vec::new();
        let files_new;
        let mut files_updated = 0usize;

        if full {
            // Full mode: process everything
            paths_to_process = all_paths.clone();
            files_new = paths_to_process.len();
        } else {
            // Incremental mode: check mtime first, then hash only if mtime changed.
            //
            // Phase 1: Partition into new files vs files needing mtime check.
            let mut new_paths = Vec::new();
            let mut candidates_to_check: Vec<(PathBuf, &str)> = Vec::new();

            for path in &all_paths {
                let source_ref = path.to_string_lossy();
                match existing_hashes.get(source_ref.as_ref()) {
                    None => {
                        new_paths.push(path.clone());
                    }
                    Some((old_hash, stored_mtime_ms)) => {
                        // Quick mtime check: stat() is ~1000x cheaper than read+hash
                        let abs_path = self.vault_path.join(path);
                        let mtime_changed = std::fs::metadata(&abs_path)
                            .and_then(|m| m.modified())
                            .ok()
                            .map(|mtime| {
                                let fs_mtime_ms = mtime
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                                    as i64;
                                fs_mtime_ms > *stored_mtime_ms
                            })
                            .unwrap_or(true); // if stat fails, assume changed

                        if mtime_changed {
                            candidates_to_check.push((path.clone(), old_hash.as_str()));
                        }
                    }
                }
            }

            files_new = new_paths.len();
            paths_to_process.extend(new_paths);

            // Phase 2: For files with newer mtime, read+hash in parallel to confirm change.
            let vault_root = &self.vault_path;
            let changed: Vec<PathBuf> = candidates_to_check
                .par_iter()
                .filter_map(|(path, old_hash)| {
                    let abs_path = vault_root.join(path);
                    let content = std::fs::read_to_string(&abs_path).ok()?;
                    let mut hasher = Sha256::new();
                    hasher.update(content.as_bytes());
                    let current_hash = format!("{:x}", hasher.finalize());
                    if current_hash != *old_hash {
                        Some(path.clone())
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
            .filter(|source_ref| !all_source_refs.contains_key(*source_ref))
            .cloned()
            .collect();
        let files_deleted = deleted_paths.len();

        // Step 5: Process new + changed files
        on_progress(&format!("Processing {} files...", paths_to_process.len()));
        let processor = DocumentProcessor::new(self.vault_path.clone());
        let results = processor.process_all(&paths_to_process);

        let mut processed_docs: Vec<ProcessedDocument> = Vec::new();
        for result in results {
            match result {
                Ok(doc) => processed_docs.push(doc),
                Err(e) => warn!("Failed to process file: {}", e),
            }
        }

        // Step 6: Upsert processed documents into the database (single transaction)
        on_progress("Upserting documents...");
        self.db.upsert_documents_batch(&processed_docs)?;

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

        // Step 9: Compute trending scores for all entities (bulk fetch)
        on_progress("Computing trending scores...");
        self.compute_trending_scores()?;

        let duration_ms = start.elapsed().as_millis() as u64;

        info!(
            files_discovered,
            files_new,
            files_updated,
            files_deleted,
            entities_extracted,
            duration_ms,
            "indexing complete"
        );

        Ok(IndexingStats {
            files_discovered,
            files_new,
            files_updated,
            files_deleted,
            entities_extracted,
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

            // Full file body (frontmatter stripped) for document-level entities.
            // Only stored when small enough that the catalyst pipeline's inline
            // context filter (`INLINE_CONTEXT_MAX_WORDS`) will actually keep it;
            // larger bodies are always filtered out by that reader, so we store
            // `None` rather than pay to persist context that's never used. A
            // truncated snippet is deliberately avoided here: it would newly
            // pass the reader's word-count filter and change centroid behavior.
            let full_body: Option<String> = doc.chunks.first().and_then(|_| {
                let body = crate::document::strip_frontmatter(&doc.content).to_string();
                if body.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS {
                    Some(body)
                } else {
                    None
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
                    },
                ));
            }
        }

        entries
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
        let now_millis = chrono::Utc::now().timestamp_millis();

        // Build entity_id -> frequency lookup
        let entity_freq: HashMap<i64, i64> = entities
            .iter()
            .filter_map(|e| e.id.map(|id| (id, e.frequency)))
            .collect();

        // Single bulk fetch of all occurrence dates
        let all_occ_dates = self.db.get_all_occurrence_dates()?;

        // Group by entity_id and compute scores in one pass
        // (rows are ordered by entity_id from the query)
        struct Accum {
            weighted_sum: f64,
            frequency_recent: i64,
            timeline_buckets: HashMap<String, i64>,
        }

        let mut accums: HashMap<i64, Accum> = HashMap::new();

        for (entity_id, effective_date_opt, created_at) in &all_occ_dates {
            let effective_date = effective_date_opt.unwrap_or(*created_at);
            let age_days = ((now_millis - effective_date) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);

            let acc = accums.entry(*entity_id).or_insert_with(|| Accum {
                weighted_sum: 0.0,
                frequency_recent: 0,
                timeline_buckets: HashMap::new(),
            });

            // Exponential decay with a half-life of 30 days
            let weight = (-0.693_147 / 30.0 * age_days).exp();
            acc.weighted_sum += weight;

            if age_days <= 365.0 {
                acc.frequency_recent += 1;
            }

            if let Some(dt) = chrono::DateTime::from_timestamp_millis(effective_date) {
                let bucket = dt.format("%Y-%m").to_string();
                *acc.timeline_buckets.entry(bucket).or_insert(0) += 1;
            }
        }

        // Build batch update
        let updates: Vec<(i64, EntityTrends)> = entities
            .iter()
            .filter_map(|e| {
                let entity_id = e.id?;
                let frequency = entity_freq.get(&entity_id).copied().unwrap_or(0) as f64;

                let acc = accums.remove(&entity_id);
                let (weighted_sum, frequency_recent, timeline_buckets) = match acc {
                    Some(a) => (a.weighted_sum, a.frequency_recent, a.timeline_buckets),
                    None => (0.0, 0, HashMap::new()),
                };

                let recency_score = weighted_sum * (1.0 + (1.0 + frequency).ln());

                Some((
                    entity_id,
                    EntityTrends {
                        frequency_recent,
                        recency_score,
                        timeline_buckets,
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
    // 14. Oversized frontmatter/folder bodies store None, not a truncated body
    // -----------------------------------------------------------------------
    #[test]
    fn oversized_body_stores_none_context() {
        use crate::models::INLINE_CONTEXT_MAX_WORDS;

        let db = test_db();
        let vault = tempfile::tempdir().unwrap();

        // A body comfortably over the 400-word inline-context threshold.
        let big_body = "word ".repeat(INLINE_CONTEXT_MAX_WORDS + 100);
        assert!(big_body.split_whitespace().count() >= INLINE_CONTEXT_MAX_WORDS);

        write_md(
            vault.path(),
            "people/big.md",
            &format!("---\ntags: [oversized]\npeople: \"[[BigLink]]\"\n---\n{big_body}"),
        );

        // A small body, well under the threshold, in the same folder so the
        // folder entity gets one oversized and one small occurrence.
        write_md(
            vault.path(),
            "people/small.md",
            "---\ntags: [oversized]\npeople: \"[[BigLink]]\"\n---\nA short note.\n",
        );

        let pipeline = IndexingPipeline::new(&db, vault.path().to_path_buf());
        pipeline.run(true, |_| {}).unwrap();

        let entities = db.get_all_entities().unwrap();

        let tag_entity = entities
            .iter()
            .find(|e| e.name == "oversized" && e.entity_type == EntityType::Tag)
            .expect("oversized tag should exist");
        let link_entity = entities
            .iter()
            .find(|e| e.name == "biglink" && e.entity_type == EntityType::Link)
            .expect("biglink link should exist");
        let folder_entity = entities
            .iter()
            .find(|e| e.name == "people" && e.entity_type == EntityType::Folder)
            .expect("people folder entity should exist");

        for entity in [&tag_entity, &link_entity, &folder_entity] {
            let occs = db.get_entity_occurrences(entity.id.unwrap()).unwrap();
            assert_eq!(occs.len(), 2, "entity {} should have 2 occurrences", entity.name);

            let big_occ = occs
                .iter()
                .find(|o| o.file_path.ends_with("big.md"))
                .expect("big.md occurrence should exist");
            assert!(
                big_occ.context.is_none(),
                "oversized body context for entity {} should be None, not a truncated snippet",
                entity.name
            );

            let small_occ = occs
                .iter()
                .find(|o| o.file_path.ends_with("small.md"))
                .expect("small.md occurrence should exist");
            assert!(
                small_occ.context.is_some(),
                "small body context for entity {} should still be stored",
                entity.name
            );
        }
    }
}
