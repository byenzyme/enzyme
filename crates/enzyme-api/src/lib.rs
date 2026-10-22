use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use tokio::sync::RwLock;
use tracing::{info, warn};

mod explore_pipeline;
mod explore_pool;
mod prompts;
pub mod routes;
mod segment_parser;

use enzyme_core::llm::{LlmClient, resolve_llm_client};

use explore_pool::ExplorePool;

pub(crate) struct GenerationContext {
    pub client: LlmClient,
    pub model: String,
}

enum GenerationSetup {
    Ready(GenerationContext),
    Unavailable(String),
}

impl GenerationSetup {
    fn get(&self) -> Result<&GenerationContext, &str> {
        match self {
            Self::Ready(generation) => Ok(generation),
            Self::Unavailable(message) => Err(message),
        }
    }
}

// `ui/dist` is a build artifact (`npm run build` in `ui/`) and is not tracked,
// so a fresh clone must still compile: a missing folder embeds nothing and
// `serve` answers the UI routes with a build hint instead.
#[derive(Embed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct UiAssets;

pub struct AppState {
    pub vault_path: PathBuf,
    pub paths: enzyme_core::config::paths::EnzymePaths,
    pub pool: RwLock<Option<ExplorePool>>,
    generation: GenerationSetup,
}

impl AppState {
    pub(crate) fn generation(&self) -> Result<&GenerationContext, &str> {
        self.generation.get()
    }
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
        return (
            StatusCode::NOT_FOUND,
            "Explore UI assets are not built into this binary; run `npm run build` in ui/ and rebuild.",
        )
            .into_response();
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

/// Whether a `Host` header names this machine's loopback interface. The UI and
/// the Vite dev proxy both reach the server as localhost; any other name means
/// a page on another origin resolved its own domain to 127.0.0.1 (DNS
/// rebinding) and is trying to read or write the vault through this server.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

async fn require_loopback_host(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_host);
    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            "enzyme serve only answers requests addressed to localhost",
        )
            .into_response();
    }
    next.run(request).await
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::version::router())
        .merge(routes::chunks::router())
        .merge(routes::explore::router())
        .merge(routes::warm::router())
        .merge(routes::steer::router())
        .merge(routes::preview::router())
        .merge(routes::notes::router())
        .with_state(state)
        .fallback(serve_embedded)
        .layer(middleware::from_fn(require_loopback_host))
}

pub async fn serve(vault_path: PathBuf, port: u16) -> Result<()> {
    let paths = enzyme_core::config::paths::EnzymePaths::new(&vault_path)?;
    let generation = match resolve_llm_client().await {
        Ok((config, client)) => {
            info!(model = config.model, "LLM generation configured");
            GenerationSetup::Ready(GenerationContext {
                client,
                model: config.model,
            })
        }
        Err(error) => {
            let message = format!("{error:#}");
            warn!("LLM generation unavailable at server setup: {message}");
            GenerationSetup::Unavailable(message)
        }
    };

    let state = Arc::new(AppState {
        vault_path,
        paths,
        pool: RwLock::new(None),
        generation,
    });

    let app = build_router(state);

    // Loopback only: the server reads and writes the vault without
    // authentication, so it must not be reachable from the network.
    let addr = format!("127.0.0.1:{port}");
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

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    #[test]
    fn accepts_only_loopback_host_headers() {
        for host in [
            "localhost",
            "localhost:8765",
            "127.0.0.1:8765",
            "[::1]:8765",
            "LOCALHOST:5173",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8765",
            "127.0.0.1.evil.example",
            "192.168.1.5:8765",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }
}
