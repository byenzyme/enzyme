//! `enzyme compact`: rewrite the index file to return free pages to disk.
//!
//! SQLite keeps pages freed by dropped tables and deleted rows inside the file
//! for reuse. Opening an index never compacts it, because rewriting a large
//! file would stall whichever command opened it first. This command does it on
//! request, holding the same locks as a refresh.

use std::path::Path;

use anyhow::{Context, Result, bail};
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;

struct FileStats {
    page_size: i64,
    page_count: i64,
    free_pages: i64,
}

impl FileStats {
    fn read(db: &Database) -> Result<Self> {
        let pragma = |name: &str| -> Result<i64> {
            db.query_read(&format!("PRAGMA {name}"), vec![], |row| row.get::<i64>(0))?
                .into_iter()
                .next()
                .with_context(|| format!("PRAGMA {name} returned no row"))
        };
        Ok(Self {
            page_size: pragma("page_size")?,
            page_count: pragma("page_count")?,
            free_pages: pragma("freelist_count")?,
        })
    }

    fn bytes(&self) -> i64 {
        self.page_size * self.page_count
    }

    fn free_bytes(&self) -> i64 {
        self.page_size * self.free_pages
    }
}

fn mb(bytes: i64) -> f64 {
    bytes as f64 / 1_000_000.0
}

pub fn run_compact(vault_path: &Path, json: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    if !paths.vault_db.exists() {
        bail!(
            "no Enzyme index at {}; run `enzyme init` first",
            paths.vault_db.display()
        );
    }
    let enzyme_dir = paths.vault_db.parent().context("vault DB has no parent")?;
    // A detached refresh writes under refresh.lock, not workspace.lock.
    let _refresh_lock = super::lock::acquire_refresh_lock(enzyme_dir)
        .context("a background refresh is running; try `enzyme compact` again when it finishes")?;

    let db = Database::open(&paths.vault_db)?;
    let before = FileStats::read(&db)?;
    db.execute_write("VACUUM", vec![])?;
    // Fold the rewritten pages back into the main file so it actually shrinks.
    db.query_read("PRAGMA wal_checkpoint(TRUNCATE)", vec![], |_| Ok(()))?;
    let after = FileStats::read(&db)?;

    let reclaimed = (before.bytes() - after.bytes()).max(0);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "completed",
                "bytes_before": before.bytes(),
                "bytes_after": after.bytes(),
                "bytes_reclaimed": reclaimed,
                "free_bytes_before": before.free_bytes(),
            })
        );
    } else {
        println!(
            "Compacted the index: {:.1} MB → {:.1} MB ({:.1} MB reclaimed).",
            mb(before.bytes()),
            mb(after.bytes()),
            mb(reclaimed)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_reclaims_pages_freed_by_a_dropped_table() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        let paths = EnzymePaths::new(vault).unwrap();
        std::fs::create_dir_all(paths.vault_db.parent().unwrap()).unwrap();
        {
            let db = Database::open(&paths.vault_db).unwrap();
            db.execute_write("CREATE TABLE filler (blob BLOB)", vec![])
                .unwrap();
            for _ in 0..64 {
                db.execute_write("INSERT INTO filler VALUES (zeroblob(65536))", vec![])
                    .unwrap();
            }
            db.execute_write("DROP TABLE filler", vec![]).unwrap();
            assert!(FileStats::read(&db).unwrap().free_pages > 0);
        }

        run_compact(vault, true).unwrap();

        let db = Database::open(&paths.vault_db).unwrap();
        assert_eq!(FileStats::read(&db).unwrap().free_pages, 0);
    }
}
