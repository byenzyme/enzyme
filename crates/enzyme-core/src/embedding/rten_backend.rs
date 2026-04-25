use std::path::Path;

use anyhow::Result;
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::NdTensor;
use tokenizers::Tokenizer;
use tracing::debug;

use super::backend::EmbeddingBackend;
use super::normalize::{l2_normalize, mean_pool_flat};

/// Maximum sequence length for the model. Inputs are padded/truncated to this length.
const MAX_SEQ_LEN: usize = 128;

/// Embedding backend powered by [rten](https://github.com/nicktolhurst/rten),
/// a pure-Rust ONNX runtime. Used for Arctic Embed XS (22M params, 384 dims).
pub struct RtenBackend {
    model: Model,
    tokenizer: Tokenizer,
    dim: usize,
}

impl RtenBackend {
    /// Load an ONNX model and HuggingFace `tokenizer.json`.
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let model = Model::load_file(model_path)
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

        // Probe the model's output dimension by running a dummy input
        let dim = {
            let dummy_enc = tokenizer
                .encode("hello", true)
                .map_err(|e| anyhow::anyhow!("tokenizer probe failed: {e}"))?;
            let ids: Vec<i32> = dummy_enc.get_ids().iter().map(|&id| id as i32).collect();
            let mask: Vec<i32> = dummy_enc
                .get_attention_mask()
                .iter()
                .map(|&m| m as i32)
                .collect();

            let mut padded_ids = vec![0i32; MAX_SEQ_LEN];
            let mut padded_mask = vec![0i32; MAX_SEQ_LEN];
            for i in 0..ids.len().min(MAX_SEQ_LEN) {
                padded_ids[i] = ids[i];
                padded_mask[i] = mask[i];
            }

            let ids_tensor = NdTensor::from_data([1, MAX_SEQ_LEN], padded_ids);
            let mask_tensor = NdTensor::from_data([1, MAX_SEQ_LEN], padded_mask);
            let type_tensor = NdTensor::from_data([1, MAX_SEQ_LEN], vec![0i32; MAX_SEQ_LEN]);

            let input_ids_id = model.node_id("input_ids")?;
            let attention_mask_id = model.node_id("attention_mask")?;
            let token_type_ids_id = model.node_id("token_type_ids")?;
            let output_id = model.output_ids()[0];

            let inputs = vec![
                (input_ids_id, ids_tensor.into()),
                (attention_mask_id, mask_tensor.into()),
                (token_type_ids_id, type_tensor.into()),
            ];

            let [output] = model.run_n(inputs, [output_id], None)?;
            let hs: NdTensor<f32, 3> = output.try_into()?;
            hs.shape()[2]
        };

        debug!(
            model_path = %model_path.display(),
            tokenizer_path = %tokenizer_path.display(),
            max_seq_len = MAX_SEQ_LEN,
            dim = dim,
            "loaded RtenBackend"
        );

        Ok(Self {
            model,
            tokenizer,
            dim,
        })
    }

    /// Tokenize a batch of texts into flat `input_ids` and `attention_mask` vectors.
    fn tokenize(&self, texts: &[&str]) -> Result<(Vec<i32>, Vec<i32>)> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();

        let mut input_ids: Vec<i32> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);
        let mut attention_mask: Vec<i32> = Vec::with_capacity(batch_size * MAX_SEQ_LEN);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();

            for i in 0..MAX_SEQ_LEN {
                input_ids.push(if i < ids.len() { ids[i] as i32 } else { 0 });
                attention_mask.push(if i < mask.len() { mask[i] as i32 } else { 0 });
            }
        }

        Ok((input_ids, attention_mask))
    }

    /// Post-process model output: mean pool token embeddings using the
    /// attention mask, then L2-normalize.
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

impl EmbeddingBackend for RtenBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let batch_size = texts.len();
        let (input_ids, attention_mask) = self.tokenize(texts)?;

        let mask_f32: Vec<f32> = attention_mask.iter().map(|&v| v as f32).collect();

        let ids_tensor = NdTensor::from_data([batch_size, MAX_SEQ_LEN], input_ids);
        let mask_tensor = NdTensor::from_data([batch_size, MAX_SEQ_LEN], attention_mask);
        let token_type_ids = vec![0i32; batch_size * MAX_SEQ_LEN];
        let type_tensor = NdTensor::from_data([batch_size, MAX_SEQ_LEN], token_type_ids);

        let input_ids_id = self
            .model
            .node_id("input_ids")
            .map_err(|e| anyhow::anyhow!("missing input_ids node: {e}"))?;
        let attention_mask_id = self
            .model
            .node_id("attention_mask")
            .map_err(|e| anyhow::anyhow!("missing attention_mask node: {e}"))?;
        let token_type_ids_id = self
            .model
            .node_id("token_type_ids")
            .map_err(|e| anyhow::anyhow!("missing token_type_ids node: {e}"))?;

        let output_id = self.model.output_ids()[0];

        let inputs = vec![
            (input_ids_id, ids_tensor.into()),
            (attention_mask_id, mask_tensor.into()),
            (token_type_ids_id, type_tensor.into()),
        ];

        let [output] = self
            .model
            .run_n(inputs, [output_id], None)
            .map_err(|e| anyhow::anyhow!("rten inference failed: {e}"))?;

        let hs: NdTensor<f32, 3> = output
            .try_into()
            .map_err(|e| anyhow::anyhow!("failed to extract f32 output: {e}"))?;

        let shape = hs.shape();
        let seq_len = shape[1];
        let hidden_dim = shape[2];
        let hs_data = hs.into_data();

        Ok(Self::pool_and_normalize(
            &hs_data,
            &mask_f32,
            batch_size,
            seq_len,
            hidden_dim,
        ))
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "rten-arctic-xs"
    }
}
