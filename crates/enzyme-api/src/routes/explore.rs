use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::post;
use axum::{Json, Router};
use tokio_stream::wrappers::ReceiverStream;

use crate::AppState;
use crate::explore_pipeline::{self, ExploreEvent, ExploreRequest};

fn event_to_sse(event: &ExploreEvent) -> Event {
    match event {
        ExploreEvent::Thinking { message } => Event::default()
            .event("explore_thinking")
            .data(serde_json::json!({"text": message}).to_string()),
        ExploreEvent::Search(data) => Event::default()
            .event("explore_search")
            .data(serde_json::to_string(data).unwrap_or_default()),
        ExploreEvent::SynthesisSegment(data) => Event::default()
            .event("explore_synthesis_segment")
            .data(serde_json::to_string(data).unwrap_or_default()),
        ExploreEvent::Synthesis { text } => Event::default()
            .event("explore_synthesis")
            .data(serde_json::json!({"text": text}).to_string()),
        ExploreEvent::Complete(data) => Event::default()
            .event("explore_complete")
            .data(serde_json::to_string(data).unwrap_or_default()),
        ExploreEvent::Error { message } => Event::default()
            .event("error")
            .data(serde_json::json!({"message": message}).to_string()),
    }
}

async fn explore(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ExploreRequest>,
) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

    // Spawn the pipeline work on a background task so the handler can return
    // the SSE stream immediately. The task sends events through the channel.
    tokio::spawn(async move {
        let vault_path = state.vault_path.clone();
        let generation = match state.generation() {
            Ok(generation) => generation,
            Err(message) => {
                let event = ExploreEvent::Error {
                    message: format!(
                        "LLM generation was not configured at server setup: {message}"
                    ),
                };
                let _ = tx.send(Ok(event_to_sse(&event))).await;
                return;
            }
        };

        // Check if pool is loaded and fresh
        let use_fast_path = {
            let pool_guard = state.pool.read().await;
            pool_guard.as_ref().map(|p| !p.is_stale()).unwrap_or(false)
        };

        let events = if use_fast_path {
            let pool_guard = state.pool.read().await;
            let pool = pool_guard.as_ref().unwrap();
            explore_pipeline::run_fast_pipeline(&vault_path, pool, generation, &request).await
        } else {
            explore_pipeline::run_standard_pipeline(&vault_path, generation, &request).await
        };

        for event in &events {
            if tx.send(Ok(event_to_sse(event))).await.is_err() {
                break; // Client disconnected
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/explore", post(explore))
}
