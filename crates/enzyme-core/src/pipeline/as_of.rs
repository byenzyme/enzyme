//! The reference instant for time-dependent reading behavior. Without a vault
//! `as of` clause every caller keeps the wall clock and its legacy future-date
//! handling; with one, evidence dated after the resolved instant is out of scope.
use anyhow::{Context, Result};
use enzyme_spec::AsOf;
use tracing::info;

use crate::db::Database;

/// Resolve a vault's declared `as of` to epoch milliseconds. `None` means no
/// clause, so callers use the wall clock exactly as before.
pub fn resolve(db: &Database, as_of: Option<&AsOf>) -> Result<Option<i64>> {
    let Some(as_of) = as_of else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp_millis();
    let instant = match as_of {
        AsOf::Date { date } => end_of_day(date)?,
        // Future-dated outliers never define the latest evidence.
        AsOf::LatestEvidence => db.latest_evidence_ms(now)?.unwrap_or(now),
    };
    info!(
        ?as_of,
        reference = %chrono::DateTime::from_timestamp_millis(instant)
            .map_or_else(|| instant.to_string(), |d| d.to_rfc3339()),
        "resolved reading reference time"
    );
    Ok(Some(instant))
}

/// The last millisecond of a `YYYY-MM-DD` UTC day.
pub fn end_of_day(date: &str) -> Result<i64> {
    let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .with_context(|| format!("invalid as of date {date:?}"))?;
    Ok(day
        .and_hms_milli_opt(23, 59, 59, 999)
        .context("end of day")?
        .and_utc()
        .timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_of(clause: &str) -> Option<AsOf> {
        enzyme_spec::resolve(
            vec![enzyme_spec::parse(&format!("vault \"/notes\" {{ {clause} }}")).unwrap()],
            std::path::Path::new("/home/test"),
        )
        .unwrap()
        .vaults
        .remove(0)
        .learning
        .as_of
    }

    #[test]
    fn dates_end_their_utc_day_and_latest_evidence_ignores_future_outliers() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(resolve(&db, as_of("").as_ref()).unwrap(), None);
        assert_eq!(resolve(&db, None).unwrap(), None);
        let day = resolve(&db, as_of(r#"as of "2024-10-01""#).as_ref())
            .unwrap()
            .unwrap();
        assert_eq!(
            chrono::DateTime::from_timestamp_millis(day)
                .unwrap()
                .to_rfc3339(),
            "2024-10-01T23:59:59.999+00:00"
        );
        db.execute_write(
            "INSERT INTO entities (id,name,type,frequency,trends) VALUES (1,'topic','link',3,'{}')",
            vec![],
        )
        .unwrap();
        // 2024-03-05, 2024-06-07 (creation fallback), and a far-future outlier.
        db.execute_write(
            "INSERT INTO entity_occurrences (entity_id,file_path,created_at,effective_date,date_source)
             VALUES (1,'a.md',0,1709596800000,'frontmatter'), (1,'b.md',1717718400000,NULL,'filesystem'),
                    (1,'c.md',0,4102444800000,'frontmatter')",
            vec![],
        )
        .unwrap();
        assert_eq!(
            resolve(&db, as_of("as of latest evidence").as_ref()).unwrap(),
            Some(1717718400000)
        );
    }
}
