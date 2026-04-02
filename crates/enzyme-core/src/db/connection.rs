use std::future::Future;
use std::path::Path;

use libsql::{Builder, Connection, Database as LibsqlDb};
use tracing::{debug, info};

use super::errors::DbError;
use super::migrations;

/// Core database handle backed by a libsql local connection.
///
/// All public methods are sync-shaped: they block the calling thread via
/// [`block_in_place_sync`] and drive the underlying async libsql calls to
/// completion.  CLI callers run inside a tokio multi-thread runtime and use
/// `block_in_place` to yield the worker back while blocking; test callers get
/// a fresh current-thread runtime per call.
pub struct Database {
    _db: LibsqlDb,
    pub(crate) conn: Connection,
}

/// Drive a future to completion from any thread context.
///
/// - Inside a tokio multi-thread runtime: uses `block_in_place` so the worker
///   thread is yielded back to the scheduler and no deadlock occurs.
/// - Outside any runtime (test context): creates a one-shot current-thread
///   runtime, runs the future, then drops it.
pub(crate) fn block_in_place_sync<F: Future>(f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("tokio runtime")
            .block_on(f),
    }
}

impl Database {
    /// Open (or create) a database at `path`, apply all pending migrations, and
    /// return a ready-to-use `Database`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref().to_owned();
        info!(?path, "opening database");
        block_in_place_sync(async move {
            let path_str = path
                .to_str()
                .ok_or_else(|| DbError::Migration("non-UTF8 database path".into()))?;
            let db = Builder::new_local(path_str)
                .build()
                .await
                .map_err(DbError::Libsql)?;
            let conn = db.connect().map_err(DbError::Libsql)?;
            apply_pragmas(&conn, true).await?;
            migrations::run_all_on_conn(&conn).await?;
            Ok(Self { _db: db, conn })
        })
    }

    /// Open an **in-memory** database — useful for tests and ephemeral sessions.
    pub fn open_in_memory() -> Result<Self, DbError> {
        debug!("opening in-memory database");
        block_in_place_sync(async {
            let db = Builder::new_local(":memory:")
                .build()
                .await
                .map_err(DbError::Libsql)?;
            let conn = db.connect().map_err(DbError::Libsql)?;
            apply_pragmas(&conn, false).await?;
            migrations::run_all_on_conn(&conn).await?;
            Ok(Self { _db: db, conn })
        })
    }

    // -- Convenience wrappers -------------------------------------------------

    /// Execute a write statement and return the number of rows changed.
    pub fn execute_write(
        &self,
        sql: &str,
        params: Vec<libsql::Value>,
    ) -> Result<usize, DbError> {
        let sql = sql.to_owned();
        block_in_place_sync(async move {
            self.conn
                .execute(&sql, params)
                .await
                .map(|n| n as usize)
                .map_err(DbError::Libsql)
        })
    }

    /// Run a read query, mapping each row with the provided closure.
    pub fn query_read<T, F>(
        &self,
        sql: &str,
        params: Vec<libsql::Value>,
        mut map_row: F,
    ) -> Result<Vec<T>, DbError>
    where
        F: FnMut(libsql::Row) -> Result<T, libsql::Error>,
    {
        let sql = sql.to_owned();
        block_in_place_sync(async move {
            let mut rows = self
                .conn
                .query(&sql, params)
                .await
                .map_err(DbError::Libsql)?;
            let mut results = Vec::new();
            while let Some(row) = rows.next().await.map_err(DbError::Libsql)? {
                results.push(map_row(row).map_err(DbError::Libsql)?);
            }
            Ok(results)
        })
    }
}

// -- Pragma helpers -----------------------------------------------------------

async fn apply_pragmas(conn: &Connection, wal: bool) -> Result<(), DbError> {
    // Pragmas that return a result row must use query() + drain.
    // execute_batch() fails on pragmas that produce output rows.
    let mut pragmas: Vec<String> = Vec::new();
    if wal {
        pragmas.push("PRAGMA journal_mode = WAL".into());
    }
    pragmas.push("PRAGMA foreign_keys = ON".into());
    pragmas.push("PRAGMA synchronous = NORMAL".into());
    pragmas.push("PRAGMA busy_timeout = 5000".into());
    pragmas.push("PRAGMA cache_size = -20000".into());
    pragmas.push(format!("PRAGMA mmap_size = {}", 256 * 1024 * 1024));

    for sql in &pragmas {
        let mut rows = conn
            .query(sql.as_str(), ())
            .await
            .map_err(DbError::Libsql)?;
        while rows.next().await.map_err(DbError::Libsql)?.is_some() {}
    }
    Ok(())
}
