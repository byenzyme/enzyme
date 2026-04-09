mod applied_target;
mod document;
mod entity;
mod catalyst;
mod embedding;

pub use applied_target::*;
pub use document::*;
pub use entity::*;
pub use catalyst::*;
pub use embedding::*;

/// Enzyme stores all timestamps as milliseconds since Unix epoch.
pub type Timestamp = i64;

/// Embedding vector dimensionality. Depends on the active backend.
#[cfg(feature = "model2vec")]
pub const EMBEDDING_DIMENSION: usize = 256;
#[cfg(not(feature = "model2vec"))]
pub const EMBEDDING_DIMENSION: usize = 384;
