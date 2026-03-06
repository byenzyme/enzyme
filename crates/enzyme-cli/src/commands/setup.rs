use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const MODEL_URL: &str =
    "https://github.com/jshph/enzyme/releases/latest/download/model.onnx";
const TOKENIZER_URL: &str =
    "https://github.com/jshph/enzyme/releases/latest/download/tokenizer.json";

/// Bump this when swapping to a new model so existing installs re-download.
const MODEL_VERSION: &str = "arctic-xs-v1";

fn models_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("ENZYME_MODEL_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".enzyme").join("models"))
}

fn model_present(dir: &Path) -> bool {
    if !dir.join("model.onnx").exists() || !dir.join("tokenizer.json").exists() {
        return false;
    }
    // Check version marker — forces re-download when model changes.
    match std::fs::read_to_string(dir.join(".model_version")) {
        Ok(v) => v.trim() == MODEL_VERSION,
        Err(_) => false,
    }
}

async fn download_file(url: &str, dest: &Path) -> Result<()> {
    eprintln!(
        "  Downloading {}...",
        dest.file_name().unwrap_or_default().to_string_lossy()
    );

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .context("Failed to start download")?
        .error_for_status()
        .context("Download request failed")?;

    let total = resp.content_length();
    let bytes = resp.bytes().await.context("Failed to read response body")?;

    tokio::fs::write(dest, &bytes)
        .await
        .context("Failed to write file")?;

    if let Some(total) = total {
        eprintln!(
            "  Saved {} ({:.1} MB)",
            dest.file_name().unwrap_or_default().to_string_lossy(),
            total as f64 / 1_048_576.0
        );
    }

    Ok(())
}

pub async fn run_setup(check: bool) -> Result<()> {
    let dir = models_dir()?;

    if model_present(&dir) {
        if !check {
            println!("Model OK — {}", dir.display());
        }
        return Ok(());
    }

    if check {
        std::process::exit(1);
    }

    eprintln!("Downloading embedding model to {}...", dir.display());

    tokio::fs::create_dir_all(&dir)
        .await
        .context("Failed to create models directory")?;

    download_file(MODEL_URL, &dir.join("model.onnx")).await?;
    download_file(TOKENIZER_URL, &dir.join("tokenizer.json")).await?;

    // Write version marker so future upgrades can detect stale models.
    std::fs::write(dir.join(".model_version"), MODEL_VERSION)
        .context("Failed to write model version marker")?;

    println!("Model OK — {}", dir.display());

    Ok(())
}
