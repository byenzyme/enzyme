//! Wire protocol types shared between the daemon server and client.
//!
//! The protocol is wire-compatible with the Python Enzyme daemon:
//! - Request framing: `Content-Length: N\n{json}`
//! - Response framing: `{json}\n`
//!
//! Actions:
//! - `ping` — health check
//! - `embed` — embed a single text
//! - `embed_batch` — embed multiple texts
//! - `shutdown` — graceful shutdown

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Idle timeout before the daemon auto-exits (seconds).
pub const IDLE_TIMEOUT_SECS: u64 = 300;

/// How often the idle checker runs (seconds).
pub const IDLE_CHECK_INTERVAL_SECS: u64 = 10;

/// Per-request timeout when the client talks to the daemon (seconds).
pub const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Delay between auto-start retry attempts (milliseconds).
pub const AUTO_START_RETRY_MS: u64 = 250;

/// Maximum number of retry attempts when waiting for the daemon to become ready.
pub const AUTO_START_MAX_RETRIES: u32 = 20;

/// Socket filename within `~/.enzyme/`.
pub const SOCKET_FILENAME: &str = "embedding_daemon.sock";

/// PID filename within `~/.enzyme/`.
pub const PID_FILENAME: &str = "embedding_daemon.pid";

/// Log filename within `~/.enzyme/`.
pub const LOG_FILENAME: &str = "embedding_daemon.log";

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A request sent from the client to the daemon.
///
/// Serialized as internally-tagged JSON (`"action": "<variant>"`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Health check.
    Ping,
    /// Embed a single text.
    Embed { text: String },
    /// Embed a batch of texts.
    EmbedBatch { texts: Vec<String> },
    /// Ask the daemon to shut down.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A response sent from the daemon to the client.
///
/// Serialized as untagged JSON so each variant maps directly to the expected
/// top-level keys.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DaemonResponse {
    /// Reply to `ping`.
    Ping {
        status: String,
        model_loaded: bool,
        backend: String,
    },
    /// Reply to `embed`.
    Embed { embedding: Vec<f32> },
    /// Reply to `embed_batch`.
    EmbedBatch { embeddings: Vec<Vec<f32>> },
    /// Reply to `shutdown`.
    Shutdown { status: String },
    /// Error response.
    Error { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ping_roundtrip() {
        let req = DaemonRequest::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""action":"ping"#));
        let _: DaemonRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn request_embed_roundtrip() {
        let req = DaemonRequest::Embed {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""action":"embed"#));
        let _: DaemonRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn request_embed_batch_roundtrip() {
        let req = DaemonRequest::EmbedBatch {
            texts: vec!["a".into(), "b".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""action":"embed_batch"#));
        let _: DaemonRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn request_shutdown_roundtrip() {
        let req = DaemonRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""action":"shutdown"#));
        let _: DaemonRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn response_ping_roundtrip() {
        let resp = DaemonResponse::Ping {
            status: "ok".into(),
            model_loaded: true,
            backend: "ort".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"ok"#));
        // Untagged deserialization — need to specify the type
        let _: DaemonResponse = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn response_embed_roundtrip() {
        let resp = DaemonResponse::Embed {
            embedding: vec![0.1, 0.2, 0.3],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let _: DaemonResponse = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn response_error_roundtrip() {
        let resp = DaemonResponse::Error {
            error: "something broke".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("something broke"));
        let _: DaemonResponse = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn constants_are_sane() {
        assert_eq!(IDLE_TIMEOUT_SECS, 300);
        assert_eq!(IDLE_CHECK_INTERVAL_SECS, 10);
        assert_eq!(REQUEST_TIMEOUT_SECS, 30);
        assert_eq!(AUTO_START_RETRY_MS, 250);
        assert_eq!(AUTO_START_MAX_RETRIES, 20);
    }
}
