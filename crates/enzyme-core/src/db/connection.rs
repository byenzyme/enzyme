use std::path::Path;

use rusqlite::Connection;
use tracing::{debug, info};

use super::errors::DbError;
use super::migrations;

/// Core database handle with separate read and write connections.
///
/// SQLite WAL mode allows concurrent reads while a single writer holds the lock.
/// We maintain two connections:
///   - `write_conn`: Exclusive writer with moderate cache (10 000 pages).
///   - `read_conn`:  Read-only connection with large cache (20 000 pages) and
///                   memory-mapped I/O for fast scans.
pub struct Database {
    write_conn: Connection,
    read_conn: Connection,
}

// -- Pragma tuning constants --------------------------------------------------

const WRITE_CACHE_SIZE: i64 = -10000; // negative = KiB (10 MB)
const READ_CACHE_SIZE: i64 = -20000; // 20 MB
const READ_MMAP_SIZE: i64 = 256 * 1024 * 1024; // 256 MB

impl Database {
    /// Open (or create) a database at `path`, apply all pending migrations, and
    /// return a ready-to-use `Database`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        info!(?path, "opening database");

        let write_conn = Self::open_write_connection(path)?;
        let read_conn = Self::open_read_connection(path)?;

        let mut db = Self {
            write_conn,
            read_conn,
        };

        // Run migrations on the write connection.
        migrations::run_all(&mut db)?;

        Ok(db)
    }

    /// Open an **in-memory** database — useful for tests and ephemeral sessions.
    ///
    /// Internally this creates a temporary file in the OS temp directory so
    /// that both write and read connections work correctly with WAL mode and
    /// foreign-key enforcement.  (SQLite shared-cache in-memory databases
    /// disable foreign keys and have table-level locking issues.)
    pub fn open_in_memory() -> Result<Self, DbError> {
        debug!("opening in-memory database (temp file backed)");

        let path = std::env::temp_dir().join(format!("enzyme-test-{}.db", uuid::Uuid::new_v4()));

        let write_conn = Self::open_write_connection(&path)?;
        let read_conn = Self::open_read_connection(&path)?;

        let mut db = Self {
            write_conn,
            read_conn,
        };

        migrations::run_all(&mut db)?;
        Ok(db)
    }

    // -- Connection factories -------------------------------------------------

    fn open_write_connection(path: &Path) -> Result<Connection, DbError> {
        let conn = Connection::open(path)?;
        Self::apply_write_pragmas(&conn)?;
        Ok(conn)
    }

    fn open_read_connection(path: &Path) -> Result<Connection, DbError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        Self::apply_read_pragmas(&conn)?;
        Ok(conn)
    }

    // -- Pragma helpers -------------------------------------------------------

    fn apply_write_pragmas(conn: &Connection) -> Result<(), DbError> {
        conn.execute_batch(
            &format!(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA cache_size = {WRITE_CACHE_SIZE};"
            ),
        )?;
        Ok(())
    }

    fn apply_read_pragmas(conn: &Connection) -> Result<(), DbError> {
        conn.execute_batch(
            &format!(
                "PRAGMA foreign_keys = ON;
                 PRAGMA cache_size = {READ_CACHE_SIZE};
                 PRAGMA mmap_size = {READ_MMAP_SIZE};"
            ),
        )?;
        Ok(())
    }

    // -- Public access --------------------------------------------------------

    /// Borrow the write connection (for INSERT / UPDATE / DELETE / DDL).
    pub fn write_conn(&self) -> &Connection {
        &self.write_conn
    }

    /// Borrow the write connection mutably (required for transactions).
    pub fn write_conn_mut(&mut self) -> &mut Connection {
        &mut self.write_conn
    }

    /// Borrow the read connection (for SELECT queries).
    pub fn read_conn(&self) -> &Connection {
        &self.read_conn
    }

    // -- Convenience wrappers -------------------------------------------------

    /// Execute a write statement and return the number of rows changed.
    pub fn execute_write(&self, sql: &str, params: &[&dyn rusqlite::types::ToSql]) -> Result<usize, DbError> {
        Ok(self.write_conn.execute(sql, params)?)
    }

    /// Run a read query, mapping each row with the provided closure.
    pub fn query_read<T, F>(&self, sql: &str, params: &[&dyn rusqlite::types::ToSql], map_row: F) -> Result<Vec<T>, DbError>
    where
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let mut stmt = self.read_conn.prepare(sql)?;
        let rows = stmt.query_map(params, map_row)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Execute a closure inside a transaction on the write connection.
    ///
    /// The transaction is committed when the closure returns `Ok` and rolled
    /// back on `Err`.
    pub fn transaction<T, F>(&mut self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, DbError>,
    {
        let tx = self.write_conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }
}
