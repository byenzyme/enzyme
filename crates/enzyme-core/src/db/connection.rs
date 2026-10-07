use std::future::Future;
use std::path::Path;
use std::sync::OnceLock;

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

static SQLITE_RUNTIME_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialize the process-wide SQLite runtime before either of the two Enzyme
/// libsql-backed Rust wrappers opens a connection.
///
/// libsql 0.9.30 and libsql-rusqlite 0.9.30 share native SQLite globals but
/// keep independent Rust initialization state. Opening a tiny libsql database
/// here deliberately triggers the libsql serialized runtime initializer first.
/// Embedders that combine both wrappers must call this synchronously during
/// process startup, before opening any SQLite connection or starting background
/// work that could open one.
pub fn initialize_sqlite_runtime() -> Result<(), DbError> {
    match SQLITE_RUNTIME_INIT.get_or_init(|| {
        #[allow(deprecated)]
        libsql::Database::open_in_memory()
            .map(drop)
            .map_err(|error| format!("libsql initializer database open failed: {error}"))
    }) {
        Ok(()) => Ok(()),
        Err(error) => Err(DbError::Initialization(error.clone())),
    }
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
        Self::open_with_busy_timeout(path, 30_000)
    }

    /// Open a database with a caller-specific SQLite busy timeout.
    ///
    /// Best-effort background bookkeeping uses a short timeout so it cannot
    /// inherit the maintenance-oriented 30-second wait from [`Self::open`].
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout_ms: u64,
    ) -> Result<Self, DbError> {
        initialize_sqlite_runtime()?;
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
            apply_pragmas(&conn, true, busy_timeout_ms).await?;
            migrations::run_all_on_conn(&conn).await?;
            Ok(Self { _db: db, conn })
        })
    }

    /// Open an existing database strictly for reading: SQLite read-only flags,
    /// `query_only`, and no pragmas or migrations that could change the file.
    /// A schema older than this binary surfaces as a query error rather than
    /// being upgraded in place.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_read_only_with_busy_timeout(path, 30_000)
    }

    /// [`Self::open_read_only`] with a caller-chosen busy timeout, for quick
    /// read-only views that should report a busy index rather than wait.
    pub fn open_read_only_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout_ms: u64,
    ) -> Result<Self, DbError> {
        initialize_sqlite_runtime()?;
        let path = path.as_ref().to_owned();
        if !path.is_file() {
            return Err(DbError::Migration(format!(
                "database not found: {}",
                path.display()
            )));
        }
        info!(?path, "opening database read-only");
        block_in_place_sync(async move {
            let path_str = path
                .to_str()
                .ok_or_else(|| DbError::Migration("non-UTF8 database path".into()))?;
            let db = Builder::new_local(path_str)
                .flags(libsql::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .build()
                .await
                .map_err(DbError::Libsql)?;
            let conn = db.connect().map_err(DbError::Libsql)?;
            for sql in [
                format!("PRAGMA busy_timeout = {busy_timeout_ms}"),
                "PRAGMA query_only = ON".to_string(),
            ] {
                let mut rows = conn.query(&sql, ()).await.map_err(DbError::Libsql)?;
                while rows.next().await.map_err(DbError::Libsql)?.is_some() {}
            }
            Ok(Self { _db: db, conn })
        })
    }

    /// Confirm every table of the current schema, and every column migrations
    /// add, exists without migrating.
    ///
    /// Pair with [`Self::open_read_only`] when a reader must refuse an index
    /// built by an older binary with a clear error instead of a query failure.
    pub fn validate_expected_schema(&self) -> Result<(), DbError> {
        let tables: std::collections::HashSet<String> = self
            .query_read(
                "SELECT name FROM sqlite_master WHERE type = 'table'",
                vec![],
                |row| row.get::<String>(0),
            )?
            .into_iter()
            .collect();
        if let Some(missing) = super::schema::EXPECTED_TABLES
            .iter()
            .find(|table| !tables.contains(**table))
        {
            return Err(DbError::Migration(format!(
                "search index schema is not compatible: missing table {missing}"
            )));
        }
        for (table, column) in super::schema::MIGRATED_COLUMNS {
            let present = !self
                .query_read(
                    "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
                    vec![
                        libsql::Value::Text((*table).to_string()),
                        libsql::Value::Text((*column).to_string()),
                    ],
                    |row| row.get::<i64>(0),
                )?
                .is_empty();
            if !present {
                return Err(DbError::Migration(format!(
                    "search index schema is not compatible: missing column {table}.{column}"
                )));
            }
        }
        Ok(())
    }

    /// Open an **in-memory** database — useful for tests and ephemeral sessions.
    pub fn open_in_memory() -> Result<Self, DbError> {
        initialize_sqlite_runtime()?;
        debug!("opening in-memory database");
        block_in_place_sync(async {
            let db = Builder::new_local(":memory:")
                .build()
                .await
                .map_err(DbError::Libsql)?;
            let conn = db.connect().map_err(DbError::Libsql)?;
            apply_pragmas(&conn, false, 30_000).await?;
            migrations::run_all_on_conn(&conn).await?;
            Ok(Self { _db: db, conn })
        })
    }

    /// Open a **remote** Turso database — for hosted search endpoints.
    ///
    /// Connects to a Turso database via HTTP. No local file is created.
    /// Pragmas and migrations are skipped (the DB was uploaded pre-migrated).
    pub fn open_remote(url: &str, auth_token: &str) -> Result<Self, DbError> {
        initialize_sqlite_runtime()?;
        info!(url, "opening remote database");
        block_in_place_sync(async move {
            let db = Builder::new_remote(url.to_string(), auth_token.to_string())
                .build()
                .await
                .map_err(DbError::Libsql)?;
            let conn = db.connect().map_err(DbError::Libsql)?;
            // No pragmas or migrations for remote — the uploaded DB is already set up
            Ok(Self { _db: db, conn })
        })
    }

    // -- Convenience wrappers -------------------------------------------------

    /// Execute a write statement and return the number of rows changed.
    pub fn execute_write(&self, sql: &str, params: Vec<libsql::Value>) -> Result<usize, DbError> {
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

    /// Create a transactionally consistent, standalone SQLite snapshot.
    /// Catalyst epoch builders use this frozen copy so active reads and corpus
    /// writes can continue against the source database.
    pub fn snapshot_to(&self, path: impl AsRef<Path>) -> Result<(), DbError> {
        let path = path.as_ref();
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| DbError::Migration(format!("remove old snapshot: {e}")))?;
        }
        let quoted = path
            .to_str()
            .ok_or_else(|| DbError::Migration("non-UTF8 snapshot path".into()))?
            .replace('\'', "''");
        let sql = format!("VACUUM INTO '{quoted}'");
        block_in_place_sync(async {
            self.conn
                .execute_batch(&sql)
                .await
                .map(|_| ())
                .map_err(DbError::Libsql)
        })
    }
}

// -- Pragma helpers -----------------------------------------------------------

async fn apply_pragmas(conn: &Connection, wal: bool, busy_timeout_ms: u64) -> Result<(), DbError> {
    // Pragmas that return a result row must use query() + drain.
    // execute_batch() fails on pragmas that produce output rows.
    let mut pragmas: Vec<String> = vec![format!("PRAGMA busy_timeout = {busy_timeout_ms}")];
    if wal {
        pragmas.push("PRAGMA journal_mode = WAL".into());
    }
    pragmas.push("PRAGMA foreign_keys = ON".into());
    pragmas.push("PRAGMA synchronous = NORMAL".into());
    // The normal 30-second caller budget lets one-time maintenance stall
    // behind concurrent work; best-effort callers can explicitly choose less.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_schema_validation_rejects_foreign_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enzyme.db");
        drop(Database::open(&path).unwrap());
        Database::open_read_only(&path)
            .unwrap()
            .validate_expected_schema()
            .unwrap();

        let foreign = dir.path().join("foreign.db");
        {
            let conn = rusqlite::Connection::open(&foreign).unwrap();
            conn.execute_batch("CREATE TABLE docs (id INTEGER)")
                .unwrap();
        }
        let error = Database::open_read_only(&foreign)
            .unwrap()
            .validate_expected_schema()
            .unwrap_err();
        assert!(error.to_string().contains("missing table"), "{error}");
    }

    #[test]
    fn read_only_open_reads_but_never_writes_or_creates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enzyme.db");
        assert!(Database::open_read_only(&path).is_err());
        assert!(!path.exists(), "read-only open must not create a database");
        {
            let db = Database::open(&path).unwrap();
            db.execute_write(
                "INSERT INTO entities (name, type, frequency, trends) VALUES ('a', 'tag', 1, '{}')",
                vec![],
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let names = db
            .query_read("SELECT name FROM entities", vec![], |row| {
                row.get::<String>(0)
            })
            .unwrap();
        assert_eq!(names, ["a"]);
        assert!(
            db.execute_write("DELETE FROM entities", vec![]).is_err(),
            "writes must fail"
        );
        drop(db);
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
