use std::fs;
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::embedding::get_embedding_service;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
struct EmbeddedEntry {
    id: String,
    text_hash: String,
    vector: Vec<f32>,
}

#[derive(Serialize)]
struct EmbedEntriesResponse {
    model: String,
    dimension: usize,
    items: Vec<EmbeddedEntry>,
}

pub fn run_embed_entries(input: Option<&Path>) -> Result<()> {
    let raw = match input {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
        None => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("failed to read entries JSON from stdin")?;
            buf
        }
    };

    let parsed: Value = serde_json::from_str(&raw).context("failed to parse entries JSON")?;
    let entries = extract_entries(&parsed)?;
    if entries.is_empty() {
        bail!("no entries found; expected an array or an object with an entries array");
    }

    let ids: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| entry_id(entry, i))
        .collect();
    let texts: Vec<String> = entries.iter().map(entry_text).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();

    let home = EnzymePaths::enzyme_home()?;
    let service = get_embedding_service(&home)?;
    let vectors = service.embed_documents(&refs)?;

    let items = ids
        .into_iter()
        .zip(texts)
        .zip(vectors)
        .map(|((id, text), vector)| EmbeddedEntry {
            id,
            text_hash: text_hash(&text),
            vector,
        })
        .collect();

    let response = EmbedEntriesResponse {
        model: service.backend_name().to_string(),
        dimension: service.dimension(),
        items,
    };

    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn extract_entries(value: &Value) -> Result<Vec<Value>> {
    if let Some(entries) = value.as_array() {
        return Ok(entries.clone());
    }

    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        return Ok(entries.clone());
    }

    if let Some(entry) = value.get("entry") {
        return Ok(vec![entry.clone()]);
    }

    bail!("expected an array, {{\"entries\": [...]}} or {{\"entry\": ...}}")
}

fn entry_id(entry: &Value, index: usize) -> String {
    for key in ["id", "source", "source_ref", "url", "title"] {
        if let Some(s) = entry.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    format!("entry-{index}")
}

fn entry_text(entry: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["title", "content", "notes", "body", "comment"] {
        if let Some(s) = entry.get(key).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed);
            }
        }
    }
    parts.join("\n\n")
}

fn text_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}
