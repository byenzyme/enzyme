//! Binary-update sentinel for vault databases.
//!
//! Agent-facing entry points (MCP tools, `enzyme refresh --quiet`, `enzyme
//! status`) need a cheap way to answer: "was this vault last written by the
//! running binary, and if not, is a full regen required or was migration
//! enough?" Enzyme's policy is regen-over-migration: when the live schema has
//! structurally drifted from what this binary's schema + migrations produce,
//! the caller reruns `enzyme init` from scratch rather than authoring a
//! bespoke migration. Auth (`~/.enzyme/auth.json`) and global config live
//! outside the vault DB, so a regen never touches them.
//!
//! Fast path: a single `config`-table read comparing the stored binary
//! version against the caller's. Only on a version change does the check pay
//! for fingerprinting — a structural hash of the live DB compared against a
//! fresh in-memory DB run through the same schema + migrations.

use libsql::{Connection, Value};
use sha2::{Digest, Sha256};

use anyhow::Result;

use super::connection::{Database, block_in_place_sync};

/// Config key holding the version of the binary that last validated this DB.
pub const BINARY_VERSION_KEY: &str = "sentinel.binary_version";
/// Config key holding the structural schema fingerprint at last validation.
pub const SCHEMA_FINGERPRINT_KEY: &str = "sentinel.schema_fingerprint";

/// Outcome of an update-sentinel check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Stored binary version matches the running binary; nothing to do.
    UpToDate,
    /// The binary changed but migrations fully reconciled the schema; the
    /// sentinel was (or can be) advanced for free — no regen.
    SentinelRefreshed { previous_version: Option<String> },
    /// The live schema diverges from what this binary produces; a full
    /// `enzyme init` regen is required.
    RegenRequired { reason: String },
}

/// Check the sentinel, advancing it when the schema is compatible.
///
/// Never writes when regen is required, so a failed/skipped regen keeps
/// re-triggering on subsequent agent calls.
pub fn check_and_mark(db: &Database, binary_version: &str) -> Result<UpdateStatus> {
    let status = peek(db, binary_version)?;
    if let UpdateStatus::SentinelRefreshed { .. } = &status {
        let live = live_schema_fingerprint(db)?;
        db.set_config(SCHEMA_FINGERPRINT_KEY, &live)?;
        db.set_config(BINARY_VERSION_KEY, binary_version)?;
    }
    Ok(status)
}

/// Read-only variant of [`check_and_mark`] for `status`-style reporting.
pub fn peek(db: &Database, binary_version: &str) -> Result<UpdateStatus> {
    let stored = db.get_config(BINARY_VERSION_KEY)?;
    if stored.as_deref() == Some(binary_version) {
        return Ok(UpdateStatus::UpToDate);
    }
    let live = live_schema_fingerprint(db)?;
    let expected = expected_schema_fingerprint()?;
    if live == expected {
        Ok(UpdateStatus::SentinelRefreshed {
            previous_version: stored,
        })
    } else {
        Ok(UpdateStatus::RegenRequired {
            reason: format!(
                "vault schema (written by binary {}) no longer matches what binary {} produces",
                stored.as_deref().unwrap_or("unknown"),
                binary_version
            ),
        })
    }
}

/// Structural fingerprint of the live database: every user table's column
/// set (name, type, notnull, default, pk), order-insensitive so a column
/// added via `ALTER TABLE` hashes identically to one declared in `CREATE
/// TABLE`. Indexes are excluded — `CREATE INDEX IF NOT EXISTS` self-heals on
/// every open, so a missing index never warrants a regen.
pub fn live_schema_fingerprint(db: &Database) -> Result<String> {
    block_in_place_sync(fingerprint_conn(&db.conn))
}

/// Fingerprint of a pristine database produced by this binary's schema +
/// migrations, via a throwaway in-memory DB.
pub fn expected_schema_fingerprint() -> Result<String> {
    let fresh = Database::open_in_memory()?;
    live_schema_fingerprint(&fresh)
}

async fn fingerprint_conn(conn: &Connection) -> Result<String> {
    let mut tables: Vec<String> = Vec::new();
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        tables.push(row.get::<String>(0)?);
    }
    drop(rows);

    let mut hasher = Sha256::new();
    for table in &tables {
        let quoted = table.replace('"', "\"\"");
        let mut rows = conn
            .query(&format!("PRAGMA table_info(\"{quoted}\")"), ())
            .await?;
        let mut columns: Vec<String> = Vec::new();
        while let Some(row) = rows.next().await? {
            let name: String = row.get(1)?;
            let col_type: String = row.get(2)?;
            let notnull: i64 = row.get(3)?;
            let default = match row.get_value(4)? {
                Value::Null => String::new(),
                Value::Text(s) => s,
                Value::Integer(i) => i.to_string(),
                Value::Real(r) => r.to_string(),
                Value::Blob(b) => format!("blob:{}", b.len()),
            };
            let pk: i64 = row.get(5)?;
            columns.push(format!("{name}|{col_type}|{notnull}|{default}|{pk}"));
        }
        columns.sort();
        hasher.update(table.as_bytes());
        hasher.update(b"(");
        for column in &columns {
            hasher.update(column.as_bytes());
            hasher.update(b";");
        }
        hasher.update(b")");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_fingerprint_matches_expected() {
        let db = Database::open_in_memory().expect("open");
        let live = live_schema_fingerprint(&db).expect("live fp");
        let expected = expected_schema_fingerprint().expect("expected fp");
        assert_eq!(live, expected);
    }

    #[test]
    fn sentinel_lifecycle() {
        let db = Database::open_in_memory().expect("open");
        // Fresh DB, never marked: version mismatch but schema compatible.
        match check_and_mark(&db, "1.2.3").expect("check") {
            UpdateStatus::SentinelRefreshed { previous_version } => {
                assert_eq!(previous_version, None)
            }
            other => panic!("expected refresh, got {other:?}"),
        }
        // Second check with same version: fast path.
        assert_eq!(
            check_and_mark(&db, "1.2.3").expect("check"),
            UpdateStatus::UpToDate
        );
        // Version bump with unchanged schema: sentinel advances for free.
        match check_and_mark(&db, "1.2.4").expect("check") {
            UpdateStatus::SentinelRefreshed { previous_version } => {
                assert_eq!(previous_version.as_deref(), Some("1.2.3"))
            }
            other => panic!("expected refresh, got {other:?}"),
        }
    }

    #[test]
    fn drifted_schema_requires_regen_and_keeps_sentinel_stale() {
        let db = Database::open_in_memory().expect("open");
        check_and_mark(&db, "1.2.3").expect("check");
        db.execute_write("ALTER TABLE docs ADD COLUMN legacy_probe TEXT", vec![])
            .expect("alter");
        match check_and_mark(&db, "1.2.4").expect("check") {
            UpdateStatus::RegenRequired { .. } => {}
            other => panic!("expected regen, got {other:?}"),
        }
        // Sentinel must not have advanced.
        assert_eq!(
            db.get_config(BINARY_VERSION_KEY).expect("get").as_deref(),
            Some("1.2.3")
        );
    }
}
