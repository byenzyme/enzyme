use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::info;

use super::backend::EmbeddingBackend;

/// High-level embedding service that wraps an [`EmbeddingBackend`] with progress
/// reporting and timing.
///
/// The service deliberately does not pre-batch document embeddings. Backends are
/// responsible for deciding how to process the full input slice; ESE in
/// particular benefits from seeing the whole corpus so its Rayon work stealing
/// can schedule across all inputs.
///
/// # Example
///
/// ```ignore
/// let backend = EseBackend::new();
/// let service = EmbeddingService::new(Box::new(backend));
///
/// let vectors = service.embed_documents(&["hello world", "foo bar"])?;
/// let query_vec = service.embed_query("search term")?;
/// ```
pub struct EmbeddingService {
    backend: Box<dyn EmbeddingBackend>,
}

impl EmbeddingService {
    /// Create a new service wrapping the given backend.
    pub fn new(backend: Box<dyn EmbeddingBackend>) -> Self {
        Self { backend }
    }

    /// Embed multiple document chunks.
    ///
    /// Returns one vector per input chunk, in the same order.
    pub fn embed_documents(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_documents_with_progress(chunks, |_, _| {})
    }

    /// Embed multiple document chunks, calling `on_progress(completed, total)`
    /// after the backend returns. Useful for driving CLI progress bars.
    pub fn embed_documents_with_progress<F>(
        &self,
        chunks: &[&str],
        on_progress: F,
    ) -> Result<Vec<Vec<f32>>>
    where
        F: FnMut(usize, usize),
    {
        let (embeddings, _) = self.embed_documents_timed(chunks, on_progress)?;
        Ok(embeddings)
    }

    /// Like [`embed_documents_with_progress`] but also returns the time spent at
    /// the backend boundary.
    pub fn embed_documents_timed<F>(
        &self,
        chunks: &[&str],
        mut on_progress: F,
    ) -> Result<(Vec<Vec<f32>>, Duration)>
    where
        F: FnMut(usize, usize),
    {
        if chunks.is_empty() {
            return Ok((vec![], Duration::ZERO));
        }

        let total = chunks.len();

        info!(
            backend = self.backend.name(),
            total_chunks = total,
            "embedding documents"
        );

        let t = Instant::now();
        let all_embeddings = self.backend.embed_batch(chunks)?;
        let encode_time = t.elapsed();

        on_progress(all_embeddings.len(), total);

        debug_assert_eq!(all_embeddings.len(), total);
        Ok((all_embeddings, encode_time))
    }

    /// Embed a single query string.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.backend.embed(query)
    }

    /// The dimensionality of the output embeddings.
    pub fn dimension(&self) -> usize {
        self.backend.dimension()
    }

    /// The name of the underlying backend.
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock backend that returns a constant vector for each input.
    /// Tracks how many times `embed_batch` was called.
    struct MockBackend {
        dim: usize,
        call_count: Arc<AtomicUsize>,
    }

    impl MockBackend {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl EmbeddingBackend for MockBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0; self.dim]).collect())
        }

        fn dimension(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn test_embed_documents_empty() {
        let backend = MockBackend::new(4);
        let service = EmbeddingService::new(Box::new(backend));
        let result = service.embed_documents(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_embed_documents_invokes_backend_once() {
        let backend = MockBackend::new(4);
        let call_count = backend.call_count.clone();
        let service = EmbeddingService::new(Box::new(backend));

        let texts: Vec<&str> = vec!["a", "b", "c", "d", "e"];
        let result = service.embed_documents(&texts).unwrap();

        assert_eq!(result.len(), 5);
        for v in &result {
            assert_eq!(v.len(), 4);
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_progress_callback_invoked_once_after_backend_returns() {
        let backend = MockBackend::new(2);
        let service = EmbeddingService::new(Box::new(backend));

        let mut progress_calls: Vec<(usize, usize)> = vec![];
        let texts: Vec<&str> = vec!["a", "b", "c", "d", "e"];

        service
            .embed_documents_with_progress(&texts, |completed, total| {
                progress_calls.push((completed, total));
            })
            .unwrap();

        assert_eq!(progress_calls, vec![(5, 5)]);
    }

    #[test]
    fn test_embed_query() {
        let backend = MockBackend::new(4);
        let service = EmbeddingService::new(Box::new(backend));

        let result = service.embed_query("hello").unwrap();
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_dimension_and_name() {
        let backend = MockBackend::new(384);
        let service = EmbeddingService::new(Box::new(backend));

        assert_eq!(service.dimension(), 384);
        assert_eq!(service.backend_name(), "mock");
    }

    /// Verify the service works correctly when the backend returns an error.
    #[test]
    fn test_backend_error_propagated() {
        struct FailBackend;

        impl EmbeddingBackend for FailBackend {
            fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                anyhow::bail!("simulated failure")
            }

            fn dimension(&self) -> usize {
                4
            }

            fn name(&self) -> &str {
                "fail"
            }
        }

        let service = EmbeddingService::new(Box::new(FailBackend));
        let result = service.embed_documents(&["hello"]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated failure")
        );
    }
}
