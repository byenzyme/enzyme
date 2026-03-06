//! Async Unix socket daemon that keeps the ONNX embedding model warm.
//!
//! The server auto-exits after [`IDLE_TIMEOUT_SECS`] of inactivity and
//! handles SIGTERM/SIGINT/SIGHUP for graceful shutdown.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use super::backend::EmbeddingBackend;
use super::daemon_protocol::*;
use super::model_files::resolve_model_files;
use super::RtenBackend;

/// Shared daemon state.
pub struct DaemonServer {
    backend: Box<dyn EmbeddingBackend>,
    last_activity: Mutex<Instant>,
    shutdown: AtomicBool,
}

impl DaemonServer {
    /// Create a new server, loading the embedding model from `enzyme_home`.
    pub fn new(enzyme_home: &Path) -> Result<Self> {
        let model_files = resolve_model_files(enzyme_home)?;
        let backend = RtenBackend::load(&model_files.model_path, &model_files.tokenizer_path)?;
        info!("daemon: loaded RtenBackend");

        Ok(Self {
            backend: Box::new(backend),
            last_activity: Mutex::new(Instant::now()),
            shutdown: AtomicBool::new(false),
        })
    }

    /// Run the daemon, listening on `socket_path` and writing its PID to `pid_path`.
    ///
    /// Returns when the daemon shuts down (idle timeout or signal).
    pub async fn run(self: Arc<Self>, socket_path: &Path, pid_path: &Path) -> Result<()> {
        // Clean up stale socket if present.
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .with_context(|| format!("failed to remove stale socket: {}", socket_path.display()))?;
        }

        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("failed to bind Unix socket: {}", socket_path.display()))?;
        info!(socket = %socket_path.display(), "daemon: listening");

        // Write PID file.
        std::fs::write(pid_path, std::process::id().to_string())
            .with_context(|| format!("failed to write PID file: {}", pid_path.display()))?;

        let socket_path = socket_path.to_path_buf();
        let pid_path = pid_path.to_path_buf();

        // Spawn idle checker.
        let idle_server = Arc::clone(&self);
        tokio::spawn(async move {
            idle_checker(idle_server).await;
        });

        // Spawn signal handler.
        let signal_server = Arc::clone(&self);
        tokio::spawn(async move {
            signal_handler(signal_server).await;
        });

        // Accept loop.
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let conn_server = Arc::clone(&self);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(conn_server, stream).await {
                                    warn!("daemon: connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("daemon: accept error: {e}");
                        }
                    }
                }
                _ = shutdown_signal(&self) => {
                    info!("daemon: shutdown flag set, exiting accept loop");
                    break;
                }
            }
        }

        // Cleanup.
        Self::cleanup(&socket_path, &pid_path);
        info!("daemon: exited cleanly");
        Ok(())
    }

    /// Touch the last-activity timestamp.
    fn touch(&self) {
        if let Ok(mut t) = self.last_activity.lock() {
            *t = Instant::now();
        }
    }

    /// Dispatch a request to the appropriate handler.
    fn dispatch(&self, request: DaemonRequest) -> DaemonResponse {
        self.touch();

        match request {
            DaemonRequest::Ping => {
                debug!("dispatch: ping");
                DaemonResponse::Ping {
                    status: "ok".into(),
                    model_loaded: true,
                    backend: self.backend.name().into(),
                }
            }
            DaemonRequest::Embed { text } => {
                info!("dispatch: embed (len={})", text.len());
                match self.backend.embed(&text) {
                    Ok(embedding) => DaemonResponse::Embed { embedding },
                    Err(e) => DaemonResponse::Error {
                        error: e.to_string(),
                    },
                }
            }
            DaemonRequest::EmbedBatch { texts } => {
                info!("dispatch: embed_batch (count={})", texts.len());
                let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                match self.backend.embed_batch(&text_refs) {
                    Ok(embeddings) => DaemonResponse::EmbedBatch { embeddings },
                    Err(e) => DaemonResponse::Error {
                        error: e.to_string(),
                    },
                }
            }
            DaemonRequest::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                DaemonResponse::Shutdown {
                    status: "shutting_down".into(),
                }
            }
        }
    }

    /// Remove socket and PID files.
    fn cleanup(socket_path: &Path, pid_path: &Path) {
        if let Err(e) = std::fs::remove_file(socket_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("daemon: failed to remove socket: {e}");
            }
        }
        if let Err(e) = std::fs::remove_file(pid_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("daemon: failed to remove PID file: {e}");
            }
        }
    }
}

/// Handle a single client connection.
///
/// Supports two framing modes:
/// 1. Content-Length framing: `Content-Length: N\n{json}`
/// 2. Legacy newline framing: `{json}\n`
async fn handle_connection(
    server: Arc<DaemonServer>,
    stream: tokio::net::UnixStream,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    loop {
        let mut first_line = String::new();
        let n = reader.read_line(&mut first_line).await?;
        if n == 0 {
            // Client disconnected.
            break;
        }

        let first_line = first_line.trim();
        if first_line.is_empty() {
            continue;
        }

        let json_body = if first_line.starts_with("Content-Length:") {
            // Content-Length framing.
            let length: usize = first_line
                .strip_prefix("Content-Length:")
                .unwrap()
                .trim()
                .parse()
                .with_context(|| format!("invalid Content-Length: {first_line}"))?;

            let mut buf = vec![0u8; length];
            tokio::io::AsyncReadExt::read_exact(&mut reader, &mut buf).await?;
            String::from_utf8(buf)?
        } else {
            // Legacy newline framing — the first line IS the JSON.
            first_line.to_string()
        };

        debug!("daemon: received request: {json_body}");

        let request: DaemonRequest = serde_json::from_str(&json_body)
            .with_context(|| format!("invalid request JSON: {json_body}"))?;

        let response = server.dispatch(request);

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

/// Periodically checks idle time and sets the shutdown flag.
async fn idle_checker(server: Arc<DaemonServer>) {
    let interval = std::time::Duration::from_secs(IDLE_CHECK_INTERVAL_SECS);
    let timeout = std::time::Duration::from_secs(IDLE_TIMEOUT_SECS);

    loop {
        tokio::time::sleep(interval).await;

        if server.shutdown.load(Ordering::SeqCst) {
            break;
        }

        let idle_duration = server
            .last_activity
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default();

        if idle_duration >= timeout {
            info!(
                "daemon: idle for {}s, shutting down",
                idle_duration.as_secs()
            );
            server.shutdown.store(true, Ordering::SeqCst);
            break;
        }
    }
}

/// Wait for SIGTERM, SIGINT, or SIGHUP and set the shutdown flag.
async fn signal_handler(server: Arc<DaemonServer>) {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
    let mut sighup =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).unwrap();

    tokio::select! {
        _ = sigterm.recv() => info!("daemon: received SIGTERM"),
        _ = sigint.recv() => info!("daemon: received SIGINT"),
        _ = sighup.recv() => info!("daemon: received SIGHUP"),
    }

    server.shutdown.store(true, Ordering::SeqCst);
}

/// Future that resolves when the shutdown flag is set. Used in `tokio::select!`.
async fn shutdown_signal(server: &DaemonServer) {
    loop {
        if server.shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
