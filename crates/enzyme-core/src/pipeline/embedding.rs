use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::Timestamp;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of documents to embed per run (most recent by `created_at`).
/// Mirrors `MAX_EMBEDDING_FILES` from the Python implementation.
pub const MAX_EMBEDDING_FILES: usize = 1024;

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

pub struct EmbeddingPipeline<'a> {
    db: &'a Database,
    service: &'a EmbeddingService,
}

pub struct EmbeddingStats {
    pub documents_embedded: usize,
    pub chunks_embedded: usize,
    pub documents_pruned: usize,
    /// Wall-clock time for the entire pipeline run, in milliseconds.
    pub duration_ms: u64,
}

impl<'a> EmbeddingPipeline<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self { db, service }
    }

    /// Run embedding (capped to most recent N documents).
    pub fn run(&self, on_progress: impl Fn(usize, usize)) -> Result<EmbeddingStats> {
        self.run_inner(Some(MAX_EMBEDDING_FILES), None, on_progress)
    }

    /// Run embedding with no document cap — embeds all documents in the DB.
    /// Used by `apply` where the target directory should be fully embedded.
    pub fn run_all(&self, on_progress: impl Fn(usize, usize)) -> Result<EmbeddingStats> {
        self.run_inner(None, None, on_progress)
    }

    /// Run embedding only for documents indexed since `since_ms` (epoch millis).
    pub fn run_since(&self, since_ms: i64, on_progress: impl Fn(usize, usize)) -> Result<EmbeddingStats> {
        self.run_inner(Some(MAX_EMBEDDING_FILES), Some(since_ms), on_progress)
    }

    fn run_inner(&self, max_docs: Option<usize>, indexed_since: Option<i64>, on_progress: impl Fn(usize, usize)) -> Result<EmbeddingStats> {
        let start = Instant::now();

        // Step 1: Prune stale chunk embeddings for deleted docs.
        let documents_pruned = self.db.prune_stale_chunk_embeddings()?;
        if documents_pruned > 0 {
            info!(documents_pruned, "pruned stale chunk embeddings");
        }

        // Step 2: Find documents needing embedding
        let docs_needing = self.db.get_documents_needing_embedding(max_docs, indexed_since)?;
        if docs_needing.is_empty() {
            info!("No documents need embedding");
            return Ok(EmbeddingStats {
                documents_embedded: 0,
                chunks_embedded: 0,
                documents_pruned,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        let total_docs = docs_needing.len();
        info!(total_docs, "documents need embedding");

        // Step 3: Collect all chunks into one flat list for batched embedding.
        let mut flat_chunks: Vec<&str> = Vec::new();
        let mut doc_chunk_ranges: Vec<(usize, usize)> = Vec::new();
        let mut non_empty_docs: Vec<usize> = Vec::new();

        for (i, (_doc_id, _source_ref, chunks)) in docs_needing.iter().enumerate() {
            if chunks.is_empty() {
                continue;
            }
            let start_idx = flat_chunks.len();
            for chunk in chunks {
                flat_chunks.push(chunk.as_str());
            }
            doc_chunk_ranges.push((start_idx, chunks.len()));
            non_empty_docs.push(i);
        }

        let total_chunks = flat_chunks.len();
        info!(total_chunks, total_docs = non_empty_docs.len(), "embedding all chunks in cross-document batches");

        // Step 4: Embed all chunks at once.
        let total_non_empty = non_empty_docs.len();
        let all_vectors = self.service.embed_documents_with_progress(
            &flat_chunks,
            |completed_chunks, _total_chunks| {
                let mut docs_done = 0;
                for (start, count) in &doc_chunk_ranges {
                    if start + count <= completed_chunks {
                        docs_done += 1;
                    } else {
                        break;
                    }
                }
                on_progress(docs_done, total_non_empty);
            },
        )?;

        // Step 5: Scatter results back to documents and write to DB.
        let mut chunks_embedded = 0usize;
        let mut documents_embedded = 0usize;

        for (range_idx, &doc_idx) in non_empty_docs.iter().enumerate() {
            let (doc_id, source_ref, _chunks) = &docs_needing[doc_idx];
            let (start_idx, count) = doc_chunk_ranges[range_idx];
            let vectors = &all_vectors[start_idx..start_idx + count];

            // Replace old chunk embeddings for this file
            self.db.delete_chunk_embeddings_for_file(source_ref)?;

            // Insert new chunk embeddings
            let entries: Vec<(String, u32, Vec<f32>)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (source_ref.clone(), i as u32, v.clone()))
                .collect();
            self.db.upsert_chunk_embeddings(&entries)?;

            // Mark as embedded in DB
            let now: Timestamp = chrono::Utc::now().timestamp_millis();
            match self.db.get_document_by_path(source_ref) {
                Ok(Some(doc)) => {
                    self.db
                        .mark_document_embedded(*doc_id, &doc.content_hash, now)?;
                }
                Ok(None) => {
                    warn!(
                        source_ref,
                        "document disappeared from DB before marking embedded"
                    );
                }
                Err(e) => {
                    warn!(source_ref, "failed to look up document for hash: {}", e);
                }
            }

            chunks_embedded += count;
            documents_embedded += 1;
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        info!(
            documents_embedded,
            chunks_embedded,
            duration_ms,
            "embedding complete"
        );

        Ok(EmbeddingStats {
            documents_embedded,
            chunks_embedded,
            documents_pruned,
            duration_ms,
        })
    }
}
