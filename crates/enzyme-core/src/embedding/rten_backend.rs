use std::path::Path;

use anyhow::{Context, Result};
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::NdTensor;
use tokenizers::Tokenizer;
use tracing::debug;

use super::backend::EmbeddingBackend;
use super::normalize::{l2_normalize, mean_pool};
use crate::models::EMBEDDING_DIMENSION;

/// Maximum sequence length for the model. Inputs are padded/truncated to this length.
const MAX_SEQ_LEN: usize = 128;

/// Embedding backend powered by [rten](https://github.com/robertknight/rten),
/// a pure-Rust ONNX inference engine.
///
/// The model is expected to be a sentence-transformer style encoder
/// (e.g. MiniLM) that accepts `input_ids` and `attention_mask` and produces
/// `last_hidden_state` of shape `[batch, seq_len, hidden_dim]`.
///
/// **Note:** rten v0.14 requires models in `.rten` format. Use `rten-convert`
/// to convert from ONNX. Direct `.onnx` loading is available from v0.23+.
pub struct RtenBackend {
    model: Model,
    tokenizer: Tokenizer,
}

impl RtenBackend {
    /// Load a model from an `.rten` file and a HuggingFace `tokenizer.json`.
    ///
    /// The tokenizer is configured with padding to [`MAX_SEQ_LEN`] and truncation
    /// so that all inputs have uniform length.
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let model = Model::load_file(model_path)
            .map_err(|e| anyhow::anyhow!("failed to load rten model: {e}"))?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        // Configure padding to fixed length so every input has the same shape.
        let pad_params = tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::Fixed(MAX_SEQ_LEN),
            pad_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        };
        tokenizer.with_padding(Some(pad_params));

        // Configure truncation to max length.
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
            "loaded RtenBackend"
        );

        Ok(Self { model, tokenizer })
    }

    /// Tokenize a batch of texts into `input_ids` and `attention_mask` tensors.
    ///
    /// Returns `(input_ids, attention_mask)` where each is an `NdTensor<i32, 2>`
    /// with shape `[batch_size, MAX_SEQ_LEN]`.
    fn tokenize(&self, texts: &[&str]) -> Result<(NdTensor<i32, 2>, NdTensor<i32, 2>)> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();

        let mut input_ids_flat: Vec<i32> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);
        let mut attention_mask_flat: Vec<i32> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();

            // The tokenizer should have padded/truncated to MAX_SEQ_LEN,
            // but clamp defensively.
            for i in 0..MAX_SEQ_LEN {
                input_ids_flat.push(if i < ids.len() { ids[i] as i32 } else { 0 });
                attention_mask_flat.push(if i < mask.len() { mask[i] as i32 } else { 0 });
            }
        }

        let input_ids = NdTensor::from_data([batch_size, MAX_SEQ_LEN], input_ids_flat);
        let attention_mask = NdTensor::from_data([batch_size, MAX_SEQ_LEN], attention_mask_flat);

        Ok((input_ids, attention_mask))
    }

    /// Post-process model output: mean pool token embeddings using the
    /// attention mask, then L2-normalize.
    ///
    /// `hidden_states_flat` is the raw model output flattened from shape
    /// `[batch, seq_len, hidden_dim]`.
    /// `mask_flat` is the attention mask flattened from shape `[batch, MAX_SEQ_LEN]`,
    /// stored as `f32`.
    fn pool_and_normalize(
        hidden_states_flat: &[f32],
        mask_flat: &[f32],
        batch_size: usize,
        seq_len: usize,
        hidden_dim: usize,
    ) -> Vec<Vec<f32>> {
        let mut results = Vec::with_capacity(batch_size);

        for b in 0..batch_size {
            // Extract per-token embeddings for this batch item.
            let mut token_embeddings = Vec::with_capacity(seq_len);
            for s in 0..seq_len {
                let offset = b * seq_len * hidden_dim + s * hidden_dim;
                token_embeddings.push(hidden_states_flat[offset..offset + hidden_dim].to_vec());
            }

            // Extract per-token attention mask for this batch item.
            let mask_offset = b * MAX_SEQ_LEN;
            let mask_end = (mask_offset + seq_len).min(mask_flat.len());
            let mask_slice = &mask_flat[mask_offset..mask_end];

            let mut pooled = mean_pool(&token_embeddings, mask_slice);
            l2_normalize(&mut pooled);
            results.push(pooled);
        }

        results
    }
}

impl EmbeddingBackend for RtenBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let (input_ids, attention_mask) = self.tokenize(texts)?;

        // Preserve the attention mask as f32 for pooling after inference.
        let batch_size = attention_mask.shape()[0];
        let mask_data: Vec<f32> = attention_mask.iter().map(|&v| v as f32).collect();

        // Resolve model graph node IDs.
        let input_ids_id = self
            .model
            .node_id("input_ids")
            .map_err(|e| anyhow::anyhow!("model missing 'input_ids' input: {e}"))?;
        let attention_mask_id = self
            .model
            .node_id("attention_mask")
            .map_err(|e| anyhow::anyhow!("model missing 'attention_mask' input: {e}"))?;

        let output_ids = self.model.output_ids();
        anyhow::ensure!(!output_ids.is_empty(), "model has no outputs");
        let last_hidden_state_id = output_ids[0];

        // Run inference.
        let inputs = vec![
            (input_ids_id, input_ids.into()),
            (attention_mask_id, attention_mask.into()),
        ];

        let mut outputs = self
            .model
            .run(inputs, &[last_hidden_state_id], None)
            .map_err(|e| anyhow::anyhow!("model inference failed: {e}"))?;

        anyhow::ensure!(!outputs.is_empty(), "model returned no outputs");

        // Extract float tensor with shape [batch, seq_len, hidden_dim].
        let hidden_states = outputs
            .remove(0)
            .into_tensor::<f32>()
            .context("expected f32 output tensor from model")?;

        let hs_shape = hidden_states.shape();
        anyhow::ensure!(
            hs_shape.len() == 3,
            "expected 3D output [batch, seq_len, hidden], got shape {:?}",
            hs_shape
        );

        let seq_len = hs_shape[1];
        let hidden_dim = hs_shape[2];
        let hs_data = hidden_states.to_vec();

        Ok(Self::pool_and_normalize(
            &hs_data,
            &mask_data,
            batch_size,
            seq_len,
            hidden_dim,
        ))
    }

    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn name(&self) -> &str {
        "rten"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the full post-processing pipeline: mean pool + L2 normalize.
    #[test]
    fn test_pool_and_normalize_basic() {
        // Simulate model output: batch=1, seq_len=4, hidden_dim=3
        // Tokens: [1,2,3], [4,5,6], [0,0,0], [0,0,0]
        // Mask:   [1,      1,       0,       0]
        let hidden = vec![
            1.0, 2.0, 3.0, // token 0
            4.0, 5.0, 6.0, // token 1
            0.0, 0.0, 0.0, // token 2 (pad)
            0.0, 0.0, 0.0, // token 3 (pad)
        ];
        // mask_flat has MAX_SEQ_LEN entries per batch item; only first 4 matter
        let mut mask = vec![0.0f32; MAX_SEQ_LEN];
        mask[0] = 1.0;
        mask[1] = 1.0;

        let results = RtenBackend::pool_and_normalize(&hidden, &mask, 1, 4, 3);

        assert_eq!(results.len(), 1);
        let v = &results[0];
        assert_eq!(v.len(), 3);

        // Mean of attended tokens: [2.5, 3.5, 4.5]
        // Norm = sqrt(2.5^2 + 3.5^2 + 4.5^2) = sqrt(6.25 + 12.25 + 20.25) = sqrt(38.75)
        let expected_norm = (2.5f32.powi(2) + 3.5f32.powi(2) + 4.5f32.powi(2)).sqrt();
        assert!((v[0] - 2.5 / expected_norm).abs() < 1e-5);
        assert!((v[1] - 3.5 / expected_norm).abs() < 1e-5);
        assert!((v[2] - 4.5 / expected_norm).abs() < 1e-5);

        // Verify unit norm.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    /// Test multi-item batch post-processing.
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
        // Batch 0: only first token attended
        mask[0] = 1.0;
        // Batch 1: both tokens attended
        mask[MAX_SEQ_LEN] = 1.0;
        mask[MAX_SEQ_LEN + 1] = 1.0;

        let results = RtenBackend::pool_and_normalize(&hidden, &mask, 2, 2, 2);

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
