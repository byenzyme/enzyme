use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::info;

use super::backend::EmbeddingBackend;

/// Default number of texts to embed per model invocation.
///
/// MiniLM (384-dim, seq_len=128) at batch=128 uses ~25 MB for the output
/// tensor — well within reason on any modern machine. Larger batches
/// amortise per-batch overhead (ONNX Runtime thread-pool scheduling,
/// daemon socket round-trips).
const DEFAULT_BATCH_SIZE: usize = 128;

/// High-level embedding service that wraps an [`EmbeddingBackend`] with batching
/// and optional progress reporting.
///
/// # Example
///
/// ```ignore
/// let backend = RtenBackend::load(&model_path, &tokenizer_path)?;
/// let service = EmbeddingService::new(Box::new(backend));
///
/// let vectors = service.embed_documents(&["hello world", "foo bar"])?;
/// let query_vec = service.embed_query("search term")?;
/// ```
pub struct EmbeddingService {
    backend: Box<dyn EmbeddingBackend>,
    batch_size: usize,
}

impl EmbeddingService {
    /// Create a new service wrapping the given backend with the default batch size.
    pub fn new(backend: Box<dyn EmbeddingBackend>) -> Self {
        Self {
            backend,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Create a new service with a custom batch size.
    pub fn with_batch_size(backend: Box<dyn EmbeddingBackend>, batch_size: usize) -> Self {
        assert!(batch_size > 0, "batch_size must be > 0");
        Self {
            backend,
            batch_size,
        }
    }

    /// Embed multiple document chunks in batches.
    ///
    /// Returns one vector per input chunk, in the same order.
    pub fn embed_documents(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_documents_with_progress(chunks, |_, _| {})
    }

    /// Embed multiple document chunks, calling `on_progress(completed, total)` after
    /// each batch completes. Useful for driving CLI progress bars.
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

    /// Like [`embed_documents_with_progress`] but also returns the time spent
    /// purely on backend inference (excludes batching overhead and progress callbacks).
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
        let mut all_embeddings = Vec::with_capacity(total);
        let mut encode_time = Duration::ZERO;

        info!(
            backend = self.backend.name(),
            total_chunks = total,
            batch_size = self.batch_size,
            "embedding documents"
        );

        for batch in chunks.chunks(self.batch_size) {
            let t = Instant::now();
            let batch_embeddings = self.backend.embed_batch(batch)?;
            encode_time += t.elapsed();

            all_embeddings.extend(batch_embeddings);

            on_progress(all_embeddings.len(), total);
        }

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
    fn test_embed_documents_single_batch() {
        let backend = MockBackend::new(4);
        let service = EmbeddingService::with_batch_size(Box::new(backend), 10);

        let texts: Vec<&str> = vec!["a", "b", "c"];
        let result = service.embed_documents(&texts).unwrap();

        assert_eq!(result.len(), 3);
        for v in &result {
            assert_eq!(v.len(), 4);
        }
    }

    #[test]
    fn test_embed_documents_multiple_batches() {
        let backend = MockBackend::new(3);
        let call_count = backend.call_count.clone();
        let service = EmbeddingService::with_batch_size(Box::new(backend), 2);

        let texts: Vec<&str> = vec!["a", "b", "c", "d", "e"];
        let result = service.embed_documents(&texts).unwrap();

        assert_eq!(result.len(), 5);
        // 5 items / batch_size 2 = 3 batches (2 + 2 + 1)
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_embed_documents_exact_batch_boundary() {
        let backend = MockBackend::new(2);
        let call_count = backend.call_count.clone();
        let service = EmbeddingService::with_batch_size(Box::new(backend), 3);

        let texts: Vec<&str> = vec!["a", "b", "c", "d", "e", "f"];
        let result = service.embed_documents(&texts).unwrap();

        assert_eq!(result.len(), 6);
        // 6 items / batch_size 3 = exactly 2 batches
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_progress_callback_invoked() {
        let backend = MockBackend::new(2);
        let service = EmbeddingService::with_batch_size(Box::new(backend), 2);

        let mut progress_calls: Vec<(usize, usize)> = vec![];
        let texts: Vec<&str> = vec!["a", "b", "c", "d", "e"];

        service
            .embed_documents_with_progress(&texts, |completed, total| {
                progress_calls.push((completed, total));
            })
            .unwrap();

        // 3 batches: (2,5), (4,5), (5,5)
        assert_eq!(progress_calls.len(), 3);
        assert_eq!(progress_calls[0], (2, 5));
        assert_eq!(progress_calls[1], (4, 5));
        assert_eq!(progress_calls[2], (5, 5));
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

    #[test]
    #[should_panic(expected = "batch_size must be > 0")]
    fn test_zero_batch_size_panics() {
        let backend = MockBackend::new(4);
        EmbeddingService::with_batch_size(Box::new(backend), 0);
    }

    #[test]
    fn test_batch_size_one() {
        let backend = MockBackend::new(2);
        let call_count = backend.call_count.clone();
        let service = EmbeddingService::with_batch_size(Box::new(backend), 1);

        let texts: Vec<&str> = vec!["a", "b", "c"];
        let result = service.embed_documents(&texts).unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
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
