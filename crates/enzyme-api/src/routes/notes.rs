use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::AppState;

#[derive(Deserialize)]
struct SaveNoteRequest {
    filename: String,
    content: String,
    subfolder: Option<String>,
}

async fn save_note(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SaveNoteRequest>,
) -> Result<Json<Value>, StatusCode> {
    let mut target_dir = state.vault_path.clone();

    if let Some(subfolder) = &request.subfolder {
        target_dir = target_dir.join(subfolder);
    }

    // Ensure directory exists
    std::fs::create_dir_all(&target_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let file_path = target_dir.join(&request.filename);

    std::fs::write(&file_path, &request.content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let relative_path = request
        .subfolder
        .as_ref()
        .map(|sf| format!("{sf}/{}", request.filename))
        .unwrap_or_else(|| request.filename.clone());

    info!(path = %file_path.display(), "Saved note");

    Ok(Json(json!({
        "success": true,
        "file_path": file_path.to_string_lossy(),
        "relative_path": relative_path,
    })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/notes/save", post(save_note))
}
