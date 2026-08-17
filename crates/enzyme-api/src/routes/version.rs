use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;

async fn version() -> Json<Value> {
    let latest = env!("CARGO_PKG_VERSION");
    let minimum_supported =
        std::env::var("ENZYME_MINIMUM_SUPPORTED_VERSION").unwrap_or_else(|_| latest.to_string());
    Json(json!({
        "latest": latest,
        "minimum_supported": minimum_supported,
    }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/version", get(version))
}
