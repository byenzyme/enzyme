use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use enzyme_core::config::auth::require_login;
use enzyme_core::config::paths::EnzymePaths;

const DEFAULT_API_BASE: &str = "https://api.enzyme.garden";

fn api_base_url() -> String {
    std::env::var("ENZYME_API_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[allow(dead_code)]
struct PublishResponse {
    slug: String,
    mcp_url: String,
    search_url: String,
    doc_count: usize,
    catalyst_count: usize,
}

pub async fn run_publish(vault_path: &Path, json: bool) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Check auth
    let auth_token = require_login(&paths.auth_token())?;

    // Check vault DB exists
    if !paths.vault_db.exists() {
        bail!("No enzyme index found. Run `enzyme init` first.");
    }

    // Read the vault DB
    let db_bytes = std::fs::read(&paths.vault_db)
        .with_context(|| format!("failed to read {}", paths.vault_db.display()))?;
    let db_size_mb = db_bytes.len() as f64 / (1024.0 * 1024.0);

    // Gzip compress
    eprint!(
        "  {}Compressing {:.1} MB...{}",
        SetForegroundColor(Color::DarkGrey),
        db_size_mb,
        ResetColor,
    );
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&db_bytes)
        .context("gzip compression failed")?;
    let compressed = encoder.finish().context("gzip finalization failed")?;
    let compressed_mb = compressed.len() as f64 / (1024.0 * 1024.0);
    // Clear line and print result
    eprint!("\r\x1b[2K");
    eprintln!(
        "  {}Uploading {:.1} MB...{}",
        SetForegroundColor(Color::DarkGrey),
        compressed_mb,
        ResetColor,
    );

    // Derive vault name from the vault directory
    let vault_name = vault_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vault")
        .to_string();

    // Upload via multipart with progress
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let part = reqwest::multipart::Part::bytes(compressed)
        .file_name("enzyme.db.gz")
        .mime_str("application/gzip")?;

    let form = reqwest::multipart::Form::new()
        .text("vault_name", vault_name)
        .part("database", part);

    let publish_url = format!("{}/v1/publish", api_base_url());
    let resp = client
        .post(&publish_url)
        .header("Authorization", format!("Bearer {}", auth_token.api_key))
        .multipart(form)
        .send()
        .await
        .context("failed to upload vault")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("publish failed ({}): {}", status, body);
    }

    let publish_resp: PublishResponse = resp
        .json()
        .await
        .context("failed to parse publish response")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&publish_resp)?);
    } else {
        eprintln!();
        eprintln!(
            "  {}Published!{} Your vault is live at:",
            SetForegroundColor(Color::Green),
            ResetColor,
        );
        eprintln!(
            "    MCP:    {}{}{}",
            SetForegroundColor(crate::display::colors::ENZYME_PURPLE),
            publish_resp.mcp_url,
            ResetColor,
        );
        eprintln!();
    }

    Ok(())
}
