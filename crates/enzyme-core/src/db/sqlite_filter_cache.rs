//! Cached decisions for versioned SQLite source filters.

use std::collections::HashMap;

use anyhow::{Context, Result};
use libsql::Value;

use super::connection::{Database, block_in_place_sync};

#[derive(Clone, Debug)]
pub struct FilterCacheUpdate {
    pub row_key: String,
    pub text_hash: String,
    pub excluded: bool,
}

impl Database {
    pub fn load_sqlite_filter_cache(
        &self,
        source_name: &str,
        policy: &str,
    ) -> Result<HashMap<String, (String, bool)>> {
        let rows = self.query_read(
            "SELECT row_key, text_hash, is_excluded FROM source_filter_cache
             WHERE source_name = ?1 AND policy = ?2",
            vec![Value::Text(source_name.into()), Value::Text(policy.into())],
            |row| {
                Ok((
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<i64>(2)? != 0,
                ))
            },
        )?;
        Ok(rows
            .into_iter()
            .map(|(key, hash, excluded)| (key, (hash, excluded)))
            .collect())
    }

    /// Apply new decisions and remove vanished rows in one transaction. A
    /// policy version change also removes obsolete decisions for this source.
    pub fn update_sqlite_filter_cache(
        &self,
        source_name: &str,
        policy: &str,
        updates: &[FilterCacheUpdate],
        vanished_keys: &[String],
    ) -> Result<()> {
        let source_name = source_name.to_owned();
        let policy = policy.to_owned();
        let updates = updates.to_vec();
        let vanished_keys = vanished_keys.to_vec();
        block_in_place_sync(async move {
            let tx = self
                .conn
                .transaction()
                .await
                .context("begin SQLite filter cache update")?;
            tx.execute(
                "DELETE FROM source_filter_cache WHERE source_name = ?1 AND policy != ?2",
                vec![
                    Value::Text(source_name.clone()),
                    Value::Text(policy.clone()),
                ],
            )
            .await?;
            for update in &updates {
                tx.execute(
                    "INSERT INTO source_filter_cache
                       (source_name, policy, row_key, text_hash, is_excluded)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(source_name, policy, row_key) DO UPDATE SET
                       text_hash = excluded.text_hash, is_excluded = excluded.is_excluded",
                    vec![
                        Value::Text(source_name.clone()),
                        Value::Text(policy.clone()),
                        Value::Text(update.row_key.clone()),
                        Value::Text(update.text_hash.clone()),
                        Value::Integer(i64::from(update.excluded)),
                    ],
                )
                .await?;
            }
            for key in &vanished_keys {
                tx.execute(
                    "DELETE FROM source_filter_cache
                     WHERE source_name = ?1 AND policy = ?2 AND row_key = ?3",
                    vec![
                        Value::Text(source_name.clone()),
                        Value::Text(policy.clone()),
                        Value::Text(key.clone()),
                    ],
                )
                .await?;
            }
            tx.commit()
                .await
                .context("commit SQLite filter cache update")?;
            Ok(())
        })
    }

    pub fn delete_sqlite_filter_cache_for_source(&self, source_name: &str) -> Result<()> {
        self.execute_write(
            "DELETE FROM source_filter_cache WHERE source_name = ?1",
            vec![Value::Text(source_name.into())],
        )?;
        Ok(())
    }
}
