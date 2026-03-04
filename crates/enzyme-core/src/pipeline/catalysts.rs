use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use tokio::task::JoinSet;
use tracing::{info, debug};
use uuid::Uuid;

use crate::config::CustomProfileGuidance;
use crate::db::{Database, Repository};
use crate::llm::{
    LlmClient, GenerationParams, ResponseFormat,
    build_system_prompt, build_user_prompt, parse_catalyst_response,
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
    /// The first starter prompt generated across all entity calls.
    pub starter_prompt: Option<String>,
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
    starter_prompt: Option<String>,
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
            starter_prompt: None,
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

            let user_prompt = build_user_prompt(
                entity_name,
                &context_samples,
                config.catalysts_per_entity,
                profile,
                config.format_key,
                config.guide_signal,
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

        // Phase 2: Fire all LLM calls concurrently
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
            join_set.spawn(async move {
                let entity_name = job.entity_name.clone();
                let entity_type = job.entity_type.clone();

                // 60s total timeout per entity (covers retries + backoff)
                let entity_timeout = std::time::Duration::from_secs(60);
                let result = tokio::time::timeout(entity_timeout, async {
                    let params = GenerationParams {
                        response_format: Some(ResponseFormat::JsonObject),
                        ..GenerationParams::default()
                    };

                    // 2 attempts (1 retry)
                    for attempt in 0..2u32 {
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
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt,
                                    "LLM call failed: {}",
                                    e
                                );
                                if attempt < 1 {
                                    let delay = std::time::Duration::from_millis(2000);
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        }
                    }
                    Err(anyhow::anyhow!("all retry attempts failed"))
                }).await;

                let response_text = match result {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => {
                        return EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            starter_prompt: None,
                            error: Some(e.to_string()),
                        };
                    }
                    Err(_) => {
                        debug!(entity_name = entity_name.as_str(), "entity timed out after 60s");
                        return EntityResult {
                            entity_name,
                            entity_type,
                            catalysts: None,
                            starter_prompt: None,
                            error: Some("timed out after 60s".to_string()),
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
                    Ok(parsed) => EntityResult {
                        entity_name,
                        entity_type,
                        catalysts: Some(parsed.pairs),
                        starter_prompt: parsed.starter_prompt,
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
                            starter_prompt: None,
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

                        // Capture the first starter prompt we see
                        if stats.starter_prompt.is_none() {
                            if let Some(prompt) = entity_result.starter_prompt {
                                if !prompt.is_empty() {
                                    stats.starter_prompt = Some(prompt);
                                }
                            }
                        }

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

        stats.duration_ms = start.elapsed().as_millis() as u64;
        Ok(stats)
    }
}
