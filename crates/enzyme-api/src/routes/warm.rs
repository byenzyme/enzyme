use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use enzyme_core::embedding::get_embedding_service;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::llm::{GenerationParams, LlmClient, Message, resolve_api_config};

use crate::explore_pool::ExplorePool;
use crate::AppState;

#[derive(Deserialize)]
struct WarmRequest {
    query: Option<String>,
}

async fn warm(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WarmRequest>,
) -> Result<Json<Value>, StatusCode> {
    let start = Instant::now();

    // Check if pool needs rebuilding
    let needs_rebuild = {
        let pool_guard = state.pool.read().await;
        pool_guard.is_none() || pool_guard.as_ref().map(|p| p.is_stale()).unwrap_or(true)
    };

    if needs_rebuild {
        info!("Building ExplorePool...");
        match ExplorePool::load(&state.vault_path) {
            Ok(pool) => {
                let mut pool_guard = state.pool.write().await;
                *pool_guard = Some(pool);
            }
            Err(e) => {
                warn!("Failed to build ExplorePool: {e}");
                return Ok(Json(json!({
                    "status": "error",
                    "message": format!("Failed to build pool: {e}"),
                })));
            }
        }
    }

    // If query provided, warm the LLM cache
    if let Some(query) = &request.query {
        let pool_guard = state.pool.read().await;
        if let Some(pool) = pool_guard.as_ref() {
            // Embed query and do KNN search to prepare context
            let paths = EnzymePaths::new(&state.vault_path)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let service = get_embedding_service(&paths.home)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            if let Ok(query_vec) = service.embed_query(query) {
                if let Ok(top_catalysts) = pool.search_catalysts(&query_vec, 12) {
                    // Send a minimal request to warm the provider cache
                    if let Ok(api_config) = resolve_api_config().await {
                        let client = LlmClient::with_base_url(&api_config.api_key, &api_config.base_url);
                        let _ = client
                            .chat_completion(
                                &api_config.model,
                                vec![Message::text("user", "warmup")],
                                GenerationParams {
                                    max_tokens: 1,
                                    temperature: 0.0,
                                    ..Default::default()
                                },
                            )
                            .await;

                        info!("LLM cache warmed with {} catalyst context", top_catalysts.len());
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed();

    let pool_info = {
        let pool_guard = state.pool.read().await;
        pool_guard.as_ref().map(|p| {
            json!({
                "catalysts": p.catalyst_metadata.len(),
                "similarities": p.similarities.len(),
            })
        })
    };

    Ok(Json(json!({
        "status": "warmed",
        "pool": pool_info,
        "elapsed_ms": elapsed.as_millis() as u64,
    })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/explore/warm", post(warm))
}
