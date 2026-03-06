//! Embedding module — generates dense vector representations of text.
//!
//! ## Architecture
//!
//! The module uses a two-layer design:
//!
//! 1. **[`EmbeddingBackend`]** trait — pluggable inference backends that convert
//!    text to vectors. The current implementation is [`RtenBackend`] (pure-Rust ONNX runtime).
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
mod rten_backend;
mod service;

pub use backend::EmbeddingBackend;
pub use daemon_client::DaemonClient;
pub use daemon_server::DaemonServer;
pub use model_files::{ModelFiles, resolve_model_files};
pub use normalize::{l2_normalize, mean_pool};
pub use rten_backend::RtenBackend;
pub use service::EmbeddingService;

static EMBEDDING_SERVICE: OnceLock<EmbeddingService> = OnceLock::new();

/// Return a cached [`EmbeddingService`], preferring the embedding daemon for
/// warm-model reuse across invocations.
///
/// On the first call this function tries to connect to (or auto-start) the
/// daemon. If that fails it falls back to loading the model in-process via
/// [`RtenBackend`].
pub fn get_embedding_service(enzyme_home: &Path) -> Result<&'static EmbeddingService> {
    if let Some(service) = EMBEDDING_SERVICE.get() {
        return Ok(service);
    }

    let service = match try_daemon_backend(enzyme_home) {
        Ok(backend) => {
            info!("using daemon embedding backend");
            EmbeddingService::new(backend)
        }
        Err(e) => {
            warn!("daemon unavailable, falling back to in-process RtenBackend: {e}");
            let model_files = resolve_model_files(enzyme_home)?;
            let backend =
                RtenBackend::load(&model_files.model_path, &model_files.tokenizer_path)?;
            EmbeddingService::new(Box::new(backend))
        }
    };

    // Benign race: if another thread set it first, our instance is dropped.
    let _ = EMBEDDING_SERVICE.set(service);
    Ok(EMBEDDING_SERVICE.get().unwrap())
}

/// Try to connect to the daemon, auto-starting it if necessary.
fn try_daemon_backend(enzyme_home: &Path) -> Result<Box<dyn EmbeddingBackend>> {
    let socket_path = enzyme_home.join(daemon_protocol::SOCKET_FILENAME);
    let client = DaemonClient::new(socket_path, enzyme_home.to_path_buf());
    client.ensure_running()?;
    Ok(Box::new(client))
}
