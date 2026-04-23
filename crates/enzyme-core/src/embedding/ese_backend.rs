use anyhow::Result;

use super::backend::EmbeddingBackend;

/// Embedding backend using ESE (Extremely Static Embeddings).
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

        let embeddings = ese::encode(texts.iter().copied());
        Ok(embeddings.into_iter().map(|arr| arr.to_vec()).collect())
    }

    fn dimension(&self) -> usize {
        ese::DIMENSIONS
    }

    fn name(&self) -> &str {
        "ese"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Embedding;

    #[test]
    fn dimension_matches_constant() {
        let backend = EseBackend::new();
        assert_eq!(backend.dimension(), crate::models::EMBEDDING_DIMENSION);
    }

    #[test]
    fn identical_texts_produce_identical_vectors() {
        let backend = EseBackend::new();
        let vecs = backend.embed_batch(&["hello world", "hello world"]).unwrap();
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
        assert!(sim < 0.9, "unrelated texts should have low similarity, got {sim}");
    }

    #[test]
    fn empty_batch_returns_empty() {
        let backend = EseBackend::new();
        let vecs = backend.embed_batch(&[]).unwrap();
        assert!(vecs.is_empty());
    }
}
