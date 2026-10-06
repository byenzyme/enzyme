//! `enzyme model` — install, list, and remove local GGUF weights.
//!
//! The registry is compiled in (`enzyme_core::llm::model_registry`); `--url`
//! bypasses it. Provider mode and model selection live in global config.toml.

use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result, bail};
use enzyme_core::config::enzyme_config::{CatalystMode, EnzymeConfig};
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
        select_model(&spec.name, None)?;
        println!(
            "{} already installed and selected ({})",
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
    select_model(&spec.name, None)?;
    let mode = EnzymeConfig::catalyst_llm_config()?.mode;
    println!(
        "installed and selected {} ({}) — catalyst mode: {mode}",
        spec.name,
        human_bytes(file_len(&dest))
    );
    Ok(())
}

pub const MODELS_SCHEMA: &str = "enzyme.models.v1";

pub fn run_list(json_output: bool) -> Result<()> {
    let installed = model_registry::installed();
    let mode = EnzymeConfig::catalyst_llm_config()?.mode;
    let active = if mode == CatalystMode::Hosted {
        None
    } else {
        model_registry::active().map(|(n, _)| n)
    };

    if json_output {
        let entry_json = |name: &str| -> Result<serde_json::Value> {
            let entry = model_registry::REGISTRY.iter().find(|e| e.name == name);
            let path = model_registry::model_path(name)?;
            let is_installed = installed.iter().any(|n| n == name);
            Ok(serde_json::json!({
                "name": name,
                "installed": is_installed,
                "active": active.as_deref() == Some(name),
                "path": is_installed.then(|| path.display().to_string()),
                "size_bytes": if is_installed { Some(file_len(&path)) } else { entry.map(|e| e.size) },
                "registry": entry.is_some(),
                "url": entry.map(|e| e.url),
                "sha256": entry.map(|e| e.sha256),
                "context_window": entry.map(|e| e.context_window),
                "description": entry.map(|e| e.description),
            }))
        };
        let mut models = Vec::new();
        for name in &installed {
            models.push(entry_json(name)?);
        }
        for entry in model_registry::REGISTRY {
            if !installed.iter().any(|n| n == entry.name) {
                models.push(entry_json(entry.name)?);
            }
        }
        let output = serde_json::json!({
            "schema": MODELS_SCHEMA,
            "mode": mode.to_string(),
            "selected": model_registry::selected_name(),
            "active": active,
            "models_dir": model_registry::models_dir()?.display().to_string(),
            "models": models,
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

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
    let mut llm = EnzymeConfig::catalyst_llm_config()?;
    if llm.mode == CatalystMode::Local && llm.local_model.as_deref() == Some(name) {
        bail!(
            "{name} is the required local catalyst model; run `enzyme model disable` or `enzyme model auto` before removing it"
        );
    }
    std::fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    if llm.local_model.as_deref() == Some(name) {
        llm.local_model = None;
        EnzymeConfig::save_catalyst_llm_config(llm)?;
    }
    println!("removed {name}");
    Ok(())
}

pub fn run_use(name: &str) -> Result<()> {
    let path = model_registry::model_path(name)?;
    if !path.exists() {
        bail!("{name} is not installed; run `enzyme model install {name}` first");
    }
    select_model(name, Some(CatalystMode::Local))?;
    println!("catalyst mode: local ({name})");
    Ok(())
}

pub fn run_auto() -> Result<()> {
    let mut llm = EnzymeConfig::catalyst_llm_config()?;
    llm.mode = CatalystMode::Auto;
    EnzymeConfig::save_catalyst_llm_config(llm)?;
    println!("catalyst mode: auto (local when selected model is installed, otherwise hosted)");
    Ok(())
}

pub fn run_disable() -> Result<()> {
    let mut llm = EnzymeConfig::catalyst_llm_config()?;
    llm.mode = CatalystMode::Hosted;
    EnzymeConfig::save_catalyst_llm_config(llm)?;
    println!("catalyst mode: hosted (local model disabled)");
    Ok(())
}

pub fn run_status() -> Result<()> {
    let llm = EnzymeConfig::catalyst_llm_config()?;
    let selected = model_registry::selected_name();
    let installed = model_registry::model_path(&selected)?.exists();
    let effective = match llm.mode {
        CatalystMode::Local => {
            if installed {
                "local"
            } else {
                "local (model missing)"
            }
        }
        CatalystMode::Hosted => "hosted",
        CatalystMode::Auto => {
            if installed {
                "local"
            } else {
                "hosted"
            }
        }
    };
    println!("Configured: {}", llm.mode);
    println!("Effective:  {effective}");
    println!(
        "Model:      {selected} ({})",
        if installed {
            "installed"
        } else {
            "not installed"
        }
    );
    println!(
        "Network:    {}",
        if effective == "local" {
            "not required"
        } else {
            "required"
        }
    );
    Ok(())
}

pub fn run_verify(name: Option<&str>) -> Result<()> {
    let name = name
        .map(str::to_string)
        .unwrap_or_else(model_registry::selected_name);
    let path = model_registry::model_path(&name)?;
    if !path.exists() {
        bail!("{name} is not installed; run `enzyme model install {name}`");
    }
    verify_gguf_header(&path)?;
    let Some(entry) = model_registry::lookup(&name) else {
        println!(
            "verified {name}: readable GGUF file ({}) — no registry checksum",
            human_bytes(file_len(&path))
        );
        return Ok(());
    };
    let digest = sha256_file(&path)?;
    if digest != entry.sha256 {
        bail!(
            "checksum mismatch for {name} (expected {}, got {digest}); remove and reinstall the model",
            entry.sha256
        );
    }
    println!("verified {name} ({})", human_bytes(file_len(&path)));
    Ok(())
}

fn select_model(name: &str, mode: Option<CatalystMode>) -> Result<()> {
    let mut llm = EnzymeConfig::catalyst_llm_config()?;
    llm.local_model = Some(name.to_string());
    if let Some(mode) = mode {
        llm.mode = mode;
    }
    EnzymeConfig::save_catalyst_llm_config(llm)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_gguf_header(path: &Path) -> Result<()> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("{} is truncated", path.display()))?;
    if &magic != b"GGUF" {
        bail!("{} is not a GGUF model file", path.display());
    }
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
