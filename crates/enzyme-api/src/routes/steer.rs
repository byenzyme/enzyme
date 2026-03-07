use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;

use enzyme_core::llm::{GenerationParams, LlmClient, Message, resolve_api_config, ResponseFormat};

use crate::prompts;
use crate::AppState;

#[derive(Deserialize)]
struct SteerRequest {
    query: String,
}

async fn steer(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SteerRequest>,
) -> Result<Json<Value>, StatusCode> {
    // Load steering petri context (with full catalyst texts + topic names)
    let petri_context =
        enzyme_core::query::format_steering_petri_context(&state.vault_path, 15)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let api_config = resolve_api_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let client = LlmClient::with_base_url(&api_config.api_key, &api_config.base_url);

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
    let parsed: Value = serde_json::from_str(&response).unwrap_or_else(|_| {
        json!({ "suggestions": [] })
    });

    Ok(Json(parsed))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/explore/steer", post(steer))
}
