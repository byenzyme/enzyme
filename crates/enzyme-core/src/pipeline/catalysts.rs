use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};
use chrono::Utc;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::{CatalystEraAudit, Database, FileChunk};
use crate::embedding::{EmbeddingService, l2_normalize};
use crate::llm::{
    CatalystProfile, DEFAULT_FORMAT_KEY, DEFAULT_MODEL, DEFAULT_PROFILE_KEY, EraContext,
    GenerationParams, LlmApiError, LlmClient, Message, ParsedCatalyst, build_local_user_prompt,
    build_system_prompt, build_user_prompt, catalyst_entries_json_schema, catalyst_json_schema,
    get_profile, hardware, parse_catalyst_response, parse_partial_catalyst_entries,
    streaming_entries_instruction,
};
use crate::models::{
    CATALYST_TEXT_FORMAT_VERSION, Catalyst, CatalystTextEra, CatalystTextHeader, Embedding, Entity,
    EntityOccurrence, EntityType, EvidenceAnchor, INLINE_CONTEXT_MAX_WORDS, parse_catalyst_text,
    render_catalyst_text,
};
use crate::pipeline::eras::{
    Era, allocate_catalysts_to_eras, assign_occurrences_to_eras, cluster_eras_periodic,
    sample_contexts_by_era,
};

/// Default context char budget for local models. Leaves room for the
/// simplified system prompt (~400 tokens) and output (~512 tokens) inside the
/// shipping checkpoint's 8K window — see `llm::hardware`.
const LOCAL_DEFAULT_MAX_CONTEXT_CHARS: usize = hardware::LOCAL_MAX_CONTEXT_CHARS;
/// Default per-entity context char budget for cloud models when unset
/// (`max_context_chars == None`). ~15K tokens of context per entity request —
/// generous for a focused catalyst prompt while keeping a single high-frequency
/// or long-note entity's job far under the model context window. A guardrail,
/// not a quality knob; mirrors `EnzymeConfig::effective_max_embedding_files`'s
/// default-on-with-escape-hatch shape (`Some(0)` = explicit unlimited).
const DEFAULT_MAX_CONTEXT_CHARS: usize = 60000;
const CHUNK_MMR_TOP_K: usize = 50;
const CHUNK_MMR_LAMBDA: f32 = 0.5;
pub const DEFAULT_MIN_EVIDENCE_DOCUMENTS: usize = 4;
pub const DEFAULT_MIN_EVIDENCE_TOKENS: usize = 128;
const EVIDENCE_QUOTE_MAX_CHARS: usize = 320;

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
- Clinical, diagnostic, or personality claims; interpretations must remain tentative

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
4. For anchorIds: cite at least one supplied evidence label for every catalyst. Never invent labels.

Always respond with valid JSON that includes the catalysts, topic names, and anchor ids.";

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

/// Resolve the three-valued per-entity context budget to a concrete char count
/// for the selection helpers (where `0` means unlimited).
///
/// - `None` → the built-in default guardrail (`LOCAL_DEFAULT_MAX_CONTEXT_CHARS`
///   for local models, `DEFAULT_MAX_CONTEXT_CHARS` for cloud),
/// - `Some(0)` → `0` (explicit unlimited — caller opted out of the guardrail),
/// - `Some(n)` → `n`.
///
/// Local models are additionally clamped to
/// [`hardware::LOCAL_MAX_CONTEXT_CHARS`], which is what the shipping 8k-token
/// checkpoint can hold alongside its 512-token completion. `Some(0)` (explicit
/// unlimited) is clamped too — the alternative is a llama.cpp context overflow.
fn resolve_context_budget(configured: Option<usize>, is_local: bool) -> usize {
    let budget = match configured {
        None => {
            if is_local {
                LOCAL_DEFAULT_MAX_CONTEXT_CHARS
            } else {
                DEFAULT_MAX_CONTEXT_CHARS
            }
        }
        Some(n) => n,
    };

    if is_local {
        let capped = if budget == 0 {
            hardware::LOCAL_MAX_CONTEXT_CHARS
        } else {
            budget.min(hardware::LOCAL_MAX_CONTEXT_CHARS)
        };
        if capped != budget {
            warn!(
                requested = budget,
                capped, "context budget clamped to the local model's window"
            );
        }
        return capped;
    }

    budget
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
    source_ref: String,
    occurrence_ids: Vec<i64>,
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
    evidence_anchors: Vec<EvidenceAnchor>,
}

fn constrain_labelled_context(mut context: SelectedContext, max_chars: usize) -> SelectedContext {
    if max_chars == 0 {
        return context;
    }
    if let Some(era_context) = context.era_context.as_mut() {
        let total = era_context
            .samples
            .iter()
            .map(|(_, s)| s.len())
            .sum::<usize>();
        if total > max_chars {
            let per_sample = max_chars / era_context.samples.len().max(1);
            for (_, sample) in &mut era_context.samples {
                *sample = lasso_truncate(sample, per_sample);
            }
        }
    } else {
        context.flat_samples = apply_context_budget(context.flat_samples, usize::MAX, max_chars);
    }
    context
}

fn evidence_quote(content: &str) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    lasso_truncate(&compact, EVIDENCE_QUOTE_MAX_CHARS)
}

fn format_anchor(anchor: &EvidenceAnchor, content: &str) -> String {
    format!(
        "[{}] source_ref={} occurrence_ids={} timestamp_ms={}\n{}",
        anchor.anchor_id,
        anchor.source_ref,
        serde_json::to_string(&anchor.occurrence_ids).unwrap_or_else(|_| "[]".into()),
        anchor.timestamp_ms,
        content
    )
}

fn occurrence_anchor(
    anchor_id: String,
    occurrence: &EntityOccurrence,
    content: &str,
) -> EvidenceAnchor {
    EvidenceAnchor {
        anchor_id,
        source_ref: occurrence.file_path.clone(),
        occurrence_ids: occurrence.id.into_iter().collect(),
        timestamp_ms: occurrence_timestamp(occurrence),
        chunk_index: None,
        quote: evidence_quote(content),
    }
}

/// Extract only one participant's entries from a structured shared-log chunk.
/// Ordinary Markdown occurrence contexts are already entity-local spans and
/// pass through unchanged.
fn participant_span(context: &str, entity_name: &str) -> Option<String> {
    let mut blocks = Vec::<(String, Vec<String>)>::new();
    let mut heading: Option<String> = None;
    let mut body = Vec::new();
    for line in context.lines() {
        if line.starts_with("## ") {
            if let Some(previous) = heading.replace(line.to_string()) {
                blocks.push((previous, std::mem::take(&mut body)));
            }
        } else if heading.is_some() {
            body.push(line.to_string());
        }
    }
    if let Some(previous) = heading {
        blocks.push((previous, body));
    }

    if blocks.is_empty() {
        return (!context.trim().is_empty()).then(|| context.to_string());
    }
    let selected = blocks
        .into_iter()
        .filter(|(heading, _)| {
            heading
                .split_once(" — ")
                .map(|(_, participants)| {
                    participants
                        .split(',')
                        .any(|name| name.trim().eq_ignore_ascii_case(entity_name))
                })
                .unwrap_or(false)
        })
        .map(|(heading, body)| {
            let body = body.join("\n").trim().to_string();
            if body.is_empty() {
                heading
            } else {
                format!("{heading}\n\n{body}")
            }
        })
        .collect::<Vec<_>>();
    (!selected.is_empty()).then(|| selected.join("\n\n"))
}

fn substantive_token_count(text: &str) -> usize {
    const BOILERPLATE: &[&str] = &[
        "unsubscribe",
        "newsletter",
        "forwarded",
        "copyright",
        "privacy",
        "preferences",
        "view",
        "browser",
        "subscribe",
        "advertisement",
    ];
    text.split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|token| {
            token.len() >= 3 && !token.starts_with("http") && !BOILERPLATE.contains(&token.as_str())
        })
        .count()
}

fn era_audits(
    eras: &[Era],
    anchors: &[EvidenceAnchor],
    catalyst_budget: usize,
    min_documents: usize,
    min_tokens: usize,
) -> Vec<EraAuditRecord> {
    let allocations = allocate_catalysts_to_eras(eras, catalyst_budget, 1);
    let groups = if eras.len() > 1 {
        eras.iter()
            .enumerate()
            .map(|(idx, era)| {
                (
                    era.label.clone(),
                    era.start.clone(),
                    era.end.clone(),
                    Some(idx),
                )
            })
            .collect::<Vec<_>>()
    } else {
        let (label, start, end) = eras.first().map_or_else(
            || {
                let mut months = anchors
                    .iter()
                    .filter_map(|anchor| month_key_from_timestamp(anchor.timestamp_ms))
                    .collect::<Vec<_>>();
                months.sort();
                let start = months.first().cloned().unwrap_or_default();
                let end = months.last().cloned().unwrap_or_else(|| start.clone());
                let label = match (start.is_empty(), start == end) {
                    (true, _) => "undated".to_string(),
                    (false, true) => start.clone(),
                    (false, false) => format!("{start} to {end}"),
                };
                (label, start, end)
            },
            |era| (era.label.clone(), era.start.clone(), era.end.clone()),
        );
        vec![(label, start, end, None)]
    };

    groups
        .into_iter()
        .map(|(label, start, end, era_idx)| {
            let evidence = anchors.iter().filter(|anchor| {
                era_idx.is_none()
                    || era_index_for_timestamp(eras, anchor.timestamp_ms) == era_idx.unwrap()
            });
            let evidence = evidence.collect::<Vec<_>>();
            let distinct_documents = evidence
                .iter()
                .map(|anchor| anchor.source_ref.as_str())
                .collect::<HashSet<_>>()
                .len();
            let substantive_tokens = evidence
                .iter()
                .map(|anchor| substantive_token_count(&anchor.quote))
                .sum();
            let reason = if distinct_documents < min_documents {
                Some(format!(
                    "thin evidence: {distinct_documents} distinct documents; minimum is {min_documents}"
                ))
            } else if substantive_tokens < min_tokens {
                Some(format!(
                    "thin evidence: {substantive_tokens} substantive tokens; minimum is {min_tokens}"
                ))
            } else {
                None
            };
            EraAuditRecord {
                label,
                start,
                end,
                occurrence_count: era_idx
                    .and_then(|idx| eras.get(idx))
                    .map_or(evidence.len(), |era| era.count.max(0) as usize),
                catalyst_budget: era_idx
                    .and_then(|idx| allocations.get(idx).copied())
                    .unwrap_or(catalyst_budget),
                distinct_documents,
                substantive_tokens,
                status: if reason.is_some() { "skipped" } else { "eligible" },
                reason,
            }
        })
        .collect()
}

fn validate_grounded_catalysts(
    catalysts: Vec<ParsedCatalyst>,
    anchors: &[EvidenceAnchor],
    required_subject: Option<&str>,
) -> (Vec<ParsedCatalyst>, usize) {
    let valid_ids = anchors
        .iter()
        .map(|anchor| anchor.anchor_id.as_str())
        .collect::<HashSet<_>>();
    let before = catalysts.len();
    let kept = catalysts
        .into_iter()
        .filter(|catalyst| {
            !catalyst.anchor_ids.is_empty()
                && catalyst
                    .anchor_ids
                    .iter()
                    .all(|id| valid_ids.contains(id.as_str()))
                && required_subject
                    .is_none_or(|subject| catalyst_names_subject(&catalyst.text, subject))
        })
        .collect::<Vec<_>>();
    let rejected = before - kept.len();
    (kept, rejected)
}

fn catalyst_names_subject(text: &str, subject: &str) -> bool {
    let text = text.to_lowercase();
    let subject = subject.trim().to_lowercase();
    if subject.is_empty() {
        return false;
    }
    const SUBJECT_OPENERS: &[&str] = &[
        "",
        "could",
        "does",
        "has",
        "how could",
        "how did",
        "how does",
        "how is",
        "how might",
        "is",
        "might",
        "what are",
        "what could",
        "what did",
        "what does",
        "what is",
        "what might",
        "what would",
        "when does",
        "where does",
        "which of",
        "why does",
        "would",
    ];
    text.match_indices(&subject).any(|(start, _)| {
        let end = start + subject.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let prefix = text[..start]
            .trim_matches(|ch: char| !ch.is_alphanumeric() && !ch.is_whitespace())
            .trim();
        SUBJECT_OPENERS.contains(&prefix)
            && before.is_none_or(|ch| !ch.is_alphanumeric())
            && after.is_none_or(|ch| !ch.is_alphanumeric())
    })
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
    let raw_era_context = if eras.len() > 1 {
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

    let mut evidence_anchors = Vec::new();
    let mut used_occurrences = HashSet::new();
    let mut label_sample = |content: &str| {
        let occurrence = occurrences
            .iter()
            .enumerate()
            .find(|(idx, occ)| {
                !used_occurrences.contains(idx)
                    && occ.context.as_deref().is_some_and(|original| {
                        original == content
                            || content
                                .split("\n[…]\n")
                                .next()
                                .is_some_and(|head| original.starts_with(head))
                    })
            })
            .or_else(|| {
                occurrences
                    .iter()
                    .enumerate()
                    .find(|(idx, occ)| !used_occurrences.contains(idx) && occ.context.is_some())
            });
        let Some((occurrence_idx, occurrence)) = occurrence else {
            return content.to_string();
        };
        used_occurrences.insert(occurrence_idx);
        let anchor = occurrence_anchor(
            format!("E{}", evidence_anchors.len() + 1),
            occurrence,
            content,
        );
        let labelled = format_anchor(&anchor, content);
        evidence_anchors.push(anchor);
        labelled
    };

    let era_context = raw_era_context.map(|mut context| {
        context.samples = context
            .samples
            .into_iter()
            .map(|(era, sample)| (era, label_sample(&sample)))
            .collect();
        context
    });
    let flat_samples = if era_context.is_some() {
        Vec::new()
    } else {
        flat_samples
            .into_iter()
            .map(|sample| label_sample(&sample))
            .collect()
    };

    constrain_labelled_context(
        SelectedContext {
            era_context,
            flat_samples,
            evidence_anchors,
        },
        max_context_chars,
    )
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
    // Ordered: MMR ties are broken by iteration order, and a HashSet's order
    // varies per process. Chunk selection has to be reproducible run over run,
    // or the per-entity context fingerprint would churn and defeat the
    // incremental skip.
    let mut candidate_pool: BTreeSet<usize> = ranked
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
    let evidence_anchors = selected
        .iter()
        .enumerate()
        .map(|(position, idx)| {
            let candidate = &candidates[*idx];
            EvidenceAnchor {
                anchor_id: format!("E{}", position + 1),
                source_ref: candidate.source_ref.clone(),
                occurrence_ids: candidate.occurrence_ids.clone(),
                timestamp_ms: candidate.timestamp,
                chunk_index: Some(candidate.chunk_index),
                quote: evidence_quote(&candidate.content),
            }
        })
        .collect::<Vec<_>>();
    if eras.len() > 1 {
        let samples = selected
            .iter()
            .enumerate()
            .map(|(position, idx)| {
                let candidate = &candidates[*idx];
                (
                    candidate.era_index,
                    format_anchor(&evidence_anchors[position], &candidate.content),
                )
            })
            .collect::<Vec<_>>();
        SelectedContext {
            era_context: Some(EraContext {
                eras: eras.to_vec(),
                era_allocations: allocate_catalysts_to_eras(eras, num_catalysts, 1),
                samples,
            }),
            flat_samples: Vec::new(),
            evidence_anchors,
        }
    } else {
        SelectedContext {
            era_context: None,
            flat_samples: selected
                .iter()
                .enumerate()
                .map(|(position, idx)| {
                    format_anchor(&evidence_anchors[position], &candidates[*idx].content)
                })
                .collect(),
            evidence_anchors,
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

    constrain_labelled_context(
        finalize_chunk_context(eras, num_catalysts, candidates, &selected),
        max_context_chars,
    )
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

    constrain_labelled_context(
        finalize_chunk_context(eras, num_catalysts, candidates, &selected),
        max_context_chars,
    )
}

fn build_chunk_candidates(
    chunks: Vec<FileChunk>,
    occurrences: &[EntityOccurrence],
    eras: &[Era],
    chunk_vectors: Option<&HashMap<(String, u32), Vec<f32>>>,
    query_vector: Option<&[f32]>,
) -> Vec<ChunkCandidate> {
    let mut file_timestamps: HashMap<&str, i64> = HashMap::new();
    let mut file_occurrence_ids: HashMap<&str, Vec<i64>> = HashMap::new();
    for occ in occurrences {
        let ts = occurrence_timestamp(occ);
        file_timestamps
            .entry(occ.file_path.as_str())
            .and_modify(|existing| *existing = (*existing).max(ts))
            .or_insert(ts);
        if let Some(id) = occ.id {
            file_occurrence_ids
                .entry(occ.file_path.as_str())
                .or_default()
                .push(id);
        }
    }

    chunks
        .into_iter()
        .filter_map(|chunk| {
            // A chunk cut from a dated append-log entry carries that entry's
            // date; without it, fall back to the file-level timestamp. This is
            // what lets one long journal spread across eras instead of
            // collapsing onto its single document date.
            let file_timestamp = *file_timestamps.get(chunk.file_path.as_str())?;
            let timestamp = chunk.entry_date.unwrap_or(file_timestamp);
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
                source_ref: chunk.file_path.clone(),
                occurrence_ids: file_occurrence_ids
                    .get(chunk.file_path.as_str())
                    .cloned()
                    .unwrap_or_default(),
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

/// Bumped when the meaning of the fingerprint changes (new inputs folded in,
/// different canonicalization), so every entity is treated as dirty once after
/// an upgrade rather than silently keeping catalysts the new inputs disagree
/// with.
const CONTEXT_HASH_VERSION: &str = "catalyst-context-v4-canonical-receipts";

/// Fold one labelled, length-prefixed field into the fingerprint. The label and
/// explicit length keep concatenation unambiguous, so no combination of field
/// values can forge another combination's digest.
fn hash_field(hasher: &mut Sha256, label: &str, value: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(value.len().to_le_bytes());
    hasher.update(b"\x1f");
    hasher.update(value);
    hasher.update(b"\x1e");
}

/// Fingerprint of everything that determines one entity's generated catalysts.
///
/// The resolved prompts already encode the ordered context chunks/eras, the
/// entity name, its profile, the output format, and the requested catalyst
/// count — they are literally the bytes sent to the model — so hashing them
/// covers the context. Model identity and the local/hosted split are folded in
/// on top, so switching engines or upgrading a checkpoint invalidates every
/// entity.
fn context_hash(
    model: &str,
    is_local: bool,
    entity_name: &str,
    entity_type: &str,
    num_catalysts: usize,
    system_prompt: &str,
    user_prompt: &str,
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "version", CONTEXT_HASH_VERSION.as_bytes());
    hash_field(&mut hasher, "model", model.as_bytes());
    hash_field(&mut hasher, "local", &[u8::from(is_local)]);
    hash_field(&mut hasher, "entity", entity_name.as_bytes());
    hash_field(&mut hasher, "entity_type", entity_type.as_bytes());
    hash_field(&mut hasher, "num_catalysts", &num_catalysts.to_le_bytes());
    hash_field(&mut hasher, "system_prompt", system_prompt.as_bytes());
    hash_field(&mut hasher, "user_prompt", user_prompt.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub struct CatalystPipeline<'a> {
    db: &'a Database,
    llm: &'a LlmClient,
    embedding: Option<&'a EmbeddingService>,
}

pub struct CatalystStats {
    pub entities_processed: usize,
    /// Entities skipped because their stored context fingerprint still matches
    /// and their catalysts are already in the database.
    pub entities_unchanged: usize,
    pub catalysts_generated: usize,
    pub errors: Vec<(String, String)>,
    pub duration_ms: u64,
    /// Number of entities that failed due to rate limiting (429 / timeout).
    pub rate_limited: usize,
    /// Entities suppressed before inference because their evidence was thin.
    pub entities_below_threshold: usize,
    /// Parsed hypotheses rejected because they did not cite supplied evidence.
    pub hypotheses_rejected: usize,
    /// (entity name, entity type) intentionally suppressed for thin evidence.
    pub suppressed_entities: Vec<(String, String)>,
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
    /// When a budget applies, samples are truncated (head+tail with lasso
    /// compression) to fit within it. Three-valued, mirroring
    /// `EnzymeConfig::effective_max_embedding_files`:
    /// - `None` → use the built-in default (`DEFAULT_MAX_CONTEXT_CHARS` for
    ///   cloud, `LOCAL_DEFAULT_MAX_CONTEXT_CHARS` for local models),
    /// - `Some(0)` → explicit unlimited (no truncation),
    /// - `Some(n)` → exactly `n` chars.
    pub max_context_chars: Option<usize>,
    /// Maximum number of context samples per entity (default: 20).
    pub max_samples: usize,
    /// LLM concurrency override. If set, overrides the auto-detected value.
    pub concurrency: Option<usize>,
    /// Stream self-contained catalyst records and write complete records before
    /// the full entity response finishes.
    pub stream_partial_catalysts: bool,
    /// Regenerate every entity even when its stored context fingerprint still
    /// matches (`--force`).
    pub force_regenerate: bool,
    /// Minimum distinct source documents required per entity-era.
    pub min_evidence_documents: usize,
    /// Minimum substantive (non-boilerplate) tokens required per entity-era.
    pub min_evidence_tokens: usize,
    /// Durable epoch associated with this run, when an orchestrator owns one.
    pub epoch_id: Option<&'a str>,
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
            max_context_chars: None,
            max_samples: 20,
            concurrency: None,
            stream_partial_catalysts: false,
            force_regenerate: false,
            min_evidence_documents: DEFAULT_MIN_EVIDENCE_DOCUMENTS,
            min_evidence_tokens: DEFAULT_MIN_EVIDENCE_TOKENS,
            epoch_id: None,
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
    /// Fingerprint of this job's inputs; stored after the catalysts commit.
    context_hash: String,
    evidence_anchors: Vec<EvidenceAnchor>,
    era_audits: Vec<EraAuditRecord>,
    participant_scoped: bool,
}

#[derive(Clone)]
struct EraAuditRecord {
    label: String,
    start: String,
    end: String,
    occurrence_count: usize,
    catalyst_budget: usize,
    distinct_documents: usize,
    substantive_tokens: usize,
    status: &'static str,
    reason: Option<String>,
}

struct SkippedEntity {
    entity_name: String,
    entity_type: String,
    era_audits: Vec<EraAuditRecord>,
    reason_code: &'static str,
}

/// Result of a single entity's catalyst generation.
struct EntityResult {
    entity_name: String,
    entity_type: String,
    context_hash: String,
    catalysts: Option<Vec<ParsedCatalyst>>,
    error: Option<String>,
    evidence_anchors: Vec<EvidenceAnchor>,
    era_audits: Vec<EraAuditRecord>,
    participant_scoped: bool,
    rejected: usize,
}

struct PartialCatalystEvent {
    entity_name: String,
    entity_type: String,
    index: usize,
    catalyst: ParsedCatalyst,
    evidence_anchors: Vec<EvidenceAnchor>,
    era_audits: Vec<EraAuditRecord>,
    participant_scoped: bool,
}

fn catalyst_era(
    parsed: &ParsedCatalyst,
    anchors: &[EvidenceAnchor],
    audits: &[EraAuditRecord],
) -> String {
    if let Some(label) = parsed
        .era_label
        .as_deref()
        .filter(|label| !label.is_empty() && audits.iter().any(|audit| audit.label == *label))
    {
        return label.to_string();
    }

    let timestamp_ms = anchors
        .iter()
        .find(|anchor| parsed.anchor_ids.contains(&anchor.anchor_id))
        .map(|anchor| anchor.timestamp_ms);
    let month = timestamp_ms.and_then(month_key_from_timestamp);
    if let Some(month) = month {
        if let Some(audit) = audits
            .iter()
            .find(|audit| !audit.label.is_empty() && month >= audit.start && month <= audit.end)
        {
            return audit.label.clone();
        }
        return month;
    }

    audits
        .iter()
        .find(|audit| !audit.label.is_empty())
        .map(|audit| audit.label.clone())
        .unwrap_or_else(|| "undated".to_string())
}

fn catalyst_from_parsed(
    entity_name: &str,
    entity_type: &str,
    parsed: ParsedCatalyst,
    available_anchors: &[EvidenceAnchor],
    era_audits: &[EraAuditRecord],
    generation_run_id: &str,
    epoch_id: Option<&str>,
    participant_scoped: bool,
    provisional: bool,
) -> Catalyst {
    let era = catalyst_era(&parsed, available_anchors, era_audits);
    let generated_at = Utc::now();
    let mut metadata = if provisional {
        serde_json::json!({"era": era, "generation_phase": "stream"})
    } else {
        serde_json::json!({"era": era})
    };
    // Legacy/cache fields only. Canonical provenance lives in `text` and every
    // machine-facing receipt/era row is rebuilt by parsing it.
    metadata["entity_type"] = serde_json::json!(entity_type);
    let cited = available_anchors
        .iter()
        .filter(|anchor| parsed.anchor_ids.contains(&anchor.anchor_id))
        .cloned()
        .collect::<Vec<_>>();
    let header = CatalystTextHeader {
        format_version: CATALYST_TEXT_FORMAT_VERSION,
        entity_id: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        era,
        generation_run_id: generation_run_id.to_string(),
        epoch_id: epoch_id.map(str::to_string),
        generated_at_ms: generated_at.timestamp_millis(),
        participant_scoped,
        eras: era_audits
            .iter()
            .map(|audit| CatalystTextEra {
                label: audit.label.clone(),
                start_month: audit.start.clone(),
                end_month: audit.end.clone(),
                distinct_documents: audit.distinct_documents,
                substantive_tokens: audit.substantive_tokens,
                occurrence_count: audit.occurrence_count,
                catalyst_budget: audit.catalyst_budget,
                status: audit.status.to_string(),
                reason: audit.reason.clone(),
            })
            .collect(),
    };
    let text = render_catalyst_text(&header, &parsed.text, &cited)
        .expect("grounded catalyst must render canonical provenance text");
    let canonical =
        parse_catalyst_text(&text).expect("rendered catalyst must pass the text grounding guard");
    assert!(!canonical.receipts.is_empty(), "ungrounded catalyst text");
    Catalyst {
        id: Uuid::new_v4().to_string(),
        text,
        entity: entity_name.to_string(),
        context: parsed.topic,
        generated_at,
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

    fn persist_audits(
        &self,
        run_id: &str,
        epoch_id: Option<&str>,
        entity_name: &str,
        entity_type: &str,
        audits: &[EraAuditRecord],
        skip_reason_code: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        for era in audits {
            let audit = CatalystEraAudit {
                generation_run_id: run_id,
                epoch_id,
                entity: entity_name,
                entity_type,
                era_label: &era.label,
                start_month: &era.start,
                end_month: &era.end,
                occurrence_count: era.occurrence_count,
                catalyst_budget: era.catalyst_budget,
                distinct_documents: era.distinct_documents,
                substantive_tokens: era.substantive_tokens,
                status: era.status,
                reason: era.reason.as_deref(),
                created_at_ms: now,
            };
            if let Some(code) = skip_reason_code
                && era.reason.is_some()
            {
                self.db.record_catalyst_generation_skip(&audit, code)?;
            }
        }
        Ok(())
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

    /// Fill in NULL occurrence contexts from the source documents.
    ///
    /// Only for the chunkless occurrence-context fallback: the inline-centroid
    /// path must NOT be hydrated, because its `INLINE_CONTEXT_MAX_WORDS`
    /// filter deliberately treats stored-NULL (formerly oversized) contexts as
    /// absent.
    fn hydrate_occurrence_contexts(
        &self,
        occurrences: &[EntityOccurrence],
    ) -> Result<Vec<EntityOccurrence>> {
        let missing: Vec<String> = occurrences
            .iter()
            .filter(|o| o.context.is_none())
            .map(|o| o.file_path.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if missing.is_empty() {
            return Ok(occurrences.to_vec());
        }
        let contents = self.db.get_doc_contents_for_paths(&missing)?;
        Ok(occurrences
            .iter()
            .map(|occ| {
                let mut occ = occ.clone();
                if occ.context.is_none() {
                    if let Some(body) = contents.get(&occ.file_path) {
                        occ.context = Some(crate::document::strip_frontmatter(body).to_string());
                    }
                }
                occ
            })
            .collect())
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
            // A log is a timeline: like a folder, select time-first with
            // guaranteed per-era representation (its chunks carry per-entry
            // dates), and don't require embeddings.
            EntityType::Folder | EntityType::AppendLog => select_folder_chunks(
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
    ) -> Result<(String, Vec<EntityJob>, Vec<SkippedEntity>)> {
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

        // Resolve the three-valued context budget to a concrete char count for
        // the selection helpers (which treat `0` as unlimited). When unset,
        // default to a model-appropriate guardrail so a single high-frequency
        // or long-note entity can't blow past the model context window.
        let max_context_chars = resolve_context_budget(config.max_context_chars, is_local);
        if config.max_context_chars.is_none() {
            info!(
                budget = max_context_chars,
                is_local, "auto-setting default context budget"
            );
        }

        let max_samples = if is_local && config.max_samples > 10 {
            10 // Cap samples for local models to keep prompts manageable
        } else {
            config.max_samples
        };

        let default_profile = get_profile(config.default_profile)
            .unwrap_or_else(|| get_profile(DEFAULT_PROFILE_KEY).unwrap());
        let chunk_preload = self.preload_chunk_selection(entities);

        let mut jobs: Vec<EntityJob> = Vec::new();
        let mut skipped: Vec<SkippedEntity> = Vec::new();

        for (entity_idx, entity) in entities.iter().enumerate() {
            let entity_id = entity.id.unwrap_or(0);
            let entity_name = &entity.name;

            let occurrences = self.db.get_entity_occurrences(entity_id)?;
            let participant_scoped = entity.entity_type == EntityType::Link
                && self.db.link_entity_has_shared_documents(entity_id)?;

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
            let selected_context = if participant_scoped {
                // Shared conversations produce one document/chunk stream for
                // all speakers, but SQLite-source occurrences retain the span
                // for the participant that created it. Keep link generation on
                // those own spans; hydrating NULLs here would reintroduce the
                // whole shared thread and erase the participant boundary.
                let own_spans = occurrences
                    .iter()
                    .filter_map(|occurrence| {
                        let mut occurrence = occurrence.clone();
                        occurrence.context =
                            participant_span(occurrence.context.as_deref()?, entity_name);
                        occurrence.context.as_ref()?;
                        Some(occurrence)
                    })
                    .collect::<Vec<_>>();
                build_occurrence_context(
                    &eras,
                    &own_spans,
                    num_catalysts,
                    max_samples,
                    max_context_chars,
                )
            } else {
                match self.build_chunk_context(
                    entity,
                    &occurrences,
                    &eras,
                    num_catalysts,
                    max_samples,
                    max_context_chars,
                    chunk_preload.as_ref(),
                    entity_idx,
                )? {
                    Some(ctx) => ctx,
                    None => {
                        // Chunkless fallback. Occurrence rows carry NULL contexts
                        // for long notes (their bodies are no longer duplicated
                        // into entity_occurrences), so rehydrate from `docs`
                        // before sampling — otherwise long-note entities would be
                        // skipped as having no context at all.
                        let occurrences = self.hydrate_occurrence_contexts(&occurrences)?;
                        build_occurrence_context(
                            &eras,
                            &occurrences,
                            num_catalysts,
                            max_samples,
                            max_context_chars,
                        )
                    }
                }
            };
            let era_context = selected_context.era_context;
            let flat_samples = selected_context.flat_samples;
            let context_samples = &flat_samples;

            if flat_samples.is_empty() && era_context.is_none() {
                debug!(
                    entity_name = entity_name.as_str(),
                    "no context samples, skipping"
                );
                skipped.push(SkippedEntity {
                    entity_name: entity_name.clone(),
                    entity_type: entity.entity_type.as_str().to_string(),
                    era_audits: vec![EraAuditRecord {
                        label: String::new(),
                        start: String::new(),
                        end: String::new(),
                        distinct_documents: 0,
                        substantive_tokens: 0,
                        occurrence_count: 0,
                        catalyst_budget: num_catalysts,
                        status: "skipped",
                        reason: Some("no context samples were available".to_string()),
                    }],
                    reason_code: "no_context",
                });
                continue;
            }

            let audits = era_audits(
                &eras,
                &selected_context.evidence_anchors,
                num_catalysts,
                config.min_evidence_documents,
                config.min_evidence_tokens,
            );
            if audits.iter().any(|audit| audit.reason.is_some()) {
                debug!(
                    entity_name = entity_name.as_str(),
                    "thin evidence, skipping"
                );
                skipped.push(SkippedEntity {
                    entity_name: entity_name.clone(),
                    entity_type: entity.entity_type.as_str().to_string(),
                    era_audits: audits,
                    reason_code: "thin_evidence",
                });
                continue;
            }

            let mut user_prompt = if is_local {
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
                    era_context.as_ref(),
                )
            };
            if participant_scoped {
                user_prompt.push_str(&format!(
                    "\n\nPARTICIPANT SCOPE: These are {entity_name}'s own spans from shared conversations. Every catalyst must use {entity_name} as its explicit grammatical subject and must name {entity_name}. Start with {entity_name}, or put {entity_name} immediately after the question opener (for example, ‘What does {entity_name}…?’). Other participants are context only, never the subject of a hypothesis."
                ));
            }

            debug!(
                entity_name = entity_name.as_str(),
                profile = profile.key,
                "built prompt for entity"
            );

            let entity_type = entity.entity_type.as_str().to_string();
            let context_hash = context_hash(
                config.model,
                is_local,
                entity_name,
                &entity_type,
                num_catalysts,
                &system_prompt,
                &user_prompt,
            );

            jobs.push(EntityJob {
                entity_name: entity_name.clone(),
                entity_type,
                user_prompt,
                context_hash,
                evidence_anchors: selected_context.evidence_anchors,
                era_audits: audits,
                participant_scoped,
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

    /// Split built jobs into the ones that must be regenerated and a count of
    /// the ones whose stored fingerprint still matches.
    ///
    /// Both conditions must hold to skip: the fingerprint matches *and* the
    /// entity still has catalysts. A hash without catalysts (rows pruned, a
    /// partially wiped database, a carried-forward epoch) is treated as dirty.
    fn partition_unchanged(
        &self,
        jobs: Vec<EntityJob>,
        force: bool,
    ) -> Result<(Vec<EntityJob>, usize)> {
        if force {
            return Ok((jobs, 0));
        }
        let stored = self.db.get_catalyst_entity_hashes()?;
        if stored.is_empty() {
            return Ok((jobs, 0));
        }
        let counts = self.db.get_catalyst_counts_by_entity()?;

        let mut fresh = Vec::with_capacity(jobs.len());
        let mut unchanged = 0usize;
        for job in jobs {
            let key = (job.entity_name.clone(), job.entity_type.clone());
            let hash_matches = stored.get(&key).is_some_and(|h| *h == job.context_hash);
            // Legacy rows carry no `entity_type` in their metadata; they count
            // for the name, mirroring `delete_catalysts_for_entity`.
            let has_catalysts = counts.get(&key).copied().unwrap_or(0) > 0
                || counts
                    .get(&(job.entity_name.clone(), String::new()))
                    .copied()
                    .unwrap_or(0)
                    > 0;
            if hash_matches && has_catalysts {
                debug!(
                    entity_name = job.entity_name.as_str(),
                    "catalyst context unchanged, skipping"
                );
                unchanged += 1;
            } else {
                fresh.push(job);
            }
        }
        Ok((fresh, unchanged))
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
            entities_unchanged: 0,
            catalysts_generated: 0,
            errors: Vec::new(),
            duration_ms: 0,
            rate_limited: 0,
            entities_below_threshold: 0,
            hypotheses_rejected: 0,
            suppressed_entities: Vec::new(),
        };
        let generation_run_id = Uuid::new_v4().to_string();

        let total = entities.len();
        info!(total, "generating catalysts for selected entities");

        let (system_prompt, jobs, skipped) = self.build_jobs(entities, config)?;
        for job in &jobs {
            self.persist_audits(
                &generation_run_id,
                config.epoch_id,
                &job.entity_name,
                &job.entity_type,
                &job.era_audits,
                None,
            )?;
        }
        for skipped_entity in &skipped {
            self.persist_audits(
                &generation_run_id,
                config.epoch_id,
                &skipped_entity.entity_name,
                &skipped_entity.entity_type,
                &skipped_entity.era_audits,
                Some(skipped_entity.reason_code),
            )?;
            if skipped_entity.reason_code == "thin_evidence" {
                self.db.delete_catalysts_for_entity(
                    &skipped_entity.entity_name,
                    &skipped_entity.entity_type,
                )?;
                self.db.delete_catalyst_entity_hash(
                    &skipped_entity.entity_name,
                    &skipped_entity.entity_type,
                )?;
                stats.entities_below_threshold += 1;
                stats.suppressed_entities.push((
                    skipped_entity.entity_name.clone(),
                    skipped_entity.entity_type.clone(),
                ));
            }
        }

        // Incremental gate: an entity whose fingerprint still matches AND whose
        // catalysts are still in the database has nothing new to say, so it
        // never reaches the model. Hashing is DB-bound and cheap; the LLM call
        // it avoids is what makes a full local run take tens of minutes.
        let (jobs, unchanged) = self.partition_unchanged(jobs, config.force_regenerate)?;
        stats.entities_unchanged = unchanged;
        if unchanged > 0 {
            info!(
                unchanged,
                regenerating = jobs.len(),
                "skipping entities with unchanged catalyst context"
            );
        }

        // Report skipped entities
        for (i, skipped_entity) in skipped.iter().enumerate() {
            on_event(CatalystEvent::EntitySkipped {
                entity_name: skipped_entity.entity_name.clone(),
                completed: i + 1,
                total,
            });
        }

        // Phase 2: Fire LLM calls with bounded concurrency
        // Remote providers can handle moderate parallelism; local models are
        // bounded by the in-process engine's context pool, which reads the same
        // `hardware::local_concurrency()` value so the two never disagree. We
        // keep concurrency conservative to avoid 429s from upstream providers,
        // and use a generous timeout to accommodate retries with exponential
        // backoff.
        let local_model = self.llm.is_local();
        let concurrency = config.concurrency.unwrap_or(if local_model {
            hardware::local_concurrency()
        } else {
            8
        });
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
        let generation_run_id = Arc::new(generation_run_id);
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
                let context_hash = job.context_hash.clone();
                let evidence_anchors = job.evidence_anchors.clone();
                let era_audits = job.era_audits.clone();
                let participant_scoped = job.participant_scoped;

                if credits_exhausted.load(Ordering::SeqCst) {
                    return EntityResult {
                        entity_name,
                        entity_type,
                        context_hash,
                        catalysts: None,
                        error: Some("skipped: provider payment-required (402)".to_string()),
                        evidence_anchors,
                        era_audits,
                        participant_scoped,
                        rejected: 0,
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
                    let name_len = entity_name.len() as u64;
                    // Exponential backoff between attempts: 2s, 4s + jitter
                    // (0-500ms). No sleep after the final attempt.
                    let backoff = |attempt: u32| async move {
                        if attempt < max_attempts - 1 {
                            let base_ms = 2000u64 * 2u64.pow(attempt);
                            let jitter_ms = (name_len * 37) % 500;
                            tokio::time::sleep(std::time::Duration::from_millis(
                                base_ms + jitter_ms,
                            ))
                            .await;
                        }
                    };
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
                                                        entity_type: entity_type.clone(),
                                                        index,
                                                        catalyst,
                                                        evidence_anchors: evidence_anchors.clone(),
                                                        era_audits: era_audits.clone(),
                                                        participant_scoped,
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

                        let response_text = match result {
                            Ok(text) => text,
                            Err(e) => {
                                last_error = e.to_string();
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt, "LLM call failed: {}", e
                                );
                                // Don't retry on a provider 402 — backoff
                                // would just delay the inevitable failure.
                                if is_payment_required_error(&e) {
                                    credits_exhausted.store(true, Ordering::SeqCst);
                                    break;
                                }
                                backoff(attempt).await;
                                continue;
                            }
                        };

                        // Validate the response before accepting it. An empty
                        // catalyst array or an unparseable body is a
                        // content-level failure the model often recovers from on
                        // a retry — unlike a transport error or credit
                        // exhaustion. Retrying here, rather than treating the
                        // first bad response as terminal, is what keeps a single
                        // flaky entity from silently landing at zero catalysts.
                        match parse_catalyst_response(&response_text) {
                            Ok(parsed) if !parsed.catalysts.is_empty() => {
                                let (grounded, rejected) = validate_grounded_catalysts(
                                    parsed.catalysts,
                                    &evidence_anchors,
                                    participant_scoped.then_some(entity_name.as_str()),
                                );
                                if !grounded.is_empty() {
                                    return Ok((grounded, rejected));
                                }
                                last_error = format!(
                                    "all {} hypotheses failed evidence grounding or participant-subject validation",
                                    rejected
                                );
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt,
                                    rejected,
                                    "ungrounded or off-subject catalysts rejected; retrying"
                                );
                            }
                            Ok(_) => {
                                last_error = "LLM returned empty catalysts array".to_string();
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt, "empty catalysts array; retrying"
                                );
                            }
                            Err(e) => {
                                last_error = format!("parse error: {e}");
                                debug!(
                                    entity_name = entity_name.as_str(),
                                    attempt,
                                    response_text = %response_text,
                                    "failed to parse catalyst response; retrying: {}",
                                    e
                                );
                            }
                        }
                        backoff(attempt).await;
                    }
                    Err(anyhow::anyhow!(
                        "all {} retry attempts failed: {}",
                        max_attempts,
                        last_error
                    ))
                })
                .await;

                match result {
                    Ok(Ok((catalysts, rejected))) => EntityResult {
                        entity_name,
                        entity_type,
                        context_hash,
                        catalysts: Some(catalysts),
                        error: None,
                        evidence_anchors,
                        era_audits,
                        participant_scoped,
                        rejected,
                    },
                    Ok(Err(e)) => EntityResult {
                        entity_name,
                        entity_type,
                        context_hash,
                        catalysts: None,
                        error: Some(e.to_string()),
                        evidence_anchors,
                        era_audits,
                        participant_scoped,
                        rejected: 0,
                    },
                    Err(_) => {
                        debug!(
                            entity_name = entity_name.as_str(),
                            timeout_secs = entity_timeout_secs,
                            "entity timed out"
                        );
                        EntityResult {
                            entity_name,
                            entity_type,
                            context_hash,
                            catalysts: None,
                            error: Some(format!("timed out after {}s", entity_timeout_secs)),
                            evidence_anchors,
                            era_audits,
                            participant_scoped,
                            rejected: 0,
                        }
                    }
                }
            });
        }

        drop(partial_tx);

        // Phase 3: Collect results as tasks complete, while applying streamed
        // provisional catalysts on the DB writer thread.
        let mut completed = skipped.len() + stats.entities_unchanged;
        // Keyed by (name, type): entity names are not unique across types.
        let mut partial_started_entities: HashSet<(String, String)> = HashSet::new();
        let mut partial_inserted_counts: HashMap<(String, String), usize> = HashMap::new();

        while !join_set.is_empty() {
            let result = tokio::select! {
                partial = partial_rx.recv() => {
                    if let Some(partial) = partial {
                        let (mut grounded, _) = validate_grounded_catalysts(
                            vec![partial.catalyst],
                            &partial.evidence_anchors,
                            partial
                                .participant_scoped
                                .then_some(partial.entity_name.as_str()),
                        );
                        let Some(grounded) = grounded.pop() else {
                            continue;
                        };
                        let entity_key =
                            (partial.entity_name.clone(), partial.entity_type.clone());
                        let inserted = partial_inserted_counts
                            .entry(entity_key.clone())
                            .or_insert(0);
                        if partial.index < *inserted {
                            continue;
                        }
                        if partial_started_entities.insert(entity_key) {
                            self.db.delete_catalysts_for_entity(
                                &partial.entity_name,
                                &partial.entity_type,
                            )?;
                        }
                        let catalyst = catalyst_from_parsed(
                            &partial.entity_name,
                            &partial.entity_type,
                            grounded,
                            &partial.evidence_anchors,
                            &partial.era_audits,
                            generation_run_id.as_str(),
                            config.epoch_id,
                            partial.participant_scoped,
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
            // Short-circuit the rest of the run on a provider 402. Whatever
            // tripped the flag has already been collected as an EntityResult
            // error; aborting here prevents N more entities from each retrying
            // and accumulating identical 402s. The message is deliberately
            // mode-neutral: a completions 402 is a provider/pool condition,
            // not an Enzyme-account billing verdict, so no top-up language.
            if credits_exhausted.load(Ordering::SeqCst) {
                join_set.abort_all();
                stats.duration_ms = start.elapsed().as_millis() as u64;
                bail!(
                    "LLM provider returned 402 Payment Required; stopping catalyst \
                     generation. If you are using hosted Enzyme, capacity may be \
                     temporarily exhausted — try again later or run with --use-env-llm."
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
                        if entity_result.participant_scoped
                            && error.contains("participant-subject validation")
                        {
                            // Do not preserve a previous whole-thread catalyst
                            // set when the replacement model output cannot pass
                            // the participant boundary. A transport failure may
                            // keep last-known-good data; a proven off-subject
                            // result may not.
                            self.db.delete_catalysts_for_entity(
                                &entity_result.entity_name,
                                &entity_result.entity_type,
                            )?;
                            self.db.delete_catalyst_entity_hash(
                                &entity_result.entity_name,
                                &entity_result.entity_type,
                            )?;
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
                        stats.hypotheses_rejected += entity_result.rejected;
                        // Delete old catalysts and insert new ones
                        self.db.delete_catalysts_for_entity(
                            &entity_result.entity_name,
                            &entity_result.entity_type,
                        )?;

                        let catalyst_texts: Vec<String> =
                            parsed_catalysts.iter().map(|c| c.text.clone()).collect();
                        let topic_names: Vec<String> =
                            parsed_catalysts.iter().map(|c| c.topic.clone()).collect();

                        let catalysts: Vec<Catalyst> = parsed_catalysts
                            .into_iter()
                            .map(|pc| {
                                catalyst_from_parsed(
                                    &entity_result.entity_name,
                                    &entity_result.entity_type,
                                    pc,
                                    &entity_result.evidence_anchors,
                                    &entity_result.era_audits,
                                    generation_run_id.as_str(),
                                    config.epoch_id,
                                    entity_result.participant_scoped,
                                    false,
                                )
                            })
                            .collect();

                        let count = catalysts.len();
                        self.db.insert_catalysts(&catalysts)?;
                        // Written only now: the fingerprint asserts that the
                        // catalysts in the database came from these inputs, so
                        // it must never outrun the rows it describes.
                        self.db.set_catalyst_entity_hash(
                            &entity_result.entity_name,
                            &entity_result.entity_type,
                            &entity_result.context_hash,
                            Utc::now().timestamp_millis(),
                        )?;
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

/// Detect a payment-required condition from a completions call. Fires ONLY on
/// a typed [`LlmApiError`] with HTTP status 402 — never on body substrings
/// (request ids, token counts, and quota values legitimately contain the
/// digits "402", and phrases like "Insufficient credits" appear in non-402
/// proxy errors and even in generated text). Used to short-circuit the
/// catalyst pipeline when the upstream provider key is spent mid-run; it is
/// deliberately credential-mode-neutral because a completions 402 is a
/// provider condition in every mode — billing errors only come from billing
/// endpoints.
fn is_payment_required_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<LlmApiError>()
        .is_some_and(|api| api.status == 402)
}

/// Absolute lower bound on catalysts for any entity that has material at all.
/// A safety net for entities that are tiny relative to the largest one, not the
/// default outcome — most entities scale above this by document count.
const HARD_MIN_CATALYSTS: usize = 3;

/// Default catalyst budget for the reference entity when a vault does not set
/// `min_top_catalysts`. Every other entity scales down from here.
const DEFAULT_TOP_CATALYSTS: usize = 15;

/// Percentile of entity document counts used as the scaling reference. Entities
/// at or above this percentile all receive the full `top_target`; smaller
/// entities scale linearly toward the floor. Anchoring on a high percentile
/// rather than the raw maximum stops a single outlier entity — e.g. a catch-all
/// `inbox` folder with an order of magnitude more notes than any real topic —
/// from dragging the whole distribution down to the floor. On small vaults where
/// every entity is comparable this collapses to "largest entity = reference",
/// i.e. plain linear-by-document-share.
const ALLOCATION_ANCHOR_PERCENTILE: usize = 90;

/// Calculate per-entity catalyst allocation from document volume.
///
/// Coverage tracks how much material each entity holds (its total occurrence
/// count, `Entity::frequency`), so entities of similar size receive similar
/// catalyst budgets. A reference document count is taken at the
/// [`ALLOCATION_ANCHOR_PERCENTILE`]th percentile; entities at or above it receive
/// `top_target` (the vault's `min_top_catalysts`, or [`DEFAULT_TOP_CATALYSTS`]
/// when unset), and every smaller entity scales down linearly by its share of
/// documents:
///
/// ```text
/// alloc_i = clamp(round(top_target * docs_i / anchor), HARD_MIN, top_target)
/// ```
///
/// This deliberately keys off total document count rather than recent activity:
/// an entity with half the material of the reference gets roughly half the
/// catalysts, not a near-floor allocation, so coverage does not fall off a cliff
/// when entities are comparably sized. The percentile anchor keeps that property
/// intact on skewed vaults, where scaling against the single largest entity would
/// otherwise floor every mid-size entity. [`HARD_MIN_CATALYSTS`] only takes effect
/// for entities that are tiny relative to the reference (or for empty vaults).
pub fn calculate_allocations(
    entities: &[Entity],
    min_top_catalysts: Option<usize>,
) -> HashMap<String, usize> {
    let top_target = min_top_catalysts
        .unwrap_or(DEFAULT_TOP_CATALYSTS)
        .max(HARD_MIN_CATALYSTS);

    // Reference document count: the value at the configured percentile (entities
    // sorted by document count, descending). The top slice sits at or above it
    // and all receive `top_target`; the rest scale linearly against it.
    let mut doc_counts: Vec<i64> = entities.iter().map(|e| e.frequency.max(0)).collect();
    doc_counts.sort_unstable_by(|a, b| b.cmp(a));
    let docs_max = doc_counts.first().copied().unwrap_or(0);
    let anchor = if doc_counts.is_empty() {
        0
    } else {
        let idx = (doc_counts.len() * (100 - ALLOCATION_ANCHOR_PERCENTILE) / 100)
            .min(doc_counts.len() - 1);
        doc_counts[idx]
    };
    // If the percentile lands on an empty entity (a vault dominated by
    // zero-document entities), fall back to the largest so the few non-empty
    // entities still scale sensibly instead of all collapsing to the floor.
    let anchor = if anchor > 0 { anchor } else { docs_max };

    entities
        .iter()
        .map(|e| {
            let docs = e.frequency.max(0);
            let alloc = if anchor <= 0 {
                // No entity has any material; fall back to the safety floor.
                HARD_MIN_CATALYSTS
            } else {
                // round(top_target * docs / anchor) with integer arithmetic.
                let scaled = (top_target as u64 * docs as u64 + anchor as u64 / 2) / anchor as u64;
                (scaled as usize).clamp(HARD_MIN_CATALYSTS, top_target)
            };
            (e.name.clone(), alloc)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EntityTrends, EntityType};
    use crate::pipeline::eras::Era;

    /// Allocation now keys off document count (`Entity::frequency`); the
    /// timeline buckets are irrelevant to it.
    fn make_entity(name: &str, doc_count: i64) -> Entity {
        Entity {
            id: Some(1),
            name: name.to_string(),
            entity_type: EntityType::Tag,
            frequency: doc_count,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: 0,
                recency_score: 0.0,
                timeline_buckets: HashMap::new(),
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
            source_ref: format!("test-{chunk_index}.md"),
            occurrence_ids: vec![i64::from(chunk_index)],
            chunk_index,
            content: content.to_string(),
            timestamp,
            era_index,
            relevance,
            vector,
        }
    }

    #[test]
    fn test_entry_dated_chunk_overrides_file_timestamp() {
        // A journal indexed on one date carries chunks from older entries;
        // the chunk's own entry date must place it in its real era instead of
        // the file's.
        let eras = vec![
            make_era("early", "2026-01", "2026-06", 1),
            make_era("late", "2026-07", "2026-12", 1),
        ];
        let jan = 1_768_262_400_000i64; // 2026-01-13
        let aug = 1_786_579_200_000i64; // 2026-08-13
        let occ = EntityOccurrence {
            id: None,
            entity_id: 0,
            file_path: "journal.md".to_string(),
            context: None,
            created_at: aug,
            effective_date: Some(aug),
            date_source: "content".to_string(),
        };
        let chunks = vec![
            FileChunk {
                file_path: "journal.md".to_string(),
                chunk_index: 0,
                content: "old entry".to_string(),
                entry_date: Some(jan),
            },
            FileChunk {
                file_path: "journal.md".to_string(),
                chunk_index: 1,
                content: "undated".to_string(),
                entry_date: None,
            },
        ];

        let candidates = build_chunk_candidates(chunks, &[occ], &eras, None, None);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].timestamp, jan);
        assert_eq!(candidates[0].era_index, 0, "dated chunk joins its own era");
        assert_eq!(candidates[1].timestamp, aug);
        assert_eq!(
            candidates[1].era_index, 1,
            "undated chunk keeps the file's era"
        );
    }

    #[test]
    fn test_allocation_empty_vault_gets_floor() {
        // No entity has any material → everyone falls back to the safety floor.
        let entities = vec![make_entity("empty", 0)];
        let allocs = calculate_allocations(&entities, None);
        assert_eq!(allocs["empty"], HARD_MIN_CATALYSTS);
    }

    #[test]
    fn test_allocation_single_entity_gets_default_top() {
        // A lone entity is the largest, so it receives the default top budget.
        let allocs = calculate_allocations(&[make_entity("solo", 5)], None);
        assert_eq!(allocs["solo"], DEFAULT_TOP_CATALYSTS);
    }

    #[test]
    fn test_allocation_scales_with_doc_count() {
        // The attune-vault shape: top has 4 docs, others 2 and 1. Coverage should
        // mirror the document ratio, not collapse to the floor.
        let entities = vec![
            make_entity("theo", 4),
            make_entity("rel", 2),
            make_entity("org", 1),
        ];
        let allocs = calculate_allocations(&entities, Some(20));
        assert_eq!(allocs["theo"], 20); // docs_max → top_target
        assert_eq!(allocs["rel"], 10); // round(20 * 2/4)
        assert_eq!(allocs["org"], 5); // round(20 * 1/4)
    }

    #[test]
    fn test_allocation_anchor_protects_major_entities_from_outlier() {
        // Power-law vault: one catch-all entity dwarfs the rest. Anchoring on the
        // 90th percentile (not the max) keeps mid-size entities off the floor —
        // under plain linear-against-max, the 400-doc topics would collapse to 3.
        let mut entities = vec![make_entity("inbox", 2000)];
        for i in 0..9 {
            entities.push(make_entity(&format!("topic{i}"), 400));
        }
        entities.push(make_entity("small", 40));
        let allocs = calculate_allocations(&entities, None); // top_target = 15
        assert_eq!(allocs["inbox"], 15); // outlier clamps to the ceiling
        assert_eq!(allocs["topic0"], 15); // at the anchor → full coverage, not floored
        assert_eq!(allocs["small"], 3); // genuinely tiny relative to the anchor
    }

    #[test]
    fn test_allocation_floor_protects_tiny_entities() {
        // 1 doc against a 200-doc leader rounds to 0 → clamped up to the floor.
        let entities = vec![make_entity("giant", 200), make_entity("tiny", 1)];
        let allocs = calculate_allocations(&entities, Some(20));
        assert_eq!(allocs["giant"], 20);
        assert_eq!(allocs["tiny"], HARD_MIN_CATALYSTS);
    }

    #[test]
    fn test_allocation_min_top_is_ceiling() {
        // Equally sized entities all reach min_top; none exceed it.
        let entities = vec![make_entity("a", 10), make_entity("b", 10)];
        let allocs = calculate_allocations(&entities, Some(12));
        assert_eq!(allocs["a"], 12);
        assert_eq!(allocs["b"], 12);
    }

    #[test]
    fn test_allocation_default_top_target_used_when_unset() {
        // Without min_top, the largest entity gets DEFAULT_TOP_CATALYSTS and the
        // rest scale from there.
        let entities = vec![make_entity("big", 10), make_entity("half", 5)];
        let allocs = calculate_allocations(&entities, None);
        assert_eq!(allocs["big"], DEFAULT_TOP_CATALYSTS);
        // round(15 * 5/10) = round(7.5) = 8.
        assert_eq!(allocs["half"], 8);
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
                .any(|(era, text)| { *era == 0 && text.contains("old era newer chunk") })
        );
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| *era == 1 && text.contains("new era chunk"))
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
                .any(|(era, text)| *era == 0 && text.contains("alpha focus"))
        );
        assert!(
            era_context
                .samples
                .iter()
                .any(|(era, text)| *era == 1 && text.contains("beta contrast"))
        );
    }

    /// Build a typed LLM error the way `send_chat_completion` does, wrapped in
    /// `anyhow` as the pipeline sees it.
    fn typed_llm_error(status: u16, body: &str) -> anyhow::Error {
        LlmApiError {
            status,
            body: body.to_string(),
        }
        .into()
    }

    /// Regression: the 402 circuit breaker must classify on the typed HTTP
    /// status, not on substrings of the raw provider error.
    ///
    /// Every case below is a realistic upstream error in which "402" (or the
    /// phrase "Insufficient credits") appears incidentally — inside a request
    /// id, a token count, a quota value, a latency figure, a model name, or a
    /// garbled proxy body. None has status 402, so none may trip the breaker.
    #[test]
    fn incidental_402_substrings_are_not_payment_required() {
        let incidental: &[(&str, u16, &str)] = &[
            (
                "google quota value",
                429,
                r#"{"error":{"code":429,"message":"Resource exhausted: quota_limit_value 4029 for quota metric 'generate_requests_per_model_per_day'","status":"RESOURCE_EXHAUSTED"}}"#,
            ),
            (
                "request id",
                500,
                r#"{"error":{"message":"internal error","request_id":"req_c4025b7f91"}}"#,
            ),
            (
                "token count",
                400,
                r#"{"error":{"message":"This model's maximum context length is 8192 tokens, however you requested 9402 tokens"}}"#,
            ),
            (
                "latency / timing figure",
                504,
                "upstream did not respond within 4028ms",
            ),
            (
                "model name containing 402",
                404,
                r#"{"error":{"message":"model 'llama-402b-instruct' does not exist"}}"#,
            ),
            (
                "rate limit reset epoch",
                429,
                "rate limit exceeded, retry after 1750014020",
            ),
            (
                "503 body carrying the phrase Insufficient credits",
                503,
                r#"{"error":{"code":503,"message":"Upstream provider unavailable. Insufficient credits could not be verified."}}"#,
            ),
        ];

        let mut misclassified = Vec::new();
        for (label, status, body) in incidental {
            if is_payment_required_error(&typed_llm_error(*status, body)) {
                misclassified.push(format!("  [{label}] status {status}: {body}"));
            }
        }
        assert!(
            misclassified.is_empty(),
            "is_payment_required_error misclassified {} provider error(s) whose status \
             is not 402. Each of these would abort the entire catalyst run:\n{}",
            misclassified.len(),
            misclassified.join("\n"),
        );
    }

    /// An untyped error (no `LlmApiError` in the chain) must never trip the
    /// breaker, whatever its text says — the old substring classifier fired on
    /// exactly this shape.
    #[test]
    fn untyped_errors_are_never_payment_required() {
        for text in [
            "Insufficient credits for this operation",
            "LLM API returned status 402 Payment Required: legacy stringified error",
        ] {
            assert!(
                !is_payment_required_error(&anyhow::anyhow!("{text}")),
                "an untyped error must not be classified by its text: {text}"
            );
        }
    }

    #[test]
    fn test_resolve_context_budget_three_valued() {
        // Unset → model-appropriate default guardrail.
        assert_eq!(
            resolve_context_budget(None, false),
            DEFAULT_MAX_CONTEXT_CHARS
        );
        // Local defaults are additionally clamped to the model window, which
        // is tighter than the historical local default.
        assert_eq!(
            resolve_context_budget(None, true),
            hardware::LOCAL_MAX_CONTEXT_CHARS
        );
        // Explicit unlimited escape hatch — cloud only; local cannot opt out
        // of its window.
        assert_eq!(resolve_context_budget(Some(0), false), 0);
        assert_eq!(
            resolve_context_budget(Some(0), true),
            hardware::LOCAL_MAX_CONTEXT_CHARS
        );
        // Explicit override is honored verbatim up to the local clamp.
        assert_eq!(resolve_context_budget(Some(1234), false), 1234);
        assert_eq!(resolve_context_budget(Some(1234), true), 1234);
        assert_eq!(
            resolve_context_budget(Some(999_999), true),
            hardware::LOCAL_MAX_CONTEXT_CHARS
        );
    }

    fn make_occurrence(file_path: &str, context: &str) -> EntityOccurrence {
        EntityOccurrence {
            id: None,
            entity_id: 1,
            file_path: file_path.to_string(),
            context: Some(context.to_string()),
            created_at: 0,
            effective_date: Some(0),
            date_source: "test".to_string(),
        }
    }

    #[test]
    fn structured_shared_context_keeps_only_named_participant_entries() {
        let context = "# shared\n\n## 2026-01-01 00:00:00 UTC — Alice\n\nALICE_ONLY\n\n## 2026-01-01 00:01:00 UTC — Bob, Carol\n\nBOB_CAROL_ONLY";
        let bob = participant_span(context, "Bob").unwrap();
        assert!(bob.contains("BOB_CAROL_ONLY"));
        assert!(!bob.contains("ALICE_ONLY"));
        assert!(participant_span(context, "Nobody").is_none());
        assert!(catalyst_names_subject(
            "What does Bob's migration boundary leave unresolved?",
            "Bob"
        ));
        assert!(!catalyst_names_subject(
            "What does Carol's role tell Bob about the migration?",
            "Bob"
        ));
    }

    /// Regression: a single high-frequency entity whose occurrences are long
    /// notes (each carrying a full body, as the Tier-2 chunkless fallback sees
    /// after `hydrate_occurrence_contexts`) must not emit an unbounded prompt.
    /// Before the default guardrail, `max_context_chars == 0` skipped all
    /// truncation and a single entity job ballooned past the model context
    /// window (observed 1.1–1.5M tokens vs a ~1M limit).
    #[test]
    fn test_long_note_entity_job_bounded_by_default_budget() {
        // 20 occurrences (the default max_samples), each a ~200K-char body.
        let body = "word ".repeat(40_000); // ~200K chars, ~40K words
        let occurrences: Vec<EntityOccurrence> = (0..20)
            .map(|i| make_occurrence(&format!("note-{i}.md"), &body))
            .collect();
        // No eras → the flat-sample path (the one that blew up) is exercised.
        let eras: Vec<Era> = vec![];

        // Cloud default (config unset → None).
        let budget = resolve_context_budget(None, false);
        assert_eq!(budget, DEFAULT_MAX_CONTEXT_CHARS);

        let selected = build_occurrence_context(&eras, &occurrences, 7, 20, budget);

        let total: usize = selected.flat_samples.iter().map(|s| s.len()).sum();
        // `lasso_truncate` emits head + marker + tail per sample, so the
        // realized total can exceed the raw budget by a few bytes per sample
        // (marker length + char-boundary rounding). Allow a small slack
        // proportional to the sample count.
        let slack = occurrences.len() * 32;
        assert!(
            total <= budget + slack,
            "flat context ({total} chars) must stay within the default budget \
             ({budget} chars, +{slack} slack); the long-note fallback is unbounded"
        );

        // Sanity: without the guardrail (explicit unlimited) the same input is
        // enormous, so the assertion above is actually meaningful.
        let unbounded = build_occurrence_context(&eras, &occurrences, 7, 20, 0);
        let unbounded_total: usize = unbounded.flat_samples.iter().map(|s| s.len()).sum();
        assert!(
            unbounded_total > budget * 10,
            "expected the unbounded fallback to vastly exceed the budget, got \
             {unbounded_total} chars"
        );
    }

    /// The companion positive cases: genuine HTTP 402s must still be detected,
    /// so a fix cannot simply make the predicate always false. The body is
    /// irrelevant — only the status decides.
    #[test]
    fn genuine_402_status_is_payment_required() {
        let genuine: &[(&str, &str)] = &[
            (
                "openrouter insufficient credits",
                r#"{"error":{"code":402,"message":"Insufficient credits. Add more using https://openrouter.ai/credits"}}"#,
            ),
            ("empty body", ""),
            (
                "body without any billing language",
                r#"{"error":{"message":"lease spent"}}"#,
            ),
        ];

        for (label, body) in genuine {
            assert!(
                is_payment_required_error(&typed_llm_error(402, body)),
                "expected a genuine HTTP 402 to be detected as payment-required, \
                 but [{label}] was not: {body}"
            );
        }
        // ...and it must survive context wrapping, since callers may add context
        // between the client and the pipeline.
        let wrapped = typed_llm_error(402, "{}").context("while generating catalysts");
        assert!(is_payment_required_error(&wrapped));
    }

    // ---------------------------------------------------------------------
    // Incremental generation: context fingerprint + skip gate
    // ---------------------------------------------------------------------

    fn hash_with(user_prompt: &str) -> String {
        context_hash(
            "gpt-4o-mini",
            false,
            "projects",
            "folder",
            7,
            "system",
            user_prompt,
        )
    }

    #[test]
    fn context_hash_is_stable_across_calls() {
        let a = hash_with("chunk one\nchunk two");
        let b = hash_with("chunk one\nchunk two");
        assert_eq!(a, b, "same inputs must produce the same fingerprint");
        assert_eq!(a.len(), 64, "expected a hex sha256 digest");
    }

    #[test]
    fn context_hash_changes_with_every_input() {
        let base = context_hash("model-a", false, "projects", "folder", 7, "system", "ctx");
        let variants = [
            (
                "context chunks",
                context_hash("model-a", false, "projects", "folder", 7, "system", "ctx2"),
            ),
            (
                "model identity",
                context_hash("model-b", false, "projects", "folder", 7, "system", "ctx"),
            ),
            (
                "hosted vs local",
                context_hash("model-a", true, "projects", "folder", 7, "system", "ctx"),
            ),
            (
                "entity name",
                context_hash("model-a", false, "writing", "folder", 7, "system", "ctx"),
            ),
            (
                "entity type",
                context_hash("model-a", false, "projects", "tag", 7, "system", "ctx"),
            ),
            (
                "catalyst count",
                context_hash("model-a", false, "projects", "folder", 8, "system", "ctx"),
            ),
            (
                "system prompt",
                context_hash("model-a", false, "projects", "folder", 7, "system2", "ctx"),
            ),
        ];
        for (label, hash) in variants {
            assert_ne!(
                base, hash,
                "changing the {label} must change the fingerprint"
            );
        }
    }

    /// Field boundaries are length-prefixed, so shifting a character across a
    /// boundary must not produce the same digest.
    #[test]
    fn context_hash_fields_are_unambiguous() {
        let a = context_hash("model-a", false, "ab", "folder", 7, "system", "ctx");
        let b = context_hash("model-a", false, "a", "bfolder", 7, "system", "ctx");
        assert_ne!(a, b);
    }

    fn job(entity: &str, hash: &str) -> EntityJob {
        EntityJob {
            entity_name: entity.to_string(),
            entity_type: "tag".to_string(),
            user_prompt: "prompt".to_string(),
            context_hash: hash.to_string(),
            evidence_anchors: Vec::new(),
            era_audits: Vec::new(),
            participant_scoped: false,
        }
    }

    fn stored_catalyst(entity: &str) -> Catalyst {
        Catalyst {
            id: format!("cat-{entity}"),
            text: format!("{entity} catalyst"),
            entity: entity.to_string(),
            context: "topic".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({ "entity_type": "tag" }),
        }
    }

    #[test]
    fn unchanged_entities_are_skipped_and_changed_ones_are_not() {
        let db = Database::open_in_memory().unwrap();
        let llm = crate::llm::LlmClient::with_base_url("dummy", "http://localhost:0");
        let pipeline = CatalystPipeline::new(&db, &llm, None);

        db.insert_catalysts(&[stored_catalyst("alpha"), stored_catalyst("beta")])
            .unwrap();
        db.set_catalyst_entity_hash("alpha", "tag", "hash-alpha", 1)
            .unwrap();
        db.set_catalyst_entity_hash("beta", "tag", "hash-beta", 1)
            .unwrap();

        // alpha unchanged, beta's context moved, gamma never generated.
        let jobs = vec![
            job("alpha", "hash-alpha"),
            job("beta", "hash-beta-new"),
            job("gamma", "hash-gamma"),
        ];
        let (fresh, unchanged) = pipeline.partition_unchanged(jobs, false).unwrap();
        assert_eq!(unchanged, 1);
        assert_eq!(
            fresh
                .iter()
                .map(|j| j.entity_name.as_str())
                .collect::<Vec<_>>(),
            vec!["beta", "gamma"]
        );
    }

    #[test]
    fn force_regenerates_everything() {
        let db = Database::open_in_memory().unwrap();
        let llm = crate::llm::LlmClient::with_base_url("dummy", "http://localhost:0");
        let pipeline = CatalystPipeline::new(&db, &llm, None);

        db.insert_catalysts(&[stored_catalyst("alpha")]).unwrap();
        db.set_catalyst_entity_hash("alpha", "tag", "hash-alpha", 1)
            .unwrap();

        let (fresh, unchanged) = pipeline
            .partition_unchanged(vec![job("alpha", "hash-alpha")], true)
            .unwrap();
        assert_eq!(unchanged, 0);
        assert_eq!(fresh.len(), 1);
    }

    /// A fingerprint whose catalysts are gone (pruned rows, a half-wiped
    /// database) must not suppress regeneration — the skip asserts that stored
    /// catalysts exist, not merely that inputs are unchanged.
    #[test]
    fn matching_hash_without_catalysts_is_dirty() {
        let db = Database::open_in_memory().unwrap();
        let llm = crate::llm::LlmClient::with_base_url("dummy", "http://localhost:0");
        let pipeline = CatalystPipeline::new(&db, &llm, None);

        db.set_catalyst_entity_hash("alpha", "tag", "hash-alpha", 1)
            .unwrap();

        let (fresh, unchanged) = pipeline
            .partition_unchanged(vec![job("alpha", "hash-alpha")], false)
            .unwrap();
        assert_eq!(unchanged, 0);
        assert_eq!(fresh.len(), 1);
    }

    /// Same name, different type: a `#projects` tag must not be skipped
    /// because a `projects/` folder's fingerprint happens to match.
    #[test]
    fn fingerprints_are_keyed_by_entity_type() {
        let db = Database::open_in_memory().unwrap();
        let llm = crate::llm::LlmClient::with_base_url("dummy", "http://localhost:0");
        let pipeline = CatalystPipeline::new(&db, &llm, None);

        db.insert_catalysts(&[Catalyst {
            metadata: serde_json::json!({ "entity_type": "folder" }),
            ..stored_catalyst("projects")
        }])
        .unwrap();
        db.set_catalyst_entity_hash("projects", "folder", "shared-hash", 1)
            .unwrap();

        let (fresh, unchanged) = pipeline
            .partition_unchanged(vec![job("projects", "shared-hash")], false)
            .unwrap();
        assert_eq!(unchanged, 0, "the tag has no fingerprint of its own");
        assert_eq!(fresh.len(), 1);
    }
}
