use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::info;

/// Resolved paths for the distilled embedding model directory.
#[derive(Debug, Clone)]
pub struct DistilledModelFiles {
    /// Directory containing `model.safetensors`, `tokenizer.json`, `config.json`.
    pub model_dir: PathBuf,
}

/// Search for distilled embedding model files (safetensors + tokenizer + config).
///
/// Search order:
/// 1. `ENZYME_MODEL_DIR` env var
/// 2. `{enzyme_home}/models/distilled/`
/// 3. `~/.enzyme/models/distilled/`
pub fn resolve_distilled_model_files(enzyme_home: &Path) -> Result<DistilledModelFiles> {
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;
    let global_enzyme = home_dir.join(".enzyme");

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(model_dir) = std::env::var("ENZYME_MODEL_DIR") {
        if !model_dir.is_empty() {
            candidates.push(PathBuf::from(model_dir));
        }
    }

    candidates.push(enzyme_home.join("models").join("distilled"));

    if enzyme_home != global_enzyme {
        candidates.push(global_enzyme.join("models").join("distilled"));
    }

    for model_dir in &candidates {
        let safetensors = model_dir.join("model.safetensors");
        let tokenizer = model_dir.join("tokenizer.json");
        let config = model_dir.join("config.json");

        if safetensors.exists() && tokenizer.exists() && config.exists() {
            info!(
                model_dir = %model_dir.display(),
                "Found embedding model files"
            );
            return Ok(DistilledModelFiles {
                model_dir: model_dir.clone(),
            });
        }
    }

    bail!(
        "Could not find embedding model files. Please place model.safetensors, \
         tokenizer.json, and config.json in one of:\n  \
         - {enzyme_home}/models/distilled/\n  \
         - {global}/models/distilled/",
        enzyme_home = enzyme_home.display(),
        global = global_enzyme.display(),
    );
}
