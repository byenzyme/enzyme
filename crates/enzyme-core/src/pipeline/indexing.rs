use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::db::{Database, Repository};
use crate::document::{DocumentProcessor, FileDiscovery, ProcessedDocument};
use crate::models::{EntityOccurrence, EntityTrends, EntityType};

/// Orchestrates the full indexing pipeline:
/// discover files -> process -> diff against DB -> upsert new/changed ->
/// delete removed -> extract entities -> compute trending scores.
pub struct IndexingPipeline<'a> {
    repo: Repository<'a>,
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
        Self {
            repo: Repository::new(db),
            vault_path,
        }
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
        let files_discovered = all_paths.len();
        info!(files_discovered, "files discovered");

        // Step 2: Get existing hashes from DB
        let existing_hashes = self.repo.get_all_document_hashes()?;

        // Step 3: Build a lookup from source_ref -> PathBuf for all discovered files
        let all_source_refs: HashMap<String, PathBuf> = all_paths
            .iter()
            .map(|p| (p.to_string_lossy().to_string(), p.clone()))
            .collect();

        // Step 4: Diff — determine which files are new, changed, or deleted
        let mut paths_to_process: Vec<PathBuf> = Vec::new();
        let mut files_new = 0usize;
        let mut files_updated = 0usize;

        if full {
            // Full mode: process everything
            paths_to_process = all_paths.clone();
            files_new = paths_to_process.len();
        } else {
            // Incremental mode: only process new and changed files
            for path in &all_paths {
                let source_ref = path.to_string_lossy().to_string();
                match existing_hashes.get(&source_ref) {
                    None => {
                        // New file — not yet in the database
                        paths_to_process.push(path.clone());
                        files_new += 1;
                    }
                    Some(old_hash) => {
                        // Existing file — check if content changed by computing hash
                        let abs_path = self.vault_path.join(path);
                        if let Ok(content) = std::fs::read_to_string(&abs_path) {
                            let mut hasher = Sha256::new();
                            hasher.update(content.as_bytes());
                            let current_hash = format!("{:x}", hasher.finalize());
                            if &current_hash != old_hash {
                                paths_to_process.push(path.clone());
                                files_updated += 1;
                            }
                        }
                    }
                }
            }
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
        self.repo.upsert_documents_batch(&processed_docs)?;

        // Step 7: Delete removed files from the database
        if !deleted_paths.is_empty() {
            on_progress(&format!("Deleting {} removed files...", deleted_paths.len()));
            self.repo.delete_documents_by_paths(&deleted_paths)?;
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
            self.repo.upsert_entities_batch(&refs)?;
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
    fn collect_entity_entries(
        docs: &[ProcessedDocument],
    ) -> Vec<(String, EntityType, EntityOccurrence)> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut entries = Vec::new();

        for doc in docs {
            let context_snippet: Option<String> = doc
                .chunks
                .first()
                .map(|c| c.chars().take(200).collect());

            for tag in &doc.tags {
                entries.push((
                    tag.clone(),
                    EntityType::Tag,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context: context_snippet.clone(),
                        created_at: now,
                        effective_date: Some(doc.modified_at),
                        date_source: doc.modified_source.as_str().to_string(),
                    },
                ));
            }

            for link in &doc.links {
                entries.push((
                    link.clone(),
                    EntityType::Link,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context: context_snippet.clone(),
                        created_at: now,
                        effective_date: Some(doc.modified_at),
                        date_source: doc.modified_source.as_str().to_string(),
                    },
                ));
            }

            for folder in &doc.folders {
                entries.push((
                    folder.clone(),
                    EntityType::Folder,
                    EntityOccurrence {
                        id: None,
                        entity_id: 0,
                        file_path: doc.source_ref.clone(),
                        context: None,
                        created_at: now,
                        effective_date: Some(doc.modified_at),
                        date_source: doc.modified_source.as_str().to_string(),
                    },
                ));
            }
        }

        entries
    }

    /// Recompute trending scores for every entity in the database.
    ///
    /// Fetches all occurrence dates in a **single query**, groups by entity_id
    /// in memory, then batch-updates all trends in one transaction.
    fn compute_trending_scores(&self) -> Result<()> {
        let entities = self.repo.get_all_entities()?;
        let now_millis = chrono::Utc::now().timestamp_millis();

        // Build entity_id -> frequency lookup
        let entity_freq: HashMap<i64, i64> = entities
            .iter()
            .filter_map(|e| e.id.map(|id| (id, e.frequency)))
            .collect();

        // Single bulk fetch of all occurrence dates
        let all_occ_dates = self.repo.get_all_occurrence_dates()?;

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
            let age_days =
                ((now_millis - effective_date) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);

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

                Some((entity_id, EntityTrends {
                    frequency_recent,
                    recency_score,
                    timeline_buckets,
                }))
            })
            .collect();

        self.repo.update_entity_trends_batch(&updates)?;

        Ok(())
    }
}
