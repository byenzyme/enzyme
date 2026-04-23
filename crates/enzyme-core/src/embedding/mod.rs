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
#[cfg(feature = "ese-backend")]
mod ese_backend;
#[cfg(feature = "model2vec-backend")]
mod model2vec_backend;
#[cfg(feature = "model2vec-backend")]
pub mod model_files;
mod normalize;
mod service;

pub use backend::EmbeddingBackend;
#[cfg(feature = "ese-backend")]
pub use ese_backend::EseBackend;
#[cfg(feature = "model2vec-backend")]
pub use model2vec_backend::DistilledBackend;
#[cfg(feature = "model2vec-backend")]
pub use model_files::{DistilledModelFiles, resolve_distilled_model_files};
pub use normalize::l2_normalize;
pub use service::EmbeddingService;

static EMBEDDING_SERVICE: OnceLock<EmbeddingService> = OnceLock::new();

/// Human-readable description of the active embedding backend for status display.
pub fn model_description(_enzyme_home: &std::path::Path) -> Option<String> {
    #[cfg(feature = "ese-backend")]
    {
        Some(format!("ese (compiled-in, dim={})", ese::DIMENSIONS))
    }
    #[cfg(all(feature = "model2vec-backend", not(feature = "ese-backend")))]
    {
        match resolve_distilled_model_files(_enzyme_home) {
            Ok(mf) => Some(mf.model_dir.display().to_string()),
            Err(_) => None,
        }
    }
    #[cfg(not(any(feature = "ese-backend", feature = "model2vec-backend")))]
    {
        None
    }
}

/// Return a cached [`EmbeddingService`] backed by ESE (weights compiled in).
#[cfg(feature = "ese-backend")]
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

/// Return a cached [`EmbeddingService`] using the distilled model2vec backend.
#[cfg(all(feature = "model2vec-backend", not(feature = "ese-backend")))]
pub fn get_embedding_service(enzyme_home: &std::path::Path) -> Result<&'static EmbeddingService> {
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

    let _ = EMBEDDING_SERVICE.set(service);
    Ok(EMBEDDING_SERVICE.get().unwrap())
}
