use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{
    SqliteSourceConfig, SqliteSourceRoles, SqliteTimestampConfig, SqliteTimestampUnit,
};
use crate::db::Database;
use crate::document::{StructuredDocumentInput, structured_document_to_processed_document};
use crate::models::EntityType;
use crate::pipeline::indexing::IndexingPipeline;

const CURSOR_KIND: &str = "sqlite-composite-v1";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqliteSourceIndexingStats {
    pub sources_queried: usize,
    pub rows_indexed: usize,
}

impl SqliteSourceIndexingStats {
    pub fn changed(&self) -> bool {
        self.rows_indexed > 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    fn to_sql_value(&self) -> Value {
        match self {
            Self::Integer(value) => Value::Integer(*value),
            Self::Real(value) => Value::Real(*value),
            Self::Text(value) => Value::Text(value.clone()),
            Self::Blob(value) => Value::Blob(value.clone()),
        }
    }
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
        let indexed = run_sqlite_source(db, name, source)
            .with_context(|| format!("failed to index SQLite source {name:?}"))?;
        stats.sources_queried += 1;
        stats.rows_indexed += indexed;
    }
    Ok(stats)
}

/// Execute one configured source and advance its cursor after durable upserts.
pub fn run_sqlite_source(
    db: &Database,
    source_name: &str,
    source: &SqliteSourceConfig,
) -> Result<usize> {
    validate_contract(source_name, source)?;

    let cursor = load_cursor(db, source_name)?;
    if let Some(values) = &cursor
        && values.len() != source.roles.id.len()
    {
        bail!(
            "stored cursor has {} columns but source config declares {} id columns",
            values.len(),
            source.roles.id.len()
        );
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let source_db = Connection::open_with_flags(&source.db, flags)
        .with_context(|| format!("failed to open {} read-only", source.db.display()))?;
    source_db
        .pragma_update(None, "query_only", true)
        .context("failed to enforce SQLite query_only mode")?;

    let sql = incremental_query(&source.query, &source.roles, cursor.is_some());
    let mut statement = source_db
        .prepare(&sql)
        .with_context(|| format!("failed to prepare source query: {sql}"))?;
    if !statement.readonly() {
        bail!("source query must be read-only");
    }
    let columns = ResolvedColumns::new(&statement, &source.roles)?;
    let params: Vec<Value> = cursor
        .as_ref()
        .map(|values| values.iter().map(CursorValue::to_sql_value).collect())
        .unwrap_or_default();
    let mut rows = statement
        .query(params_from_iter(params))
        .context("failed to execute source query")?;

    let epoch_ms = timestamp_epoch_ms(&source.timestamp)?;
    let mut documents = Vec::new();
    let mut last_id = None;
    while let Some(row) = rows.next().context("failed to read source query row")? {
        let id = columns
            .id
            .iter()
            .map(|index| CursorValue::from_ref(row.get_ref(*index)?))
            .collect::<Result<Vec<_>>>()?;
        let who = role_text_values(row, &columns.who, "who")?;
        let what = concatenated_text(row, &columns.what, "\n\n", true, "what")?
            .expect("required text role returns Some");
        let where_labels = role_text_values(row, &columns.where_columns, "where")?;
        let created_at =
            timestamp_value_to_unix_ms(row.get_ref(columns.when)?, &source.timestamp, epoch_ms)?;

        let source_ref = stable_source_ref(source_name, &id);
        let title = who
            .first()
            .cloned()
            .unwrap_or_else(|| source_name.to_string());
        documents.push(structured_document_to_processed_document(
            StructuredDocumentInput {
                source_ref,
                title,
                content: Some(what),
                notes: None,
                source: Some(source_name.to_string()),
                tags: Vec::new(),
                links: who,
                folders: where_labels,
                created_at,
            },
        ));
        last_id = Some(id);
    }
    drop(rows);
    drop(statement);

    if documents.is_empty() {
        return Ok(0);
    }

    db.upsert_documents_batch(&documents)?;
    let entity_entries = IndexingPipeline::collect_entity_entries(&documents);
    let refs: Vec<(&str, &EntityType, &crate::models::EntityOccurrence)> = entity_entries
        .iter()
        .map(|(name, entity_type, occurrence)| (name.as_str(), entity_type, occurrence))
        .collect();
    db.upsert_entities_batch(&refs)?;

    let last_id = last_id.expect("non-empty document batch has a final id");
    let cursor_json =
        serde_json::to_string(&last_id).context("failed to serialize source cursor")?;
    db.set_source_cursor(source_name, CURSOR_KIND, &cursor_json)?;

    Ok(documents.len())
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

fn load_cursor(db: &Database, source_name: &str) -> Result<Option<Vec<CursorValue>>> {
    let Some((kind, value)) = db.get_source_cursor(source_name)? else {
        return Ok(None);
    };
    if kind != CURSOR_KIND {
        bail!("unsupported stored cursor kind {kind:?}");
    }
    serde_json::from_str(&value)
        .context("failed to parse stored source cursor")
        .map(Some)
}

fn incremental_query(query: &str, roles: &SqliteSourceRoles, has_cursor: bool) -> String {
    let query = query.trim().trim_end_matches(';').trim_end();
    let id_columns: Vec<String> = roles.id.iter().map(|name| quote_identifier(name)).collect();
    let order = id_columns.join(", ");

    if !has_cursor {
        return format!("SELECT * FROM ({query}) AS enzyme_source ORDER BY {order}");
    }

    let placeholders: Vec<String> = (1..=id_columns.len())
        .map(|index| format!("?{index}"))
        .collect();
    let predicate = if id_columns.len() == 1 {
        format!("{} > {}", id_columns[0], placeholders[0])
    } else {
        format!(
            "({}) > ({})",
            id_columns.join(", "),
            placeholders.join(", ")
        )
    };
    format!("SELECT * FROM ({query}) AS enzyme_source WHERE {predicate} ORDER BY {order}")
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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
    required: bool,
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
        if required {
            bail!("source role {role:?} is NULL or empty for a row");
        }
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
}
