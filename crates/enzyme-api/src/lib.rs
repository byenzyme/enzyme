use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

mod explore_pipeline;
mod explore_pool;
mod prompts;
pub mod routes;
mod segment_parser;

use explore_pool::ExplorePool;

#[derive(Embed)]
#[folder = "../../ui/dist"]
struct UiAssets;

pub struct AppState {
    pub vault_path: PathBuf,
    pub paths: enzyme_core::config::paths::EnzymePaths,
    pub pool: RwLock<Option<ExplorePool>>,
}

fn mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

async fn serve_embedded(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    // Try the exact path, then fall back to index.html (SPA)
    let (file, resolved_path) = if let Some(file) = UiAssets::get(path) {
        (file, path.to_string())
    } else if let Some(file) = UiAssets::get("index.html") {
        (file, "index.html".to_string())
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mime = mime_for_path(&resolved_path);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CACHE_CONTROL,
                if resolved_path.starts_with("assets/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            ),
        ],
        file.data.to_vec(),
    )
        .into_response()
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .merge(routes::health::router())
        .merge(routes::chunks::router())
        .merge(routes::explore::router())
        .merge(routes::warm::router())
        .merge(routes::steer::router())
        .merge(routes::preview::router())
        .merge(routes::notes::router())
        .with_state(state)
        .layer(cors)
        .fallback(serve_embedded)
}

pub async fn serve(vault_path: PathBuf, port: u16) -> Result<()> {
    let paths = enzyme_core::config::paths::EnzymePaths::new(&vault_path)?;

    let state = Arc::new(AppState {
        vault_path,
        paths,
        pool: RwLock::new(None),
    });

    let app = build_router(state);

    let addr = format!("0.0.0.0:{port}");
    info!("enzyme-api listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("enzyme-api shut down");
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install ctrl+c handler");
}
