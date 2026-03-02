//! Synchronous client that implements [`EmbeddingBackend`] by talking to the
//! embedding daemon over a Unix socket.
//!
//! If the daemon is not running, [`DaemonClient::ensure_running`] will
//! auto-spawn it transparently.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info};

use super::backend::EmbeddingBackend;
use super::daemon_protocol::*;
use crate::models::EMBEDDING_DIMENSION;

/// An [`EmbeddingBackend`] that delegates to the embedding daemon.
pub struct DaemonClient {
    socket_path: PathBuf,
    enzyme_home: PathBuf,
}

impl DaemonClient {
    /// Create a new client targeting the given socket.
    pub fn new(socket_path: PathBuf, enzyme_home: PathBuf) -> Self {
        Self {
            socket_path,
            enzyme_home,
        }
    }

    /// Try to connect to the daemon socket.
    fn try_connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("failed to connect to daemon socket: {}", self.socket_path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            .context("failed to set read timeout")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            .context("failed to set write timeout")?;
        Ok(stream)
    }

    /// Send a request and read the response.
    fn send_request(&self, request: &DaemonRequest) -> Result<DaemonResponse> {
        let mut stream = self.try_connect()?;

        let json = serde_json::to_string(request)?;

        // Write Content-Length framed request.
        write!(stream, "Content-Length: {}\n", json.len())?;
        stream.write_all(json.as_bytes())?;
        stream.flush()?;

        // Read newline-terminated JSON response.
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;

        let response: DaemonResponse = serde_json::from_str(line.trim())
            .with_context(|| format!("invalid daemon response: {line}"))?;
        Ok(response)
    }

    /// Check whether the daemon is alive.
    pub fn ping(&self) -> bool {
        self.send_request(&DaemonRequest::Ping).is_ok()
    }

    /// Ensure the daemon is running, spawning it if necessary.
    pub fn ensure_running(&self) -> Result<()> {
        if self.ping() {
            debug!("daemon: already running");
            return Ok(());
        }

        info!("daemon: not running, starting...");

        // Clean up stale files.
        let pid_path = self.enzyme_home.join(PID_FILENAME);
        self.cleanup_stale_files(&pid_path)?;

        // Spawn the daemon process.
        self.spawn_daemon()?;

        // Wait for it to become ready.
        self.wait_for_ready()
    }

    /// Remove stale socket and PID files if the daemon is not actually running.
    fn cleanup_stale_files(&self, pid_path: &Path) -> Result<()> {
        // If we can connect, it's not stale.
        if UnixStream::connect(&self.socket_path).is_ok() {
            return Ok(());
        }

        // Remove stale files.
        for path in [&self.socket_path, &pid_path.to_path_buf()] {
            if path.exists() {
                debug!("daemon: removing stale file: {}", path.display());
                let _ = std::fs::remove_file(path);
            }
        }

        Ok(())
    }

    /// Spawn `enzyme __daemon` as a detached background process.
    fn spawn_daemon(&self) -> Result<()> {
        let exe = std::env::current_exe().context("failed to determine current executable")?;
        let log_path = self.enzyme_home.join(LOG_FILENAME);

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open daemon log: {}", log_path.display()))?;

        let stderr_file = log_file.try_clone()?;

        let mut cmd = Command::new(exe);
        cmd.arg("__daemon")
            .stdout(log_file)
            .stderr(stderr_file);

        // Detach into its own process group so it survives parent exit.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let child = cmd.spawn().context("failed to spawn daemon process")?;
        info!("daemon: spawned child pid={}", child.id());

        Ok(())
    }

    /// Poll the daemon until it responds to a ping.
    fn wait_for_ready(&self) -> Result<()> {
        let delay = Duration::from_millis(AUTO_START_RETRY_MS);

        for attempt in 1..=AUTO_START_MAX_RETRIES {
            std::thread::sleep(delay);

            if self.ping() {
                info!("daemon: ready after {attempt} attempt(s)");
                return Ok(());
            }

            debug!("daemon: not ready yet (attempt {attempt}/{AUTO_START_MAX_RETRIES})");
        }

        anyhow::bail!(
            "daemon did not become ready after {} attempts ({:.1}s)",
            AUTO_START_MAX_RETRIES,
            (AUTO_START_MAX_RETRIES as f64 * AUTO_START_RETRY_MS as f64) / 1000.0,
        )
    }
}

impl EmbeddingBackend for DaemonClient {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let request = DaemonRequest::EmbedBatch {
            texts: texts.iter().map(|s| s.to_string()).collect(),
        };

        match self.send_request(&request)? {
            DaemonResponse::EmbedBatch { embeddings } => Ok(embeddings),
            DaemonResponse::Error { error } => {
                anyhow::bail!("daemon embed_batch error: {error}")
            }
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        }
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let request = DaemonRequest::Embed {
            text: text.to_string(),
        };

        match self.send_request(&request)? {
            DaemonResponse::Embed { embedding } => Ok(embedding),
            DaemonResponse::Error { error } => {
                anyhow::bail!("daemon embed error: {error}")
            }
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        }
    }

    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn name(&self) -> &str {
        "daemon"
    }
}
