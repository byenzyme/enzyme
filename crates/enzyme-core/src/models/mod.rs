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

/// Embedding vectors are 384-dimensional f32 (mdbr-leaf-ir / MiniLM architecture).
pub const EMBEDDING_DIMENSION: usize = 384;
