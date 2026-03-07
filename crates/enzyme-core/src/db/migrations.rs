use rusqlite::Connection;

use super::errors::DbError;
use super::schema;

/// Apply all pending migrations directly on a connection.
///
/// Used during `Database::open` so that WAL mode and migrations are fully
/// applied before the read-only connection is opened.
pub fn run_all_on_conn(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(schema::SCHEMA_SQL)
        .map_err(DbError::Sqlite)?;
    Ok(())
}
