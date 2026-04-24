mod applied_target;
mod catalyst;
mod document;
mod embedding;
mod entity;

pub use applied_target::*;
pub use catalyst::*;
pub use document::*;
pub use embedding::*;
pub use entity::*;

/// Enzyme stores all timestamps as milliseconds since Unix epoch.
pub type Timestamp = i64;

/// Embedding vector dimensionality.
pub const EMBEDDING_DIMENSION: usize = ese::DIMENSIONS;
