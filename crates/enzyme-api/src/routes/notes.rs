use std::path::{Component, Path};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::AppState;

/// Raw wire shape for `/notes/save`. Accepts the current `{filename, content}`
/// payload as well as the legacy `{query, markdown}` payload sent by older
/// (possibly cached) UI bundles.
#[derive(Deserialize)]
struct SaveNoteWire {
    filename: Option<String>,
    content: Option<String>,
    // Legacy field names (pre-0.5.4 UI bundle).
    query: Option<String>,
    markdown: Option<String>,
    subfolder: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Debug))]
#[serde(try_from = "SaveNoteWire")]
struct SaveNoteRequest {
    filename: String,
    content: String,
    subfolder: Option<String>,
}

impl TryFrom<SaveNoteWire> for SaveNoteRequest {
    type Error = String;

    fn try_from(wire: SaveNoteWire) -> Result<Self, Self::Error> {
        let filename = wire
            .filename
            .filter(|f| !f.trim().is_empty())
            .or_else(|| wire.query.as_deref().map(filename_from_query))
            .ok_or("missing field `filename` (or legacy `query`)")?;

        let content = wire
            .content
            .or(wire.markdown)
            .ok_or("missing field `content` (or legacy `markdown`)")?;

        Ok(Self {
            filename,
            content,
            subfolder: wire.subfolder,
        })
    }
}

/// Derive a safe markdown filename from a free-form query string.
/// Mirrors `filenameFromQuery` in `ui/src/lib/api.ts`.
fn filename_from_query(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = cleaned.chars().take(96).collect();
    let name = truncated.trim();

    if name.is_empty() {
        "Exploration.md".to_string()
    } else {
        format!("{name}.md")
    }
}

/// Whether `path` stays inside the directory it is joined onto: relative, with
/// only ordinary components. Rejects absolute paths, `..`, and drive prefixes,
/// any of which would let a request write outside the vault.
fn is_contained(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
        && path.components().next().is_some()
}

async fn save_note(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SaveNoteRequest>,
) -> Result<Json<Value>, StatusCode> {
    let filename = Path::new(&request.filename);
    if !is_contained(filename) || filename.components().count() != 1 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(subfolder) = &request.subfolder
        && !is_contained(Path::new(subfolder))
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut target_dir = state.vault_path.clone();

    if let Some(subfolder) = &request.subfolder {
        target_dir = target_dir.join(subfolder);
    }

    // Ensure directory exists
    std::fs::create_dir_all(&target_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let file_path = target_dir.join(&request.filename);

    std::fs::write(&file_path, &request.content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let relative_path = request
        .subfolder
        .as_ref()
        .map(|sf| format!("{sf}/{}", request.filename))
        .unwrap_or_else(|| request.filename.clone());

    info!(path = %file_path.display(), "Saved note");

    Ok(Json(json!({
        "success": true,
        "file_path": file_path.to_string_lossy(),
        "relative_path": relative_path,
    })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/notes/save", post(save_note))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contained_paths_stay_inside_the_vault() {
        assert!(is_contained(Path::new("My Note.md")));
        assert!(is_contained(Path::new("Syntheses/2026")));
        assert!(!is_contained(Path::new("")));
        assert!(!is_contained(Path::new("../outside.md")));
        assert!(!is_contained(Path::new("Syntheses/../../outside")));
        assert!(!is_contained(Path::new("/home/user/.ssh/authorized_keys")));
        assert!(!is_contained(Path::new("./note.md")));
    }

    #[test]
    fn deserializes_current_payload_shape() {
        let req: SaveNoteRequest = serde_json::from_value(json!({
            "filename": "My Note.md",
            "content": "# Hello",
            "subfolder": "Syntheses",
        }))
        .unwrap();

        assert_eq!(req.filename, "My Note.md");
        assert_eq!(req.content, "# Hello");
        assert_eq!(req.subfolder.as_deref(), Some("Syntheses"));
    }

    #[test]
    fn deserializes_legacy_query_markdown_shape() {
        let req: SaveNoteRequest = serde_json::from_value(json!({
            "query": "how do enzymes fold?",
            "markdown": "# Synthesis\n\nbody",
        }))
        .unwrap();

        assert_eq!(req.filename, "how do enzymes fold.md");
        assert_eq!(req.content, "# Synthesis\n\nbody");
        assert_eq!(req.subfolder, None);
    }

    #[test]
    fn explicit_filename_wins_over_legacy_query() {
        let req: SaveNoteRequest = serde_json::from_value(json!({
            "filename": "explicit.md",
            "content": "new",
            "query": "legacy query",
            "markdown": "old",
        }))
        .unwrap();

        assert_eq!(req.filename, "explicit.md");
        assert_eq!(req.content, "new");
    }

    #[test]
    fn rejects_payload_missing_both_shapes() {
        let err = serde_json::from_value::<SaveNoteRequest>(json!({
            "content": "orphaned body",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("filename"));

        let err = serde_json::from_value::<SaveNoteRequest>(json!({
            "query": "a query with no body",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("content"));
    }

    #[test]
    fn legacy_query_is_sanitized_into_a_safe_filename() {
        let req: SaveNoteRequest = serde_json::from_value(json!({
            "query": "  what/about\\paths: *weird* \"chars\"? <ok|no>\t",
            "markdown": "body",
        }))
        .unwrap();

        assert_eq!(req.filename, "what about paths weird chars ok no.md");

        let req: SaveNoteRequest = serde_json::from_value(json!({
            "query": "   ",
            "markdown": "body",
        }))
        .unwrap();
        assert_eq!(req.filename, "Exploration.md");
    }
}
