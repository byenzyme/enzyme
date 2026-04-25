use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::{EmbeddingBackend, EmbeddingService, get_embedding_service};
use enzyme_core::search::context::{ContextSearch, DirectSearchResult};

/// A single benchmark query with optional metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkQuery {
    pub query: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Results for one query across all conditions.
#[derive(Debug, Serialize)]
struct QueryBenchmarkResult {
    query: String,
    category: Option<String>,
    conditions: Vec<ConditionResult>,
}

/// Results for a single condition (e.g., "ese_direct", "ese_catalysts").
#[derive(Debug, Serialize)]
struct ConditionResult {
    condition: String,
    backend: String,
    search_mode: String,
    latency_ms: f64,
    results: Vec<BenchmarkResultItem>,
}

#[derive(Debug, Serialize)]
struct BenchmarkResultItem {
    rank: usize,
    file_path: String,
    chunk_index: i64,
    similarity: f64,
    /// Query-result cosine similarity judged by each embedding model.
    /// These are independent of which condition produced the result —
    /// they measure how relevant the result is to the query under each model's geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    ese_query_sim: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arctic_query_sim: Option<f64>,
    /// First 200 chars of content for review
    content_preview: String,
}

pub async fn run_benchmark(
    vault_path: &Path,
    queries_path: &Path,
    limit: usize,
    conditions: &[String],
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        anyhow::bail!("Vault not initialized. Run 'enzyme init' first.");
    }

    // Load queries
    let queries = load_queries(queries_path)?;
    eprintln!("Loaded {} queries from {}", queries.len(), queries_path.display());

    let db = Database::open(&paths.vault_db)?;
    let ese_service = get_embedding_service(&paths.home)?;

    // Parse which conditions to run
    let run_all = conditions.is_empty();
    let run_ese_direct = run_all || conditions.iter().any(|c| c == "ese_direct");
    let run_ese_catalysts = run_all || conditions.iter().any(|c| c == "ese_catalysts");
    let run_arctic_direct = run_all || conditions.iter().any(|c| c == "arctic_direct");
    let run_arctic_catalysts = run_all || conditions.iter().any(|c| c == "arctic_catalysts");

    // Load Arctic backend if needed
    #[cfg(feature = "rten")]
    let arctic_service: Option<EmbeddingService> = if run_arctic_direct || run_arctic_catalysts {
        let arctic = load_arctic_backend(&paths.home).await?;
        Some(arctic)
    } else {
        None
    };
    #[cfg(not(feature = "rten"))]
    let arctic_service: Option<EmbeddingService> = if run_arctic_direct || run_arctic_catalysts {
        anyhow::bail!("Arctic backend requires the 'rten' feature. Rebuild with: cargo build --features rten");
    } else {
        None
    };

    // Pre-compute Arctic chunk embeddings if needed (expensive, do once)
    #[allow(unused_variables)]
    let arctic_chunk_embeddings: Option<Vec<(String, u32, Vec<f32>)>> =
        if let Some(ref arctic_svc) = arctic_service {
            eprintln!("Pre-computing Arctic chunk embeddings...");
            let start = Instant::now();
            let embeddings = compute_chunk_embeddings_with_backend(&db, arctic_svc)?;
            eprintln!(
                "  Computed {} chunk embeddings in {:.1}s",
                embeddings.len(),
                start.elapsed().as_secs_f64()
            );
            Some(embeddings)
        } else {
            None
        };

    // Build chunk embedding lookup maps for cross-model relevance scoring
    use enzyme_core::models::Embedding;
    use std::collections::HashMap;

    eprintln!("Loading ESE chunk embeddings for cross-model scoring...");
    let ese_chunk_map: HashMap<(String, u32), Vec<f32>> = {
        let all = db.get_all_chunk_embeddings()?;
        all.into_iter().map(|(fp, ci, vec)| ((fp, ci), vec)).collect()
    };
    eprintln!("  {} chunks indexed", ese_chunk_map.len());

    let arctic_chunk_map: Option<HashMap<(String, u32), &Vec<f32>>> =
        arctic_chunk_embeddings.as_ref().map(|embs| {
            embs.iter().map(|(fp, ci, vec)| ((fp.clone(), *ci), vec)).collect()
        });

    // Run benchmark
    let mut all_results: Vec<QueryBenchmarkResult> = Vec::new();

    for (i, bq) in queries.iter().enumerate() {
        eprintln!("[{}/{}] {}", i + 1, queries.len(), &bq.query);

        // Pre-compute query embeddings under each model for cross-scoring
        let ese_query_vec = ese_service.embed_query(&bq.query)?;
        let arctic_query_vec: Option<Vec<f32>> = arctic_service
            .as_ref()
            .map(|svc| svc.embed_query(&bq.query))
            .transpose()?;

        let mut conditions_results: Vec<ConditionResult> = Vec::new();

        // Condition 1: ESE direct
        if run_ese_direct {
            let search = ContextSearch::new(&db, ese_service);
            let start = Instant::now();
            let results = search.direct_search(&bq.query, limit)?;
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            conditions_results.push(build_condition_result(
                "ese_direct", "ese", "direct", latency, &results,
            ));
        }

        // Condition 2: ESE + catalysts (current Enzyme)
        if run_ese_catalysts {
            let search = ContextSearch::new(&db, ese_service);
            let start = Instant::now();
            let response = search.catalyze_search(&bq.query, limit)?;
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            conditions_results.push(ConditionResult {
                condition: "ese_catalysts".to_string(),
                backend: "ese".to_string(),
                search_mode: "catalyst_mediated".to_string(),
                latency_ms: (latency * 100.0).round() / 100.0,
                results: response
                    .results
                    .iter()
                    .enumerate()
                    .map(|(rank, r)| BenchmarkResultItem {
                        rank: rank + 1,
                        file_path: r.file_path.clone(),
                        chunk_index: 0, // catalyze response doesn't expose chunk_index
                        similarity: r.similarity,
                        ese_query_sim: None,
                        arctic_query_sim: None,
                        content_preview: r.content.chars().take(200).collect(),
                    })
                    .collect(),
            });
        }

        // Condition 3: Arctic direct
        if run_arctic_direct {
            if let (Some(arctic_svc), Some(arctic_chunks)) =
                (&arctic_service, &arctic_chunk_embeddings)
            {
                let start = Instant::now();
                let query_vec = arctic_svc.embed_query(&bq.query)?;
                let results = ContextSearch::direct_search_with_embeddings(
                    &query_vec,
                    arctic_chunks,
                    &db,
                    limit,
                )?;
                let latency = start.elapsed().as_secs_f64() * 1000.0;
                conditions_results.push(build_condition_result(
                    "arctic_direct",
                    "arctic-xs",
                    "direct",
                    latency,
                    &results,
                ));
            }
        }

        // Condition 4: Arctic + catalysts
        // This is the most complex — we need to re-embed catalysts with Arctic
        // and re-compute catalyst-chunk similarities on the fly.
        // For now, skip this and note it as future work.
        if run_arctic_catalysts {
            if let (Some(arctic_svc), Some(arctic_chunks)) =
                (&arctic_service, &arctic_chunk_embeddings)
            {
                let start = Instant::now();
                let results = catalyze_search_with_backend(
                    &db, arctic_svc, arctic_chunks, &bq.query, limit,
                )?;
                let latency = start.elapsed().as_secs_f64() * 1000.0;
                conditions_results.push(build_condition_result(
                    "arctic_catalysts",
                    "arctic-xs",
                    "catalyst_mediated",
                    latency,
                    &results,
                ));
            }
        }

        // Cross-model relevance scoring: for each result in each condition,
        // compute query-chunk cosine similarity under both embedding models.
        // This judges each condition's results by an independent metric.
        for cr in &mut conditions_results {
            for item in &mut cr.results {
                // Try exact (file_path, chunk_index) match first.
                // For ese_catalysts results where chunk_index is 0 (unknown),
                // fall back to best-matching chunk from that file.
                let key = (item.file_path.clone(), item.chunk_index as u32);

                // ESE judge
                let ese_sim = ese_chunk_map
                    .get(&key)
                    .map(|v| Embedding::cosine_similarity(&ese_query_vec, v))
                    .or_else(|| {
                        // Fallback: find best chunk for this file
                        ese_chunk_map
                            .iter()
                            .filter(|((fp, _), _)| fp == &item.file_path)
                            .map(|(_, v)| Embedding::cosine_similarity(&ese_query_vec, v))
                            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    });
                if let Some(sim) = ese_sim {
                    item.ese_query_sim = Some((sim as f64 * 10000.0).round() / 10000.0);
                }

                // Arctic judge
                if let (Some(q_vec), Some(c_map)) = (&arctic_query_vec, &arctic_chunk_map) {
                    let arctic_sim = c_map
                        .get(&key)
                        .map(|v| Embedding::cosine_similarity(q_vec, v))
                        .or_else(|| {
                            c_map
                                .iter()
                                .filter(|((fp, _), _)| fp == &item.file_path)
                                .map(|(_, v)| Embedding::cosine_similarity(q_vec, v))
                                .max_by(|a, b| {
                                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                })
                        });
                    if let Some(sim) = arctic_sim {
                        item.arctic_query_sim = Some((sim as f64 * 10000.0).round() / 10000.0);
                    }
                }
            }
        }

        all_results.push(QueryBenchmarkResult {
            query: bq.query.clone(),
            category: bq.category.clone(),
            conditions: conditions_results,
        });
    }

    // Output summary
    let output = json!({
        "benchmark": {
            "vault": vault_path.display().to_string(),
            "queries_file": queries_path.display().to_string(),
            "total_queries": queries.len(),
            "limit": limit,
        },
        "results": all_results,
    });

    println!("{}", serde_json::to_string_pretty(&output)?);

    // Print summary table to stderr
    print_summary(&all_results);

    Ok(())
}

fn load_queries(path: &Path) -> Result<Vec<BenchmarkQuery>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read queries file: {}", path.display()))?;

    let mut queries = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<BenchmarkQuery>(line) {
            Ok(q) => queries.push(q),
            Err(e) => {
                // Try as plain text query
                if !line.starts_with('{') {
                    queries.push(BenchmarkQuery {
                        query: line.to_string(),
                        category: None,
                        source: None,
                    });
                } else {
                    anyhow::bail!("line {}: invalid JSON: {}", line_num + 1, e);
                }
            }
        }
    }
    Ok(queries)
}

fn build_condition_result(
    condition: &str,
    backend: &str,
    search_mode: &str,
    latency_ms: f64,
    results: &[DirectSearchResult],
) -> ConditionResult {
    ConditionResult {
        condition: condition.to_string(),
        backend: backend.to_string(),
        search_mode: search_mode.to_string(),
        latency_ms: (latency_ms * 100.0).round() / 100.0,
        results: results
            .iter()
            .enumerate()
            .map(|(rank, r)| BenchmarkResultItem {
                rank: rank + 1,
                file_path: r.file_path.clone(),
                chunk_index: r.chunk_index,
                similarity: r.similarity,
                ese_query_sim: None,
                arctic_query_sim: None,
                content_preview: r.content.chars().take(200).collect(),
            })
            .collect(),
    }
}

/// Compute chunk embeddings for the entire corpus using a given backend.
fn compute_chunk_embeddings_with_backend(
    db: &Database,
    service: &EmbeddingService,
) -> Result<Vec<(String, u32, Vec<f32>)>> {
    // Get all chunks from DB: (file_path, chunk_index, content)
    let chunks = db.get_all_chunks_with_embeddings()?;
    if chunks.is_empty() {
        return Ok(Vec::new());
    }

    let texts: Vec<&str> = chunks.iter().map(|(_, _, content)| content.as_str()).collect();
    let embeddings = service.embed_documents(&texts)?;

    Ok(chunks
        .into_iter()
        .zip(embeddings)
        .map(|((fp, ci, _content), vec)| (fp, ci as u32, vec))
        .collect())
}

/// Run catalyst-mediated search using an arbitrary embedding backend.
///
/// This re-embeds catalysts and computes catalyst-chunk similarities on the fly
/// (no pre-computed similarities table), then applies the same scoring as
/// `catalyze_search` — weighted sim × multi-catalyst boost.
fn catalyze_search_with_backend(
    db: &Database,
    service: &EmbeddingService,
    chunk_embeddings: &[(String, u32, Vec<f32>)],
    query: &str,
    limit: usize,
) -> Result<Vec<DirectSearchResult>> {
    use enzyme_core::models::Embedding;
    use std::collections::HashMap;

    let query_vec = service.embed_query(query)?;

    // Get and embed all catalysts
    let catalysts = db.get_all_catalysts()?;
    if catalysts.is_empty() {
        return Ok(Vec::new());
    }

    let cat_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
    let cat_embeddings = service.embed_documents(&cat_texts)?;

    // Find top 10 catalysts by similarity to query
    let max_catalysts = 10usize;
    let mut cat_sims: Vec<(usize, f32)> = cat_embeddings
        .iter()
        .enumerate()
        .map(|(i, vec)| (i, Embedding::cosine_similarity(&query_vec, vec)))
        .collect();
    cat_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    cat_sims.truncate(max_catalysts);

    // For each top catalyst, find most similar chunks
    struct ChunkScore {
        max_weighted_score: f64,
        catalyst_count: usize,
    }

    let mut chunk_scores: HashMap<usize, ChunkScore> = HashMap::new();
    let context_per_catalyst = (limit * 3 / max_catalysts.max(1)).max(10);

    for (cat_idx, cat_sim) in &cat_sims {
        let cat_vec = &cat_embeddings[*cat_idx];
        let relevance = *cat_sim as f64;

        // Compute similarity between this catalyst and all chunks
        let mut chunk_sims: Vec<(usize, f32)> = chunk_embeddings
            .iter()
            .enumerate()
            .map(|(i, (_fp, _ci, vec))| (i, Embedding::cosine_similarity(cat_vec, vec)))
            .collect();
        chunk_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        chunk_sims.truncate(context_per_catalyst);

        for (chunk_idx, chunk_sim) in chunk_sims {
            let weighted_score = chunk_sim as f64 * relevance;
            let entry = chunk_scores.entry(chunk_idx).or_insert(ChunkScore {
                max_weighted_score: 0.0,
                catalyst_count: 0,
            });
            if weighted_score > entry.max_weighted_score {
                entry.max_weighted_score = weighted_score;
            }
            entry.catalyst_count += 1;
        }
    }

    // Score with multi-catalyst boost (skip recency for benchmark purity)
    let mut scored: Vec<(usize, f64)> = chunk_scores
        .iter()
        .map(|(&idx, cs)| {
            let multi_boost = 1.0 + (cs.catalyst_count as f64 - 1.0) * 0.2;
            (idx, cs.max_weighted_score * multi_boost)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    let mut results = Vec::with_capacity(scored.len());
    for (idx, score) in &scored {
        let (ref file_path, chunk_index, _) = chunk_embeddings[*idx];
        let content = db
            .get_chunk_content(file_path, chunk_index as i64)?
            .unwrap_or_default();
        results.push(DirectSearchResult {
            file_path: file_path.clone(),
            chunk_index: chunk_index as i64,
            content,
            similarity: (*score * 1000.0).round() / 1000.0,
        });
    }

    Ok(results)
}

fn print_summary(results: &[QueryBenchmarkResult]) {
    // Aggregate latency and overlap stats per condition
    use std::collections::HashMap;

    struct CondStats {
        latencies: Vec<f64>,
    }

    let mut stats: HashMap<String, CondStats> = HashMap::new();

    for qr in results {
        for cr in &qr.conditions {
            let entry = stats
                .entry(cr.condition.clone())
                .or_insert(CondStats { latencies: vec![] });
            entry.latencies.push(cr.latency_ms);
        }
    }

    eprintln!("\n--- Benchmark Summary ---");
    eprintln!("{:<20} {:>8} {:>8} {:>8}", "Condition", "P50 ms", "P95 ms", "Mean ms");
    eprintln!("{}", "-".repeat(50));

    let mut conditions: Vec<_> = stats.keys().collect();
    conditions.sort();

    for cond in &conditions {
        let s = &stats[cond.as_str()];
        let mut lat = s.latencies.clone();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = lat[lat.len() / 2];
        let p95 = lat[(lat.len() as f64 * 0.95) as usize];
        let mean = lat.iter().sum::<f64>() / lat.len() as f64;
        eprintln!("{:<20} {:>8.1} {:>8.1} {:>8.1}", cond, p50, p95, mean);
    }

    // Cross-model relevance quality
    eprintln!("\n--- Mean Query-Result Similarity (relevance proxy) ---");
    eprintln!(
        "{:<22} {:>12} {:>12}",
        "Condition", "ESE judge", "Arctic judge"
    );
    eprintln!("{}", "-".repeat(50));

    for cond in &conditions {
        let mut ese_sims: Vec<f64> = Vec::new();
        let mut arctic_sims: Vec<f64> = Vec::new();

        for qr in results {
            for cr in &qr.conditions {
                if &cr.condition != *cond {
                    continue;
                }
                for item in &cr.results {
                    if let Some(s) = item.ese_query_sim {
                        ese_sims.push(s);
                    }
                    if let Some(s) = item.arctic_query_sim {
                        arctic_sims.push(s);
                    }
                }
            }
        }

        let ese_mean = if ese_sims.is_empty() {
            "n/a".to_string()
        } else {
            format!("{:.4}", ese_sims.iter().sum::<f64>() / ese_sims.len() as f64)
        };
        let arctic_mean = if arctic_sims.is_empty() {
            "n/a".to_string()
        } else {
            format!(
                "{:.4}",
                arctic_sims.iter().sum::<f64>() / arctic_sims.len() as f64
            )
        };

        eprintln!("{:<22} {:>12} {:>12}", cond, ese_mean, arctic_mean);
    }

    // Per-category relevance quality
    let categories: std::collections::HashSet<String> = results
        .iter()
        .filter_map(|r| r.category.clone())
        .collect();
    if !categories.is_empty() {
        eprintln!("\n--- Per-Category Mean Query-Result Similarity ---");
        let mut cats: Vec<_> = categories.iter().collect();
        cats.sort();

        for cat in cats {
            eprintln!("\n  [{}]", cat);
            eprintln!(
                "  {:<22} {:>12} {:>12}",
                "Condition", "ESE judge", "Arctic judge"
            );

            for cond in &conditions {
                let mut ese_sims: Vec<f64> = Vec::new();
                let mut arctic_sims: Vec<f64> = Vec::new();

                for qr in results {
                    if qr.category.as_deref() != Some(cat.as_str()) {
                        continue;
                    }
                    for cr in &qr.conditions {
                        if &cr.condition != *cond {
                            continue;
                        }
                        for item in &cr.results {
                            if let Some(s) = item.ese_query_sim {
                                ese_sims.push(s);
                            }
                            if let Some(s) = item.arctic_query_sim {
                                arctic_sims.push(s);
                            }
                        }
                    }
                }

                let ese_mean = if ese_sims.is_empty() {
                    "n/a".to_string()
                } else {
                    format!("{:.4}", ese_sims.iter().sum::<f64>() / ese_sims.len() as f64)
                };
                let arctic_mean = if arctic_sims.is_empty() {
                    "n/a".to_string()
                } else {
                    format!(
                        "{:.4}",
                        arctic_sims.iter().sum::<f64>() / arctic_sims.len() as f64
                    )
                };

                eprintln!("  {:<22} {:>12} {:>12}", cond, ese_mean, arctic_mean);
            }
        }
    }

    // Cross-condition overlap analysis
    if results.iter().any(|qr| qr.conditions.len() > 1) {
        eprintln!("\n--- Top-10 Overlap (Jaccard) ---");
        let condition_names: Vec<String> = results
            .first()
            .map(|qr| qr.conditions.iter().map(|c| c.condition.clone()).collect())
            .unwrap_or_default();

        for i in 0..condition_names.len() {
            for j in (i + 1)..condition_names.len() {
                let mut jaccards = Vec::new();
                for qr in results {
                    let a_files: std::collections::HashSet<_> = qr
                        .conditions
                        .iter()
                        .find(|c| c.condition == condition_names[i])
                        .map(|c| c.results.iter().map(|r| &r.file_path).collect())
                        .unwrap_or_default();
                    let b_files: std::collections::HashSet<_> = qr
                        .conditions
                        .iter()
                        .find(|c| c.condition == condition_names[j])
                        .map(|c| c.results.iter().map(|r| &r.file_path).collect())
                        .unwrap_or_default();

                    if !a_files.is_empty() || !b_files.is_empty() {
                        let intersection = a_files.intersection(&b_files).count();
                        let union = a_files.union(&b_files).count();
                        if union > 0 {
                            jaccards.push(intersection as f64 / union as f64);
                        }
                    }
                }
                if !jaccards.is_empty() {
                    let mean_j = jaccards.iter().sum::<f64>() / jaccards.len() as f64;
                    eprintln!(
                        "  {} vs {}: {:.3}",
                        condition_names[i], condition_names[j], mean_j
                    );
                }
            }
        }
    }
}

#[cfg(feature = "rten")]
async fn load_arctic_backend(enzyme_home: &Path) -> Result<EmbeddingService> {
    use enzyme_core::embedding::RtenBackend;

    let models_dir = enzyme_home.join("models").join("arctic-xs");

    let model_path = models_dir.join("model.onnx");
    let tokenizer_path = models_dir.join("tokenizer.json");

    if !model_path.exists() || !tokenizer_path.exists() {
        eprintln!("Arctic Embed XS model not found. Downloading...");
        tokio::fs::create_dir_all(&models_dir).await?;

        let model_url =
            "https://huggingface.co/Snowflake/snowflake-arctic-embed-xs/resolve/main/onnx/model_quantized.onnx";
        let tokenizer_url =
            "https://huggingface.co/Snowflake/snowflake-arctic-embed-xs/resolve/main/tokenizer.json";

        let client = reqwest::Client::new();

        if !model_path.exists() {
            eprintln!("  Downloading model.onnx...");
            let bytes = client.get(model_url).send().await?.bytes().await?;
            tokio::fs::write(&model_path, &bytes).await?;
            eprintln!("  Saved ({:.1} MB)", bytes.len() as f64 / 1_048_576.0);
        }

        if !tokenizer_path.exists() {
            eprintln!("  Downloading tokenizer.json...");
            let bytes = client.get(tokenizer_url).send().await?.bytes().await?;
            tokio::fs::write(&tokenizer_path, &bytes).await?;
            eprintln!("  Saved");
        }
    }

    let backend = RtenBackend::load(&model_path, &tokenizer_path)?;
    eprintln!(
        "Loaded Arctic Embed XS (dim={})",
        backend.dimension()
    );
    Ok(EmbeddingService::new(Box::new(backend)))
}
