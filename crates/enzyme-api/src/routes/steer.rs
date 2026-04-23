use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use enzyme_core::llm::{GenerationParams, Message, ResponseFormat, resolve_llm_client};
// Note: steer doesn't have a strict schema — response is parsed as generic JSON Value.
// We use JsonObject here since we just need valid JSON, not a specific shape.

use crate::AppState;
use crate::prompts;

#[derive(Deserialize)]
struct SteerRequest {
    query: String,
}

async fn steer(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SteerRequest>,
) -> Result<Json<Value>, StatusCode> {
    // Load steering petri context (with full catalyst texts + topic names)
    let petri_context = enzyme_core::query::format_steering_petri_context(&state.vault_path, 15)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (api_config, client) = resolve_llm_client()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let user_prompt = format!(
        "## Petri — Catalyst Vocabulary\n{petri_context}\n\n\
         ## User's Prompt\n{}\n\n\
         Find 2-3 topics that offer a different angle and generate bent prompts.",
        request.query
    );

    let response = client
        .chat_completion(
            &api_config.model,
            vec![
                Message::text("system", prompts::STEERING_SYSTEM_PROMPT),
                Message::text("user", &user_prompt),
            ],
            GenerationParams {
                max_tokens: 1024,
                temperature: 0.7,
                response_format: Some(ResponseFormat::JsonObject),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            warn!("Steering LLM call failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Parse the response as JSON; pass through if valid, wrap if not
    let parsed: Value =
        serde_json::from_str(&response).unwrap_or_else(|_| json!({ "suggestions": [] }));

    Ok(Json(parsed))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/explore/steer", post(steer))
}
