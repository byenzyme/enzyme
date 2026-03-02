//! Embedding module — generates dense vector representations of text.
//!
//! ## Architecture
//!
//! The module uses a two-layer design:
//!
//! 1. **[`EmbeddingBackend`]** trait — pluggable inference backends that convert
//!    text to vectors. The current implementation is [`RtenBackend`] (pure-Rust
//!    ONNX via rten). Future backends may include ONNX Runtime with CoreML EP.
//!
//! 2. **[`EmbeddingService`]** — high-level orchestrator that adds batching and
//!    progress reporting on top of any backend.
//!
//! ## Quick start
//!
//! ```ignore
//! use enzyme_core::embedding::{EmbeddingService, RtenBackend};
//!
//! let backend = RtenBackend::load(&model_path, &tokenizer_path)?;
//! let service = EmbeddingService::new(Box::new(backend));
//!
//! let embeddings = service.embed_documents(&["hello world", "foo bar"])?;
//! let query_vec = service.embed_query("search term")?;
//! ```

mod backend;
pub mod model_files;
mod normalize;
mod ort_backend;
mod rten_backend;
mod service;

pub use backend::EmbeddingBackend;
pub use model_files::{ModelFiles, resolve_model_files};
pub use normalize::{l2_normalize, mean_pool};
pub use ort_backend::OrtBackend;
pub use rten_backend::RtenBackend;
pub use service::EmbeddingService;
