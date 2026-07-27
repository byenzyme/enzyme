use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use chrono::{Datelike, Utc};
use futures::StreamExt;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::{Database, FileChunk};
use crate::embedding::{EmbeddingService, l2_normalize};
use crate::llm::{
    CatalystProfile, DEFAULT_FORMAT_KEY, DEFAULT_MODEL, DEFAULT_PROFILE_KEY, EraContext,
    GenerationParams, LlmClient, Message, ParsedCatalyst, build_local_user_prompt,
    build_system_prompt, build_user_prompt, catalyst_entries_json_schema, catalyst_json_schema,
    get_profile, parse_catalyst_response, parse_partial_catalyst_entries,
    streaming_entries_instruction,
};
use crate::models::{
    Catalyst, Embedding, Entity, EntityOccurrence, EntityType, INLINE_CONTEXT_MAX_WORDS,
};
use crate::pipeline::eras::{
    Era, allocate_catalysts_to_eras, assign_occurrences_to_eras, cluster_eras_periodic,
    sample_contexts_by_era,
};

/// Default context char budget for local models (~4K tokens of excerpts).
/// Leaves room for the simplified system prompt (~400 tokens) and output (~500 tokens)
/// within a 16K per-slot context window.
const LOCAL_DEFAULT_MAX_CONTEXT_CHARS: usize = 16000;
const CHUNK_MMR_TOP_K: usize = 50;
const CHUNK_MMR_LAMBDA: f32 = 0.5;

/// Simplified system prompt for local fine-tuned models.
/// Profiles, vault overview, and format template are baked in via SFT training.
const LOCAL_SYSTEM_PROMPT: &str = "\
You generate catalyst questions for a knowledge base. A catalyst is a \
single-sentence probing question grounded in specific excerpts from a user's notes.

Good catalysts:
- Probe tensions, shifts, or what's at stake — not summaries
- Use vocabulary from the excerpts themselves
- Each question explores a DIFFERENT facet (different angle, tension, time period)
- Are retrieval-functional: they'd match relevant content when used as search queries

Bad catalysts:
- Generic questions answerable without reading the excerpts
- Multiple catalysts that probe the same thing from slightly different wording
- Questions using abstract academic language not found in the excerpts

VOCABULARY: Avoid analytical vocabulary that distances the question from lived \
experience. Specifically banned: \"reveal,\" \"suggest about,\" \"underlying,\" \
\"foreclose,\" \"manifest,\" \"illuminate,\" \"inherent,\" \"navigate,\" \"in what ways,\" \
\"intersection of,\" \"the notion of,\" \"the pursuit of,\" \"implicitly,\" \
\"the architecture of.\" Use words that appeared in the excerpts or that a person \
would use in conversation.

INSTRUCTIONS:
1. Ground each catalyst in a specific phrase from the excerpts. Then probe \
AROUND the grounded meaning — ask about what it costs, what it forecloses, \
what it's in friction with, or what it quietly assumes.
2. Each catalyst must be a single sentence question. Keep every catalyst to \
one sentence. IMPORTANT: Vary your sentence openings — do NOT start multiple \
catalysts with the same word.
3. For topicNames: 3-5 lowercase words capturing each inquiry.

Always respond with valid JSON that includes the catalysts and topic names.";

/// Lasso compression: keep head 60% + tail 40% with a `[…]` marker.
/// Preserves structural bookends (entity/instruction at start and end).
fn lasso_truncate(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let marker = "\n[…]\n";
    let head_budget = (max_chars * 60) / 100;
    let tail_budget = max_chars
        .saturating_sub(head_budget)
        .saturating_sub(marker.len());

    // Snap to char boundaries
    let head_end = text
        .char_indices()
        .take_while(|(i, _)| *i <= head_budget)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let tail_start = text
        .char_indices()
        .rev()
        .take_while(|(i, _)| text.len() - *i <= tail_budget)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(text.len());

    format!("{}{marker}{}", &text[..head_end], &text[tail_start..])
}

/// Apply context budget: limit sample count, then truncate total chars via lasso.
fn apply_context_budget(samples: Vec<String>, max_samples: usize, max_chars: usize) -> Vec<String> {
    let mut samples: Vec<String> = samples.into_iter().take(max_samples).collect();
    if max_chars == 0 {
        return samples;
    }

    let total: usize = samples.iter().map(|s| s.len()).sum();
    if total <= max_chars {
        return samples;
    }

    // Per-sample budget, distributed proportionally
    let per_sample = max_chars / samples.len().max(1);
    samples = samples
        .into_iter()
        .map(|s| lasso_truncate(&s, per_sample))
        .collect();
    samples
}

#[derive(Clone)]
struct ChunkCandidate {
    chunk_index: u32,
    content: String,
    timestamp: i64,
    era_index: usize,
    relevance: f32,
    vector: Option<Vec<f32>>,
}

struct ChunkSelectionPreload {
    chunk_vectors: HashMap<(String, u32), Vec<f32>>,
    entity_vectors: Vec<Option<Vec<f32>>>,
}

struct SelectedContext {
    era_context: Option<EraContext>,
    flat_samples: Vec<String>,
}

fn occurrence_timestamp(occ: &EntityOccurrence) -> i64 {
    occ.effective_date.unwrap_or(occ.created_at)
}

fn month_key_from_timestamp(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ts).map(|dt| dt.format("%Y-%m").to_string())
}

fn era_index_for_timestamp(eras: &[Era], ts: i64) -> usize {
    let Some(key) = month_key_from_timestamp(ts) else {
        return eras.len();
    };
    eras.iter()
        .position(|era| key.as_str() >= era.start.as_str() && key.as_str() <= era.end.as_str())
        .unwrap_or(eras.len())
}

fn build_occurrence_context(
    eras: &[Era],
    occurrences: &[EntityOccurrence],
    num_catalysts: usize,
    max_samples: usize,
    max_context_chars: usize,
) -> SelectedContext {
    let era_context = if eras.len() > 1 {
        let bucketed = assign_occurrences_to_eras(eras, occurrences);
        let era_allocs = allocate_catalysts_to_eras(eras, num_catalysts, 1);
        let samples = sample_contexts_by_era(eras, &bucketed, max_samples);

        let samples = if max_context_chars > 0 {
            let total: usize = samples.iter().map(|(_, s)| s.len()).sum();
            if total > max_context_chars {
                let per_sample = max_context_chars / samples.len().max(1);
                samples
                    .into_iter()
                    .map(|(idx, s)| (idx, lasso_truncate(&s, per_sample)))
                    .collect()
            } else {
                samples
            }
        } else {
            samples
        };

        if samples.iter().all(|(_, s)| s.is_empty()) {
            None
        } else {
            Some(EraContext {
                eras: eras.to_vec(),
                era_allocations: era_allocs,
                samples,
            })
        }
    } else {
        None
    };

    let flat_samples = apply_context_budget(
        occurrences
            .iter()
            .filter_map(|o| o.context.clone())
            .collect(),
        max_samples,
        max_context_chars,
    );

    SelectedContext {
        era_context,
        flat_samples,
    }
}

fn push_candidate(
    selected: &mut Vec<usize>,
    total_chars: &mut usize,
    candidates: &[ChunkCandidate],
    idx: usize,
    max_samples: usize,
    max_chars: usize,
) -> bool {
    if selected.len() >= max_samples || selected.contains(&idx) {
        return false;
    }
    let len = candidates[idx].content.len();
    if max_chars > 0 && *total_chars + len > max_chars {
        return false;
    }
    selected.push(idx);
    *total_chars += len;
    true
}

fn mmr_fill(
    candidates: &[ChunkCandidate],
    selected: &mut Vec<usize>,
    total_chars: &mut usize,
    max_samples: usize,
    max_chars: usize,
) {
    let mut ranked: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.vector.is_some())
        .map(|(idx, _)| idx)
        .collect();
    ranked.sort_by(|a, b| {
        candidates[*b]
            .relevance
            .partial_cmp(&candidates[*a].relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let seed_count = selected.len();
    let mut candidate_pool: HashSet<usize> = ranked
        .iter()
        .copied()
        .filter(|idx| !selected.contains(idx))
        .take(CHUNK_MMR_TOP_K + seed_count)
        .collect();
    for idx in selected.iter().copied() {
        candidate_pool.remove(&idx);
    }

    while !candidate_pool.is_empty() && selected.len() < max_samples {
        let mut best_idx = None;
        let mut best_score = f32::NEG_INFINITY;

        for idx in candidate_pool.iter().copied() {
            let rel = candidates[idx].relevance;
            let max_sim = selected
                .iter()
                .filter_map(|sel_idx| {
                    let a = candidates[idx].vector.as_ref()?;
                    let b = candidates[*sel_idx].vector.as_ref()?;
                    if a.len() != b.len() {
                        return None;
                    }
                    Some(Embedding::cosine_similarity(a, b))
                })
                .fold(0.0f32, f32::max);
            let mmr = CHUNK_MMR_LAMBDA * rel - (1.0 - CHUNK_MMR_LAMBDA) * max_sim;
            if mmr > best_score {
                best_score = mmr;
                best_idx = Some(idx);
            }
        }

        let Some(best_idx) = best_idx else {
            break;
        };
        candidate_pool.remove(&best_idx);
        let _ = push_candidate(
            selected,
            total_chars,
            candidates,
            best_idx,
            max_samples,
            max_chars,
        );
    }
}

fn finalize_chunk_context(
    eras: &[Era],
    num_catalysts: usize,
    candidates: &[ChunkCandidate],
    selected: &[usize],
) -> SelectedContext {
    if eras.len() > 1 {
        let samples = selected
            .iter()
            .map(|idx| {
                let candidate = &candidates[*idx];
                (candidate.era_index, candidate.content.clone())
            })
            .collect::<Vec<_>>();
        SelectedContext {
            era_context: Some(EraContext {
                eras: eras.to_vec(),
                era_allocations: allocate_catalysts_to_eras(eras, num_catalysts, 1),
                samples,
            }),
            flat_samples: Vec::new(),
        }
    } else {
        SelectedContext {
            era_context: None,
            flat_samples: selected
                .iter()
                .map(|idx| candidates[*idx].content.clone())
                .collect(),
        }
    }
}

fn select_folder_chunks(
    eras: &[Era],
    num_catalysts: usize,
    candidates: &[ChunkCandidate],
    max_samples: usize,
    max_context_chars: usize,
) -> SelectedContext {
    let mut ranked = candidates.iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.1.timestamp
            .cmp(&a.1.timestamp)
            .then_with(|| a.1.chunk_index.cmp(&b.1.chunk_index))
    });

    let mut selected = Vec::new();
    let mut total_chars = 0usize;

    if eras.len() > 1 {
        for era_idx in 0..eras.len() {
            if let Some((idx, _)) = ranked.iter().find(|(_, c)| c.era_index == era_idx) {
                let _ = push_candidate(
                    &mut selected,
                    &mut total_chars,
                    candidates,
                    *idx,
                    max_samples,
                    max_context_chars,
                );
            }
        }
    }

    for (idx, _) in ranked {
        if selected.len() >= max_samples {
            break;
        }
        let _ = push_candidate(
            &mut selected,
            &mut total_chars,
            candidates,
            idx,
            max_samples,
            max_context_chars,
        );
    }

    finalize_chunk_context(eras, num_catalysts, candidates, &selected)
}

fn select_semantic_chunks(
    eras: &[Era],
    num_catalysts: usize,
    candidates: &[ChunkCandidate],
    max_samples: usize,
    max_context_chars: usize,
) -> SelectedContext {
    let mut selected = Vec::new();
    let mut total_chars = 0usize;

    if eras.len() > 1 {
        for era_idx in 0..eras.len() {
            let best_in_era = candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| c.era_index == era_idx && c.vector.is_some())
                .max_by(|a, b| {
                    a.1.relevance
                        .partial_cmp(&b.1.relevance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(idx, _)| idx);
            if let Some(idx) = best_in_era {
                let _ = push_candidate(
                    &mut selected,
                    &mut total_chars,
                    candidates,
                    idx,
                    max_samples,
                    max_context_chars,
                );
            }
        }
    }

    mmr_fill(
        candidates,
        &mut selected,
        &mut total_chars,
        max_samples,
        max_context_chars,
    );

    finalize_chunk_context(eras, num_catalysts, candidates, &selected)
}

fn build_chunk_candidates(
    chunks: Vec<FileChunk>,
    occurrences: &[EntityOccurrence],
    eras: &[Era],
    chunk_vectors: Option<&HashMap<(String, u32), Vec<f32>>>,
    query_vector: Option<&[f32]>,
) -> Vec<ChunkCandidate> {
    let mut file_timestamps: HashMap<&str, i64> = HashMap::new();
    for occ in occurrences {
        let ts = occurrence_timestamp(occ);
        file_timestamps
            .entry(occ.file_path.as_str())
            .and_modify(|existing| *existing = (*existing).max(ts))
            .or_insert(ts);
    }

    chunks
        .into_iter()
        .filter_map(|chunk| {
            let timestamp = *file_timestamps.get(chunk.file_path.as_str())?;
            let era_index = era_index_for_timestamp(eras, timestamp);
            let vector = chunk_vectors.and_then(|vectors| {
                vectors
                    .get(&(chunk.file_path.clone(), chunk.chunk_index))
                    .cloned()
            });
            let relevance = match (query_vector, vector.as_ref()) {
                (Some(query), Some(vector)) if query.len() == vector.len() => {
                    Embedding::cosine_similarity(query, vector)
                }
                _ => 0.0,
            };

            Some(ChunkCandidate {
                chunk_index: chunk.chunk_index,
                content: chunk.content,
                timestamp,
                era_index,
                relevance,
                vector,
            })
        })
        .collect()
}

pub struct CatalystPipeline<'a> {
    db: &'a Database,
    llm: &'a LlmClient,
    embedding: Option<&'a EmbeddingService>,
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
    /// Optional prompt-level context for the broader vault.
    pub prompt_context: Option<&'a str>,
    /// LLM model to use for catalyst generation.
    pub model: &'a str,
    /// Per-entity catalyst count overrides from volume-based allocation.
    pub per_entity_allocations: Option<&'a HashMap<String, usize>>,
    /// Maximum total characters across all context samples per entity.
    /// When set, samples are truncated (head+tail with lasso compression)
    /// to fit within this budget. `0` means unlimited (default).
    pub max_context_chars: usize,
    /// Maximum number of context samples per entity (default: 20).
    pub max_samples: usize,
    /// LLM concurrency override. If set, overrides the auto-detected value.
    pub concurrency: Option<usize>,
    /// Stream self-contained catalyst records and write complete records before
    /// the full entity response finishes.
    pub stream_partial_catalysts: bool,
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
            prompt_context: None,
            model: DEFAULT_MODEL,
            per_entity_allocations: None,
            max_context_chars: 0,
            max_samples: 20,
            concurrency: None,
            stream_partial_catalysts: false,
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
    /// An entity finished with generated catalyst texts and topic names.
    EntityCompleted {
        entity_name: String,
        entity_type: String,
        catalyst_texts: Vec<String>,
        topic_names: Vec<String>,
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
    catalysts: Option<Vec<ParsedCatalyst>>,
    error: Option<String>,
}

struct PartialCatalystEvent {
    entity_name: String,
    index: usize,
    catalyst: ParsedCatalyst,
}

fn catalyst_from_parsed(entity_name: &str, parsed: ParsedCatalyst, provisional: bool) -> Catalyst {
    let metadata = match (parsed.era_label.as_ref(), provisional) {
        (Some(era), true) => serde_json::json!({"era": era, "generation_phase": "stream"}),
        (Some(era), false) => serde_json::json!({"era": era}),
        (None, true) => serde_json::json!({"generation_phase": "stream"}),
        (None, false) => serde_json::json!({}),
    };
    Catalyst {
        id: Uuid::new_v4().to_string(),
        text: parsed.text,
        entity: entity_name.to_string(),
        context: parsed.topic,
        generated_at: Utc::now(),
        metadata,
    }
}

impl<'a> CatalystPipeline<'a> {
    pub fn new(
        db: &'a Database,
        llm: &'a LlmClient,
        embedding: Option<&'a EmbeddingService>,
    ) -> Self {
        Self { db, llm, embedding }
    }

    fn preload_chunk_selection(&self, entities: &[Entity]) -> Option<ChunkSelectionPreload> {
        let chunk_vectors = match self.db.get_all_chunk_embeddings() {
            Ok(rows) if !rows.is_empty() => rows
                .into_iter()
                .map(|(file_path, chunk_index, vector)| ((file_path, chunk_index), vector))
                .collect::<HashMap<_, _>>(),
            Ok(_) => return None,
            Err(e) => {
                warn!("failed to load chunk embeddings, falling back to occurrence contexts: {e}");
                return None;
            }
        };

        let Some(service) = self.embedding else {
            return Some(ChunkSelectionPreload {
                chunk_vectors,
                entity_vectors: vec![None; entities.len()],
            });
        };

        let mut entity_vectors = vec![None; entities.len()];

        // Tags and folders: embed the entity name directly.
        let name_positions: Vec<(usize, &str)> = entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entity_type == EntityType::Tag)
            .map(|(idx, e)| (idx, e.name.as_str()))
            .collect();

        if !name_positions.is_empty() {
            let names: Vec<&str> = name_positions.iter().map(|(_, n)| *n).collect();
            match service.embed_documents(&names) {
                Ok(vectors) => {
                    for ((idx, _), vector) in name_positions.into_iter().zip(vectors.into_iter()) {
                        entity_vectors[idx] = Some(vector);
                    }
                }
                Err(e) => {
                    warn!("failed to embed tag names: {e}");
                }
            }
        }

        // Links: embed inline occurrence contexts and use centroid.
        // Falls back to entity name when no inline contexts exist.
        let link_positions: Vec<(usize, &Entity)> = entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entity_type == EntityType::Link)
            .collect();

        if !link_positions.is_empty() {
            // Collect inline contexts for all links, plus fallback names
            let mut centroid_texts: Vec<(usize, Vec<String>)> = Vec::new();
            let mut fallback_positions: Vec<(usize, &str)> = Vec::new();

            for &(idx, entity) in &link_positions {
                let entity_id = entity.id.unwrap_or(0);
                let inline_contexts: Vec<String> = self
                    .db
                    .get_entity_occurrences(entity_id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|occ| {
                        let ctx = occ.context?;
                        if ctx.split_whitespace().count() < INLINE_CONTEXT_MAX_WORDS {
                            Some(ctx)
                        } else {
                            None
                        }
                    })
                    .collect();

                if inline_contexts.is_empty() {
                    fallback_positions.push((idx, entity.name.as_str()));
                } else {
                    centroid_texts.push((idx, inline_contexts));
                }
            }

            // Embed all inline contexts in one batch, then compute per-entity centroids
            if !centroid_texts.is_empty() {
                let dim = service.dimension();
                let mut all_texts: Vec<&str> = Vec::new();
                let mut ranges: Vec<(usize, usize, usize)> = Vec::new(); // (entity_idx, start, count)

                for (idx, contexts) in &centroid_texts {
                    let start = all_texts.len();
                    all_texts.extend(contexts.iter().map(|s| s.as_str()));
                    ranges.push((*idx, start, contexts.len()));
                }

                match service.embed_documents(&all_texts) {
                    Ok(vectors) => {
                        for (entity_idx, start, count) in ranges {
                            let mut centroid = vec![0.0f32; dim];
                            for v in &vectors[start..start + count] {
                                for (c, val) in centroid.iter_mut().zip(v.iter()) {
                                    *c += val;
                                }
                            }
                            for c in centroid.iter_mut() {
                                *c /= count as f32;
                            }
                            l2_normalize(&mut centroid);
                            entity_vectors[entity_idx] = Some(centroid);
                        }
                    }
                    Err(e) => {
                        warn!("failed to embed link contexts, falling back to names: {e}");
                        // Move all centroid entities to fallback
                        for (idx, entity) in &link_positions {
                            if entity_vectors[*idx].is_none() {
                                fallback_positions.push((*idx, entity.name.as_str()));
                            }
                        }
                        // Deduplicate fallback positions
                        fallback_positions.sort_by_key(|(idx, _)| *idx);
                        fallback_positions.dedup_by_key(|(idx, _)| *idx);
                    }
                }
            }

            // Embed fallback names (links with no inline contexts)
            if !fallback_positions.is_empty() {
                let names: Vec<&str> = fallback_positions.iter().map(|(_, n)| *n).collect();
                debug!(
                    count = fallback_positions.len(),
                    "link entities with no inline contexts, using name embedding"
                );
                match service.embed_documents(&names) {
                    Ok(vectors) => {
                        for ((idx, _), vector) in
                            fallback_positions.into_iter().zip(vectors.into_iter())
                        {
                            entity_vectors[idx] = Some(vector);
                        }
                    }
                    Err(e) => {
                        warn!("failed to embed fallback link names: {e}");
                    }
                }
            }
        }

        Some(ChunkSelectionPreload {
            chunk_vectors,
            entity_vectors,
        })
    }

    fn build_chunk_context(
        &self,
        entity: &Entity,
        occurrences: &[EntityOccurrence],
        eras: &[Era],
        num_catalysts: usize,
        max_samples: usize,
        max_context_chars: usize,
        preload: Option<&ChunkSelectionPreload>,
        entity_idx: usize,
    ) -> Result<Option<SelectedContext>> {
        let file_paths = occurrences
            .iter()
            .map(|occ| occ.file_path.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if file_paths.is_empty() {
            return Ok(None);
        }

        let chunks = self.db.get_chunks_for_files(&file_paths)?;
        if chunks.is_empty() {
            return Ok(None);
        }

        let chunk_vectors = preload.map(|p| &p.chunk_vectors);
        let query_vector = preload
            .and_then(|p| p.entity_vectors.get(entity_idx))
            .and_then(|v| v.as_deref());
        let candidates =
            build_chunk_candidates(chunks, occurrences, eras, chunk_vectors, query_vector);
        if candidates.is_empty() {
            return Ok(None);
        }

        let selected = match entity.entity_type {
            EntityType::Folder => select_folder_chunks(
                eras,
                num_catalysts,
                &candidates,
                max_samples,
                max_context_chars,
            ),
            EntityType::Tag | EntityType::Link => {
                let scorable = candidates
                    .iter()
                    .any(|c| c.vector.is_some() && c.relevance > 0.0);
                if !scorable {
                    return Ok(None);
                }
                select_semantic_chunks(
                    eras,
                    num_catalysts,
                    &candidates,
                    max_samples,
                    max_context_chars,
                )
            }
        };

        if selected.era_context.is_none() && selected.flat_samples.is_empty() {
            Ok(None)
        } else {
            Ok(Some(selected))
        }
    }

    /// Build entity jobs (system prompt + per-entity user prompts) from entities and config.
    ///
    /// This is Phase 1 of the catalyst pipeline — synchronous, DB-bound.
    /// Extracted so both `run()` and `dump_prompts()` share the same prompt construction.
    fn build_jobs(
        &self,
        entities: &[Entity],
        config: &CatalystConfig<'_>,
    ) -> Result<(String, Vec<EntityJob>, Vec<String>)> {
        let is_local = self.llm.is_local();

        let system_prompt = if is_local {
            // Simplified system prompt for local models (~400 tokens).
            // Profiles, vault overview, and format template are baked into
            // the fine-tuned model via SFT. Only core instructions remain.
            info!("using simplified system prompt for local model");
            LOCAL_SYSTEM_PROMPT.to_string()
        } else {
            build_system_prompt(Some(config.format_key), config.prompt_context)
        };

        // Auto-apply context budget for local models if not explicitly set.
        let max_context_chars = if config.max_context_chars == 0 && is_local {
            info!(
                budget = LOCAL_DEFAULT_MAX_CONTEXT_CHARS,
                "auto-setting context budget for local model"
            );
            LOCAL_DEFAULT_MAX_CONTEXT_CHARS
        } else {
            config.max_context_chars
        };

        let max_samples = if is_local && config.max_samples > 10 {
            10 // Cap samples for local models to keep prompts manageable
        } else {
            config.max_samples
        };

        let default_profile = get_profile(config.default_profile)
            .unwrap_or_else(|| get_profile(DEFAULT_PROFILE_KEY).unwrap());
        let chunk_preload = self.preload_chunk_selection(entities);

        let mut jobs: Vec<EntityJob> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for (entity_idx, entity) in entities.iter().enumerate() {
            let entity_id = entity.id.unwrap_or(0);
            let entity_name = &entity.name;

            let occurrences = self.db.get_entity_occurrences(entity_id)?;

            let profile: &CatalystProfile = config
                .entity_profiles
                .get(entity_name)
                .and_then(|key| get_profile(key))
                .unwrap_or(default_profile);

            let num_catalysts = config
                .per_entity_allocations
                .and_then(|a| a.get(entity_name).copied())
                .unwrap_or(config.catalysts_per_entity);

            let eras = cluster_eras_periodic(&entity.trends.timeline_buckets, 3, num_catalysts);
            let selected_context = self
                .build_chunk_context(
                    entity,
                    &occurrences,
                    &eras,
                    num_catalysts,
                    max_samples,
                    max_context_chars,
                    chunk_preload.as_ref(),
                    entity_idx,
                )?
                .unwrap_or_else(|| {
                    build_occurrence_context(
                        &eras,
                        &occurrences,
                        num_catalysts,
                        max_samples,
                        max_context_chars,
                    )
                });
            let era_context = selected_context.era_context;
            let flat_samples = selected_context.flat_samples;
            let context_samples = &flat_samples;

            if flat_samples.is_empty() && era_context.is_none() {
                debug!(
                    entity_name = entity_name.as_str(),
                    "no context samples, skipping"
                );
                skipped.push(entity_name.clone());
                continue;
            }

            let user_prompt = if is_local {
                build_local_user_prompt(
                    entity_name,
                    context_samples,
                    num_catalysts,
                    Some(profile),
                    era_context.as_ref(),
                )
            } else {
                build_user_prompt(
                    entity_name,
                    context_samples,
                    num_catalysts,
                    profile,
                    config.format_key,
                    Some(entity),
                )
            };

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

        Ok((system_prompt, jobs, skipped))
    }

    /// Dump prompts to a JSONL file for training data generation.
    ///
    /// Runs Phase 1 (prompt building) only — no LLM calls. Each line is:
    /// `{"entity": "...", "entity_type": "...", "system_prompt": "...", "user_prompt": "..."}`
    pub fn dump_prompts(
        &self,
        entities: &[Entity],
        config: &CatalystConfig<'_>,
        output_path: &std::path::Path,
    ) -> Result<usize> {
        let (system_prompt, jobs, skipped) = self.build_jobs(entities, config)?;

        info!(
            jobs = jobs.len(),
            skipped = skipped.len(),
            "dumping prompts (no LLM calls)"
        );

        let mut file = std::fs::File::create(output_path)?;

        // Write system prompt as first line (shared across all entities)
        let sys_line = serde_json::json!({
            "_type": "system_prompt",
            "system_prompt": system_prompt,
        });
        writeln!(file, "{}", sys_line)?;

        // Write per-entity prompts
        for job in &jobs {
            let line = serde_json::json!({
                "entity": job.entity_name,
                "entity_type": job.entity_type,
                "user_prompt": job.user_prompt,
            });
            writeln!(file, "{}", line)?;
        }

        info!(path = %output_path.display(), count = jobs.len(), "wrote prompt dump");
        Ok(jobs.len())
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

        let (system_prompt, jobs, skipped) = self.build_jobs(entities, config)?;

        // Report skipped entities
        for (i, name) in skipped.iter().enumerate() {
            on_event(CatalystEvent::EntitySkipped {
                entity_name: name.clone(),
                completed: i + 1,
                total,
            });
        }

        // Phase 2: Fire LLM calls with bounded concurrency
        // Remote providers can handle moderate parallelism; local models typically
        // serve one request at a time. We keep concurrency conservative to avoid
        // 429s from upstream providers, and use a generous timeout to accommodate
        // retries with exponential backoff.
        let local_model = self.llm.is_local();
        let concurrency = config
            .concurrency
            .unwrap_or(if local_model { 1 } else { 8 });
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let entity_timeout_secs: u64 = if local_model { 300 } else { 30 };

        // Build the response_format once based on provider capabilities. Local
        // SmolLM2 checkpoints were trained before eraLabels were stable, so do
        // not require that field for local inference yet.
        let structured_output = self.llm.structured_output_support();
        let response_format = if config.stream_partial_catalysts {
            structured_output.build_response_format(
                "catalyst_entries_response",
                catalyst_entries_json_schema(!local_model),
            )
        } else {
            structured_output
                .build_response_format("catalyst_response", catalyst_json_schema(!local_model))
        };

        let llm = Arc::new(self.llm.clone());
        let system_prompt = Arc::new(system_prompt);
        let mut join_set: JoinSet<EntityResult> = JoinSet::new();
        let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<PartialCatalystEvent>();
        // Tripped by any task that sees a 402 / insufficient-credits error.
        // Other in-flight tasks short-circuit, and the main loop aborts the rest.
        let credits_exhausted = Arc::new(AtomicBool::new(false));

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
            let local_model = local_model;
            let credits_exhausted = Arc::clone(&credits_exhausted);
            let stream_partial_catalysts = config.stream_partial_catalysts;
            let partial_tx = partial_tx.clone();
            join_set.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let entity_name = job.entity_name.clone();
                let entity_type = job.entity_type.clone();

                if credits_exhausted.load(Ordering::SeqCst) {
                    return EntityResult {
                        entity_name,
                        entity_type,
                        catalysts: None,
                        error: Some("skipped: credits exhausted".to_string()),
                    };
                }

                let entity_timeout = std::time::Duration::from_secs(entity_timeout_secs);
                let result = tokio::time::timeout(entity_timeout, async {
                    let params = GenerationParams {
                        response_format,
                        // Local fine-tunes have small context windows; cap output
                        // tokens so input + output fits within the model's limit.
                        max_tokens: if local_model { 512 } else { 8192 },
                        ..GenerationParams::default()
                    };

                    // 3 attempts with exponential backoff + jitter
                    let max_attempts = 3u32;
                    let mut last_error = String::new();
                    for attempt in 0..max_attempts {
                        let result = if stream_partial_catalysts {
                            let user_prompt = streaming_entries_instruction(&job.user_prompt);
                            let messages = if llm.is_openrouter() {
                                vec![Message::cached("user", system_prompt.as_str(), user_prompt)]
                            } else {
                                vec![
                                    Message::text("system", system_prompt.as_str()),
                                    Message::text("user", user_prompt),
                                ]
                            };
                            match llm
                                .chat_completion_streaming(&model, messages, params.clone())
                                .await
                            {
                                Ok(stream) => {
                                    let mut stream = std::pin::pin!(stream);
                                    let mut response = String::new();
                                    let mut sent = 0usize;
                                    let mut stream_error: Option<anyhow::Error> = None;

                                    while let Some(delta) = stream.next().await {
                                        match delta {
                                            Ok(text) => {
                                                response.push_str(&text);
                                                let parsed =
                                                    parse_partial_catalyst_entries(&response);
                                                for (index, catalyst) in
                                                    parsed.into_iter().enumerate().skip(sent)
                                                {
                                                    sent = index + 1;
                                                    let _ = partial_tx.send(PartialCatalystEvent {
                                                        entity_name: entity_name.clone(),
                                                        index,
                                                        catalyst,
                                                    });
                                                }
                                            }
                                            Err(e) => {
                                                stream_error = Some(e);
                                                break;
                                            }
                                        }
                                    }

                                    match stream_error {
                                        Some(e) => Err(e),
                                        None => Ok(response),
                                    }
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            llm.chat_completion_cached(
                                &model,
                                &system_prompt,
                                &job.user_prompt,
                                params.clone(),
                            )
                            .await
                        };

                        match result {
                            Ok(r) => return Ok(r),
                            Err(e) => {
                                last_error = e.to_string();
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt, "LLM call failed: {}", e
                                );
                                // Don't retry on credit exhaustion — backoff
                                // would just delay the inevitable failure.
                                if is_credits_exhausted_error(&last_error) {
                                    credits_exhausted.store(true, Ordering::SeqCst);
                                    break;
                                }
                                if attempt < max_attempts - 1 {
                                    // Exponential backoff: 2s, 4s + jitter (0-500ms)
                                    let base_ms = 2000u64 * 2u64.pow(attempt);
                                    let jitter_ms = (entity_name.len() as u64 * 37) % 500;
                                    let delay =
                                        std::time::Duration::from_millis(base_ms + jitter_ms);
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        }
                    }
                    Err(anyhow::anyhow!(
                        "all {} retry attempts failed: {}",
                        max_attempts,
                        last_error
                    ))
                })
                .await;

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
                        debug!(
                            entity_name = entity_name.as_str(),
                            timeout_secs = entity_timeout_secs,
                            "entity timed out"
                        );
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
                    response_text = %response_text,
                    "LLM response received"
                );

                match parse_catalyst_response(&response_text) {
                    Ok(parsed) if parsed.catalysts.is_empty() => {
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
                        catalysts: Some(parsed.catalysts),
                        error: None,
                    },
                    Err(e) => {
                        debug!(
                            entity_name = entity_name.as_str(),
                            response_text = %response_text,
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

        drop(partial_tx);

        // Phase 3: Collect results as tasks complete, while applying streamed
        // provisional catalysts on the DB writer thread.
        let mut completed = skipped.len();
        let mut partial_started_entities: HashSet<String> = HashSet::new();
        let mut partial_inserted_counts: HashMap<String, usize> = HashMap::new();

        while !join_set.is_empty() {
            let result = tokio::select! {
                partial = partial_rx.recv() => {
                    if let Some(partial) = partial {
                        let inserted = partial_inserted_counts
                            .entry(partial.entity_name.clone())
                            .or_insert(0);
                        if partial.index < *inserted {
                            continue;
                        }
                        if partial_started_entities.insert(partial.entity_name.clone()) {
                            self.db.delete_catalysts_for_entity(&partial.entity_name)?;
                        }
                        let catalyst = catalyst_from_parsed(
                            &partial.entity_name,
                            partial.catalyst,
                            true,
                        );
                        self.db.insert_catalysts(&[catalyst])?;
                        *inserted = partial.index + 1;
                    }
                    continue;
                }
                result = join_set.join_next() => result,
            };

            let Some(result) = result else {
                continue;
            };
            // Short-circuit the rest of the run on credit exhaustion. Whatever
            // tripped the flag has already been collected as an EntityResult
            // error; aborting here prevents N more entities from each retrying
            // and accumulating identical 402s.
            if credits_exhausted.load(Ordering::SeqCst) {
                join_set.abort_all();
                stats.duration_ms = start.elapsed().as_millis() as u64;
                bail!(
                    "Insufficient credits — catalyst generation aborted. \
                     Visit app.enzyme.garden/plans to top up."
                );
            }
            match result {
                Ok(entity_result) => {
                    completed += 1;

                    if let Some(ref error) = entity_result.error {
                        debug!(
                            entity_name = entity_result.entity_name.as_str(),
                            "catalyst generation failed: {}", error
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

                    if let Some(parsed_catalysts) = entity_result.catalysts {
                        // Delete old catalysts and insert new ones
                        self.db
                            .delete_catalysts_for_entity(&entity_result.entity_name)?;

                        let catalyst_texts: Vec<String> =
                            parsed_catalysts.iter().map(|c| c.text.clone()).collect();
                        let topic_names: Vec<String> =
                            parsed_catalysts.iter().map(|c| c.topic.clone()).collect();

                        let catalysts: Vec<Catalyst> = parsed_catalysts
                            .into_iter()
                            .map(|pc| catalyst_from_parsed(&entity_result.entity_name, pc, false))
                            .collect();

                        let count = catalysts.len();
                        self.db.insert_catalysts(&catalysts)?;
                        stats.catalysts_generated += count;

                        on_event(CatalystEvent::EntityCompleted {
                            entity_name: entity_result.entity_name.clone(),
                            entity_type: entity_result.entity_type,
                            catalyst_texts,
                            topic_names,
                            completed,
                            total,
                        });

                        info!(
                            entity_name = entity_result.entity_name.as_str(),
                            count, "generated catalysts"
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
            debug!(
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
/// Bands: 0→min, 1-5→min to base, 6-10→base area, 11-20→base to upper, 20+→up to max
///
/// When `min_top_catalysts` is set, the largest entity (by volume) is guaranteed
/// at least that many catalysts. The base and max are derived accordingly,
/// matching enzyme-python's `min_top_catalysts` parameter.
/// Detect 402 / insufficient-credit errors from the LLM client error string.
/// Used to short-circuit the catalyst pipeline when the free-tier credit gate
/// or upstream OpenRouter ephemeral key has been exhausted mid-run.
fn is_credits_exhausted_error(err: &str) -> bool {
    // Match the HTTP 402 status line or its reason phrase, not the digits
    // "402" anywhere — they appear incidentally in request ids, token counts,
    // quota values, and model names.
    err.contains("status 402 ")
        || err.contains("Payment Required")
        || err.contains("Insufficient credits")
}

pub fn calculate_allocations(
    entities: &[Entity],
    min_top_catalysts: Option<usize>,
) -> HashMap<String, usize> {
    let min_per_entity: usize = 3;
    let mut base: usize = 7;
    let mut max_per_entity: usize = 15;

    if let Some(min_top) = min_top_catalysts {
        base = (min_top * 6 / 10).max(min_per_entity + 1);
        max_per_entity = max_per_entity.max(min_top);
    }

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

    let entity_volumes: Vec<(&Entity, i64)> = entities
        .iter()
        .map(|e| {
            let volume: i64 = month_keys
                .iter()
                .filter_map(|k| e.trends.timeline_buckets.get(k))
                .sum();
            (e, volume)
        })
        .collect();

    let mut allocations: HashMap<String, usize> = entity_volumes
        .iter()
        .map(|(e, volume)| {
            let volume = *volume;
            let alloc = match volume {
                0 => min_per_entity,
                1..=5 => min_per_entity + ((volume as usize) * 2) / 5,
                6..=10 => 5 + ((volume as usize - 5) * 2) / 5,
                11..=20 => base + ((volume as usize - 10) * 3) / 10,
                _ => {
                    let excess = ((volume as usize) - 20).min(20);
                    10 + (excess * (max_per_entity - 10)) / 20
                }
            };
            (e.name.clone(), alloc.clamp(min_per_entity, max_per_entity))
        })
        .collect();

    // Boost the top entity if it didn't reach min_top_catalysts
    if let Some(min_top) = min_top_catalysts {
        if let Some((top_entity, _)) = entity_volumes.iter().max_by_key(|(_, v)| *v) {
            let current = allocations.get(&top_entity.name).copied().unwrap_or(0);
            if current < min_top {
                allocations.insert(top_entity.name.clone(), min_top.min(max_per_entity));
            }
        }
    }

    allocations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EntityTrends, EntityType};
    use crate::pipeline::eras::Era;

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

    fn make_era(label: &str, start: &str, end: &str, count: i64) -> Era {
        Era {
            label: label.to_string(),
            start: start.to_string(),
            end: end.to_string(),
            count,
        }
    }

    fn make_chunk_candidate(
        chunk_index: u32,
        content: &str,
        timestamp: i64,
        era_index: usize,
        relevance: f32,
        vector: Option<Vec<f32>>,
    ) -> ChunkCandidate {
        ChunkCandidate {
            chunk_index,
            content: content.to_string(),
            timestamp,
            era_index,
            relevance,
            vector,
        }
    }

    #[test]
    fn test_allocation_zero_volume() {
        let entities = vec![make_entity("empty", HashMap::new())];
        let allocs = calculate_allocations(&entities, None);
        assert_eq!(allocs["empty"], 3);
    }

    #[test]
    fn test_allocation_low_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("low", HashMap::from([(key, 3)]))];
        let allocs = calculate_allocations(&entities, None);
        assert!(allocs["low"] >= 3 && allocs["low"] <= 5);
    }

    #[test]
    fn test_allocation_high_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("high", HashMap::from([(key, 30)]))];
        let allocs = calculate_allocations(&entities, None);
        // volume=30, excess=min(10,20)=10, alloc=10+(10*5)/20=12
        assert!(allocs["high"] >= 10 && allocs["high"] <= 15);
    }

    #[test]
    fn test_allocation_medium_volume() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![make_entity("med", HashMap::from([(key, 15)]))];
        let allocs = calculate_allocations(&entities, None);
        assert!(allocs["med"] >= 7 && allocs["med"] <= 10);
    }

    #[test]
    fn test_allocation_spans_months() {
        let now = Utc::now();
        let k1 = format!("{:04}-{:02}", now.year(), now.month());
        let prev_month = if now.month() == 1 {
            12
        } else {
            now.month() - 1
        };
        let prev_year = if now.month() == 1 {
            now.year() - 1
        } else {
            now.year()
        };
        let k2 = format!("{:04}-{:02}", prev_year, prev_month);
        let entities = vec![make_entity("span", HashMap::from([(k1, 4), (k2, 4)]))];
        let allocs = calculate_allocations(&entities, None);
        // 8 total → band 6-10 → 5-7
        assert!(allocs["span"] >= 5 && allocs["span"] <= 7);
    }

    #[test]
    fn test_allocation_min_top_catalysts() {
        let now = Utc::now();
        let key = format!("{:04}-{:02}", now.year(), now.month());
        let entities = vec![
            make_entity("big", HashMap::from([(key.clone(), 50)])),
            make_entity("small", HashMap::from([(key, 2)])),
        ];
        let allocs = calculate_allocations(&entities, Some(25));
        // Top entity should be boosted to at least 25
        assert!(allocs["big"] >= 25);
        // Small entity should stay low
        assert!(allocs["small"] <= 5);
    }

    #[test]
    fn test_select_folder_chunks_covers_each_era() {
        let eras = vec![
            make_era("2025-Q1", "2025-01", "2025-03", 3),
            make_era("2025-Q2", "2025-04", "2025-06", 4),
        ];
        let candidates = vec![
            make_chunk_candidate(0, "old era older chunk", 100, 0, 0.0, None),
            make_chunk_candidate(1, "old era newer chunk", 200, 0, 0.0, None),
            make_chunk_candidate(2, "new era chunk", 300, 1, 0.0, None),
        ];

        let selected = select_folder_chunks(&eras, 3, &candidates, 2, 0);
        let era_context = selected.era_context.expect("expected era context");

        assert_eq!(era_context.samples.len(), 2);
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| { *era == 0 && text == "old era newer chunk" })
        );
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| *era == 1 && text == "new era chunk")
        );
    }

    #[test]
    fn test_select_semantic_chunks_seeds_eras_before_mmr_fill() {
        let eras = vec![
            make_era("2025-Q1", "2025-01", "2025-03", 2),
            make_era("2025-Q2", "2025-04", "2025-06", 2),
        ];
        let candidates = vec![
            make_chunk_candidate(0, "alpha focus", 100, 0, 0.95, Some(vec![1.0, 0.0])),
            make_chunk_candidate(1, "alpha duplicate", 110, 0, 0.90, Some(vec![0.99, 0.01])),
            make_chunk_candidate(2, "beta contrast", 200, 1, 0.80, Some(vec![0.7, 0.3])),
            make_chunk_candidate(3, "beta weak", 210, 1, 0.20, Some(vec![0.0, 1.0])),
        ];

        let selected = select_semantic_chunks(&eras, 3, &candidates, 3, 0);
        let era_context = selected.era_context.expect("expected era context");

        assert_eq!(era_context.samples.len(), 3);
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| *era == 0 && text == "alpha focus")
        );
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| *era == 1 && text == "beta contrast")
        );
    }

    /// Regression: `is_credits_exhausted_error` must classify a *payment
    /// required* condition, not merely spot the digits "402" anywhere in a raw
    /// provider error string.
    ///
    /// Every string below is a realistic upstream error in which "402" appears
    /// incidentally — inside a request id, a token count, a quota value, a
    /// latency figure, or a model name. Misclassifying any of them aborts the
    /// whole catalyst run and tells the user to top up Enzyme credits, which is
    /// wrong in every case and doubly wrong under BYOK / `--use-env-llm`.
    ///
    /// Expected result: RED — the current implementation is
    /// `err.contains("402") || err.contains("Insufficient credits")`.
    #[test]
    fn incidental_402_substrings_are_not_credit_exhaustion() {
        let incidental: &[(&str, &str)] = &[
            (
                "google quota value",
                r#"LLM API returned status 429 Too Many Requests: {"error":{"code":429,"message":"Resource exhausted: quota_limit_value 4029 for quota metric 'generate_requests_per_model_per_day'","status":"RESOURCE_EXHAUSTED"}}"#,
            ),
            (
                "request id",
                r#"LLM API returned status 500 Internal Server Error: {"error":{"message":"internal error","request_id":"req_c4025b7f91"}}"#,
            ),
            (
                "token count",
                r#"LLM API returned status 400 Bad Request: {"error":{"message":"This model's maximum context length is 8192 tokens, however you requested 9402 tokens"}}"#,
            ),
            (
                "latency / timing figure",
                "LLM API returned status 504 Gateway Timeout: upstream did not respond within 4028ms",
            ),
            (
                "model name containing 402",
                r#"LLM API returned status 404 Not Found: {"error":{"message":"model 'llama-402b-instruct' does not exist"}}"#,
            ),
            (
                "rate limit reset epoch",
                "LLM API returned status 429 Too Many Requests: rate limit exceeded, retry after 1750014020",
            ),
        ];

        let mut misclassified = Vec::new();
        for (label, err) in incidental {
            if is_credits_exhausted_error(err) {
                misclassified.push(format!("  [{label}] {err}"));
            }
        }
        assert!(
            misclassified.is_empty(),
            "is_credits_exhausted_error misclassified {} provider error(s) that merely \
             contain the digits \"402\" incidentally. Each of these aborts the entire \
             catalyst run with a hosted-billing message:\n{}",
            misclassified.len(),
            misclassified.join("\n"),
        );
    }

    /// The companion positive cases: genuine payment-required errors must still
    /// be detected, so a fix cannot simply make the predicate always false.
    #[test]
    fn genuine_payment_required_errors_are_credit_exhaustion() {
        let genuine: &[(&str, &str)] = &[
            (
                "http 402 status line",
                r#"LLM API returned status 402 Payment Required: {"error":{"message":"Insufficient credits"}}"#,
            ),
            (
                "openrouter insufficient credits",
                r#"LLM API returned status 402 Payment Required: {"error":{"code":402,"message":"Insufficient credits. Add more using https://openrouter.ai/credits"}}"#,
            ),
            (
                "enzyme hosted credit gate",
                "Insufficient credits for this operation",
            ),
        ];

        for (label, err) in genuine {
            assert!(
                is_credits_exhausted_error(err),
                "expected a genuine payment-required error to be detected as credit \
                 exhaustion, but [{label}] was not: {err}"
            );
        }
    }
}
