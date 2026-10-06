pub mod catalog;
pub mod config;
pub mod db;
pub mod document;
pub mod embedding;
pub mod kernel;
pub mod llm;
pub mod models;
pub mod pipeline;
pub mod query;
pub mod search;
pub mod sqlite_source;

pub use db::initialize_sqlite_runtime;

/// Serializes unit tests that set the process-wide `ENZYME_HOME`.
#[cfg(test)]
pub(crate) static ENZYME_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
