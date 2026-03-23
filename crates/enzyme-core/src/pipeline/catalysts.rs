use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use chrono::{Datelike, Utc};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, debug, warn};
use uuid::Uuid;

use crate::config::CustomProfileGuidance;
use crate::db::{Database, Repository};
use crate::llm::{
    LlmClient, GenerationParams,
    build_system_prompt, build_user_prompt, parse_catalyst_response,
    catalyst_json_schema,
    get_profile, CatalystProfile, DEFAULT_PROFILE_KEY, DEFAULT_FORMAT_KEY,
    DEFAULT_MODEL,
};
use crate::models::{Catalyst, Entity};

pub struct CatalystPipeline<'a> {
    repo: Repository<'a>,
    llm: &'a LlmClient,
}

pub struct CatalystStats {
    pub entities_processed: usize,
    pub catalysts_generated: usize,
    pub errors: Vec<(String, String)>,
    pub duration_ms: u64,
    /// Number of entities that failed due to rate limiting (429 / timeout).
    pub rate_limited: usize,
}

/// Configuration for a catalyst generation run.
pub struct CatalystConfig<'a> {
    /// Per-entity catalyst profile overrides (entity name -> profile key).
    pub entity_profiles: &'a HashMap<String, String>,
    /// Number of catalysts to generate per entity.
    pub catalysts_per_entity: usize,
    /// Default profile key for entities without an override.
    pub default_profile: &'a str,
    /// Catalyst output format key (question, thesis, claim).
    pub format_key: &'a str,
    /// Custom profile guidance from vault guide generator.
    pub custom_guidance: Option<&'a CustomProfileGuidance>,
    /// Optional guide signal for calibrating catalyst generation.
    pub guide_signal: Option<&'a str>,
    /// Optional ENZYME_GUIDE content for vault context.
    pub vault_guide: Option<&'a str>,
    /// LLM model to use for catalyst generation.
    pub model: &'a str,
    /// Per-entity catalyst count overrides from volume-based allocation.
    pub per_entity_allocations: Option<&'a HashMap<String, usize>>,
}

impl<'a> Default for CatalystConfig<'a> {
    fn default() -> Self {
        // We can't store a reference to a temporary HashMap in a default,
        // so callers should always construct this explicitly.
        // This exists for convenience in tests.
        static EMPTY_MAP: std::sync::LazyLock<HashMap<String, String>> =
            std::sync::LazyLock::new(HashMap::new);
        Self {
            entity_profiles: &EMPTY_MAP,
            catalysts_per_entity: 7,
            default_profile: DEFAULT_PROFILE_KEY,
            format_key: DEFAULT_FORMAT_KEY,
            custom_guidance: None,
            guide_signal: None,
            vault_guide: None,
            model: DEFAULT_MODEL,
            per_entity_allocations: None,
        }
    }
}

/// Progress event emitted during catalyst generation.
#[derive(Debug, Clone)]
pub enum CatalystEvent {
    /// An entity is being processed.
    EntityStarted {
        entity_name: String,
        entity_type: String,
    },
    /// An entity finished with generated catalyst texts.
    EntityCompleted {
        entity_name: String,
        entity_type: String,
        catalyst_texts: Vec<String>,
        completed: usize,
        total: usize,
    },
    /// An entity was skipped (no context samples).
    EntitySkipped {
        entity_name: String,
        completed: usize,
        total: usize,
    },
    /// An entity failed.
    EntityFailed {
        entity_name: String,
        entity_type: String,
        error: String,
        completed: usize,
        total: usize,
    },
}

/// Per-entity context needed for an LLM call.
struct EntityJob {
    entity_name: String,
    entity_type: String,
    user_prompt: String,
}

/// Result of a single entity's catalyst generation.
struct EntityResult {
    entity_name: String,
    entity_type: String,
    catalysts: Option<Vec<(String, String)>>,
    error: Option<String>,
}

impl<'a> CatalystPipeline<'a> {
    pub fn new(db: &'a Database, llm: &'a LlmClient) -> Self {
        Self {
            repo: Repository::new(db),
            llm,
        }
    }

    /// Run catalyst generation for the given pre-selected entities.
    pub async fn run(
        &self,
        entities: &[Entity],
        config: &CatalystConfig<'_>,
        on_event: impl Fn(CatalystEvent),
    ) -> Result<CatalystStats> {
        let start = Instant::now();
        let mut stats = CatalystStats {
            entities_processed: 0,
            catalysts_generated: 0,
            errors: Vec::new(),
            duration_ms: 0,
            rate_limited: 0,
        };

        let total = entities.len();
        info!(total, "generating catalysts for selected entities");

        // Build system prompt once (shared across all entities for caching)
        let system_prompt = build_system_prompt(
            Some(config.format_key),
            config.custom_guidance,
            config.vault_guide,
        );

        // Resolve the default profile once
        let default_profile = get_profile(config.default_profile)
            .unwrap_or_else(|| get_profile(DEFAULT_PROFILE_KEY).unwrap());

        // Phase 1: Pre-fetch all context and build LLM prompts (synchronous, DB-bound)
        let mut jobs: Vec<EntityJob> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for entity in entities {
            let entity_id = entity.id.unwrap_or(0);
            let entity_name = &entity.name;

            let occurrences = self.repo.get_entity_occurrences(entity_id)?;
            let context_samples: Vec<String> = occurrences
                .iter()
                .filter_map(|o| o.context.clone())
                .take(20)
                .collect();

            if context_samples.is_empty() {
                debug!(entity_name = entity_name.as_str(), "no context samples, skipping");
                skipped.push(entity_name.clone());
                continue;
            }

            // Resolve per-entity profile
            let profile: &CatalystProfile = config
                .entity_profiles
                .get(entity_name)
                .and_then(|key| get_profile(key))
                .unwrap_or(default_profile);

            let num_catalysts = config.per_entity_allocations
                .and_then(|a| a.get(entity_name).copied())
                .unwrap_or(config.catalysts_per_entity);

            let user_prompt = build_user_prompt(
                entity_name,
                &context_samples,
                num_catalysts,
                profile,
                config.format_key,
                config.guide_signal,
                Some(entity),
            );

            debug!(
                entity_name = entity_name.as_str(),
                profile = profile.key,
                "built prompt for entity"
            );

            jobs.push(EntityJob {
                entity_name: entity_name.clone(),
                entity_type: entity.entity_type.as_str().to_string(),
                user_prompt,
            });
        }

        // Report skipped entities
        for (i, name) in skipped.iter().enumerate() {
            on_event(CatalystEvent::EntitySkipped {
                entity_name: name.clone(),
                completed: i + 1,
                total,
            });
        }

        // Phase 2: Fire LLM calls with bounded concurrency
        // Remote providers can handle high parallelism; local models typically
        // serve one request at a time. Paid OpenRouter has no platform rate
        // limit (bottleneck is the upstream provider), so we start generous
        // and let per-task exponential backoff absorb any 429s.
        let is_remote = self.llm.is_openrouter();
        let concurrency = if is_remote { 20 } else { 1 };
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let entity_timeout_secs: u64 = if is_remote { 90 } else { 300 };

        // Build the response_format once based on provider capabilities
        let structured_output = self.llm.structured_output_support();
        let response_format = structured_output
            .build_response_format("catalyst_response", catalyst_json_schema());

        let llm = Arc::new(self.llm.clone());
        let system_prompt = Arc::new(system_prompt);
        let mut join_set: JoinSet<EntityResult> = JoinSet::new();

        let model = config.model.to_string();
        for job in jobs {
            on_event(CatalystEvent::EntityStarted {
                entity_name: job.entity_name.clone(),
                entity_type: job.entity_type.clone(),
            });
            let llm = Arc::clone(&llm);
            let system_prompt = Arc::clone(&system_prompt);
            let model = model.clone();
            let sem = Arc::clone(&semaphore);
            let response_format = response_format.clone();
            join_set.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let entity_name = job.entity_name.clone();
                let entity_type = job.entity_type.clone();

                let entity_timeout = std::time::Duration::from_secs(entity_timeout_secs);
                let result = tokio::time::timeout(entity_timeout, async {
                    let params = GenerationParams {
                        response_format,
                        ..GenerationParams::default()
                    };

                    // 3 attempts with exponential backoff + jitter
                    let max_attempts = 3u32;
                    let mut last_error = String::new();
                    for attempt in 0..max_attempts {
                        let result = llm
                            .chat_completion_cached(
                                &model,
                                &system_prompt,
                                &job.user_prompt,
                                params.clone(),
                            )
                            .await;

                        match result {
                            Ok(r) => return Ok(r),
                            Err(e) => {
                                last_error = e.to_string();
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt,
                                    "LLM call failed: {}",
                                    e
                                );
                                if attempt < max_attempts - 1 {
                                    // Exponential backoff: 2s, 4s + jitter (0-500ms)
                                    let base_ms = 2000u64 * 2u64.pow(attempt);
                                    let jitter_ms = (entity_name.len() as u64 * 37) % 500;
                                    let delay = std::time::Duration::from_millis(base_ms + jitter_ms);
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        }
                    }
                    Err(anyhow::anyhow!("all {} retry attempts failed: {}", max_attempts, last_error))
                }).await;

                let response_text = match result {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => {
                        return EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            error: Some(e.to_string()),
                        };
                    }
                    Err(_) => {
                        debug!(entity_name = entity_name.as_str(), timeout_secs = entity_timeout_secs, "entity timed out");
                        return EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            error: Some(format!("timed out after {}s", entity_timeout_secs)),
                        };
                    }
                };

                debug!(
                    entity_name = entity_name.as_str(),
                    response_len = response_text.len(),
                    response_preview = %&response_text[..response_text.len().min(300)],
                    "LLM response received"
                );

                match parse_catalyst_response(&response_text) {
                    Ok(parsed) if parsed.pairs.is_empty() => {
                        debug!(
                            entity_name = entity_name.as_str(),
                            "LLM returned empty catalysts array"
                        );
                        EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            error: Some("LLM returned empty catalysts array".to_string()),
                        }
                    }
                    Ok(parsed) => EntityResult {
                        entity_name,
                        entity_type,
                        catalysts: Some(parsed.pairs),
                        error: None,
                    },
                    Err(e) => {
                        debug!(
                            entity_name = entity_name.as_str(),
                            response_preview = %&response_text[..response_text.len().min(500)],
                            "failed to parse catalyst response: {}",
                            e
                        );
                        EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            error: Some(format!("parse error: {e}")),
                        }
                    }
                }
            });
        }

        // Phase 3: Collect results as tasks complete
        let mut completed = skipped.len();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(entity_result) => {
                    completed += 1;

                    if let Some(ref error) = entity_result.error {
                        debug!(
                            entity_name = entity_result.entity_name.as_str(),
                            "catalyst generation failed: {}",
                            error
                        );
                        let is_rate_limited = error.contains("429")
                            || error.contains("rate")
                            || error.contains("timed out");
                        if is_rate_limited {
                            stats.rate_limited += 1;
                        }
                        on_event(CatalystEvent::EntityFailed {
                            entity_name: entity_result.entity_name.clone(),
                            entity_type: entity_result.entity_type.clone(),
                            error: error.clone(),
                            completed,
                            total,
                        });
                        stats
                            .errors
                            .push((entity_result.entity_name, error.clone()));
                        continue;
                    }

                    if let Some(pairs) = entity_result.catalysts {
                        // Delete old catalysts and insert new ones
                        self.repo
                            .delete_catalysts_for_entity(&entity_result.entity_name)?;

                        let catalyst_texts: Vec<String> =
                            pairs.iter().map(|(t, _)| t.clone()).collect();

                        let catalysts: Vec<Catalyst> = pairs
                            .into_iter()
                            .map(|(text, topic)| Catalyst {
                                id: Uuid::new_v4().to_string(),
                                text,
                                entity: entity_result.entity_name.clone(),
                                context: topic,
                                generated_at: Utc::now(),
                                metadata: serde_json::json!({}),
                            })
                            .collect();

                        let count = catalysts.len();
                        self.repo.insert_catalysts(&catalysts)?;
                        stats.catalysts_generated += count;

                        on_event(CatalystEvent::EntityCompleted {
                            entity_name: entity_result.entity_name.clone(),
                            entity_type: entity_result.entity_type,
                            catalyst_texts,
                            completed,
                            total,
                        });

                        info!(
                            entity_name = entity_result.entity_name.as_str(),
                            count,
                            "generated catalysts"
                        );
                    }

                    stats.entities_processed += 1;
                }
                Err(e) => {
                    debug!("tokio task panicked: {e}");
                }
            }
        }

        if !stats.errors.is_empty() {
            warn!(
                total_errors = stats.errors.len(),
                rate_limited = stats.rate_limited,
                "catalyst generation completed with errors"
            );
        }

        stats.duration_ms = start.elapsed().as_millis() as u64;
        Ok(stats)
    }
}

/// Calculate per-entity catalyst allocation from 3-month activity.
///
/// Bands: 0→3, 1-5→3-5, 6-10→5-7, 11-20→7-10, 20+→up to 15
pub fn calculate_allocations(entities: &[Entity]) -> HashMap<String, usize> {
    let now = Utc::now();
    let month_keys: Vec<String> = (0..3)
        .map(|i| {
            let y = now.year();
            let m = now.month() as i32 - i;
            let (year, month) = if m <= 0 {
                (y - 1, (m + 12) as u32)
            } else {
                (y, m as u32)
            };
            format!("{:04}-{:02}", year, month)
        })
        .collect();

    entities
        .iter()
        .map(|e| {
            let volume: i64 = month_keys
                .iter()
                .filter_map(|k| e.trends.timeline_buckets.get(k))
                .sum();
            let alloc = match volume {
                0 => 3,
                1..=5 => 3 + ((volume as usize - 1) * 2) / 4,
                6..=10 => 5 + ((volume as usize - 5) * 2) / 5,
                11..=20 => 7 + ((volume as usize - 10) * 3) / 10,
                _ => (10 + (volume as usize - 20)).min(15),
            };
            (e.name.clone(), alloc)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EntityTrends, EntityType};

    fn make_entity(name: &str, buckets: HashMap<String, i64>) -> Entity {
        Entity {
            id: Some(1),
            name: name.to_string(),
            entity_type: EntityType::Tag,
            frequency: 10,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: 0,
                recency_score: 0.0,
                timeline_buckets: buckets,
            },
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn test_allocation_zero_volume() {
        let entities = vec![make_entity("empty", HashMap::new())];
        let allocs = calculate_allocations(&entities);
        assert_eq!(allocs["empty"], 3);
    }

    #[test]
    fn test_allocation_low_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("low", HashMap::from([(key, 3)]))];
        let allocs = calculate_allocations(&entities);
        assert!(allocs["low"] >= 3 && allocs["low"] <= 5);
    }

    #[test]
    fn test_allocation_high_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("high", HashMap::from([(key, 30)]))];
        let allocs = calculate_allocations(&entities);
        assert_eq!(allocs["high"], 15);
    }

    #[test]
    fn test_allocation_medium_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("med", HashMap::from([(key, 15)]))];
        let allocs = calculate_allocations(&entities);
        assert!(allocs["med"] >= 7 && allocs["med"] <= 10);
    }

    #[test]
    fn test_allocation_spans_months() {
        let now = Utc::now();
        let k1 = format!("{:04}-{:02}", now.year(), now.month());
        let prev_month = if now.month() == 1 { 12 } else { now.month() - 1 };
        let prev_year = if now.month() == 1 { now.year() - 1 } else { now.year() };
        let k2 = format!("{:04}-{:02}", prev_year, prev_month);
        let entities = vec![make_entity("span", HashMap::from([(k1, 4), (k2, 4)]))];
        let allocs = calculate_allocations(&entities);
        // 8 total → band 6-10 → 5-7
        assert!(allocs["span"] >= 5 && allocs["span"] <= 7);
    }
}
