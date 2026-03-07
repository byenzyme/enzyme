use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use tokio_stream::wrappers::ReceiverStream;

use crate::explore_pipeline::{self, ExploreEvent, ExploreRequest};
use crate::AppState;

fn event_to_sse(event: &ExploreEvent) -> Event {
    let (event_type, data) = match event {
        ExploreEvent::Thinking { .. } => ("explore_thinking", serde_json::to_string(event)),
        ExploreEvent::Search(_) => ("explore_search", serde_json::to_string(event)),
        ExploreEvent::SynthesisSegment(_) => {
            ("explore_synthesis_segment", serde_json::to_string(event))
        }
        ExploreEvent::Synthesis { .. } => ("explore_synthesis", serde_json::to_string(event)),
        ExploreEvent::Complete(_) => ("explore_complete", serde_json::to_string(event)),
        ExploreEvent::Error { .. } => ("error", serde_json::to_string(event)),
    };

    let data_str = data.unwrap_or_else(|_| "{}".to_string());
    Event::default().event(event_type).data(data_str)
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

        // Check if pool is loaded and fresh
        let use_fast_path = {
            let pool_guard = state.pool.read().await;
            pool_guard
                .as_ref()
                .map(|p| !p.is_stale())
                .unwrap_or(false)
        };

        let events = if use_fast_path {
            let pool_guard = state.pool.read().await;
            let pool = pool_guard.as_ref().unwrap();
            explore_pipeline::run_fast_pipeline(&vault_path, pool, &request).await
        } else {
            explore_pipeline::run_standard_pipeline(&vault_path, &request).await
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
