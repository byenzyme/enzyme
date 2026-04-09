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
use tracing::{info, warn};

mod backend;
mod daemon_client;
pub mod daemon_protocol;
pub mod daemon_server;
pub mod model_files;
mod normalize;
#[cfg(feature = "model2vec")]
mod model2vec_backend;
mod rten_backend;
mod service;

pub use backend::EmbeddingBackend;
pub use daemon_client::DaemonClient;
pub use daemon_server::DaemonServer;
pub use model_files::{ModelFiles, resolve_model_files};
pub use normalize::{l2_normalize, mean_pool};
#[cfg(feature = "model2vec")]
pub use model2vec_backend::DistilledBackend;
#[cfg(feature = "model2vec")]
pub use model_files::resolve_distilled_model_files;
pub use rten_backend::RtenBackend;
pub use service::EmbeddingService;

static EMBEDDING_SERVICE: OnceLock<EmbeddingService> = OnceLock::new();

/// Return a cached [`EmbeddingService`].
///
/// With the `model2vec` feature, loads the distilled backend directly — no
/// daemon needed since inference is a table lookup.
///
/// Without `model2vec`, tries the daemon first and falls back to in-process
/// rten (ONNX).
pub fn get_embedding_service(enzyme_home: &Path) -> Result<&'static EmbeddingService> {
    if let Some(service) = EMBEDDING_SERVICE.get() {
        return Ok(service);
    }

    let service = create_embedding_service(enzyme_home)?;

    // Benign race: if another thread set it first, our instance is dropped.
    let _ = EMBEDDING_SERVICE.set(service);
    Ok(EMBEDDING_SERVICE.get().unwrap())
}

#[cfg(feature = "model2vec")]
fn create_embedding_service(enzyme_home: &Path) -> Result<EmbeddingService> {
    let model_files = resolve_distilled_model_files(enzyme_home)?;
    let backend = DistilledBackend::load(&model_files.model_dir)?;
    info!(backend = backend.name(), dim = backend.dimension(), "using distilled embedding backend");
    Ok(EmbeddingService::new(Box::new(backend)))
}

#[cfg(not(feature = "model2vec"))]
fn create_embedding_service(enzyme_home: &Path) -> Result<EmbeddingService> {
    match try_daemon_backend(enzyme_home) {
        Ok(backend) => {
            info!("using daemon embedding backend");
            Ok(EmbeddingService::new(backend))
        }
        Err(e) => {
            warn!("daemon unavailable, falling back to in-process RtenBackend: {e}");
            let model_files = resolve_model_files(enzyme_home)?;
            let backend =
                RtenBackend::load(&model_files.model_path, &model_files.tokenizer_path)?;
            Ok(EmbeddingService::new(Box::new(backend)))
        }
    }
}

/// Try to connect to the daemon, auto-starting it if necessary.
#[cfg(not(feature = "model2vec"))]
fn try_daemon_backend(enzyme_home: &Path) -> Result<Box<dyn EmbeddingBackend>> {
    let socket_path = enzyme_home.join(daemon_protocol::SOCKET_FILENAME);
    let client = DaemonClient::new(socket_path, enzyme_home.to_path_buf());
    client.ensure_running()?;
    Ok(Box::new(client))
}
