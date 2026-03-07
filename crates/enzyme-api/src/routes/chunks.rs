use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use enzyme_core::db::{Database, Repository};

use crate::AppState;

async fn get_chunks(State(state): State<Arc<AppState>>) -> Result<Json<Value>, StatusCode> {
    let db = Database::open(&state.paths.vault_db).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let repo = Repository::new(&db);

    let rows = repo
        .get_all_chunks_with_embeddings()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let chunks: Vec<Value> = rows
        .into_iter()
        .map(|(file_path, chunk_index, content)| {
            json!({
                "file_path": file_path,
                "chunk_index": chunk_index,
                "content": content,
            })
        })
        .collect();

    Ok(Json(json!({ "chunks": chunks })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/chunks", get(get_chunks))
}
