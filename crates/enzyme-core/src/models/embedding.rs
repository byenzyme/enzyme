use serde::{Deserialize, Serialize};

use super::EMBEDDING_DIMENSION;

/// A dense vector embedding for a document chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    pub vector: Vec<f32>,
    pub file_path: String,
    pub chunk_index: i64,
    pub model: String,
    pub generated_at: f64,
}

impl Embedding {
    /// Cosine similarity between two embedding vectors.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len());
        let mut dot = 0.0f32;
        let mut norm_a = 0.0f32;
        let mut norm_b = 0.0f32;
        for i in 0..a.len() {
            dot += a[i] * b[i];
            norm_a += a[i] * a[i];
            norm_b += b[i] * b[i];
        }
        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom == 0.0 { 0.0 } else { dot / denom }
    }

    /// Validate that the embedding has the expected dimensionality.
    pub fn validate_dimension(&self) -> bool {
        self.vector.len() == EMBEDDING_DIMENSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((Embedding::cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(Embedding::cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((Embedding::cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0];
        let b = vec![0.0, 0.0];
        assert_eq!(Embedding::cosine_similarity(&a, &b), 0.0);
    }
}
