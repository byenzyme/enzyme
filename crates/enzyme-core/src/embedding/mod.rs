//! Embedding module — generates dense vector representations of text.
//!
//! ## Architecture
//!
//! The module uses a two-layer design:
//!
//! 1. **[`EmbeddingBackend`]** trait — pluggable inference backends that convert
//!    text to vectors.
//!
//! 2. **[`EmbeddingService`]** — high-level orchestrator that adds batching and
//!    progress reporting on top of any backend.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use tracing::info;

mod backend;
mod model2vec_backend;
pub mod model_files;
mod normalize;
mod service;

pub use backend::EmbeddingBackend;
pub use model_files::{DistilledModelFiles, resolve_distilled_model_files};
pub use model2vec_backend::DistilledBackend;
pub use normalize::l2_normalize;
pub use service::EmbeddingService;

static EMBEDDING_SERVICE: OnceLock<EmbeddingService> = OnceLock::new();

/// Return a cached [`EmbeddingService`] using the distilled model2vec backend.
pub fn get_embedding_service(enzyme_home: &Path) -> Result<&'static EmbeddingService> {
    if let Some(service) = EMBEDDING_SERVICE.get() {
        return Ok(service);
    }

    let model_files = resolve_distilled_model_files(enzyme_home)?;
    let backend = DistilledBackend::load(&model_files.model_dir)?;
    info!(
        backend = backend.name(),
        dim = backend.dimension(),
        "using distilled embedding backend"
    );
    let service = EmbeddingService::new(Box::new(backend));

    // Benign race: if another thread set it first, our instance is dropped.
    let _ = EMBEDDING_SERVICE.set(service);
    Ok(EMBEDDING_SERVICE.get().unwrap())
}
