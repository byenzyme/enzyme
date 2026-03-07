use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};
use enzyme_core::embedding::get_embedding_service;

use crate::AppState;

#[derive(Deserialize)]
struct PreviewRequest {
    query: String,
}

async fn preview(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PreviewRequest>,
) -> Json<Value> {
    let start = Instant::now();

    // Requires pool loaded
    let pool_guard = state.pool.read().await;
    let pool = match pool_guard.as_ref() {
        Some(p) => p,
        None => {
            return Json(json!({
                "results": [],
                "elapsed_ms": 0,
                "message": "Pool not loaded. Call /explore/warm first.",
            }));
        }
    };

    let paths = match EnzymePaths::new(&state.vault_path) {
        Ok(p) => p,
        Err(_) => return Json(json!({ "results": [], "elapsed_ms": 0 })),
    };

    let service = match get_embedding_service(&paths.home) {
        Ok(s) => s,
        Err(_) => return Json(json!({ "results": [], "elapsed_ms": 0 })),
    };

    // Embed query
    let query_vec = match service.embed_query(&request.query) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to embed query: {e}");
            return Json(json!({ "results": [], "elapsed_ms": 0 }));
        }
    };

    // KNN search
    let top_catalysts = match pool.search_catalysts(&query_vec, 12) {
        Ok(c) => c,
        Err(e) => {
            warn!("KNN search failed: {e}");
            return Json(json!({ "results": [], "elapsed_ms": 0 }));
        }
    };

    // Aggregate results
    let mut doc_scores: HashMap<String, (f64, usize)> = HashMap::new();

    for (catalyst_id, _) in &top_catalysts {
        if let Some(sims) = pool.get_similarities(catalyst_id) {
            for sd in sims {
                let entry = doc_scores
                    .entry(sd.file_path.clone())
                    .or_insert((0.0, 0));
                if sd.similarity > entry.0 {
                    entry.0 = sd.similarity;
                }
                entry.1 += 1;
            }
        }
    }

    // Apply multi-catalyst boost and sort
    let db = match Database::open(&paths.vault_db) {
        Ok(d) => d,
        Err(_) => return Json(json!({ "results": [], "elapsed_ms": 0 })),
    };
    let repo = Repository::new(&db);

    let mut results: Vec<Value> = doc_scores
        .into_iter()
        .map(|(file_path, (max_sim, count))| {
            let boosted = max_sim * (1.0 + 0.2 * (count as f64 - 1.0));
            let content = repo
                .get_chunk_content(&file_path, 0)
                .ok()
                .flatten()
                .unwrap_or_default();
            json!({
                "file_path": file_path,
                "score": (boosted * 1000.0).round() / 1000.0,
                "content": content,
            })
        })
        .collect();

    results.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let elapsed = start.elapsed();

    Json(json!({
        "results": results,
        "elapsed_ms": elapsed.as_millis() as u64,
    }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/explore/preview", post(preview))
}
