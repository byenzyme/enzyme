use super::connection::Database;
use super::errors::DbError;
use super::schema;

/// Apply all pending migrations to the database.
pub fn run_all(db: &mut Database) -> Result<(), DbError> {
    db.write_conn()
        .execute_batch(schema::SCHEMA_SQL)
        .map_err(DbError::Sqlite)?;
    Ok(())
}
