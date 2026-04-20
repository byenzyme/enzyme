use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    let vault_configured = state.paths.vault_db.exists();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "vault_configured": vault_configured,
    }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/health", get(health))
}
