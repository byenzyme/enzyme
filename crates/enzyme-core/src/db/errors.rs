/// Database error type.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Libsql(#[from] libsql::Error),

    #[error("migration error: {0}")]
    Migration(String),
}
