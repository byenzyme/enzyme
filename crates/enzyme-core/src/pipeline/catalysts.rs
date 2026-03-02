use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use tokio::task::JoinSet;
use tracing::{info, warn, debug};
use uuid::Uuid;

use crate::db::{Database, Repository};
use crate::llm::{
    LlmClient, Message, GenerationParams, build_system_prompt, build_user_prompt,
    parse_catalyst_response,
};
use crate::models::Catalyst;

pub struct CatalystPipeline<'a> {
    repo: Repository<'a>,
    llm: &'a LlmClient,
}

pub struct CatalystStats {
    pub entities_processed: usize,
    pub catalysts_generated: usize,
    pub errors: Vec<(String, String)>,
    pub duration_ms: u64,
}

/// Per-entity context needed for an LLM call.
struct EntityJob {
    entity_name: String,
    messages: Vec<Message>,
}

/// Result of a single entity's catalyst generation.
struct EntityResult {
    entity_name: String,
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

    pub async fn run(
        &self,
        top_entities: usize,
        catalysts_per_entity: usize,
        on_progress: impl Fn(&str, usize, usize),
    ) -> Result<CatalystStats> {
        let start = Instant::now();
        let mut stats = CatalystStats {
            entities_processed: 0,
            catalysts_generated: 0,
            errors: Vec::new(),
            duration_ms: 0,
        };

        let entities = self.repo.get_top_entities(top_entities, "recency_score")?;
        let total = entities.len();
        info!(total, "generating catalysts for top entities");

        let system_prompt = build_system_prompt();

        // Phase 1: Pre-fetch all context and build LLM prompts (synchronous, DB-bound)
        let mut jobs: Vec<EntityJob> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for entity in &entities {
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

            let user_prompt =
                build_user_prompt(entity_name, &context_samples, catalysts_per_entity);

            jobs.push(EntityJob {
                entity_name: entity_name.clone(),
                messages: vec![
                    Message {
                        role: "system".to_string(),
                        content: system_prompt.clone(),
                    },
                    Message {
                        role: "user".to_string(),
                        content: user_prompt,
                    },
                ],
            });
        }

        // Report skipped entities (folders like Articles, Books, Tweets, etc.)
        for name in &skipped {
            on_progress(name, 0, total);
        }

        // Phase 2: Fire all LLM calls concurrently
        let llm = Arc::new(self.llm.clone());
        let mut join_set: JoinSet<EntityResult> = JoinSet::new();

        for job in jobs {
            let llm = Arc::clone(&llm);
            join_set.spawn(async move {
                let entity_name = job.entity_name.clone();

                // Retry loop: 3 attempts with exponential backoff
                let mut response = None;
                for attempt in 0..3u32 {
                    match llm
                        .chat_completion(
                            "anthropic/claude-sonnet-4",
                            job.messages.clone(),
                            GenerationParams::default(),
                        )
                        .await
                    {
                        Ok(r) => {
                            response = Some(r);
                            break;
                        }
                        Err(e) => {
                            warn!(
                                entity_name = entity_name.as_str(),
                                attempt,
                                "LLM call failed: {}",
                                e
                            );
                            if attempt < 2 {
                                let delay = std::time::Duration::from_millis(
                                    1000 * 2u64.pow(attempt),
                                );
                                tokio::time::sleep(delay).await;
                            }
                        }
                    }
                }

                let Some(response_text) = response else {
                    return EntityResult {
                        entity_name,
                        catalysts: None,
                        error: Some("all retry attempts failed".to_string()),
                    };
                };

                match parse_catalyst_response(&response_text) {
                    Ok(pairs) => EntityResult {
                        entity_name,
                        catalysts: Some(pairs),
                        error: None,
                    },
                    Err(e) => EntityResult {
                        entity_name,
                        catalysts: None,
                        error: Some(format!("parse error: {e}")),
                    },
                }
            });
        }

        // Phase 3: Collect results as they arrive and write to DB
        let mut completed = skipped.len();
        while let Some(result) = join_set.join_next().await {
            completed += 1;

            let entity_result = match result {
                Ok(r) => r,
                Err(e) => {
                    warn!("tokio task panicked: {e}");
                    continue;
                }
            };

            on_progress(&entity_result.entity_name, completed, total);

            if let Some(error) = entity_result.error {
                warn!(
                    entity_name = entity_result.entity_name.as_str(),
                    "catalyst generation failed: {}",
                    error
                );
                stats
                    .errors
                    .push((entity_result.entity_name, error));
                continue;
            }

            if let Some(pairs) = entity_result.catalysts {
                // Delete old catalysts and insert new ones
                self.repo
                    .delete_catalysts_for_entity(&entity_result.entity_name)?;

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
                info!(
                    entity_name = entity_result.entity_name.as_str(),
                    count,
                    "generated catalysts"
                );
            }

            stats.entities_processed += 1;
        }

        stats.duration_ms = start.elapsed().as_millis() as u64;
        Ok(stats)
    }
}
