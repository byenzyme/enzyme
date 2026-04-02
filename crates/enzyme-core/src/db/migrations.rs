use libsql::Connection;

use super::errors::DbError;
use super::schema;

/// Apply all pending migrations on a connection.
///
/// Called during `Database::open` after pragmas are set.
pub async fn run_all_on_conn(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(schema::SCHEMA_SQL)
        .await
        .map_err(DbError::Libsql)?;
    Ok(())
}
