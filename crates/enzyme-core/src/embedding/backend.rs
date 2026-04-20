use anyhow::Result;

/// Trait for embedding backends.
///
/// Implementations generate dense vectors from text. The trait is object-safe
/// so backends can be stored as `Box<dyn EmbeddingBackend>`.
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of text strings, returning one vector per input.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single text string.
    ///
    /// The default implementation delegates to [`embed_batch`](Self::embed_batch).
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty embedding result"))
    }

    /// The dimensionality of the output embeddings.
    fn dimension(&self) -> usize;

    /// Human-readable name for this backend.
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the trait is object-safe by constructing a trait object.
    #[test]
    fn trait_is_object_safe() {
        struct Dummy;

        impl EmbeddingBackend for Dummy {
            fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
            }

            fn dimension(&self) -> usize {
                4
            }

            fn name(&self) -> &str {
                "dummy"
            }
        }

        // This line proves the trait is object-safe.
        let backend: Box<dyn EmbeddingBackend> = Box::new(Dummy);
        assert_eq!(backend.dimension(), 4);
        assert_eq!(backend.name(), "dummy");
    }

    #[test]
    fn default_embed_delegates_to_embed_batch() {
        struct Echo;

        impl EmbeddingBackend for Echo {
            fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|t| vec![t.len() as f32; 3]).collect())
            }

            fn dimension(&self) -> usize {
                3
            }

            fn name(&self) -> &str {
                "echo"
            }
        }

        let backend = Echo;
        let v = backend.embed("hello").unwrap();
        assert_eq!(v, vec![5.0, 5.0, 5.0]);
    }

    #[test]
    fn embed_returns_error_on_empty_batch_result() {
        struct Empty;

        impl EmbeddingBackend for Empty {
            fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                Ok(vec![]) // deliberately empty
            }

            fn dimension(&self) -> usize {
                3
            }

            fn name(&self) -> &str {
                "empty"
            }
        }

        let backend = Empty;
        let result = backend.embed("anything");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("empty embedding result")
        );
    }
}
