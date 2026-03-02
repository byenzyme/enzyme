use std::fs;
use std::io::{Read as IoRead, Write as IoWrite, BufReader, BufWriter};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::db::{Database, Repository};
use crate::embedding::EmbeddingService;
use crate::models::{Timestamp, EMBEDDING_DIMENSION};

// ---------------------------------------------------------------------------
// EmbeddingPipeline
// ---------------------------------------------------------------------------

/// Orchestrates embedding generation for document chunks and persists the
/// resulting vectors to a compact binary file on disk.
pub struct EmbeddingPipeline<'a> {
    repo: Repository<'a>,
    service: &'a EmbeddingService,
    embeddings_dir: PathBuf,
}

/// Summary statistics returned after an embedding run.
pub struct EmbeddingStats {
    /// Number of documents whose chunks were successfully embedded.
    pub documents_embedded: usize,
    /// Total number of chunks embedded across all documents.
    pub chunks_embedded: usize,
    /// Wall-clock time for the entire pipeline run, in milliseconds.
    pub duration_ms: u64,
}

/// A single stored embedding entry (private — not part of the public API).
struct StoredEmbedding {
    file_path: String,
    chunk_index: u32,
    vector: Vec<f32>,
}

impl<'a> EmbeddingPipeline<'a> {
    /// Create a new pipeline.
    ///
    /// - `db` — the Enzyme database handle (used to read/write document metadata).
    /// - `service` — the embedding model service.
    /// - `embeddings_dir` — directory where `embeddings.bin` will be stored.
    pub fn new(db: &'a Database, service: &'a EmbeddingService, embeddings_dir: PathBuf) -> Self {
        Self {
            repo: Repository::new(db),
            service,
            embeddings_dir,
        }
    }

    /// Run the embedding pipeline.
    ///
    /// 1. Query the database for documents whose embeddings are stale or missing.
    /// 2. Embed each document's chunks via the [`EmbeddingService`].
    /// 3. Merge new embeddings into the on-disk store (replacing stale entries).
    /// 4. Mark documents as embedded in the database.
    ///
    /// `on_progress(completed_docs, total_docs)` is called after each document is
    /// processed so callers can drive a progress bar.
    pub fn run(&self, on_progress: impl Fn(usize, usize)) -> Result<EmbeddingStats> {
        let start = Instant::now();

        // Step 1: Find documents needing embedding
        let docs_needing = self.repo.get_documents_needing_embedding()?;
        if docs_needing.is_empty() {
            info!("No documents need embedding");
            return Ok(EmbeddingStats {
                documents_embedded: 0,
                chunks_embedded: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        let total_docs = docs_needing.len();
        info!(total_docs, "documents need embedding");

        // Ensure the embeddings directory exists
        fs::create_dir_all(&self.embeddings_dir)
            .context("failed to create embeddings directory")?;

        // Load existing embeddings so we can merge
        let mut all_embeddings = self.load_embeddings()?;

        let mut chunks_embedded = 0usize;
        let mut documents_embedded = 0usize;

        // Step 2: Embed each document's chunks
        for (doc_id, source_ref, chunks) in &docs_needing {
            if chunks.is_empty() {
                continue;
            }

            let chunk_refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();

            match self.service.embed_documents(&chunk_refs) {
                Ok(vectors) => {
                    // Remove old embeddings for this source_ref
                    all_embeddings.retain(|e| e.file_path != *source_ref);

                    // Add new embeddings
                    for (i, vector) in vectors.into_iter().enumerate() {
                        all_embeddings.push(StoredEmbedding {
                            file_path: source_ref.clone(),
                            chunk_index: i as u32,
                            vector,
                        });
                    }

                    // Mark as embedded in DB — use the document's current content_hash
                    let now: Timestamp = chrono::Utc::now().timestamp_millis();
                    match self.repo.get_document_by_path(source_ref) {
                        Ok(Some(doc)) => {
                            self.repo
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

                    chunks_embedded += chunk_refs.len();
                    documents_embedded += 1;
                }
                Err(e) => {
                    warn!(source_ref, "failed to embed document: {}", e);
                }
            }

            on_progress(documents_embedded, total_docs);
        }

        // Step 3: Save all embeddings to disk
        self.save_embeddings(&all_embeddings)?;

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
            duration_ms,
        })
    }

    // -----------------------------------------------------------------------
    // Embedding storage (binary format)
    // -----------------------------------------------------------------------
    //
    // Format of `embeddings.bin`:
    //
    // ```text
    // [count: u32 LE]
    // For each embedding:
    //   [file_path_len: u32 LE][file_path: bytes][chunk_index: u32 LE][vector: f32 * 384 LE]
    // ```
    //
    // We rebuild HNSW / BM25 indices from these embeddings on every load, so
    // no separate index file is needed.

    fn embeddings_path(&self) -> PathBuf {
        self.embeddings_dir.join("embeddings.bin")
    }

    fn load_embeddings(&self) -> Result<Vec<StoredEmbedding>> {
        let path = self.embeddings_path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path)
            .with_context(|| format!("failed to open embeddings file: {}", path.display()))?;
        let mut reader = BufReader::new(file);

        let mut buf4 = [0u8; 4];

        // Read count
        reader
            .read_exact(&mut buf4)
            .context("failed to read embedding count")?;
        let count = u32::from_le_bytes(buf4) as usize;

        let mut embeddings = Vec::with_capacity(count);

        for _ in 0..count {
            // Read file_path_len
            reader
                .read_exact(&mut buf4)
                .context("failed to read file path length")?;
            let path_len = u32::from_le_bytes(buf4) as usize;

            // Read file_path
            let mut path_bytes = vec![0u8; path_len];
            reader
                .read_exact(&mut path_bytes)
                .context("failed to read file path bytes")?;
            let file_path =
                String::from_utf8(path_bytes).context("invalid UTF-8 in embedding file path")?;

            // Read chunk_index
            reader
                .read_exact(&mut buf4)
                .context("failed to read chunk index")?;
            let chunk_index = u32::from_le_bytes(buf4);

            // Read vector (EMBEDDING_DIMENSION * 4 bytes)
            let mut vector = vec![0f32; EMBEDDING_DIMENSION];
            // SAFETY: We reinterpret the Vec<f32> backing memory as a &mut [u8]
            // for a single `read_exact` call. The slice length is exactly
            // `EMBEDDING_DIMENSION * size_of::<f32>()` bytes, matching the layout
            // of the f32 vector. The data is written in native-LE format (x86/ARM)
            // and read back on the same architecture.
            let vector_bytes: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(
                    vector.as_mut_ptr() as *mut u8,
                    EMBEDDING_DIMENSION * std::mem::size_of::<f32>(),
                )
            };
            reader
                .read_exact(vector_bytes)
                .context("failed to read embedding vector")?;

            embeddings.push(StoredEmbedding {
                file_path,
                chunk_index,
                vector,
            });
        }

        debug!(count = embeddings.len(), "loaded embeddings from disk");
        Ok(embeddings)
    }

    fn save_embeddings(&self, embeddings: &[StoredEmbedding]) -> Result<()> {
        let path = self.embeddings_path();
        let file = fs::File::create(&path)
            .with_context(|| format!("failed to create embeddings file: {}", path.display()))?;
        let mut writer = BufWriter::new(file);

        // Write count
        writer
            .write_all(&(embeddings.len() as u32).to_le_bytes())
            .context("failed to write embedding count")?;

        for emb in embeddings {
            let path_bytes = emb.file_path.as_bytes();
            writer.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(path_bytes)?;
            writer.write_all(&emb.chunk_index.to_le_bytes())?;

            // SAFETY: We reinterpret the Vec<f32> backing memory as a &[u8] for
            // a single `write_all` call. See `load_embeddings` for the symmetric
            // read. Both sides agree on LE byte order and vector length.
            let vector_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    emb.vector.as_ptr() as *const u8,
                    emb.vector.len() * std::mem::size_of::<f32>(),
                )
            };
            writer.write_all(vector_bytes)?;
        }

        writer.flush().context("failed to flush embeddings file")?;
        info!(
            count = embeddings.len(),
            path = %path.display(),
            "saved embeddings to disk"
        );
        Ok(())
    }
}
