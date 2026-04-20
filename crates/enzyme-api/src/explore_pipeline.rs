//! Explore pipeline: decompose → search → synthesize orchestration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::{GenerationParams, LlmClient, Message, resolve_api_config};
use enzyme_core::search::context::ContextSearch;

use crate::explore_pool::ExplorePool;
use crate::prompts;
use crate::segment_parser::SegmentExtractor;

/// SSE event types matching the Python implementation.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ExploreEvent {
    #[serde(rename = "explore_thinking")]
    Thinking { message: String },
    #[serde(rename = "explore_search")]
    Search(SearchEventData),
    #[serde(rename = "explore_synthesis_segment")]
    SynthesisSegment(SegmentEventData),
    #[serde(rename = "explore_synthesis")]
    Synthesis { text: String },
    #[serde(rename = "explore_complete")]
    Complete(CompleteEventData),
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchEventData {
    pub query: String,
    pub results: Vec<SearchResultItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResultItem {
    pub file_path: String,
    pub aggregated_score: f64,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentEventData {
    pub segment_index: usize,
    pub total_segments: Option<usize>,
    pub explanation: String,
    pub cited_documents: Vec<CitedDocEvent>,
    pub theme: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CitedDocEvent {
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompleteEventData {
    pub total_results: usize,
    pub total_queries: usize,
    pub all_cited_paths: Vec<String>,
}

/// Request body for POST /explore.
#[derive(Debug, Deserialize)]
pub struct ExploreRequest {
    pub query: String,
    #[serde(default = "default_max_queries")]
    pub max_queries: usize,
    #[serde(default = "default_results_per_query")]
    pub results_per_query: usize,
}

fn default_max_queries() -> usize {
    3
}
fn default_results_per_query() -> usize {
    5
}

#[derive(Deserialize)]
struct DecompositionResponse {
    queries: Vec<String>,
}

/// Resolve a cited document from the LLM output: map doc_id → file_path.
fn resolve_cited_doc(
    cd: crate::segment_parser::CitedDocument,
    id_to_path: &HashMap<String, String>,
    all_cited_paths: &mut Vec<String>,
) -> Option<CitedDocEvent> {
    // Try doc_id first
    let doc_id = cd.doc_id.unwrap_or_default();
    let file_path = if let Some(fp) = id_to_path.get(&doc_id) {
        fp.clone()
    } else if let Some(fp) = cd.file_path {
        // Fallback: LLM may have produced file_path directly
        fp
    } else {
        return None;
    };

    if !all_cited_paths.contains(&file_path) {
        all_cited_paths.push(file_path.clone());
    }

    Some(CitedDocEvent {
        file_path,
        excerpt: cd.excerpt,
        heading: cd.heading,
        block_ref: cd.block_ref,
    })
}

/// Build the shared findings context string matching the Python format.
///
/// Format per search branch:
/// ```text
/// ### Search: "sub-query"
/// [D1] **file_path** (score: 0.85)
/// content...
/// ```
fn build_findings_context(
    _sub_queries: &[String],
    all_results: &[SearchResultItem],
    doc_index_map: &HashMap<String, String>,
) -> String {
    // Group results by the sub-query they came from isn't possible after merging,
    // so we just format all results with their doc IDs
    let mut parts = Vec::new();
    for r in all_results {
        if let Some(doc_id) = doc_index_map.get(&r.file_path) {
            parts.push(format!(
                "\n[{doc_id}] **{}** (score: {:.4})\n{}",
                r.file_path, r.aggregated_score, r.content
            ));
        }
    }
    parts.join("\n")
}

/// Streaming synthesis helper — consumes the LLM stream and produces events.
/// Separated to keep `Database` out of scope across await points.
async fn run_synthesis(
    client: &LlmClient,
    model: &str,
    query: &str,
    findings_context: &str,
    id_to_path: &HashMap<String, String>,
    total_results: usize,
    total_queries: usize,
) -> Vec<ExploreEvent> {
    let mut events = Vec::new();

    events.push(ExploreEvent::Thinking {
        message: "Weaving findings together...".to_string(),
    });

    let synthesis_prompt =
        format!("## Original Question\n{query}\n\n## Findings\n{findings_context}");

    match client
        .chat_completion_streaming(
            model,
            vec![
                Message::text("system", prompts::SYNTHESIS_STREAMING_SYSTEM_PROMPT),
                Message::text("user", &synthesis_prompt),
            ],
            GenerationParams {
                max_tokens: 4096,
                temperature: 0.5,
                ..Default::default()
            },
        )
        .await
    {
        Ok(stream) => {
            use futures::StreamExt;
            let mut stream = std::pin::pin!(stream);
            let mut extractor = SegmentExtractor::new();
            let mut full_text = String::new();
            let mut segment_index = 0usize;
            let mut all_cited_paths: Vec<String> = Vec::new();

            while let Some(token_result) = stream.next().await {
                match token_result {
                    Ok(token) => {
                        full_text.push_str(&token);
                        let segments = extractor.feed(&token);
                        for seg in segments {
                            let cited_docs: Vec<CitedDocEvent> = seg
                                .cited_documents
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|cd| {
                                    resolve_cited_doc(cd, id_to_path, &mut all_cited_paths)
                                })
                                .collect();

                            events.push(ExploreEvent::SynthesisSegment(SegmentEventData {
                                segment_index,
                                total_segments: None,
                                explanation: seg.explanation.unwrap_or_default(),
                                cited_documents: cited_docs,
                                theme: seg.theme.unwrap_or_default(),
                            }));
                            segment_index += 1;
                        }
                    }
                    Err(e) => {
                        warn!("Streaming error: {e}");
                        break;
                    }
                }
            }

            events.push(ExploreEvent::Synthesis { text: full_text });
            events.push(ExploreEvent::Complete(CompleteEventData {
                total_results,
                total_queries,
                all_cited_paths,
            }));
        }
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Synthesis streaming failed: {e}"),
            });
        }
    }

    events
}

/// Run the standard explore pipeline (no pool / pool stale).
pub async fn run_standard_pipeline(
    vault_path: &std::path::Path,
    request: &ExploreRequest,
) -> Vec<ExploreEvent> {
    let mut events = Vec::new();

    events.push(ExploreEvent::Thinking {
        message: "Analyzing your query...".to_string(),
    });

    // 1. Load petri context (sync — DB opened and dropped within this call)
    let petri_context = match enzyme_core::query::format_petri_context(vault_path, 20) {
        Ok(ctx) => ctx,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to load petri context: {e}"),
            });
            return events;
        }
    };

    // 2. Resolve API config (async, no DB)
    let api_config = match resolve_api_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to resolve API config: {e}"),
            });
            return events;
        }
    };
    let client = LlmClient::with_base_url(&api_config.api_key, &api_config.base_url);
    let model = api_config.model.clone();

    // 3. LLM decomposition (async, no DB)
    events.push(ExploreEvent::Thinking {
        message: "Decomposing query into sub-queries...".to_string(),
    });

    let decomp_prompt = format!(
        "## Petri\n{petri_context}\n\n## User Query\n{}\n\nProduce up to {} queries.",
        request.query, request.max_queries
    );

    let sub_queries = match client
        .chat_completion(
            &model,
            vec![
                Message::text("system", prompts::DECOMPOSITION_SYSTEM_PROMPT),
                Message::text("user", &decomp_prompt),
            ],
            GenerationParams {
                max_tokens: 512,
                temperature: 0.3,
                response_format: Some(enzyme_core::llm::ResponseFormat::JsonObject),
                ..Default::default()
            },
        )
        .await
    {
        Ok(response) => match serde_json::from_str::<DecompositionResponse>(&response) {
            Ok(parsed) => {
                let mut queries = parsed.queries;
                queries.truncate(request.max_queries);
                queries
            }
            Err(_) => vec![request.query.clone()],
        },
        Err(e) => {
            warn!("Decomposition failed, using original query: {e}");
            vec![request.query.clone()]
        }
    };

    debug!("Decomposed into {} sub-queries", sub_queries.len());

    // 4. Fan-out search per sub-query (sync DB operations — scoped so DB is dropped before synthesis)
    let search_result = {
        let paths = match EnzymePaths::new(vault_path) {
            Ok(p) => p,
            Err(e) => {
                events.push(ExploreEvent::Error {
                    message: format!("Failed to resolve paths: {e}"),
                });
                return events;
            }
        };

        let service = match get_embedding_service(&paths.home) {
            Ok(s) => s,
            Err(e) => {
                events.push(ExploreEvent::Error {
                    message: format!("Failed to get embedding service: {e}"),
                });
                return events;
            }
        };

        let db = match Database::open(&paths.vault_db) {
            Ok(d) => d,
            Err(e) => {
                events.push(ExploreEvent::Error {
                    message: format!("Failed to open database: {e}"),
                });
                return events;
            }
        };

        let search = ContextSearch::new(&db, service);

        let mut all_results: Vec<SearchResultItem> = Vec::new();
        let mut doc_index_map: HashMap<String, String> = HashMap::new();
        let mut doc_counter = 1usize;
        let mut search_events = Vec::new();

        for sub_query in &sub_queries {
            search_events.push(ExploreEvent::Thinking {
                message: format!("Searching: {sub_query}"),
            });

            match search.catalyze_search(sub_query, request.results_per_query) {
                Ok(response) => {
                    let results: Vec<SearchResultItem> = response
                        .results
                        .into_iter()
                        .map(|r| SearchResultItem {
                            file_path: r.file_path.clone(),
                            aggregated_score: r.similarity,
                            content: r.content,
                        })
                        .collect();

                    for r in &results {
                        if !doc_index_map.contains_key(&r.file_path) {
                            doc_index_map.insert(r.file_path.clone(), format!("D{doc_counter}"));
                            doc_counter += 1;
                        }
                    }

                    search_events.push(ExploreEvent::Search(SearchEventData {
                        query: sub_query.clone(),
                        results: results.clone(),
                    }));

                    all_results.extend(results);
                }
                Err(e) => {
                    warn!("Search failed for sub-query '{}': {}", sub_query, e);
                }
            }
        }
        // db, search, repo all dropped here
        (all_results, doc_index_map, search_events)
    };

    let (all_results, doc_index_map, search_events) = search_result;
    events.extend(search_events);

    if all_results.is_empty() {
        events.push(ExploreEvent::Complete(CompleteEventData {
            total_results: 0,
            total_queries: sub_queries.len(),
            all_cited_paths: Vec::new(),
        }));
        return events;
    }

    // 5. Build findings context with doc IDs (matching Python format)
    let findings_context = build_findings_context(&sub_queries, &all_results, &doc_index_map);

    let id_to_path: HashMap<String, String> = doc_index_map
        .iter()
        .map(|(path, id)| (id.clone(), path.clone()))
        .collect();

    // 6. Streaming synthesis (async, no DB — fully Send-safe)
    let synthesis_events = run_synthesis(
        &client,
        &model,
        &request.query,
        &findings_context,
        &id_to_path,
        all_results.len(),
        sub_queries.len(),
    )
    .await;

    events.extend(synthesis_events);
    events
}

/// Run the fast explore path using a pre-loaded ExplorePool.
pub async fn run_fast_pipeline(
    vault_path: &std::path::Path,
    pool: &ExplorePool,
    request: &ExploreRequest,
) -> Vec<ExploreEvent> {
    let mut events = Vec::new();

    events.push(ExploreEvent::Thinking {
        message: "Using cached pool for fast search...".to_string(),
    });

    // 1. Embed query (sync, no DB)
    let paths = match EnzymePaths::new(vault_path) {
        Ok(p) => p,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to resolve paths: {e}"),
            });
            return events;
        }
    };

    let service = match get_embedding_service(&paths.home) {
        Ok(s) => s,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to get embedding service: {e}"),
            });
            return events;
        }
    };

    let query_vec = match service.embed_query(&request.query) {
        Ok(v) => v,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to embed query: {e}"),
            });
            return events;
        }
    };

    // 2. KNN over catalyst embeddings → top 12 catalysts
    let top_catalysts = match pool.search_catalysts(&query_vec, 12) {
        Ok(c) => c,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("KNN search failed: {e}"),
            });
            return events;
        }
    };

    // 3. Lookup pre-computed similarities, aggregate, and fetch content (sync DB — scoped)
    let (final_results, doc_index_map, search_events, total_queries) = {
        let db = match Database::open(&paths.vault_db) {
            Ok(d) => d,
            Err(e) => {
                events.push(ExploreEvent::Error {
                    message: format!("Failed to open database: {e}"),
                });
                return events;
            }
        };
        let mut doc_scores: HashMap<String, (f64, usize, String)> = HashMap::new();
        let top_catalyst_count = top_catalysts.len().min(4);
        let mut search_events = Vec::new();

        for (i, (catalyst_id, _relevance)) in top_catalysts.iter().enumerate() {
            if let Some(sims) = pool.get_similarities(catalyst_id) {
                let results: Vec<SearchResultItem> = sims
                    .iter()
                    .take(request.results_per_query)
                    .filter_map(|sd| {
                        let content = db
                            .get_chunk_content(&sd.file_path, sd.chunk_index)
                            .ok()
                            .flatten()
                            .unwrap_or_default();

                        let entry = doc_scores.entry(sd.file_path.clone()).or_insert((
                            0.0,
                            0,
                            content.clone(),
                        ));
                        if sd.similarity > entry.0 {
                            entry.0 = sd.similarity;
                        }
                        entry.1 += 1;

                        Some(SearchResultItem {
                            file_path: sd.file_path.clone(),
                            aggregated_score: sd.similarity,
                            content,
                        })
                    })
                    .collect();

                if i < top_catalyst_count {
                    let cat_meta = pool.get_catalyst_meta(catalyst_id);
                    let query_label = cat_meta
                        .map(|m| m.text.clone())
                        .unwrap_or_else(|| catalyst_id.clone());

                    search_events.push(ExploreEvent::Search(SearchEventData {
                        query: query_label,
                        results,
                    }));
                }
            }
        }

        // Apply multi-catalyst boost
        let mut final_results: Vec<SearchResultItem> = doc_scores
            .into_iter()
            .map(|(file_path, (max_sim, count, content))| {
                let boosted = max_sim * (1.0 + 0.2 * (count as f64 - 1.0));
                SearchResultItem {
                    file_path,
                    aggregated_score: boosted,
                    content,
                }
            })
            .collect();

        final_results.sort_by(|a, b| {
            b.aggregated_score
                .partial_cmp(&a.aggregated_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Build doc index map
        let mut doc_index_map: HashMap<String, String> = HashMap::new();
        for (i, r) in final_results.iter().enumerate() {
            doc_index_map
                .entry(r.file_path.clone())
                .or_insert_with(|| format!("D{}", i + 1));
        }

        let total_queries = top_catalysts.len();
        // db, repo dropped here
        (final_results, doc_index_map, search_events, total_queries)
    };

    events.extend(search_events);

    if final_results.is_empty() {
        events.push(ExploreEvent::Complete(CompleteEventData {
            total_results: 0,
            total_queries,
            all_cited_paths: Vec::new(),
        }));
        return events;
    }

    // Build findings context (matching Python format)
    let findings_context = build_findings_context(&[], &final_results, &doc_index_map);

    let id_to_path: HashMap<String, String> = doc_index_map
        .iter()
        .map(|(path, id)| (id.clone(), path.clone()))
        .collect();

    // Resolve API config and run synthesis (async, no DB)
    let api_config = match resolve_api_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            events.push(ExploreEvent::Error {
                message: format!("Failed to resolve API config: {e}"),
            });
            return events;
        }
    };
    let client = LlmClient::with_base_url(&api_config.api_key, &api_config.base_url);

    let synthesis_events = run_synthesis(
        &client,
        &api_config.model,
        &request.query,
        &findings_context,
        &id_to_path,
        final_results.len(),
        total_queries,
    )
    .await;

    events.extend(synthesis_events);
    events
}
