use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use libsql::Value;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{
    SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit,
};
use crate::db::{Database, initialize_sqlite_runtime};
use crate::document::{
    ProcessedDocument, StructuredDocumentInput, StructuredLogDocumentInput, StructuredLogEntry,
    structured_document_to_processed_document, structured_log_document_to_processed_document,
};
use crate::models::{EntityOccurrence, EntityType};
use crate::pipeline::indexing::IndexingPipeline;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSourceValidation {
    pub row_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SqliteSourceIndexingStats {
    pub sources_queried: usize,
    pub rows_seen: usize,
    pub buckets_seen: usize,
    pub documents_rebuilt: usize,
    pub documents_skipped: usize,
    pub documents_pruned: usize,
    /// Source rows dropped because every mapped `what` column was NULL/empty.
    pub rows_skipped_empty: usize,
}

impl SqliteSourceIndexingStats {
    pub fn changed(&self) -> bool {
        self.documents_rebuilt > 0 || self.documents_pruned > 0
    }

    fn add_assign(&mut self, other: &Self) {
        self.sources_queried += other.sources_queried;
        self.rows_seen += other.rows_seen;
        self.buckets_seen += other.buckets_seen;
        self.documents_rebuilt += other.documents_rebuilt;
        self.documents_skipped += other.documents_skipped;
        self.documents_pruned += other.documents_pruned;
        self.rows_skipped_empty += other.rows_skipped_empty;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum CursorValue {
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl CursorValue {
    fn from_ref(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Null => bail!("id role columns may not be NULL"),
            ValueRef::Integer(value) => Ok(Self::Integer(value)),
            ValueRef::Real(value) if value.is_finite() => Ok(Self::Real(value)),
            ValueRef::Real(_) => bail!("id role columns may not contain non-finite REAL values"),
            ValueRef::Text(value) => Ok(Self::Text(
                std::str::from_utf8(value)
                    .context("id role column contains non-UTF-8 text")?
                    .to_string(),
            )),
            ValueRef::Blob(value) => Ok(Self::Blob(value.to_vec())),
        }
    }
}

#[derive(Debug, Clone)]
struct SourceRow {
    id: Vec<CursorValue>,
    who: Vec<String>,
    what: String,
    where_labels: Vec<String>,
    where_key: Vec<Option<String>>,
    created_at: i64,
}

#[derive(Debug)]
struct PendingDocument {
    document: ProcessedDocument,
    occurrences: Vec<(String, EntityType, EntityOccurrence)>,
}

/// Validate a configured SQLite source without reading or modifying Enzyme's
/// own database. The source is opened read-only, its query is prepared, all
/// configured role columns are resolved, and the query's total rows counted.
pub fn validate_sqlite_source(
    source_name: &str,
    source: &SqliteSourceConfig,
) -> Result<SqliteSourceValidation> {
    validate_contract(source_name, source)?;
    ensure_source_path_exists(&source.db)?;
    let source_db = open_source_read_only(&source.db)?;
    let query = source.query.trim().trim_end_matches(';').trim_end();

    let statement = source_db
        .prepare(query)
        .with_context(|| format!("failed to prepare source query: {query}"))?;
    if !statement.readonly() {
        bail!("source query must be read-only");
    }
    ResolvedColumns::new(&statement, &source.roles)?;
    drop(statement);

    let count_sql = format!("SELECT COUNT(*) FROM ({query}) AS enzyme_source");
    let row_count = source_db
        .query_row(&count_sql, [], |row| row.get::<_, i64>(0))
        .with_context(|| "failed to count source query rows")?;
    let row_count =
        u64::try_from(row_count).context("source query returned a negative row count")?;

    Ok(SqliteSourceValidation { row_count })
}

/// Check whether a configured source database can be reached read-only.
/// Query validity is intentionally separate so status remains lightweight.
pub fn check_sqlite_source_reachability(path: &Path) -> Result<()> {
    ensure_source_path_exists(path)?;
    open_source_read_only(path).map(drop)
}

fn ensure_source_path_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("source database does not exist: {}", path.display());
    }
    if !path.is_file() {
        bail!("source database is not a file: {}", path.display());
    }
    Ok(())
}

fn open_source_read_only(path: &Path) -> Result<Connection> {
    initialize_sqlite_runtime()?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let source_db = Connection::open_with_flags(path, flags)
        .with_context(|| format!("failed to open {} read-only", path.display()))?;
    source_db
        .pragma_update(None, "query_only", true)
        .context("failed to enforce SQLite query_only mode")?;
    Ok(source_db)
}

/// Execute every configured SQLite source in deterministic source-name order.
pub fn run_configured_sqlite_sources(
    db: &Database,
    sources: &HashMap<String, SqliteSourceConfig>,
) -> Result<SqliteSourceIndexingStats> {
    let mut names: Vec<&String> = sources.keys().collect();
    names.sort();

    let mut stats = SqliteSourceIndexingStats::default();
    for name in names {
        let source = sources
            .get(name)
            .expect("source name was collected from the same map");
        let source_stats = run_sqlite_source(db, name, source)
            .with_context(|| format!("failed to index SQLite source {name:?}"))?;
        stats.add_assign(&source_stats);
    }
    stats.documents_pruned += reconcile_removed_source_namespaces(db, sources)?;
    Ok(stats)
}

/// Remove documents and refresh orientation for SQLite source names no longer
/// present in the active configuration. Comparing the escaped namespace in
/// every stored source_ref also catches legacy documents that predate
/// source_refreshes tracking.
fn reconcile_removed_source_namespaces(
    db: &Database,
    sources: &HashMap<String, SqliteSourceConfig>,
) -> Result<usize> {
    let configured_namespaces: HashSet<String> = sources
        .keys()
        .map(|name| escape_source_name(name))
        .collect();
    let stale_paths: Vec<String> = db
        .get_all_document_hashes()?
        .into_keys()
        .filter(|source_ref| {
            let Some(rest) = source_ref.strip_prefix("sqlite:") else {
                return false;
            };
            let Some((namespace, _)) = rest.split_once('/') else {
                return false;
            };
            !configured_namespaces.contains(namespace)
        })
        .collect();

    if !stale_paths.is_empty() {
        db.remove_occurrences_for_paths(&stale_paths)?;
    }
    let documents_pruned = db.delete_documents_by_paths(&stale_paths)?;

    let configured_names: HashSet<&str> = sources.keys().map(String::as_str).collect();
    let stale_refreshes = db.query_read(
        "SELECT source_name FROM source_refreshes",
        Vec::<Value>::new(),
        |row| row.get::<String>(0),
    )?;
    for source_name in stale_refreshes {
        if !configured_names.contains(source_name.as_str()) {
            db.execute_write(
                "DELETE FROM source_refreshes WHERE source_name = ?1",
                vec![Value::Text(source_name)],
            )?;
        }
    }
    if documents_pruned > 0 {
        db.prune_entities_without_occurrences()?;
    }
    Ok(documents_pruned)
}

/// Execute one configured source as a complete fingerprint reconciliation.
/// Every configured query is read in full: matching fingerprints are skipped,
/// changed fingerprints are rebuilt, and documents absent from the result are
/// pruned. A configured `where` role activates monthly conversation buckets;
/// sources without it retain one document per source row.
pub fn run_sqlite_source(
    db: &Database,
    source_name: &str,
    source: &SqliteSourceConfig,
) -> Result<SqliteSourceIndexingStats> {
    validate_contract(source_name, source)?;
    ensure_source_path_exists(&source.db)?;
    let source_db = open_source_read_only(&source.db)?;

    let sql = source.query.trim().trim_end_matches(';').trim_end();
    let mut statement = source_db
        .prepare(sql)
        .with_context(|| format!("failed to prepare source query: {sql}"))?;
    if !statement.readonly() {
        bail!("source query must be read-only");
    }
    let columns = ResolvedColumns::new(&statement, &source.roles)?;
    let mut rows = statement
        .query([])
        .context("failed to execute source query")?;

    let epoch_ms = timestamp_epoch_ms(&source.timestamp)?;
    let mut source_rows = Vec::new();
    let mut rows_skipped_empty = 0usize;
    while let Some(row) = rows.next().context("failed to read source query row")? {
        // Content-free rows (e.g. attachment-only or reaction messages whose
        // body lives outside the mapped `what` columns) are skipped rather than
        // aborting the whole index. Mapping `what` to a BLOB column is still a
        // hard error via `scalar_text`.
        let Some(what) = concatenated_text(row, &columns.what, "\n\n", "what")? else {
            rows_skipped_empty += 1;
            continue;
        };
        let id = columns
            .id
            .iter()
            .map(|index| CursorValue::from_ref(row.get_ref(*index)?))
            .collect::<Result<Vec<_>>>()?;
        let who = role_text_values(row, &columns.who, "who")?;
        let where_labels = role_text_values(row, &columns.where_columns, "where")?;
        let created_at =
            timestamp_value_to_unix_ms(row.get_ref(columns.when)?, &source.timestamp, epoch_ms)?;
        let where_key = role_optional_text_values(row, &columns.where_columns, "where")?;
        source_rows.push(SourceRow {
            id,
            who,
            what,
            where_labels,
            where_key,
            created_at,
        });
    }
    drop(rows);
    drop(statement);

    source_rows.sort_by(compare_source_rows);
    let rows_seen = source_rows.len();
    let pending = if source.roles.r#where.is_empty() {
        build_row_documents(source_name, source_rows)
    } else {
        build_bucket_documents(source_name, source_rows)?
    };

    let existing = source_document_hashes(db, source_name)?;
    let current_paths: HashSet<&str> = pending
        .iter()
        .map(|item| item.document.source_ref.as_str())
        .collect();
    let pruned_paths: Vec<String> = existing
        .keys()
        .filter(|path| !current_paths.contains(path.as_str()))
        .cloned()
        .collect();
    let mut rebuilt = Vec::new();
    let mut skipped = 0usize;
    for item in pending {
        match existing.get(&item.document.source_ref) {
            Some((fingerprint, _)) if fingerprint == &item.document.content_hash => skipped += 1,
            _ => rebuilt.push(item),
        }
    }

    let rebuilt_paths: Vec<String> = rebuilt
        .iter()
        .map(|item| item.document.source_ref.clone())
        .collect();
    if !rebuilt_paths.is_empty() {
        db.remove_occurrences_for_paths(&rebuilt_paths)?;
        let documents: Vec<ProcessedDocument> =
            rebuilt.iter().map(|item| item.document.clone()).collect();
        db.upsert_documents_batch(&documents)?;
        let entity_entries: Vec<(String, EntityType, EntityOccurrence)> = rebuilt
            .iter()
            .flat_map(|item| item.occurrences.iter().cloned())
            .collect();
        let refs: Vec<(&str, &EntityType, &EntityOccurrence)> = entity_entries
            .iter()
            .map(|(name, entity_type, occurrence)| (name.as_str(), entity_type, occurrence))
            .collect();
        db.upsert_entities_batch(&refs)?;
    }
    let documents_pruned = db.delete_documents_by_paths(&pruned_paths)?;
    db.prune_entities_without_occurrences()?;

    let refreshed_at = Utc::now().timestamp_millis();
    db.set_source_refresh_status(
        source_name,
        rows_seen,
        rebuilt.len() + skipped,
        refreshed_at,
    )?;

    Ok(SqliteSourceIndexingStats {
        sources_queried: 1,
        rows_seen,
        buckets_seen: rebuilt.len() + skipped,
        documents_rebuilt: rebuilt.len(),
        documents_skipped: skipped,
        documents_pruned,
        rows_skipped_empty,
    })
}

fn validate_contract(source_name: &str, source: &SqliteSourceConfig) -> Result<()> {
    if source_name.trim().is_empty() {
        bail!("source name may not be empty");
    }
    if !source.db.is_absolute() {
        bail!(
            "source database path must be absolute: {}",
            source.db.display()
        );
    }
    if source.query.trim().is_empty() {
        bail!("source query may not be empty");
    }
    if source.roles.id.is_empty() {
        bail!("source roles.id must contain at least one result column");
    }
    if source.roles.what.is_empty() {
        bail!("source roles.what must contain at least one result column");
    }
    if source.roles.when.trim().is_empty() {
        bail!("source roles.when may not be empty");
    }
    Ok(())
}

fn source_document_hashes(
    db: &Database,
    source_name: &str,
) -> Result<HashMap<String, (String, i64)>> {
    let prefix = format!("sqlite:{}/", escape_source_name(source_name));
    Ok(db
        .get_all_document_hashes()?
        .into_iter()
        .filter(|(source_ref, _)| source_ref.starts_with(&prefix))
        .collect())
}

fn build_row_documents(source_name: &str, rows: Vec<SourceRow>) -> Vec<PendingDocument> {
    rows.into_iter()
        .map(|row| {
            let created_at = row.created_at;
            let who = row.who;
            let where_labels = row.where_labels;
            let mut document = structured_document_to_processed_document(StructuredDocumentInput {
                source_ref: stable_source_ref(source_name, &row.id),
                title: who
                    .first()
                    .cloned()
                    .unwrap_or_else(|| source_name.to_string()),
                content: Some(row.what),
                notes: None,
                source: Some(source_name.to_string()),
                tags: Vec::new(),
                links: who.clone(),
                folders: where_labels.clone(),
                created_at,
            });
            // The base content_hash only covers the rendered title/body. Row-mode
            // reconciliation (below) compares on content_hash, so bind the rest of
            // the record — `when`, every `who`, and `where` — into it. Otherwise an
            // edit to a timestamp or a non-leading participant leaves the hash equal
            // and the row is skipped, so stored dates/links/entity occurrences rot.
            document.content_hash =
                row_reconcile_hash(&document.content_hash, created_at, &who, &where_labels);
            let occurrences =
                IndexingPipeline::collect_entity_entries(std::slice::from_ref(&document));
            PendingDocument {
                document,
                occurrences,
            }
        })
        .collect()
}

/// Fold a row's non-body record fields into its reconciliation hash so that
/// edits to `when`/`who`/`where` (which the rendered content_hash misses) are
/// detected on refresh.
fn row_reconcile_hash(
    base_content_hash: &str,
    created_at: i64,
    who: &[String],
    where_labels: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_content_hash.as_bytes());
    hasher.update(created_at.to_le_bytes());
    hasher.update([b'\x1f']);
    for value in who {
        hasher.update(value.as_bytes());
        hasher.update([b'\x1e']);
    }
    hasher.update([b'\x1d']);
    for value in where_labels {
        hasher.update(value.as_bytes());
        hasher.update([b'\x1e']);
    }
    format!("{:x}", hasher.finalize())
}

fn build_bucket_documents(source_name: &str, rows: Vec<SourceRow>) -> Result<Vec<PendingDocument>> {
    let mut buckets: BTreeMap<(Vec<Option<String>>, String), Vec<SourceRow>> = BTreeMap::new();
    for row in rows {
        let month = DateTime::<Utc>::from_timestamp_millis(row.created_at)
            .context("when timestamp is outside the UTC calendar range")?
            .format("%Y-%m")
            .to_string();
        buckets
            .entry((row.where_key.clone(), month))
            .or_default()
            .push(row);
    }

    let mut documents = Vec::with_capacity(buckets.len());
    for ((where_key, month), mut rows) in buckets {
        rows.sort_by(compare_source_rows);
        let fingerprint = bucket_fingerprint(&rows);
        let where_digest = stable_where_digest(&where_key);
        let source_ref = format!(
            "sqlite:{}/{where_digest}/{month}",
            escape_source_name(source_name)
        );
        let folders: Vec<String> = rows
            .iter()
            .flat_map(|row| row.where_labels.iter().cloned())
            .map(|folder| (folder, ()))
            .collect::<BTreeMap<_, ()>>()
            .into_keys()
            .collect();
        let title = if folders.is_empty() {
            format!("{source_name} {month}")
        } else {
            folders.join(" / ")
        };
        let entries: Vec<StructuredLogEntry> = rows
            .iter()
            .map(|row| StructuredLogEntry {
                who: row.who.clone(),
                content: row.what.clone(),
                created_at: row.created_at,
            })
            .collect();
        let output = structured_log_document_to_processed_document(StructuredLogDocumentInput {
            source_ref: source_ref.clone(),
            title,
            source: Some(source_name.to_string()),
            folders,
            entries,
            fingerprint,
        });
        let now = Utc::now().timestamp_millis();
        let mut occurrences = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let context = output.entry_contexts.get(index).cloned();
            for who in &row.who {
                occurrences.push((
                    who.clone(),
                    EntityType::Link,
                    source_occurrence(&source_ref, context.clone(), now, row.created_at),
                ));
            }
            for folder in &row.where_labels {
                occurrences.push((
                    folder.clone(),
                    EntityType::Folder,
                    source_occurrence(&source_ref, context.clone(), now, row.created_at),
                ));
            }
        }
        documents.push(PendingDocument {
            document: output.document,
            occurrences,
        });
    }
    Ok(documents)
}

fn source_occurrence(
    source_ref: &str,
    context: Option<String>,
    created_at: i64,
    effective_date: i64,
) -> EntityOccurrence {
    EntityOccurrence {
        id: None,
        entity_id: 0,
        file_path: source_ref.to_string(),
        context,
        created_at,
        effective_date: Some(effective_date),
        date_source: "content".to_string(),
    }
}

fn compare_source_rows(left: &SourceRow, right: &SourceRow) -> Ordering {
    left.created_at
        .cmp(&right.created_at)
        .then_with(|| compare_cursor_tuples(&left.id, &right.id))
}

fn compare_cursor_tuples(left: &[CursorValue], right: &[CursorValue]) -> Ordering {
    left.iter()
        .zip(right)
        .map(|(left, right)| compare_cursor_values(left, right))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn compare_cursor_values(left: &CursorValue, right: &CursorValue) -> Ordering {
    fn rank(value: &CursorValue) -> u8 {
        match value {
            CursorValue::Integer(_) => 0,
            CursorValue::Real(_) => 1,
            CursorValue::Text(_) => 2,
            CursorValue::Blob(_) => 3,
        }
    }
    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (CursorValue::Integer(left), CursorValue::Integer(right)) => left.cmp(right),
            (CursorValue::Real(left), CursorValue::Real(right)) => left.total_cmp(right),
            (CursorValue::Text(left), CursorValue::Text(right)) => left.cmp(right),
            (CursorValue::Blob(left), CursorValue::Blob(right)) => left.cmp(right),
            _ => Ordering::Equal,
        })
}

fn bucket_fingerprint(rows: &[SourceRow]) -> String {
    // Hash the normalized fields that actually render the stored log. This is
    // the determinism boundary for refresh: changes to raw query cells that
    // normalize away do not churn the bucket, while every visible edit does.
    let mut records: Vec<Vec<u8>> = rows
        .iter()
        .map(|row| {
            serde_json::to_vec(&(&row.who, row.created_at, &row.what))
                .expect("serializing normalized SQLite roles cannot fail")
        })
        .collect();
    records.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(b"enzyme-sqlite-bucket-v2\0");
    for record in records {
        hasher.update(record.len().to_le_bytes());
        hasher.update(&record);
    }
    format!("{:x}", hasher.finalize())
}

fn stable_where_digest(where_key: &[Option<String>]) -> String {
    let normalized: Vec<Option<&str>> = where_key
        .iter()
        .map(|value| value.as_deref().filter(|value| !value.is_empty()))
        .collect();
    let payload = if normalized.iter().all(Option::is_none) {
        serde_json::to_vec(&"fallback").expect("serializing a string cannot fail")
    } else {
        serde_json::to_vec(&normalized).expect("serializing strings cannot fail")
    };
    let mut hasher = Sha256::new();
    hasher.update(b"enzyme-sqlite-where-v1\0");
    hasher.update(payload);
    format!("{:x}", hasher.finalize())
}

struct ResolvedColumns {
    id: Vec<usize>,
    who: Vec<usize>,
    when: usize,
    what: Vec<usize>,
    where_columns: Vec<usize>,
}

impl ResolvedColumns {
    fn new(statement: &rusqlite::Statement<'_>, roles: &SqliteSourceRoles) -> Result<Self> {
        fn indices(
            statement: &rusqlite::Statement<'_>,
            names: &[String],
            role: &str,
        ) -> Result<Vec<usize>> {
            names
                .iter()
                .map(|name| {
                    statement.column_index(name).with_context(|| {
                        format!("source role {role:?} references missing result column {name:?}")
                    })
                })
                .collect()
        }

        Ok(Self {
            id: indices(statement, &roles.id, "id")?,
            who: indices(statement, &roles.who, "who")?,
            when: statement.column_index(&roles.when).with_context(|| {
                format!(
                    "source role {:?} references missing result column {:?}",
                    "when", roles.when
                )
            })?,
            what: indices(statement, &roles.what, "what")?,
            where_columns: indices(statement, &roles.r#where, "where")?,
        })
    }
}

fn concatenated_text(
    row: &rusqlite::Row<'_>,
    columns: &[usize],
    separator: &str,
    role: &str,
) -> Result<Option<String>> {
    let mut parts = Vec::new();
    for index in columns {
        let Some(value) = scalar_text(row.get_ref(*index)?, role)? else {
            continue;
        };
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join(separator)))
}

fn role_text_values(row: &rusqlite::Row<'_>, columns: &[usize], role: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for index in columns {
        let Some(value) = scalar_text(row.get_ref(*index)?, role)? else {
            continue;
        };
        let trimmed = value.trim();
        if !trimmed.is_empty() && !values.iter().any(|existing| existing == trimmed) {
            values.push(trimmed.to_string());
        }
    }
    Ok(values)
}

fn role_optional_text_values(
    row: &rusqlite::Row<'_>,
    columns: &[usize],
    role: &str,
) -> Result<Vec<Option<String>>> {
    columns
        .iter()
        .map(|index| {
            Ok(scalar_text(row.get_ref(*index)?, role)?
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()))
        })
        .collect()
}

fn scalar_text(value: ValueRef<'_>, role: &str) -> Result<Option<String>> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(Some(value.to_string())),
        ValueRef::Real(value) if value.is_finite() => Ok(Some(value.to_string())),
        ValueRef::Real(_) => bail!("source role {role:?} contains a non-finite REAL"),
        ValueRef::Text(value) => Ok(Some(
            std::str::from_utf8(value)
                .with_context(|| format!("source role {role:?} contains non-UTF-8 text"))?
                .to_string(),
        )),
        ValueRef::Blob(_) => bail!("source role {role:?} may not contain BLOB values"),
    }
}

/// Convert an integer timestamp in a configured unit/epoch to Unix milliseconds.
pub fn timestamp_to_unix_ms(value: i64, config: &SqliteTimestampConfig) -> Result<i64> {
    let epoch_ms = timestamp_epoch_ms(config)?;
    timestamp_integer_to_unix_ms(value, config.unit, epoch_ms)
}

fn timestamp_epoch_ms(config: &SqliteTimestampConfig) -> Result<i64> {
    let Some(epoch) = config.epoch.as_deref() else {
        return Ok(0);
    };
    let date = NaiveDate::parse_from_str(epoch, "%Y-%m-%d")
        .with_context(|| format!("invalid timestamp epoch {epoch:?}; expected YYYY-MM-DD"))?;
    Ok(date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc()
        .timestamp_millis())
}

fn timestamp_value_to_unix_ms(
    value: ValueRef<'_>,
    config: &SqliteTimestampConfig,
    epoch_ms: i64,
) -> Result<i64> {
    match value {
        ValueRef::Integer(value) => timestamp_integer_to_unix_ms(value, config.unit, epoch_ms),
        ValueRef::Real(value) => timestamp_real_to_unix_ms(value, config.unit, epoch_ms),
        ValueRef::Text(value) => {
            let value = std::str::from_utf8(value).context("timestamp contains non-UTF-8 text")?;
            if let Ok(integer) = value.parse::<i64>() {
                timestamp_integer_to_unix_ms(integer, config.unit, epoch_ms)
            } else {
                let real = value
                    .parse::<f64>()
                    .with_context(|| format!("timestamp {value:?} is not numeric"))?;
                timestamp_real_to_unix_ms(real, config.unit, epoch_ms)
            }
        }
        ValueRef::Null => bail!("when role column may not be NULL"),
        ValueRef::Blob(_) => bail!("when role column may not contain BLOB values"),
    }
}

fn timestamp_integer_to_unix_ms(
    value: i64,
    unit: SqliteTimestampUnit,
    epoch_ms: i64,
) -> Result<i64> {
    let value = i128::from(value);
    let offset_ms = match unit {
        SqliteTimestampUnit::S => value
            .checked_mul(1_000)
            .context("timestamp overflows during seconds conversion")?,
        SqliteTimestampUnit::Ms => value,
        SqliteTimestampUnit::Us => value / 1_000,
        SqliteTimestampUnit::Ns => value / 1_000_000,
    };
    let unix_ms = i128::from(epoch_ms)
        .checked_add(offset_ms)
        .context("timestamp overflows while applying epoch")?;
    i64::try_from(unix_ms).context("timestamp is outside the Unix-millisecond range")
}

fn timestamp_real_to_unix_ms(value: f64, unit: SqliteTimestampUnit, epoch_ms: i64) -> Result<i64> {
    if !value.is_finite() {
        bail!("timestamp contains a non-finite REAL");
    }
    let factor = match unit {
        SqliteTimestampUnit::S => 1_000.0,
        SqliteTimestampUnit::Ms => 1.0,
        SqliteTimestampUnit::Us => 0.001,
        SqliteTimestampUnit::Ns => 0.000_001,
    };
    let unix_ms = value.mul_add(factor, epoch_ms as f64).round();
    if unix_ms < i64::MIN as f64 || unix_ms > i64::MAX as f64 {
        bail!("timestamp is outside the Unix-millisecond range");
    }
    Ok(unix_ms as i64)
}

/// Deterministic, namespace-separated identity derived from source name and the
/// complete typed id tuple.
fn stable_source_ref(source_name: &str, id: &[CursorValue]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_name.len().to_le_bytes());
    hasher.update(source_name.as_bytes());
    hasher.update(serde_json::to_vec(id).expect("serializing supported cursor values cannot fail"));
    let digest = format!("{:x}", hasher.finalize());
    format!("sqlite:{}/{digest}", escape_source_name(source_name))
}

fn escape_source_name(source_name: &str) -> String {
    let mut escaped = String::new();
    for byte in source_name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_units_and_apple_epoch_convert_to_unix_ms() {
        for (unit, value) in [
            (SqliteTimestampUnit::S, 1_700_000_000),
            (SqliteTimestampUnit::Ms, 1_700_000_000_000),
            (SqliteTimestampUnit::Us, 1_700_000_000_000_000),
            (SqliteTimestampUnit::Ns, 1_700_000_000_000_000_000),
        ] {
            assert_eq!(
                timestamp_to_unix_ms(value, &SqliteTimestampConfig { unit, epoch: None }).unwrap(),
                1_700_000_000_000
            );
        }

        assert_eq!(
            timestamp_to_unix_ms(
                0,
                &SqliteTimestampConfig {
                    unit: SqliteTimestampUnit::Ms,
                    epoch: Some("2001-01-01".to_string()),
                },
            )
            .unwrap(),
            978_307_200_000
        );
    }

    #[test]
    fn stable_identity_uses_source_and_complete_typed_id() {
        let id = vec![CursorValue::Integer(7), CursorValue::Text("a/b".into())];
        let same = stable_source_ref("messages", &id);
        assert_eq!(same, stable_source_ref("messages", &id));
        assert_ne!(same, stable_source_ref("mail", &id));
        assert_ne!(
            same,
            stable_source_ref(
                "messages",
                &[CursorValue::Integer(7), CursorValue::Text("other".into())]
            )
        );
        assert!(same.starts_with("sqlite:messages/"));
    }

    fn source_row(id: i64, created_at: i64, body: &str) -> SourceRow {
        SourceRow {
            id: vec![CursorValue::Integer(id)],
            who: vec!["Alice".to_string()],
            what: body.to_string(),
            where_labels: vec!["Thread".to_string()],
            where_key: vec![Some("Thread".to_string())],
            created_at,
        }
    }

    #[test]
    fn bucket_fingerprint_is_order_independent_and_ignores_row_ids() {
        let first = source_row(1, 1_700_000_000_000, "first");
        let second = source_row(2, 1_700_000_001_000, "second");
        let restored_first = source_row(900, 1_700_000_000_000, "first");
        let restored_second = source_row(400, 1_700_000_001_000, "second");

        assert_eq!(
            bucket_fingerprint(&[first.clone(), second.clone()]),
            bucket_fingerprint(&[second, first])
        );
        assert_eq!(
            bucket_fingerprint(&[restored_second, restored_first]),
            bucket_fingerprint(&[
                source_row(1, 1_700_000_000_000, "first"),
                source_row(2, 1_700_000_001_000, "second"),
            ])
        );
    }

    #[test]
    fn bucket_fingerprint_changes_when_rendered_content_is_edited() {
        let original = source_row(1, 1_700_000_000_000, "original");
        let edited = source_row(1, 1_700_000_000_000, "edited in place");

        assert_ne!(
            bucket_fingerprint(&[original]),
            bucket_fingerprint(&[edited])
        );
    }

    #[test]
    fn bucket_rendering_is_deterministic_for_unsorted_query_rows() {
        let first = source_row(1, 1_700_000_000_000, "first");
        let second = source_row(2, 1_700_000_001_000, "second");
        let forward =
            build_bucket_documents("messages", vec![first.clone(), second.clone()]).unwrap();
        let reverse = build_bucket_documents("messages", vec![second, first]).unwrap();

        assert_eq!(forward.len(), 1);
        assert_eq!(
            forward[0].document.source_ref,
            reverse[0].document.source_ref
        );
        assert_eq!(forward[0].document.content, reverse[0].document.content);
        assert_eq!(
            forward[0].document.content_hash,
            reverse[0].document.content_hash
        );
    }

    #[test]
    fn null_where_uses_one_stable_fallback_digest() {
        assert_eq!(stable_where_digest(&[None]), stable_where_digest(&[None]));
        assert_eq!(
            stable_where_digest(&[None, Some(String::new())]),
            stable_where_digest(&[None])
        );
        assert_ne!(
            stable_where_digest(&[None]),
            stable_where_digest(&[Some("Thread".to_string())])
        );
    }
}
