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

use std::sync::OnceLock;

use anyhow::Result;
use tracing::info;

mod backend;
mod ese_backend;
mod normalize;
mod service;

pub use backend::EmbeddingBackend;
pub use ese_backend::EseBackend;
pub use normalize::l2_normalize;
pub use service::EmbeddingService;

static EMBEDDING_SERVICE: OnceLock<EmbeddingService> = OnceLock::new();

/// Human-readable description of the active embedding backend for status display.
pub fn model_description(_enzyme_home: &std::path::Path) -> Option<String> {
    Some(format!("ese (compiled-in, dim={})", ese::DIMENSIONS))
}

/// Return a cached [`EmbeddingService`] backed by ESE (weights compiled in).
pub fn get_embedding_service(_enzyme_home: &std::path::Path) -> Result<&'static EmbeddingService> {
    if let Some(service) = EMBEDDING_SERVICE.get() {
        return Ok(service);
    }

    let backend = EseBackend::new();
    info!(
        backend = backend.name(),
        dim = backend.dimension(),
        "using ese embedding backend"
    );
    let service = EmbeddingService::new(Box::new(backend));

    let _ = EMBEDDING_SERVICE.set(service);
    Ok(EMBEDDING_SERVICE.get().unwrap())
}
