use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::{Catalyst, CatalystSimilarity, Embedding};

/// Compute a stable SHA-256 fingerprint over catalyst ids and texts.
///
/// Catalysts are sorted by id before hashing so the result is independent of
/// the order they appear in the slice.  The returned string is a lowercase hex
/// digest.
pub fn catalysts_fingerprint(catalysts: &[Catalyst]) -> String {
    let mut pairs: Vec<(&str, &str)> = catalysts
        .iter()
        .map(|c| (c.id.as_str(), c.text.as_str()))
        .collect();
    pairs.sort_by_key(|(id, _)| *id);

    let mut hasher = Sha256::new();
    for (id, text) in &pairs {
        hasher.update(id.as_bytes());
        hasher.update(b"\x00");
        hasher.update(text.as_bytes());
        hasher.update(b"\x00");
    }
    format!("{:x}", hasher.finalize())
}

pub struct SimilarityPipeline<'a> {
    db: &'a Database,
    service: &'a EmbeddingService,
}

pub struct SimilarityStats {
    pub catalysts_processed: usize,
    pub similarities_computed: usize,
    pub documents_compared: usize,
    pub duration_ms: u64,
}

pub const CATALYST_TOP_K: usize = 50;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IncrementalSimilarityStats {
    pub chunks_added: usize,
    pub chunks_updated: usize,
    pub chunks_deleted: usize,
    pub candidate_scores: usize,
    pub catalysts_considered_for_new_chunks: usize,
    pub catalysts_refilled: usize,
    pub obsolete_rows_removed: usize,
    pub rows_upserted: usize,
    pub rows_trimmed: usize,
    pub full_repair: bool,
    pub duration_ms: u64,
}

impl<'a> SimilarityPipeline<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self { db, service }
    }

    /// Run similarity using catalysts from the local database.
    pub fn run(&self, on_progress: impl Fn(usize, usize)) -> Result<SimilarityStats> {
        let catalysts = self.db.get_all_catalysts()?;
        self.run_with_catalysts(&catalysts, on_progress)
    }

    /// Run similarity with externally-provided catalysts (e.g. from a source
    /// vault during catalyze target prep).
    ///
    /// Catalyst embeddings are saved to the database for reuse in `catalyze_search`.
    pub fn run_with_catalysts(
        &self,
        catalysts: &[Catalyst],
        on_progress: impl Fn(usize, usize),
    ) -> Result<SimilarityStats> {
        let start = Instant::now();

        if catalysts.is_empty() {
            return Ok(SimilarityStats {
                catalysts_processed: 0,
                similarities_computed: 0,
                documents_compared: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        let catalyst_ids: Vec<String> = catalysts.iter().map(|c| c.id.clone()).collect();
        self.db
            .delete_catalyst_similarities_for_catalysts(&catalyst_ids)?;
        self.db.prune_catalyst_similarities_except(&catalyst_ids)?;

        // Embed all catalyst texts
        let catalyst_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
        let catalyst_vecs = self.service.embed_documents(&catalyst_texts)?;

        // Cache catalyst embeddings to DB for fast reuse in catalyze_search
        let entries: Vec<(&str, &[f32])> = catalysts
            .iter()
            .zip(catalyst_vecs.iter())
            .map(|(c, v)| (c.id.as_str(), v.as_slice()))
            .collect();
        self.db.upsert_catalyst_embeddings(&entries)?;

        // Load all chunk embeddings from DB
        let chunk_embeddings = self.db.get_all_chunk_embeddings()?;
        if chunk_embeddings.is_empty() {
            return Ok(SimilarityStats {
                catalysts_processed: 0,
                similarities_computed: 0,
                documents_compared: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        // Count unique documents
        let documents_compared = {
            let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (file_path, _, _) in &chunk_embeddings {
                unique.insert(file_path.as_str());
            }
            unique.len()
        };

        let total = catalysts.len();
        // Score catalysts independently across the Rayon pool. Each catalyst's
        // dot products remain serial, preserving the exact floating-point
        // operation order of the old implementation. IndexedParallelIterator
        // also preserves catalyst order in the collected result.
        let records_by_catalyst: Vec<Vec<CatalystSimilarity>> = catalysts
            .par_iter()
            .zip(catalyst_vecs.par_iter())
            .map(|(catalyst, cat_vec)| {
                let mut sims: Vec<(f64, &str, u32)> = chunk_embeddings
                    .iter()
                    .map(|(file_path, chunk_index, vector)| {
                        let sim = Embedding::cosine_similarity(cat_vec, vector) as f64;
                        (sim, file_path.as_str(), *chunk_index)
                    })
                    .collect();

                // Make ties deterministic at the top-K boundary.
                sims.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(b.1))
                        .then_with(|| a.2.cmp(&b.2))
                });
                sims.truncate(CATALYST_TOP_K);

                let created_at = Utc::now();
                sims.into_iter()
                    .map(|(sim, file_path, chunk_index)| CatalystSimilarity {
                        id: None,
                        catalyst_id: catalyst.id.clone(),
                        catalyst_text: catalyst.text.clone(),
                        file_path: file_path.to_string(),
                        chunk_index: chunk_index as i64,
                        similarity: sim,
                        chunk_metadata: serde_json::json!({}),
                        timeline_data: serde_json::json!({}),
                        created_at,
                    })
                    .collect()
            })
            .collect();

        let similarities_computed = records_by_catalyst.iter().map(Vec::len).sum();
        let similarity_records: Vec<CatalystSimilarity> = records_by_catalyst
            .into_iter()
            .enumerate()
            .flat_map(|(i, records)| {
                on_progress(i + 1, total);
                records
            })
            .collect();
        self.db.upsert_catalyst_similarities(&similarity_records)?;

        let duration_ms = start.elapsed().as_millis() as u64;
        info!(
            catalysts_processed = total,
            similarities_computed, duration_ms, "similarity computation complete"
        );

        Ok(SimilarityStats {
            catalysts_processed: total,
            similarities_computed,
            documents_compared,
            duration_ms,
        })
    }

    /// Maintain the active catalyst-to-corpus top-K after a document embedding
    /// delta. New chunks are compared with every active catalyst. Catalysts
    /// whose top-K lost an updated/deleted row are the only ones rescored
    /// against the whole current corpus for exact refill.
    pub fn maintain_incremental(
        &self,
        before: &[(String, u32, Vec<f32>)],
        after: &[(String, u32, Vec<f32>)],
    ) -> Result<IncrementalSimilarityStats> {
        let start = Instant::now();
        let before_map: HashMap<(&str, u32), &Vec<f32>> = before
            .iter()
            .map(|(p, i, v)| ((p.as_str(), *i), v))
            .collect();
        let after_map: HashMap<(&str, u32), &Vec<f32>> = after
            .iter()
            .map(|(p, i, v)| ((p.as_str(), *i), v))
            .collect();

        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut deleted = Vec::new();
        for (key, vector) in &after_map {
            match before_map.get(key) {
                None => added.push((key.0.to_string(), key.1, (*vector).clone())),
                Some(old) if *old != *vector => {
                    updated.push((key.0.to_string(), key.1, (*vector).clone()))
                }
                _ => {}
            }
        }
        for key in before_map.keys() {
            if !after_map.contains_key(key) {
                deleted.push((key.0.to_string(), key.1));
            }
        }

        let catalysts = self.db.get_all_catalysts()?;
        if catalysts.is_empty() {
            return Ok(IncrementalSimilarityStats {
                chunks_added: added.len(),
                chunks_updated: updated.len(),
                chunks_deleted: deleted.len(),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            });
        }

        let cached = self.db.get_all_catalyst_embeddings()?;
        let cached_ids: HashSet<&str> = cached.iter().map(|(id, _)| id.as_str()).collect();
        let active_ids: HashSet<&str> = catalysts.iter().map(|c| c.id.as_str()).collect();
        if cached_ids != active_ids {
            let full = self.run_with_catalysts(&catalysts, |_, _| {})?;
            return Ok(IncrementalSimilarityStats {
                chunks_added: added.len(),
                chunks_updated: updated.len(),
                chunks_deleted: deleted.len(),
                candidate_scores: catalysts.len().saturating_mul(after.len()),
                catalysts_considered_for_new_chunks: catalysts.len(),
                rows_upserted: full.similarities_computed,
                full_repair: true,
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            });
        }

        let vectors: HashMap<&str, &Vec<f32>> =
            cached.iter().map(|(id, v)| (id.as_str(), v)).collect();
        let catalyst_by_id: HashMap<&str, &Catalyst> =
            catalysts.iter().map(|c| (c.id.as_str(), c)).collect();

        let obsolete_before = self
            .db
            .query_read(
                "SELECT COUNT(*) FROM catalyst_similarities cs
             WHERE NOT EXISTS (SELECT 1 FROM chunk_embeddings ce
               WHERE ce.file_path=cs.file_path AND ce.chunk_index=cs.chunk_index)",
                vec![],
                |row| row.get::<i64>(0),
            )?
            .into_iter()
            .next()
            .unwrap_or(0) as usize;
        let mut refill = self.db.prune_obsolete_catalyst_similarities()?;
        let updated_keys: Vec<(String, u32)> =
            updated.iter().map(|(p, i, _)| (p.clone(), *i)).collect();
        let mut updated_rows_removed = 0usize;
        for (path, index) in &updated_keys {
            updated_rows_removed += self
                .db
                .query_read(
                    "SELECT COUNT(*) FROM catalyst_similarities
                     WHERE file_path=?1 AND chunk_index=?2",
                    vec![
                        libsql::Value::Text(path.clone()),
                        libsql::Value::Integer(*index as i64),
                    ],
                    |row| row.get::<i64>(0),
                )?
                .into_iter()
                .next()
                .unwrap_or(0) as usize;
        }
        refill.extend(
            self.db
                .delete_catalyst_similarities_for_chunks(&updated_keys)?,
        );

        let mut stats = IncrementalSimilarityStats {
            chunks_added: added.len(),
            chunks_updated: updated.len(),
            chunks_deleted: deleted.len(),
            obsolete_rows_removed: obsolete_before + updated_rows_removed,
            ..Default::default()
        };

        // Updated chunks participate as new candidates after their obsolete
        // scores are removed. Catalysts selected for exact refill skip this
        // merge because the refill considers the entire current corpus.
        let candidates: Vec<(String, u32, Vec<f32>)> = added.into_iter().chain(updated).collect();
        for catalyst in &catalysts {
            if refill.contains(&catalyst.id) {
                continue;
            }
            let cat_vec = vectors[catalyst.id.as_str()];
            if !candidates.is_empty() {
                stats.catalysts_considered_for_new_chunks += 1;
            }
            let existing = self
                .db
                .get_similarities_for_catalyst(&catalyst.id, CATALYST_TOP_K)?;
            let threshold = (existing.len() >= CATALYST_TOP_K)
                .then(|| existing.last().map(|s| s.similarity))
                .flatten();
            let mut records = Vec::new();
            for (path, index, vector) in &candidates {
                let similarity = Embedding::cosine_similarity(cat_vec, vector) as f64;
                stats.candidate_scores += 1;
                if threshold.is_none_or(|minimum| similarity >= minimum) {
                    records.push(CatalystSimilarity {
                        id: None,
                        catalyst_id: catalyst.id.clone(),
                        catalyst_text: catalyst.text.clone(),
                        file_path: path.clone(),
                        chunk_index: *index as i64,
                        similarity,
                        chunk_metadata: serde_json::json!({}),
                        timeline_data: serde_json::json!({}),
                        created_at: Utc::now(),
                    });
                }
            }
            stats.rows_upserted += records.len();
            self.db.upsert_catalyst_similarities(&records)?;
            stats.rows_trimmed += self
                .db
                .trim_catalyst_similarities(&catalyst.id, CATALYST_TOP_K)?;
        }

        // Exact refill only for catalyst top-K sets that actually lost a row.
        for catalyst_id in &refill {
            let Some(catalyst) = catalyst_by_id.get(catalyst_id.as_str()) else {
                continue;
            };
            let Some(cat_vec) = vectors.get(catalyst_id.as_str()) else {
                continue;
            };
            let mut scored: Vec<(f64, &str, u32)> = after
                .iter()
                .map(|(path, index, vector)| {
                    (
                        Embedding::cosine_similarity(cat_vec, vector) as f64,
                        path.as_str(),
                        *index,
                    )
                })
                .collect();
            stats.candidate_scores += scored.len();
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(b.1))
                    .then_with(|| a.2.cmp(&b.2))
            });
            scored.truncate(CATALYST_TOP_K);
            self.db
                .delete_catalyst_similarities_for_catalysts(&[catalyst_id.clone()])?;
            let records: Vec<CatalystSimilarity> = scored
                .into_iter()
                .map(|(similarity, path, index)| CatalystSimilarity {
                    id: None,
                    catalyst_id: catalyst.id.clone(),
                    catalyst_text: catalyst.text.clone(),
                    file_path: path.to_string(),
                    chunk_index: index as i64,
                    similarity,
                    chunk_metadata: serde_json::json!({}),
                    timeline_data: serde_json::json!({}),
                    created_at: Utc::now(),
                })
                .collect();
            stats.rows_upserted += records.len();
            self.db.upsert_catalyst_similarities(&records)?;
            stats.catalysts_refilled += 1;
        }
        stats.duration_ms = start.elapsed().as_millis() as u64;
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::{EmbeddingBackend, EmbeddingService};
    use crate::models::Catalyst;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AxisBackend;

    impl EmbeddingBackend for AxisBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn dimension(&self) -> usize {
            2
        }

        fn name(&self) -> &str {
            "axis-test"
        }
    }

    fn make_catalyst(id: &str, text: &str) -> Catalyst {
        Catalyst {
            id: id.to_string(),
            text: text.to_string(),
            entity: String::new(),
            context: String::new(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let cats = vec![make_catalyst("a", "foo"), make_catalyst("b", "bar")];
        let fp1 = catalysts_fingerprint(&cats);
        let fp2 = catalysts_fingerprint(&cats);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_order_insensitive() {
        let cats1 = vec![make_catalyst("a", "foo"), make_catalyst("b", "bar")];
        let cats2 = vec![make_catalyst("b", "bar"), make_catalyst("a", "foo")];
        assert_eq!(catalysts_fingerprint(&cats1), catalysts_fingerprint(&cats2));
    }

    #[test]
    fn fingerprint_changes_with_different_text() {
        let cats1 = vec![make_catalyst("a", "foo")];
        let cats2 = vec![make_catalyst("a", "bar")];
        assert_ne!(catalysts_fingerprint(&cats1), catalysts_fingerprint(&cats2));
    }

    #[test]
    fn full_rebuild_batches_rows_and_breaks_top_k_ties_deterministically() {
        let db = Database::open_in_memory().unwrap();
        let service = EmbeddingService::new(Box::new(AxisBackend));
        let catalysts = vec![
            make_catalyst("cat-a", "axis"),
            make_catalyst("cat-b", "axis"),
            make_catalyst("cat-c", "axis"),
        ];
        db.insert_catalysts(&catalysts).unwrap();
        let chunks: Vec<(String, u32, Vec<f32>)> = (0..60)
            .rev()
            .map(|i| (format!("doc-{i:03}"), 0, vec![1.0, 0.0]))
            .collect();
        db.replace_chunk_embeddings_for_documents(vec![], chunks, vec![])
            .unwrap();

        let progress = AtomicUsize::new(0);
        let stats = SimilarityPipeline::new(&db, &service)
            .run_with_catalysts(&catalysts, |done, total| {
                assert_eq!(total, catalysts.len());
                assert_eq!(done, progress.fetch_add(1, Ordering::SeqCst) + 1);
            })
            .unwrap();
        assert_eq!(progress.load(Ordering::SeqCst), catalysts.len());
        assert_eq!(
            stats.similarities_computed,
            catalysts.len() * CATALYST_TOP_K
        );

        let first: Vec<(String, String, i64, f64)> = db
            .query_read(
                "SELECT catalyst_id,file_path,chunk_index,similarity
                 FROM catalyst_similarities
                 ORDER BY catalyst_id,file_path,chunk_index",
                vec![],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(first.len(), 150); // exercises more than one 100-row INSERT
        for catalyst in ["cat-a", "cat-b", "cat-c"] {
            let paths: Vec<&str> = first
                .iter()
                .filter(|row| row.0 == catalyst)
                .map(|row| row.1.as_str())
                .collect();
            let expected: Vec<String> =
                (0..CATALYST_TOP_K).map(|i| format!("doc-{i:03}")).collect();
            assert_eq!(
                paths,
                expected.iter().map(String::as_str).collect::<Vec<_>>()
            );
        }

        SimilarityPipeline::new(&db, &service)
            .run_with_catalysts(&catalysts, |_, _| {})
            .unwrap();
        let second: Vec<(String, String, i64, f64)> = db
            .query_read(
                "SELECT catalyst_id,file_path,chunk_index,similarity
                 FROM catalyst_similarities
                 ORDER BY catalyst_id,file_path,chunk_index",
                vec![],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(first, second);
    }

    fn setup_incremental_db(
        chunk_count: usize,
    ) -> (Database, EmbeddingService, Vec<(String, u32, Vec<f32>)>) {
        let db = Database::open_in_memory().unwrap();
        let service = EmbeddingService::new(Box::new(AxisBackend));
        let catalysts = vec![make_catalyst("cat", "axis")];
        db.insert_catalysts(&catalysts).unwrap();
        let chunks: Vec<(String, u32, Vec<f32>)> = (0..chunk_count)
            .map(|i| {
                let angle = (i as f32) * 0.01;
                (format!("doc-{i}"), 0, vec![angle.cos(), angle.sin()])
            })
            .collect();
        db.replace_chunk_embeddings_for_documents(vec![], chunks.clone(), vec![])
            .unwrap();
        SimilarityPipeline::new(&db, &service)
            .run_with_catalysts(&catalysts, |_, _| {})
            .unwrap();
        (db, service, chunks)
    }

    #[test]
    fn new_chunk_is_merged_without_full_matrix_recompute() {
        let (db, service, before) = setup_incremental_db(2);
        let mut after = before.clone();
        after.push(("new-best".into(), 0, vec![1.0, 0.0]));
        db.replace_chunk_embeddings_for_documents(
            vec!["new-best".into()],
            vec![("new-best".into(), 0, vec![1.0, 0.0])],
            vec![],
        )
        .unwrap();
        let stats = SimilarityPipeline::new(&db, &service)
            .maintain_incremental(&before, &after)
            .unwrap();
        assert!(!stats.full_repair);
        assert_eq!(stats.candidate_scores, 1);
        assert!(
            db.get_similarities_for_catalyst("cat", CATALYST_TOP_K)
                .unwrap()
                .iter()
                .any(|row| row.file_path == "new-best")
        );
    }

    #[test]
    fn deleted_top_hit_refills_exact_top_k_but_nonqualifying_delete_does_not() {
        let (db, service, before) = setup_incremental_db(CATALYST_TOP_K + 1);
        db.delete_chunk_embeddings_for_file("doc-0").unwrap();
        let after = db.get_all_chunk_embeddings().unwrap();
        let stats = SimilarityPipeline::new(&db, &service)
            .maintain_incremental(&before, &after)
            .unwrap();
        assert_eq!(stats.catalysts_refilled, 1);
        let rows = db
            .get_similarities_for_catalyst("cat", CATALYST_TOP_K)
            .unwrap();
        assert_eq!(rows.len(), CATALYST_TOP_K);
        assert!(rows.iter().all(|row| row.file_path != "doc-0"));

        let before = after;
        // doc-50 was the prior non-qualifying 51st row after doc-0 was removed
        // and therefore cannot invalidate the materialized top-K.
        db.replace_chunk_embeddings_for_documents(
            vec!["far-away".into()],
            vec![("far-away".into(), 0, vec![-1.0, 0.0])],
            vec![],
        )
        .unwrap();
        let with_nonqualifying = db.get_all_chunk_embeddings().unwrap();
        SimilarityPipeline::new(&db, &service)
            .maintain_incremental(&before, &with_nonqualifying)
            .unwrap();
        db.delete_chunk_embeddings_for_file("far-away").unwrap();
        let after_delete = db.get_all_chunk_embeddings().unwrap();
        let stats = SimilarityPipeline::new(&db, &service)
            .maintain_incremental(&with_nonqualifying, &after_delete)
            .unwrap();
        assert_eq!(stats.catalysts_refilled, 0);
    }

    #[test]
    fn updated_top_hit_removes_stale_score_and_refills() {
        let (db, service, before) = setup_incremental_db(CATALYST_TOP_K + 1);
        db.replace_chunk_embeddings_for_documents(
            vec!["doc-0".into()],
            vec![("doc-0".into(), 0, vec![-1.0, 0.0])],
            vec![],
        )
        .unwrap();
        let after = db.get_all_chunk_embeddings().unwrap();
        let stats = SimilarityPipeline::new(&db, &service)
            .maintain_incremental(&before, &after)
            .unwrap();
        assert_eq!(stats.catalysts_refilled, 1);
        let rows = db
            .get_similarities_for_catalyst("cat", CATALYST_TOP_K)
            .unwrap();
        assert_eq!(rows.len(), CATALYST_TOP_K);
        assert!(rows.iter().all(|row| row.file_path != "doc-0"));
    }
}
