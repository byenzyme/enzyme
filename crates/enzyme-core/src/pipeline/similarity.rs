use std::io::{BufWriter, Read as IoRead, Write as IoWrite, BufReader};
use std::path::PathBuf;
use std::time::Instant;
use std::fs;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::info;

use crate::db::{Database, Repository};
use crate::embedding::EmbeddingService;
use crate::models::{Catalyst, Embedding, CatalystSimilarity, EMBEDDING_DIMENSION};

pub struct SimilarityPipeline<'a> {
    repo: Repository<'a>,
    service: &'a EmbeddingService,
    embeddings_dir: PathBuf,
}

pub struct SimilarityStats {
    pub catalysts_processed: usize,
    pub similarities_computed: usize,
    pub documents_compared: usize,
    pub duration_ms: u64,
}

struct StoredEmbedding {
    file_path: String,
    chunk_index: u32,
    vector: Vec<f32>,
}

impl<'a> SimilarityPipeline<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService, embeddings_dir: PathBuf) -> Self {
        Self {
            repo: Repository::new(db),
            service,
            embeddings_dir,
        }
    }

    /// Run similarity using catalysts from the local database.
    pub fn run(&self, on_progress: impl Fn(usize, usize)) -> Result<SimilarityStats> {
        let catalysts = self.repo.get_all_catalysts()?;
        self.run_with_catalysts(&catalysts, true, on_progress)
    }

    /// Run similarity with externally-provided catalysts (e.g. from a source vault via `apply`).
    ///
    /// When `cache_catalyst_embeddings` is true, catalyst embeddings are saved to disk
    /// for reuse in `catalyze_search`. Set to false when the catalysts come from an
    /// external source and caching would be misleading.
    pub fn run_with_catalysts(
        &self,
        catalysts: &[Catalyst],
        cache_catalyst_embeddings: bool,
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

        // Embed all catalyst texts
        let catalyst_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
        let catalyst_embeddings = self.service.embed_documents(&catalyst_texts)?;

        // Cache catalyst embeddings to disk for fast reuse in catalyze_search
        if cache_catalyst_embeddings {
            let cat_ids: Vec<&str> = catalysts.iter().map(|c| c.id.as_str()).collect();
            self.save_catalyst_embeddings(&cat_ids, &catalyst_embeddings)?;
        }

        // Load all chunk embeddings from disk
        let chunk_embeddings = self.load_embeddings()?;
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
            for emb in &chunk_embeddings {
                unique.insert(&emb.file_path);
            }
            unique.len()
        };

        let total = catalysts.len();
        let mut similarities_computed = 0usize;

        // For each catalyst, compute cosine similarity against all chunk embeddings
        for (i, (catalyst, cat_vec)) in catalysts.iter().zip(catalyst_embeddings.iter()).enumerate() {
            on_progress(i + 1, total);

            // Compute similarities against all chunks
            let mut sims: Vec<(f64, &StoredEmbedding)> = chunk_embeddings
                .iter()
                .map(|chunk_emb| {
                    let sim = Embedding::cosine_similarity(cat_vec, &chunk_emb.vector) as f64;
                    (sim, chunk_emb)
                })
                .collect();

            // Sort by similarity descending, keep top 50
            sims.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            sims.truncate(50);

            // Create CatalystSimilarity records
            let similarity_records: Vec<CatalystSimilarity> = sims
                .into_iter()
                .map(|(sim, chunk_emb)| CatalystSimilarity {
                    id: None,
                    catalyst_id: catalyst.id.clone(),
                    catalyst_text: catalyst.text.clone(),
                    file_path: chunk_emb.file_path.clone(),
                    chunk_index: chunk_emb.chunk_index as i64,
                    similarity: sim,
                    chunk_metadata: serde_json::json!({}),
                    timeline_data: serde_json::json!({}),
                    created_at: Utc::now(),
                })
                .collect();

            similarities_computed += similarity_records.len();
            self.repo.upsert_catalyst_similarities(&similarity_records)?;
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        info!(catalysts_processed = total, similarities_computed, duration_ms, "similarity computation complete");

        Ok(SimilarityStats {
            catalysts_processed: total,
            similarities_computed,
            documents_compared,
            duration_ms,
        })
    }

    fn load_embeddings(&self) -> Result<Vec<StoredEmbedding>> {
        let path = self.embeddings_dir.join("embeddings.bin");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path)
            .with_context(|| format!("failed to open embeddings file: {}", path.display()))?;
        let mut reader = BufReader::new(file);

        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4) as usize;

        let mut embeddings = Vec::with_capacity(count);

        for _ in 0..count {
            reader.read_exact(&mut buf4)?;
            let path_len = u32::from_le_bytes(buf4) as usize;

            let mut path_bytes = vec![0u8; path_len];
            reader.read_exact(&mut path_bytes)?;
            let file_path = String::from_utf8(path_bytes)
                .context("invalid UTF-8 in embedding file path")?;

            reader.read_exact(&mut buf4)?;
            let chunk_index = u32::from_le_bytes(buf4);

            let mut vector = vec![0f32; EMBEDDING_DIMENSION];
            let vector_bytes: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(
                    vector.as_mut_ptr() as *mut u8,
                    EMBEDDING_DIMENSION * 4,
                )
            };
            reader.read_exact(vector_bytes)?;

            embeddings.push(StoredEmbedding {
                file_path,
                chunk_index,
                vector,
            });
        }

        Ok(embeddings)
    }

    /// Save catalyst embeddings to disk for reuse in `catalyze_search`.
    ///
    /// Format: `[count: u32 LE]` then per entry:
    /// `[id_len: u32 LE][id_bytes][vector: f32 * EMBEDDING_DIMENSION LE]`
    fn save_catalyst_embeddings(&self, ids: &[&str], embeddings: &[Vec<f32>]) -> Result<()> {
        let path = self.embeddings_dir.join("catalyst_embeddings.bin");
        fs::create_dir_all(&self.embeddings_dir)?;
        let file = fs::File::create(&path)
            .with_context(|| format!("failed to create catalyst embeddings file: {}", path.display()))?;
        let mut writer = BufWriter::new(file);

        writer.write_all(&(ids.len() as u32).to_le_bytes())?;

        for (id, vec) in ids.iter().zip(embeddings.iter()) {
            let id_bytes = id.as_bytes();
            writer.write_all(&(id_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(id_bytes)?;

            let vector_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    vec.as_ptr() as *const u8,
                    vec.len() * std::mem::size_of::<f32>(),
                )
            };
            writer.write_all(vector_bytes)?;
        }

        writer.flush()?;
        info!(count = ids.len(), path = %path.display(), "saved catalyst embeddings to disk");
        Ok(())
    }
}

/// A cached catalyst embedding loaded from disk.
pub struct CachedCatalystEmbedding {
    pub id: String,
    pub vector: Vec<f32>,
}

/// Load cached catalyst embeddings from `catalyst_embeddings.bin`.
///
/// Returns `None` if the file doesn't exist (not yet computed).
pub fn load_catalyst_embeddings(embeddings_dir: &std::path::Path) -> Result<Option<Vec<CachedCatalystEmbedding>>> {
    let path = embeddings_dir.join("catalyst_embeddings.bin");
    if !path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(&path)
        .with_context(|| format!("failed to open catalyst embeddings: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut buf4 = [0u8; 4];
    reader.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut result = Vec::with_capacity(count);

    for _ in 0..count {
        reader.read_exact(&mut buf4)?;
        let id_len = u32::from_le_bytes(buf4) as usize;

        let mut id_bytes = vec![0u8; id_len];
        reader.read_exact(&mut id_bytes)?;
        let id = String::from_utf8(id_bytes).context("invalid UTF-8 in catalyst id")?;

        let mut vector = vec![0f32; EMBEDDING_DIMENSION];
        let vector_bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(
                vector.as_mut_ptr() as *mut u8,
                EMBEDDING_DIMENSION * std::mem::size_of::<f32>(),
            )
        };
        reader.read_exact(vector_bytes)?;

        result.push(CachedCatalystEmbedding { id, vector });
    }

    info!(count = result.len(), "loaded cached catalyst embeddings");
    Ok(Some(result))
}
