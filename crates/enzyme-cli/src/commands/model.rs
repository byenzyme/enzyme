//! `enzyme model` — install, list, and remove local GGUF weights.
//!
//! Deliberately three verbs and one line of output per action. The registry is
//! compiled in (`enzyme_core::llm::model_registry`); `--url` bypasses it.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use enzyme_core::llm::model_registry::{self, ModelEntry};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// What to fetch: a registry entry, or an ad-hoc URL from `--url`.
struct DownloadSpec {
    name: String,
    url: String,
    sha256: Option<String>,
    size: Option<u64>,
}

impl From<&ModelEntry> for DownloadSpec {
    fn from(entry: &ModelEntry) -> Self {
        Self {
            name: entry.name.to_string(),
            url: entry.url.to_string(),
            sha256: Some(entry.sha256.to_string()),
            size: Some(entry.size),
        }
    }
}

pub async fn run_install(
    name: Option<String>,
    url: Option<String>,
    sha256: Option<String>,
) -> Result<()> {
    let spec = match url {
        Some(url) => DownloadSpec {
            name: name.unwrap_or_else(|| model_registry::DEFAULT_MODEL.to_string()),
            url,
            sha256,
            size: None,
        },
        None => {
            let name = name.unwrap_or_else(|| model_registry::DEFAULT_MODEL.to_string());
            let entry = model_registry::lookup(&name).with_context(|| {
                format!(
                    "unknown model '{name}' (known: {}); pass --url to install an arbitrary GGUF",
                    model_registry::REGISTRY
                        .iter()
                        .map(|m| m.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            DownloadSpec::from(entry)
        }
    };

    let dest = model_registry::model_path(&spec.name)?;
    if dest.exists() {
        println!(
            "{} already installed ({})",
            spec.name,
            human_bytes(file_len(&dest))
        );
        return Ok(());
    }
    std::fs::create_dir_all(dest.parent().expect("model path has a parent"))
        .context("failed to create models directory")?;

    let part = dest.with_extension("gguf.part");
    let digest = download_resumable(&spec, &part).await?;

    if let Some(expected) = &spec.sha256 {
        if !digest.eq_ignore_ascii_case(expected) {
            // One bad byte range poisons the whole file; drop it so the next
            // run starts clean rather than resuming onto corruption.
            let _ = std::fs::remove_file(&part);
            bail!(
                "checksum mismatch for {} (expected {expected}, got {digest}) — partial download discarded, re-run to retry",
                spec.name
            );
        }
    }

    std::fs::rename(&part, &dest).context("failed to move downloaded model into place")?;
    println!("installed {} ({})", spec.name, human_bytes(file_len(&dest)));
    Ok(())
}

pub fn run_list() -> Result<()> {
    let installed = model_registry::installed();
    let active = model_registry::active().map(|(n, _)| n);

    if installed.is_empty() && model_registry::REGISTRY.is_empty() {
        println!("no models");
        return Ok(());
    }

    for name in &installed {
        let marker = if active.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        let size = model_registry::model_path(name)
            .map(|p| human_bytes(file_len(&p)))
            .unwrap_or_default();
        println!("{marker} {name}  {size}  installed");
    }
    for entry in model_registry::REGISTRY {
        if installed.iter().any(|n| n == entry.name) {
            continue;
        }
        println!(
            "  {}  {}  available — {}",
            entry.name,
            human_bytes(entry.size),
            entry.description
        );
    }
    Ok(())
}

pub fn run_rm(name: &str) -> Result<()> {
    let path = model_registry::model_path(name)?;
    if !path.exists() {
        bail!("{name} is not installed");
    }
    std::fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    println!("removed {name}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MB)
}

fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|v| std::env::var(v).ok())
        .filter(|t| !t.is_empty())
}

/// Rewrite a `github.com/<o>/<r>/releases/download/<tag>/<asset>` URL into the
/// authenticated API asset endpoint. Private-repo assets 404 on the plain
/// browser URL; the API endpoint with `Accept: application/octet-stream`
/// redirects to signed storage. Public models need no token and skip this.
async fn resolve_github_asset(client: &reqwest::Client, url: &str, token: &str) -> Result<String> {
    let Some(rest) = url.strip_prefix("https://github.com/") else {
        return Ok(url.to_string());
    };
    let parts: Vec<&str> = rest.split('/').collect();
    // <owner>/<repo>/releases/download/<tag>/<asset>
    if parts.len() < 6 || parts[2] != "releases" || parts[3] != "download" {
        return Ok(url.to_string());
    }
    let (owner, repo, tag, asset) = (parts[0], parts[1], parts[4], parts[5..].join("/"));

    #[derive(serde::Deserialize)]
    struct Release {
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        url: String,
    }

    let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}");
    let release: Release = client
        .get(&api)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "enzyme")
        .send()
        .await
        .context("failed to query GitHub release")?
        .error_for_status()
        .context("GitHub release lookup failed (bad token, or release not found)")?
        .json()
        .await
        .context("failed to parse GitHub release metadata")?;

    release
        .assets
        .into_iter()
        .find(|a| a.name == asset)
        .map(|a| a.url)
        .with_context(|| format!("release {tag} has no asset named {asset}"))
}

/// Stream to `part`, resuming with `Range` when the server allows it, and
/// return the lowercase-hex SHA-256 of the complete file.
async fn download_resumable(spec: &DownloadSpec, part: &Path) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("enzyme")
        .build()
        .context("failed to build HTTP client")?;

    let token = github_token();
    let (url, api_asset) = match &token {
        Some(t) => {
            let resolved = resolve_github_asset(&client, &spec.url, t).await?;
            let is_api = resolved != spec.url;
            (resolved, is_api)
        }
        None => (spec.url.clone(), false),
    };

    let mut have = file_len(part);
    let mut request = client.get(&url);
    if api_asset {
        request = request
            .header(
                "Authorization",
                format!("Bearer {}", token.as_deref().unwrap_or("")),
            )
            .header("Accept", "application/octet-stream");
    }
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("failed to download {}", spec.url))?
        .error_for_status()
        .with_context(|| format!("download failed for {}", spec.url))?;

    let resumed = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let total = spec.size.or_else(|| {
        response
            .content_length()
            .map(|len| len + if resumed { have } else { 0 })
    });

    let mut hasher = Sha256::new();
    let mut file = if resumed {
        // Fold the bytes already on disk into the digest before appending.
        let existing = tokio::fs::read(part)
            .await
            .context("failed to read partial download")?;
        hasher.update(&existing);
        tokio::fs::OpenOptions::new().append(true).open(part).await
    } else {
        have = 0;
        tokio::fs::File::create(part).await
    }
    .context("failed to open partial download file")?;

    let mut written = have;
    let mut last_tick = std::time::Instant::now();
    let mut stream = std::pin::pin!(response.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download stream error")?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("failed to write model data")?;
        written += chunk.len() as u64;
        if last_tick.elapsed() >= std::time::Duration::from_millis(250) {
            progress(&spec.name, written, total);
            last_tick = std::time::Instant::now();
        }
    }
    file.flush().await.ok();
    progress(&spec.name, written, total);
    if progress_enabled() {
        eprintln!();
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn progress_enabled() -> bool {
    atty::is(atty::Stream::Stderr)
}

/// Single self-overwriting line; silent when stderr is not a terminal.
fn progress(name: &str, done: u64, total: Option<u64>) {
    if !progress_enabled() {
        return;
    }
    match total {
        Some(total) if total > 0 => eprint!(
            "\r{name}  {} / {}  {:>3}%   ",
            human_bytes(done),
            human_bytes(total),
            done * 100 / total
        ),
        _ => eprint!("\r{name}  {}   ", human_bytes(done)),
    }
    let _ = std::io::stderr().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats_mb() {
        assert_eq!(human_bytes(105_454_080), "100.6 MB");
    }
}
