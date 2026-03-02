pub mod indexing;
pub mod embedding;
pub mod catalysts;
pub mod selection;
pub mod similarity;

use std::path::Path;

use crate::db::{Database, Repository};
use crate::document::FileDiscovery;

/// Lightweight staleness check: compare file count on disk vs document count in DB.
///
/// Returns `true` when the counts differ, meaning `enzyme init` should be re-run.
/// This intentionally avoids hashing — it catches the most common case (files
/// added or deleted since last init) at near-zero cost.
pub fn check_vault_stale(vault_path: &Path, db: &Database) -> bool {
    let disk_count = FileDiscovery::new(vault_path.to_path_buf())
        .discover()
        .map(|files| files.len())
        .unwrap_or(0);

    let repo = Repository::new(db);
    let db_count = repo.get_document_count().unwrap_or(0) as usize;

    disk_count != db_count
}
