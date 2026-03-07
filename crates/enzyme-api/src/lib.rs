use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

mod explore_pipeline;
mod explore_pool;
mod prompts;
pub mod routes;
mod segment_parser;

use explore_pool::ExplorePool;

pub struct AppState {
    pub vault_path: PathBuf,
    pub paths: enzyme_core::config::paths::EnzymePaths,
    pub pool: RwLock<Option<ExplorePool>>,
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
    axum::serve(listener, app).await?;

    Ok(())
}
