use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use enzyme_core::db::Database;

use crate::AppState;

async fn get_chunks(State(state): State<Arc<AppState>>) -> Result<Json<Value>, StatusCode> {
    let db = Database::open(&state.paths.vault_db).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = db
        .get_all_chunks_enriched()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let tags_map = db
        .get_all_doc_tags()
        .unwrap_or_default();

    let chunks: Vec<Value> = rows
        .into_iter()
        .map(|(doc_id, chunk_id, chunk_index, file_path, content, chunk_word_count, created_at, modified_at, title)| {
            let tags = tags_map.get(&doc_id).cloned().unwrap_or_default();
            json!({
                "chunk_id": chunk_id,
                "doc_id": doc_id,
                "chunk_index": chunk_index,
                "file_path": file_path,
                "content": content,
                "chunk_word_count": chunk_word_count,
                "created_at": created_at,
                "modified_at": modified_at,
                "title": title,
                "tags": tags,
            })
        })
        .collect();

    Ok(Json(json!({ "chunks": chunks })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/chunks", get(get_chunks))
}
