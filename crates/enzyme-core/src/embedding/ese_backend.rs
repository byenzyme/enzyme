use std::time::Instant;

use anyhow::Result;
use tracing::debug;

use super::backend::EmbeddingBackend;

/// The encoders model, width, and quantization this binary is built with.
/// Must match the `encoders` features in `crates/enzyme-core/Cargo.toml`
/// (a test checks). Stored vectors carry it, so changing the model re-embeds
/// every vault once, locally, without touching catalysts.
pub const ENCODERS_MODEL: &str = "static-retrieval-mrl-en-v1";
pub const ENCODERS_WIDTH: &str = "mrl-512";
pub const ENCODERS_QUANTIZATION: &str = "int8";

/// Embedding backend using the `encoders` crate (static embeddings).
///
/// Weights are compiled into the binary at build time — no model files needed.
pub struct EseBackend;

impl EseBackend {
    pub fn new() -> Self {
        Self
    }
}

impl EmbeddingBackend for EseBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encode_start = Instant::now();
        let embeddings = encoders::encode(texts.iter().copied());
        let raw_encode = encode_start.elapsed();

        let convert_start = Instant::now();
        let vectors = embeddings.into_iter().map(|arr| arr.to_vec()).collect();
        let conversion = convert_start.elapsed();

        debug!(
            texts = texts.len(),
            raw_encode_ms = raw_encode.as_millis() as u64,
            conversion_ms = conversion.as_millis() as u64,
            "ese embed batch complete"
        );

        Ok(vectors)
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(encoders::encode_single(text).to_vec())
    }

    fn dimension(&self) -> usize {
        encoders::DIMENSIONS
    }

    fn name(&self) -> &str {
        // Recorded in catalyst generation contracts; keep it stable so a model
        // change re-embeds without regenerating catalysts.
        "ese"
    }

    fn vector_fingerprint(&self) -> String {
        format!(
            "encoders:{ENCODERS_MODEL}:{ENCODERS_WIDTH}:{ENCODERS_QUANTIZATION}:{}",
            encoders::DIMENSIONS
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Embedding;

    #[test]
    fn fingerprint_constants_match_cargo_features() {
        let manifest = include_str!("../../Cargo.toml");
        let line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("encoders ="))
            .expect("encoders dependency line");
        for feature in [ENCODERS_MODEL, ENCODERS_WIDTH, ENCODERS_QUANTIZATION] {
            assert!(
                line.contains(&format!("\"{feature}\"")),
                "encoders features {line:?} do not include {feature:?}"
            );
        }
    }

    #[test]
    fn fingerprint_names_model_not_just_dimension() {
        let fp = EseBackend::new().vector_fingerprint();
        assert!(fp.contains(ENCODERS_MODEL), "{fp}");
        assert_ne!(fp, format!("ese:{}", encoders::DIMENSIONS));
    }

    #[test]
    fn dimension_matches_constant() {
        let backend = EseBackend::new();
        assert_eq!(backend.dimension(), crate::models::EMBEDDING_DIMENSION);
    }

    #[test]
    fn identical_texts_produce_identical_vectors() {
        let backend = EseBackend::new();
        let vecs = backend
            .embed_batch(&["hello world", "hello world"])
            .unwrap();
        let sim = Embedding::cosine_similarity(&vecs[0], &vecs[1]);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn different_texts_have_lower_similarity() {
        let backend = EseBackend::new();
        let vecs = backend
            .embed_batch(&["quantum physics", "chocolate cake recipe"])
            .unwrap();
        let sim = Embedding::cosine_similarity(&vecs[0], &vecs[1]);
        assert!(
            sim < 0.9,
            "unrelated texts should have low similarity, got {sim}"
        );
    }

    #[test]
    fn empty_batch_returns_empty() {
        let backend = EseBackend::new();
        let vecs = backend.embed_batch(&[]).unwrap();
        assert!(vecs.is_empty());
    }
}
