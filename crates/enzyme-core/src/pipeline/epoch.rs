//! Durable catalyst-epoch cadence and build coordination.
//!
//! Corpus generations are continuous desired-state markers.  They never
//! directly request LLM work: an epoch is due only at the cadence boundary,
//! when explicitly forced, or when the generation contract changes.

use anyhow::{Context, Result};
use chrono::Utc;
use libsql::Value;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::Database;
use crate::db::connection::block_in_place_sync;

pub const EPOCH_CADENCE_MS: i64 = 72 * 60 * 60 * 1000;
const RETRY_BASE_MS: i64 = 15 * 60 * 1000;
const RETRY_MAX_MS: i64 = 24 * 60 * 60 * 1000;
const BUILD_LEASE_MS: i64 = 6 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationContract {
    pub schema_version: String,
    pub prompt_version: String,
    pub catalyst_format: String,
    pub catalyst_profile: String,
    pub model: String,
    #[serde(default)]
    pub llm_mode: String,
    #[serde(default)]
    pub llm_provider: String,
    #[serde(default)]
    pub llm_base_url: String,
    pub allocation_semantics: String,
    pub catalysts_per_entity: usize,
    pub min_top_catalysts: Option<usize>,
    pub embedding_backend: String,
    pub embedding_dimension: usize,
    pub targets: Vec<String>,
    /// Complete merged vault config, used both in the fingerprint and by the
    /// shadow selector so a mid-build config edit cannot leak into the epoch.
    pub selection_config_json: String,
    /// Per-entity context char budget. Three-valued (see
    /// [`crate::pipeline::catalysts::CatalystConfig::max_context_chars`]):
    /// `None` = use built-in default, `Some(0)` = explicit unlimited,
    /// `Some(n)` = exactly `n`.
    #[serde(default)]
    pub max_context_chars: Option<usize>,
    /// Run-level ceiling for per-entity context samples. Hosted generation
    /// adapts within this ceiling based on the entity's catalyst allocation.
    pub max_context_samples: usize,
    #[serde(default = "default_min_evidence_documents")]
    pub min_evidence_documents: usize,
    #[serde(default = "default_min_evidence_tokens")]
    pub min_evidence_tokens: usize,
}

fn default_min_evidence_documents() -> usize {
    crate::pipeline::catalysts::DEFAULT_MIN_EVIDENCE_DOCUMENTS
}

fn default_min_evidence_tokens() -> usize {
    crate::pipeline::catalysts::DEFAULT_MIN_EVIDENCE_TOKENS
}

impl GenerationContract {
    pub fn fingerprint(&self) -> Result<String> {
        let json = serde_json::to_vec(self).context("serialize generation contract")?;
        Ok(format!("{:x}", Sha256::digest(json)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochState {
    pub corpus_generation: i64,
    pub corpus_fingerprint: String,
    pub active_epoch_id: Option<String>,
    pub active_epoch_corpus_generation: Option<i64>,
    pub active_epoch_completed_at_ms: Option<i64>,
    pub active_generation_contract: Option<String>,
    pub build_epoch_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochDueReason {
    Initial,
    Cadence,
    ContractChanged,
    ManualForce,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochSchedule {
    pub due: bool,
    pub scheduled: bool,
    /// A durable scheduled record exists but may not yet have a worker. This
    /// lets a later invocation retry process launch without creating an epoch.
    pub launch_needed: bool,
    pub coalesced: bool,
    pub epoch_id: Option<String>,
    pub reason: Option<EpochDueReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochClaim {
    pub epoch_id: String,
    pub corpus_generation: i64,
    pub corpus_fingerprint: String,
    pub generation_contract: String,
    pub generation_contract_json: String,
    pub snapshot_path: Option<String>,
}

pub fn corpus_fingerprint(db: &Database) -> Result<String> {
    let rows = db.query_read(
        "SELECT source_ref, content_hash FROM docs ORDER BY source_ref",
        vec![],
        |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
    )?;
    let mut hasher = Sha256::new();
    for (path, hash) in rows {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuousProjectionFence {
    pub value: String,
    pub dirty: bool,
}

impl Database {
    /// Begin an in-process pull generation that writes directly to the active
    /// materialized view instead of using the detached shadow builder.
    pub fn begin_inline_catalyst_epoch(
        &self,
        contract: &GenerationContract,
        now_ms: i64,
    ) -> Result<String> {
        let state = self.reconcile_corpus_generation(now_ms)?;
        let epoch_id = Uuid::new_v4().to_string();
        self.execute_write(
            "INSERT INTO catalyst_epochs
             (epoch_id, status, corpus_generation, corpus_fingerprint,
              generation_contract, generation_contract_json, requested_at_ms,
              started_at_ms, attempt)
             VALUES (?1, 'building', ?2, ?3, ?4, ?5, ?6, ?6, 1)",
            vec![
                Value::Text(epoch_id.clone()),
                Value::Integer(state.corpus_generation),
                Value::Text(state.corpus_fingerprint),
                Value::Text(contract.fingerprint()?),
                Value::Text(serde_json::to_string(contract)?),
                Value::Integer(now_ms),
            ],
        )?;
        Ok(epoch_id)
    }

    /// Activate a successfully generated in-process epoch after similarities
    /// have also been committed to the active materialized view.
    pub fn activate_inline_catalyst_epoch(&self, epoch_id: &str, now_ms: i64) -> Result<()> {
        block_in_place_sync(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .context("begin inline epoch activation")?;
            Self::mark_epoch_activated_on_tx(&tx, epoch_id, now_ms).await?;
            tx.commit()
                .await
                .context("commit inline epoch activation")?;
            Ok::<(), anyhow::Error>(())
        })
    }
    pub fn continuous_projection_fence(&self) -> Result<ContinuousProjectionFence> {
        let value = self
            .get_config("continuous_projection_fence")?
            .unwrap_or_else(|| "clean:legacy".to_string());
        Ok(ContinuousProjectionFence {
            dirty: value.starts_with("dirty:"),
            value,
        })
    }

    pub fn begin_continuous_projection(&self) -> Result<String> {
        let token = Uuid::new_v4().to_string();
        self.set_config("continuous_projection_fence", &format!("dirty:{token}"))?;
        Ok(token)
    }

    pub fn finish_continuous_projection(&self, token: &str) -> Result<()> {
        let current = self.continuous_projection_fence()?;
        anyhow::ensure!(
            current.value == format!("dirty:{token}"),
            "continuous projection fence changed while writer held its lock"
        );
        self.set_config("continuous_projection_fence", &format!("clean:{token}"))?;
        Ok(())
    }

    pub fn get_epoch_state(&self) -> Result<EpochState> {
        let rows = self.query_read(
            "SELECT corpus_generation, corpus_fingerprint, active_epoch_id,
                    active_epoch_corpus_generation, active_epoch_completed_at_ms,
                    active_generation_contract, build_epoch_id
             FROM catalyst_epoch_state WHERE singleton = 1",
            vec![],
            |row| {
                Ok(EpochState {
                    corpus_generation: row.get::<i64>(0)?,
                    corpus_fingerprint: row.get::<String>(1)?,
                    active_epoch_id: row.get::<Option<String>>(2)?,
                    active_epoch_corpus_generation: row.get::<Option<i64>>(3)?,
                    active_epoch_completed_at_ms: row.get::<Option<i64>>(4)?,
                    active_generation_contract: row.get::<Option<String>>(5)?,
                    build_epoch_id: row.get::<Option<String>>(6)?,
                })
            },
        )?;
        rows.into_iter()
            .next()
            .context("missing catalyst epoch state")
    }

    /// Reconcile the persisted corpus generation with the indexed corpus.
    /// Repeating this after a crash is idempotent because the content
    /// fingerprint, not a write notification, controls the increment.
    pub fn reconcile_corpus_generation(&self, now_ms: i64) -> Result<EpochState> {
        let fingerprint = corpus_fingerprint(self)?;
        self.execute_write(
            "UPDATE catalyst_epoch_state
             SET corpus_generation = corpus_generation + CASE WHEN corpus_fingerprint != ?1 THEN 1 ELSE 0 END,
                 corpus_fingerprint = ?1, updated_at_ms = ?2
             WHERE singleton = 1",
            vec![Value::Text(fingerprint), Value::Integer(now_ms)],
        )?;
        self.get_epoch_state()
    }

    pub fn schedule_catalyst_epoch(
        &self,
        now_ms: i64,
        contract: &GenerationContract,
        force: bool,
    ) -> Result<EpochSchedule> {
        let state = self.get_epoch_state()?;
        let contract_fp = contract.fingerprint()?;
        let contract_json = serde_json::to_string(contract)?;
        let retry_pending = self
            .query_read(
                "SELECT COUNT(*) FROM catalyst_epochs
                 WHERE status='failed' AND corpus_generation=?1 AND generation_contract=?2",
                vec![
                    Value::Integer(state.corpus_generation),
                    Value::Text(contract_fp.clone()),
                ],
                |row| row.get::<i64>(0),
            )?
            .into_iter()
            .next()
            .unwrap_or(0)
            > 0;

        let reason = if force {
            Some(EpochDueReason::ManualForce)
        } else if retry_pending {
            // The original cadence/contract/manual intent remains durable on
            // the failed epoch; later invocations retry it after backoff.
            Some(EpochDueReason::ManualForce)
        } else if state.active_epoch_id.is_none() {
            Some(EpochDueReason::Initial)
        } else if state.active_generation_contract.as_deref() != Some(contract_fp.as_str()) {
            Some(EpochDueReason::ContractChanged)
        } else {
            let age = state
                .active_epoch_completed_at_ms
                .map(|completed| now_ms.saturating_sub(completed))
                .unwrap_or(i64::MAX);
            let corpus_changed =
                state.active_epoch_corpus_generation != Some(state.corpus_generation);
            (age >= EPOCH_CADENCE_MS && corpus_changed).then_some(EpochDueReason::Cadence)
        };

        let Some(reason) = reason else {
            return Ok(EpochSchedule {
                due: false,
                scheduled: false,
                launch_needed: false,
                coalesced: false,
                epoch_id: None,
                reason: None,
            });
        };

        if let Some(epoch_id) = state.build_epoch_id {
            let build = self
                .query_read(
                    "SELECT status,started_at_ms FROM catalyst_epochs WHERE epoch_id=?1",
                    vec![Value::Text(epoch_id.clone())],
                    |row| Ok((row.get::<String>(0)?, row.get::<Option<i64>>(1)?)),
                )?
                .into_iter()
                .next();
            let stale_build = build.as_ref().is_some_and(|(status, started)| {
                status == "building"
                    && started
                        .is_some_and(|started| now_ms.saturating_sub(started) >= BUILD_LEASE_MS)
            });
            if stale_build {
                self.execute_write(
                    "UPDATE catalyst_epochs SET status='scheduled', started_at_ms=NULL,
                         last_error='stale build lease recovered'
                     WHERE epoch_id=?1 AND status='building'",
                    vec![Value::Text(epoch_id.clone())],
                )?;
            }
            return Ok(EpochSchedule {
                due: true,
                scheduled: false,
                launch_needed: stale_build
                    || build
                        .as_ref()
                        .is_some_and(|(status, _)| status == "scheduled"),
                coalesced: true,
                epoch_id: Some(epoch_id),
                reason: Some(reason),
            });
        }

        // Reuse a failed epoch after its backoff so its frozen desired state is
        // retryable. Before the retry boundary, report due-but-not-scheduled
        // instead of creating a replacement that bypasses backoff.
        let failed = self.query_read(
            "SELECT epoch_id, next_retry_at_ms FROM catalyst_epochs
             WHERE status = 'failed' AND corpus_generation = ?1
               AND generation_contract = ?2
             ORDER BY requested_at_ms DESC LIMIT 1",
            vec![
                Value::Integer(state.corpus_generation),
                Value::Text(contract_fp.clone()),
            ],
            |row| Ok((row.get::<String>(0)?, row.get::<Option<i64>>(1)?)),
        )?;
        let epoch_id = match failed.into_iter().next() {
            Some((epoch_id, Some(retry_at))) if retry_at > now_ms => {
                return Ok(EpochSchedule {
                    due: true,
                    scheduled: false,
                    launch_needed: false,
                    coalesced: true,
                    epoch_id: Some(epoch_id),
                    reason: Some(reason),
                });
            }
            Some((epoch_id, _)) => epoch_id,
            None => Uuid::new_v4().to_string(),
        };

        // The conditional state update is the duplicate-build guard. libsql's
        // local connection serializes writers; only the winner sees one row.
        let epoch_id_for_tx = epoch_id.clone();
        let state_fingerprint = state.corpus_fingerprint.clone();
        let claimed = block_in_place_sync(async {
            let tx = self
                .conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .context("begin epoch schedule")?;
            let changed = tx
                .execute(
                    "UPDATE catalyst_epoch_state SET build_epoch_id = ?1, updated_at_ms = ?2
                 WHERE singleton = 1 AND build_epoch_id IS NULL",
                    vec![Value::Text(epoch_id_for_tx.clone()), Value::Integer(now_ms)],
                )
                .await?;
            if changed > 0 {
                tx.execute(
                    "INSERT INTO catalyst_epochs
                        (epoch_id, status, corpus_generation, corpus_fingerprint,
                         generation_contract, generation_contract_json, requested_at_ms)
                     VALUES (?1, 'scheduled', ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(epoch_id) DO UPDATE SET
                         status = 'scheduled', requested_at_ms = excluded.requested_at_ms,
                         last_error = NULL",
                    vec![
                        Value::Text(epoch_id_for_tx),
                        Value::Integer(state.corpus_generation),
                        Value::Text(state_fingerprint),
                        Value::Text(contract_fp.clone()),
                        Value::Text(contract_json.clone()),
                        Value::Integer(now_ms),
                    ],
                )
                .await?;
            }
            tx.commit().await.context("commit epoch schedule")?;
            Ok::<bool, anyhow::Error>(changed > 0)
        })?;
        if !claimed {
            let winner = self.get_epoch_state()?.build_epoch_id;
            return Ok(EpochSchedule {
                due: true,
                scheduled: false,
                launch_needed: false,
                coalesced: true,
                epoch_id: winner,
                reason: Some(reason),
            });
        }

        Ok(EpochSchedule {
            due: true,
            scheduled: true,
            launch_needed: true,
            coalesced: false,
            epoch_id: Some(epoch_id),
            reason: Some(reason),
        })
    }

    pub fn claim_scheduled_epoch(&self, now_ms: i64) -> Result<Option<EpochClaim>> {
        let state = self.get_epoch_state()?;
        let Some(epoch_id) = state.build_epoch_id else {
            return Ok(None);
        };
        let changed = self.execute_write(
            "UPDATE catalyst_epochs SET status = 'building', started_at_ms = ?1,
                    attempt = attempt + 1
             WHERE epoch_id = ?2 AND status = 'scheduled'",
            vec![Value::Integer(now_ms), Value::Text(epoch_id.clone())],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let rows = self.query_read(
            "SELECT epoch_id, corpus_generation, corpus_fingerprint,
                    generation_contract, generation_contract_json, snapshot_path
             FROM catalyst_epochs WHERE epoch_id = ?1",
            vec![Value::Text(epoch_id)],
            |row| {
                Ok(EpochClaim {
                    epoch_id: row.get::<String>(0)?,
                    corpus_generation: row.get::<i64>(1)?,
                    corpus_fingerprint: row.get::<String>(2)?,
                    generation_contract: row.get::<String>(3)?,
                    generation_contract_json: row.get::<String>(4)?,
                    snapshot_path: row.get::<Option<String>>(5)?,
                })
            },
        )?;
        Ok(rows.into_iter().next())
    }

    /// The CLI calls this only after proving the OS worker lock is not held.
    /// It converts a process-aborted build back to the single coalesced
    /// scheduled request immediately instead of waiting for the lease timeout.
    pub fn recover_abandoned_epoch_build(&self) -> Result<bool> {
        let Some(epoch_id) = self.get_epoch_state()?.build_epoch_id else {
            return Ok(false);
        };
        Ok(self.execute_write(
            "UPDATE catalyst_epochs SET status='scheduled', started_at_ms=NULL,
                    last_error='abandoned worker recovered'
             WHERE epoch_id=?1 AND status='building'",
            vec![Value::Text(epoch_id)],
        )? > 0)
    }

    pub fn record_epoch_snapshot(&self, epoch_id: &str, path: &str) -> Result<()> {
        self.execute_write(
            "UPDATE catalyst_epochs SET snapshot_path = ?1 WHERE epoch_id = ?2",
            vec![
                Value::Text(path.to_string()),
                Value::Text(epoch_id.to_string()),
            ],
        )?;
        Ok(())
    }

    /// Coalesce corpus writes that landed after scheduling but before the
    /// frozen snapshot. Once `snapshot_path` is recorded, retries must keep
    /// using that exact desired generation instead.
    pub fn update_unsnapshotted_epoch_corpus(
        &self,
        epoch_id: &str,
        generation: i64,
        fingerprint: &str,
    ) -> Result<()> {
        self.execute_write(
            "UPDATE catalyst_epochs SET corpus_generation=?1, corpus_fingerprint=?2
             WHERE epoch_id=?3 AND status='building' AND snapshot_path IS NULL",
            vec![
                Value::Integer(generation),
                Value::Text(fingerprint.to_string()),
                Value::Text(epoch_id.to_string()),
            ],
        )?;
        Ok(())
    }

    pub fn mark_epoch_failed(&self, epoch_id: &str, now_ms: i64, error: &str) -> Result<()> {
        let attempts = self
            .query_read(
                "SELECT attempt FROM catalyst_epochs WHERE epoch_id = ?1",
                vec![Value::Text(epoch_id.to_string())],
                |row| row.get::<i64>(0),
            )?
            .into_iter()
            .next()
            .unwrap_or(1)
            .max(1);
        let shift = (attempts - 1).min(10) as u32;
        let backoff = RETRY_BASE_MS
            .saturating_mul(1_i64 << shift)
            .min(RETRY_MAX_MS);
        self.execute_write(
            "UPDATE catalyst_epochs SET status = 'failed', failed_at_ms = ?1,
                    next_retry_at_ms = ?2, last_error = ?3
             WHERE epoch_id = ?4",
            vec![
                Value::Integer(now_ms),
                Value::Integer(now_ms.saturating_add(backoff)),
                Value::Text(error.to_string()),
                Value::Text(epoch_id.to_string()),
            ],
        )?;
        self.execute_write(
            "UPDATE catalyst_epoch_state SET build_epoch_id = NULL, updated_at_ms = ?1
             WHERE singleton = 1 AND build_epoch_id = ?2",
            vec![Value::Integer(now_ms), Value::Text(epoch_id.to_string())],
        )?;
        Ok(())
    }

    /// Record a successful activation. The caller must invoke this in the same
    /// transaction that switches catalysts, embeddings, and similarities.
    pub(crate) async fn mark_epoch_activated_on_tx(
        tx: &libsql::Transaction,
        epoch_id: &str,
        completed_at_ms: i64,
    ) -> Result<()> {
        let changed = tx
            .execute(
                "UPDATE catalyst_epochs SET status = 'active', completed_at_ms = ?1,
                    activated_at_ms = ?1 WHERE epoch_id = ?2",
                vec![
                    Value::Integer(completed_at_ms),
                    Value::Text(epoch_id.to_string()),
                ],
            )
            .await?;
        anyhow::ensure!(
            changed == 1,
            "cannot activate missing catalyst epoch {epoch_id}"
        );
        tx.execute(
            "UPDATE catalyst_epochs SET status = 'superseded'
             WHERE status = 'active' AND epoch_id != ?1",
            vec![Value::Text(epoch_id.to_string())],
        )
        .await?;
        tx.execute(
            "UPDATE catalyst_epoch_state
             SET active_epoch_id = ?1,
                 active_epoch_corpus_generation = (SELECT corpus_generation FROM catalyst_epochs WHERE epoch_id = ?1),
                 active_epoch_completed_at_ms = ?2,
                 active_generation_contract = (SELECT generation_contract FROM catalyst_epochs WHERE epoch_id = ?1),
                 build_epoch_id = NULL, updated_at_ms = ?2
             WHERE singleton = 1",
            vec![Value::Text(epoch_id.to_string()), Value::Integer(completed_at_ms)],
        ).await?;
        Ok(())
    }

    /// Bootstrap legacy databases as epoch zero without changing their active
    /// catalyst tables. This makes migration natural and preserves all reads.
    pub fn bootstrap_legacy_epoch(
        &self,
        contract: &GenerationContract,
        now_ms: i64,
        covers_current_corpus: bool,
    ) -> Result<()> {
        let state = self.get_epoch_state()?;
        if state.active_epoch_id.is_some() {
            return Ok(());
        }
        let catalyst_count = self
            .query_read("SELECT COUNT(*) FROM catalysts", vec![], |row| {
                row.get::<i64>(0)
            })?
            .into_iter()
            .next()
            .unwrap_or(0);
        if catalyst_count == 0 {
            return Ok(());
        }
        let generated_ms = self
            .get_latest_catalyst_timestamp()?
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(now_ms);
        let fingerprint = contract.fingerprint()?;
        let contract_json = serde_json::to_string(contract)?;
        let epoch_id = format!("legacy-{}", Uuid::new_v4());
        self.execute_write(
            "INSERT INTO catalyst_epochs
                (epoch_id, status, corpus_generation, corpus_fingerprint,
                 generation_contract, generation_contract_json, requested_at_ms,
                 completed_at_ms, activated_at_ms)
             VALUES (?1, 'active', ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
            vec![
                Value::Text(epoch_id.clone()),
                Value::Integer(state.corpus_generation),
                Value::Text(state.corpus_fingerprint),
                Value::Text(fingerprint.clone()),
                Value::Text(contract_json),
                Value::Integer(generated_ms),
            ],
        )?;
        self.execute_write(
            "UPDATE catalyst_epoch_state SET active_epoch_id = ?1,
                    active_epoch_corpus_generation = CASE WHEN ?5 THEN corpus_generation ELSE NULL END,
                    active_epoch_completed_at_ms = ?2,
                    active_generation_contract = ?3, updated_at_ms = ?4
             WHERE singleton = 1 AND active_epoch_id IS NULL",
            vec![
                Value::Text(epoch_id),
                Value::Integer(generated_ms),
                Value::Text(fingerprint),
                Value::Integer(now_ms),
                Value::Integer(i64::from(covers_current_corpus)),
            ],
        )?;
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Catalyst, CatalystSimilarity};

    fn contract() -> GenerationContract {
        GenerationContract {
            schema_version: "epoch.v1".into(),
            prompt_version: "prompt.v1".into(),
            catalyst_format: "question".into(),
            catalyst_profile: "tension".into(),
            model: "test".into(),
            llm_mode: "hosted".into(),
            llm_provider: "enzyme-hosted".into(),
            llm_base_url: "enzyme-hosted-routing".into(),
            allocation_semantics: "alloc.v1".into(),
            catalysts_per_entity: 7,
            min_top_catalysts: None,
            embedding_backend: "test".into(),
            embedding_dimension: 3,
            targets: vec![],
            selection_config_json: "{}".into(),
            max_context_chars: None,
            max_context_samples: 40,
            min_evidence_documents: default_min_evidence_documents(),
            min_evidence_tokens: default_min_evidence_tokens(),
        }
    }

    fn activate_for_test(db: &Database, at: i64, generation: i64, c: &GenerationContract) {
        let fp = c.fingerprint().unwrap();
        db.execute_write(
            "UPDATE catalyst_epoch_state SET corpus_generation=?1,
             active_epoch_id='active', active_epoch_corpus_generation=?1,
             active_epoch_completed_at_ms=?2, active_generation_contract=?3",
            vec![
                Value::Integer(generation),
                Value::Integer(at),
                Value::Text(fp),
            ],
        )
        .unwrap();
    }

    #[test]
    fn cadence_boundary_requires_changed_corpus() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        activate_for_test(&db, 1_000, 2, &c);
        assert!(
            !db.schedule_catalyst_epoch(1_000 + EPOCH_CADENCE_MS, &c, false)
                .unwrap()
                .due
        );
        db.execute_write(
            "UPDATE catalyst_epoch_state SET corpus_generation=3",
            vec![],
        )
        .unwrap();
        assert!(
            !db.schedule_catalyst_epoch(1_000 + EPOCH_CADENCE_MS - 1, &c, false)
                .unwrap()
                .due
        );
        let at_boundary = db
            .schedule_catalyst_epoch(1_000 + EPOCH_CADENCE_MS, &c, false)
            .unwrap();
        assert!(at_boundary.due && at_boundary.scheduled);
    }

    #[test]
    fn repeated_due_requests_coalesce() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        activate_for_test(&db, 0, 1, &c);
        db.execute_write(
            "UPDATE catalyst_epoch_state SET corpus_generation=2",
            vec![],
        )
        .unwrap();
        let first = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS, &c, false)
            .unwrap();
        let second = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS + 1, &c, false)
            .unwrap();
        assert!(first.scheduled);
        assert!(second.coalesced && !second.scheduled);
        assert_eq!(first.epoch_id, second.epoch_id);
        assert!(
            db.claim_scheduled_epoch(EPOCH_CADENCE_MS + 2)
                .unwrap()
                .is_some()
        );
        assert!(
            db.claim_scheduled_epoch(EPOCH_CADENCE_MS + 3)
                .unwrap()
                .is_none()
        );
        let building = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS + 4, &c, false)
            .unwrap();
        assert!(building.coalesced && !building.launch_needed);
        let recovered = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS + 2 + BUILD_LEASE_MS, &c, false)
            .unwrap();
        assert!(recovered.coalesced && recovered.launch_needed);
        assert!(
            db.claim_scheduled_epoch(EPOCH_CADENCE_MS + 3 + BUILD_LEASE_MS)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn abandoned_build_is_immediately_recoverable_without_duplicate_epoch() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        activate_for_test(&db, 0, 1, &c);
        db.execute_write(
            "UPDATE catalyst_epoch_state SET corpus_generation=2",
            vec![],
        )
        .unwrap();
        let scheduled = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS, &c, false)
            .unwrap();
        let epoch_id = scheduled.epoch_id.unwrap();
        db.claim_scheduled_epoch(EPOCH_CADENCE_MS + 1)
            .unwrap()
            .unwrap();
        assert!(db.recover_abandoned_epoch_build().unwrap());
        let recovered = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS + 2, &c, false)
            .unwrap();
        assert_eq!(recovered.epoch_id.as_deref(), Some(epoch_id.as_str()));
        assert!(recovered.coalesced && recovered.launch_needed);
    }

    #[test]
    fn manual_force_and_contract_change_schedule_early() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        activate_for_test(&db, 0, 1, &c);
        assert!(db.schedule_catalyst_epoch(1, &c, true).unwrap().due);

        let db = Database::open_in_memory().unwrap();
        activate_for_test(&db, 0, 1, &c);
        let mut changed = c.clone();
        changed.prompt_version = "prompt.v2".into();
        assert_eq!(
            db.schedule_catalyst_epoch(1, &changed, false)
                .unwrap()
                .reason,
            Some(EpochDueReason::ContractChanged)
        );

        let db = Database::open_in_memory().unwrap();
        activate_for_test(&db, 0, 1, &c);
        let mut changed_runtime = c.clone();
        changed_runtime.llm_base_url = "https://new-router.invalid".into();
        changed_runtime.embedding_backend = "new-embedding-contract".into();
        assert_eq!(
            db.schedule_catalyst_epoch(1, &changed_runtime, false)
                .unwrap()
                .reason,
            Some(EpochDueReason::ContractChanged)
        );
    }

    fn catalyst(id: &str) -> Catalyst {
        Catalyst {
            id: id.into(),
            text: format!("{id} text"),
            entity: "entity".into(),
            context: "default".into(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn failed_build_preserves_active_epoch_and_obeys_backoff() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        db.insert_catalysts(&[catalyst("old")]).unwrap();
        db.reconcile_corpus_generation(1).unwrap();
        db.bootstrap_legacy_epoch(&c, 1, true).unwrap();
        let old_epoch = db.get_epoch_state().unwrap().active_epoch_id;
        let scheduled = db.schedule_catalyst_epoch(2, &c, true).unwrap();
        let epoch_id = scheduled.epoch_id.unwrap();
        db.claim_scheduled_epoch(3).unwrap().unwrap();
        db.mark_epoch_failed(&epoch_id, 4, "test failure").unwrap();
        assert_eq!(db.get_epoch_state().unwrap().active_epoch_id, old_epoch);
        assert_eq!(db.get_all_catalysts().unwrap()[0].id, "old");
        let retry = db.schedule_catalyst_epoch(5, &c, false).unwrap();
        assert!(retry.due && !retry.scheduled);
        assert_eq!(retry.epoch_id.as_deref(), Some(epoch_id.as_str()));
    }

    #[test]
    fn shadow_activation_is_atomic_and_switches_complete_view() {
        let db = Database::open_in_memory().unwrap();
        let shadow = Database::open_in_memory().unwrap();
        let c = contract();
        db.insert_catalysts(&[catalyst("old")]).unwrap();
        db.reconcile_corpus_generation(1).unwrap();
        db.bootstrap_legacy_epoch(&c, 1, true).unwrap();
        db.replace_chunk_embeddings_for_documents(
            vec![],
            vec![("live.md".into(), 0, vec![1.0, 0.0])],
            vec![],
        )
        .unwrap();
        shadow.insert_catalysts(&[catalyst("new")]).unwrap();
        shadow
            .upsert_catalyst_embeddings(&[("new", &[1.0f32, 0.0][..])])
            .unwrap();

        // A missing epoch record fails inside the activation transaction; the
        // old materialized view remains wholly visible.
        assert!(
            db.activate_epoch_from_shadow(&shadow, "missing", 2)
                .is_err()
        );
        assert_eq!(db.get_all_catalysts().unwrap()[0].id, "old");

        let scheduled = db.schedule_catalyst_epoch(3, &c, true).unwrap();
        let epoch_id = scheduled.epoch_id.unwrap();
        db.claim_scheduled_epoch(4).unwrap().unwrap();
        db.activate_epoch_from_shadow(&shadow, &epoch_id, 5)
            .unwrap();
        assert_eq!(db.get_all_catalysts().unwrap()[0].id, "new");
        assert_eq!(db.get_all_catalyst_embeddings().unwrap()[0].0, "new");
        assert_eq!(
            db.get_similarities_for_catalyst("new", 10).unwrap()[0].file_path,
            "live.md"
        );
        assert_eq!(
            db.get_epoch_state().unwrap().active_epoch_id.as_deref(),
            Some(epoch_id.as_str())
        );
    }

    #[test]
    fn legacy_catalysts_bootstrap_without_rewrite() {
        let db = Database::open_in_memory().unwrap();
        db.insert_catalysts(&[catalyst("legacy")]).unwrap();
        db.reconcile_corpus_generation(10).unwrap();
        db.bootstrap_legacy_epoch(&contract(), 11, true).unwrap();
        let state = db.get_epoch_state().unwrap();
        assert!(
            state
                .active_epoch_id
                .as_deref()
                .is_some_and(|id| id.starts_with("legacy-"))
        );
        assert_eq!(db.get_all_catalysts().unwrap()[0].id, "legacy");
    }

    #[test]
    fn legacy_bootstrap_marks_pre_migration_corpus_state_unknown() {
        let db = Database::open_in_memory().unwrap();
        let c = contract();
        db.insert_catalysts(&[catalyst("legacy")]).unwrap();
        // Model an old legacy catalyst set. Ordinary migration cannot prove
        // that it interpreted every write already present in the database.
        db.execute_write(
            "UPDATE catalysts SET generated_at='1970-01-01T00:00:00.001Z'",
            vec![],
        )
        .unwrap();
        db.reconcile_corpus_generation(1).unwrap();
        db.bootstrap_legacy_epoch(&c, 1, false).unwrap();
        assert_eq!(
            db.get_epoch_state().unwrap().active_epoch_corpus_generation,
            None
        );
        let due = db
            .schedule_catalyst_epoch(EPOCH_CADENCE_MS + 1, &c, false)
            .unwrap();
        assert_eq!(due.reason, Some(EpochDueReason::Cadence));
        assert!(due.scheduled);
    }

    #[test]
    fn target_projection_epoch_matches_source_after_switch() {
        let source = Database::open_in_memory().unwrap();
        let target = Database::open_in_memory().unwrap();
        let source_shadow = Database::open_in_memory().unwrap();
        let old_target_shadow = Database::open_in_memory().unwrap();
        let target_shadow = Database::open_in_memory().unwrap();
        let c = contract();
        source.insert_catalysts(&[catalyst("old")]).unwrap();
        source.reconcile_corpus_generation(1).unwrap();
        source.bootstrap_legacy_epoch(&c, 1, true).unwrap();
        let old_epoch = source.get_epoch_state().unwrap().active_epoch_id.unwrap();
        old_target_shadow
            .upsert_catalyst_embeddings(&[("old", &[0.0f32, 1.0][..])])
            .unwrap();
        target
            .upsert_catalyst_embeddings(&[("old", &[0.0f32, 1.0][..])])
            .unwrap();
        target
            .stage_epoch_projection_from_shadow(&old_target_shadow, &old_epoch)
            .unwrap();

        source_shadow.insert_catalysts(&[catalyst("new")]).unwrap();
        source_shadow
            .upsert_catalyst_embeddings(&[("new", &[1.0f32, 0.0][..])])
            .unwrap();
        target_shadow
            .upsert_catalyst_embeddings(&[("new", &[1.0f32, 0.0][..])])
            .unwrap();
        target_shadow
            .upsert_catalyst_similarities(&[CatalystSimilarity {
                id: None,
                catalyst_id: "new".into(),
                catalyst_text: "new text".into(),
                file_path: "target.md".into(),
                chunk_index: 0,
                similarity: 0.9,
                chunk_metadata: serde_json::json!({}),
                timeline_data: serde_json::json!({}),
                created_at: Utc::now(),
            }])
            .unwrap();
        let scheduled = source.schedule_catalyst_epoch(2, &c, true).unwrap();
        let claim = source.claim_scheduled_epoch(3).unwrap().unwrap();
        target
            .stage_epoch_projection_from_shadow(&target_shadow, &claim.epoch_id)
            .unwrap();
        // A failed source activation can discard the staged namespace without
        // poisoning the target's unkeyed old-epoch retry cache.
        assert_eq!(target.get_all_catalyst_embeddings().unwrap()[0].0, "old");
        // Staging cannot affect reads selected by the still-active source ID.
        assert_eq!(
            target.get_epoch_catalyst_embeddings(&old_epoch).unwrap()[0].0,
            "old"
        );
        source
            .activate_epoch_from_shadow(&source_shadow, &claim.epoch_id, 5)
            .unwrap();
        assert_eq!(
            target
                .get_epoch_catalyst_embeddings(
                    source
                        .get_epoch_state()
                        .unwrap()
                        .active_epoch_id
                        .as_deref()
                        .unwrap()
                )
                .unwrap()[0]
                .0,
            "new"
        );
        assert_eq!(
            target
                .get_epoch_similarities_for_catalysts(&claim.epoch_id, &["new"], 10)
                .unwrap()[0]
                .file_path,
            "target.md"
        );
        assert_eq!(scheduled.epoch_id.as_deref(), Some(claim.epoch_id.as_str()));
    }
}
