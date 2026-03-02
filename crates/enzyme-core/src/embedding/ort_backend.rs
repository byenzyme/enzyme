use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;
use tracing::debug;

use super::backend::EmbeddingBackend;
use super::normalize::{l2_normalize, mean_pool_flat};
use crate::models::EMBEDDING_DIMENSION;

/// Maximum sequence length for the model. Inputs are padded/truncated to this length.
const MAX_SEQ_LEN: usize = 128;

/// Embedding backend powered by [ort](https://github.com/pykeio/ort),
/// Rust bindings for ONNX Runtime.
///
/// This backend supports models with custom ONNX operators (e.g.
/// `com.microsoft/MultiHeadAttention` used by MongoDB/mdbr-leaf-ir)
/// that pure-Rust runtimes like rten cannot handle.
pub struct OrtBackend {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl OrtBackend {
    /// Load an ONNX model and HuggingFace `tokenizer.json`.
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create ort session builder: {e}"))?
            .with_intra_threads(4)
            .map_err(|e| anyhow::anyhow!("failed to set intra threads: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("failed to load onnx model: {e}"))?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        let pad_params = tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::Fixed(MAX_SEQ_LEN),
            pad_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        };
        tokenizer.with_padding(Some(pad_params));

        let trunc_params = tokenizers::TruncationParams {
            max_length: MAX_SEQ_LEN,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(trunc_params))
            .map_err(|e| anyhow::anyhow!("failed to set truncation: {e}"))?;

        debug!(
            model_path = %model_path.display(),
            tokenizer_path = %tokenizer_path.display(),
            max_seq_len = MAX_SEQ_LEN,
            "loaded OrtBackend"
        );

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    /// Tokenize a batch of texts into flat `input_ids` and `attention_mask` vectors.
    fn tokenize(&self, texts: &[&str]) -> Result<(Vec<i64>, Vec<i64>)> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();

        let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();

            for i in 0..MAX_SEQ_LEN {
                input_ids.push(if i < ids.len() { ids[i] as i64 } else { 0 });
                attention_mask.push(if i < mask.len() { mask[i] as i64 } else { 0 });
            }
        }

        Ok((input_ids, attention_mask))
    }

    /// Post-process model output: mean pool token embeddings using the
    /// attention mask, then L2-normalize.  Operates directly on the flat
    /// buffers to avoid per-token `Vec` allocations.
    fn pool_and_normalize(
        hidden_states_flat: &[f32],
        mask_flat: &[f32],
        batch_size: usize,
        seq_len: usize,
        hidden_dim: usize,
    ) -> Vec<Vec<f32>> {
        let mut results = Vec::with_capacity(batch_size);

        for b in 0..batch_size {
            let hs_offset = b * seq_len * hidden_dim;
            let hs_slice = &hidden_states_flat[hs_offset..hs_offset + seq_len * hidden_dim];

            let mask_offset = b * MAX_SEQ_LEN;
            let mask_end = (mask_offset + seq_len).min(mask_flat.len());
            let mask_slice = &mask_flat[mask_offset..mask_end];

            let mut pooled = mean_pool_flat(hs_slice, mask_slice, seq_len, hidden_dim);
            l2_normalize(&mut pooled);
            results.push(pooled);
        }

        results
    }
}

impl EmbeddingBackend for OrtBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let batch_size = texts.len();
        let (input_ids, attention_mask) = self.tokenize(texts)?;

        // Keep attention mask as f32 for pooling.
        let mask_f32: Vec<f32> = attention_mask.iter().map(|&v| v as f32).collect();

        // Create owned ort Tensor values from (shape, data) tuples.
        let ids_tensor = Tensor::from_array(
            ([batch_size, MAX_SEQ_LEN], input_ids.into_boxed_slice()),
        )
        .map_err(|e| anyhow::anyhow!("failed to create input_ids tensor: {e}"))?;

        let mask_tensor = Tensor::from_array(
            ([batch_size, MAX_SEQ_LEN], attention_mask.into_boxed_slice()),
        )
        .map_err(|e| anyhow::anyhow!("failed to create attention_mask tensor: {e}"))?;

        // BERT models require token_type_ids (all zeros for single-sentence encoding).
        let token_type_ids = vec![0i64; batch_size * MAX_SEQ_LEN];
        let type_tensor = Tensor::from_array(
            ([batch_size, MAX_SEQ_LEN], token_type_ids.into_boxed_slice()),
        )
        .map_err(|e| anyhow::anyhow!("failed to create token_type_ids tensor: {e}"))?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?;

        let outputs = session
            .run(ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
                "token_type_ids" => type_tensor
            ])
            .map_err(|e| anyhow::anyhow!("ort inference failed: {e}"))?;

        // The first output is last_hidden_state with shape [batch, seq_len, hidden_dim].
        // outputs is indexed by name or position; get the first value.
        let output_value = outputs
            .iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("model returned no outputs"))?
            .1;

        // try_extract_tensor returns (&Shape, &[f32])
        // Shape derefs to &[i64]
        let (shape, hs_data) = output_value
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract f32 output: {e}"))?;

        anyhow::ensure!(
            shape.len() == 3,
            "expected 3D output [batch, seq_len, hidden], got shape {:?}",
            &**shape
        );

        let seq_len = shape[1] as usize;
        let hidden_dim = shape[2] as usize;

        Ok(Self::pool_and_normalize(
            hs_data,
            &mask_f32,
            batch_size,
            seq_len,
            hidden_dim,
        ))
    }

    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn name(&self) -> &str {
        "ort"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_and_normalize_basic() {
        // batch=1, seq_len=4, hidden_dim=3
        let hidden = vec![
            1.0, 2.0, 3.0, // token 0
            4.0, 5.0, 6.0, // token 1
            0.0, 0.0, 0.0, // token 2 (pad)
            0.0, 0.0, 0.0, // token 3 (pad)
        ];
        let mut mask = vec![0.0f32; MAX_SEQ_LEN];
        mask[0] = 1.0;
        mask[1] = 1.0;

        let results = OrtBackend::pool_and_normalize(&hidden, &mask, 1, 4, 3);

        assert_eq!(results.len(), 1);
        let v = &results[0];
        assert_eq!(v.len(), 3);

        // Mean of attended tokens: [2.5, 3.5, 4.5]
        let expected_norm = (2.5f32.powi(2) + 3.5f32.powi(2) + 4.5f32.powi(2)).sqrt();
        assert!((v[0] - 2.5 / expected_norm).abs() < 1e-5);
        assert!((v[1] - 3.5 / expected_norm).abs() < 1e-5);
        assert!((v[2] - 4.5 / expected_norm).abs() < 1e-5);

        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn test_pool_and_normalize_batch() {
        // batch=2, seq_len=2, hidden_dim=2
        let hidden = vec![
            3.0, 4.0, // batch 0, token 0
            0.0, 0.0, // batch 0, token 1 (pad)
            1.0, 0.0, // batch 1, token 0
            0.0, 1.0, // batch 1, token 1
        ];
        let mut mask = vec![0.0f32; MAX_SEQ_LEN * 2];
        mask[0] = 1.0;
        mask[MAX_SEQ_LEN] = 1.0;
        mask[MAX_SEQ_LEN + 1] = 1.0;

        let results = OrtBackend::pool_and_normalize(&hidden, &mask, 2, 2, 2);

        assert_eq!(results.len(), 2);

        // Batch 0: mean = [3, 4], normalized = [0.6, 0.8]
        assert!((results[0][0] - 0.6).abs() < 1e-5);
        assert!((results[0][1] - 0.8).abs() < 1e-5);

        // Batch 1: mean = [0.5, 0.5], normalized = [1/sqrt(2), 1/sqrt(2)]
        let inv_sqrt2 = 1.0 / 2.0f32.sqrt();
        assert!((results[1][0] - inv_sqrt2).abs() < 1e-5);
        assert!((results[1][1] - inv_sqrt2).abs() < 1e-5);
    }

    #[test]
    fn test_dimension_constant() {
        assert_eq!(EMBEDDING_DIMENSION, 384);
    }

    #[test]
    fn test_max_seq_len_constant() {
        assert_eq!(MAX_SEQ_LEN, 128);
    }
}
