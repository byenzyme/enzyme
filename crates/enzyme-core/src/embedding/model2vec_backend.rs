use std::path::Path;

use anyhow::Result;
use model2vec::Model2Vec;
use tracing::debug;

use super::backend::EmbeddingBackend;

/// Lightweight embedding backend backed by a distilled lookup table.
pub struct DistilledBackend {
    model: Model2Vec,
    dimension: usize,
}

impl DistilledBackend {
    /// Load from a directory containing `model.safetensors`, `tokenizer.json`,
    /// and `config.json`.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let dir_str = model_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("model directory path is not valid UTF-8"))?;

        let model = Model2Vec::from_pretrained(dir_str, None, None)
            .map_err(|e| anyhow::anyhow!("failed to load embedding model: {e}"))?;

        // Probe the dimension by encoding a single dummy sentence.
        let probe = model
            .encode(&["hello"])
            .map_err(|e| anyhow::anyhow!("failed to probe model dimension: {e}"))?;
        let dimension = probe.shape()[1];

        debug!(
            model_dir = %model_dir.display(),
            dimension,
            "loaded embedding backend"
        );

        Ok(Self { model, dimension })
    }
}

impl EmbeddingBackend for DistilledBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let embeddings = self
            .model
            .encode(texts)
            .map_err(|e| anyhow::anyhow!("embedding encode failed: {e}"))?;

        let nrows = embeddings.shape()[0];
        let ncols = embeddings.shape()[1];

        let mut results = Vec::with_capacity(nrows);
        for i in 0..nrows {
            let row = embeddings.row(i);
            let vec: Vec<f32> = row.iter().copied().collect();
            debug_assert_eq!(vec.len(), ncols);
            results.push(vec);
        }

        Ok(results)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn name(&self) -> &str {
        "distilled"
    }
}
