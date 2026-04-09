use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::info;

/// Resolved paths to the embedding model files.
#[derive(Debug, Clone)]
pub struct ModelFiles {
    /// Path to the ONNX model file (e.g., `model.onnx` or `model.rten`).
    pub model_path: PathBuf,
    /// Path to the tokenizer JSON file.
    pub tokenizer_path: PathBuf,
}

/// Resolved paths for a distilled embedding model directory.
#[cfg(feature = "model2vec")]
#[derive(Debug, Clone)]
pub struct DistilledModelFiles {
    /// Directory containing `model.safetensors`, `tokenizer.json`, `config.json`.
    pub model_dir: PathBuf,
}

/// Search for distilled embedding model files (safetensors + tokenizer + config).
///
/// Search order:
/// 1. `ENZYME_MODEL_DIR` env var
/// 2. `{enzyme_home}/models/`
/// 3. `~/.enzyme/models/`
#[cfg(feature = "model2vec")]
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

/// Search for the ONNX model and tokenizer files required by the rten embedding
/// backend.
///
/// The search order is:
///
/// 1. `{enzyme_home}/models/model.rten`  + `tokenizer.json`
/// 2. `{enzyme_home}/models/model.onnx`  + `tokenizer.json`
/// 3. `~/.enzyme/models/model.rten`      + `tokenizer.json`  (fallback when
///    `enzyme_home` differs from `~/.enzyme`)
/// 4. `~/.enzyme/models/model.onnx`      + `tokenizer.json`
///
/// The first candidate pair where **both** files exist on disk is returned.
pub fn resolve_model_files(enzyme_home: &Path) -> Result<ModelFiles> {
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;
    let global_enzyme = home_dir.join(".enzyme");

    let mut candidates: Vec<PathBuf> = Vec::new();

    // Highest priority — explicit override via ENZYME_MODEL_DIR (used by the
    // bundled plugin wrapper to point at plugin/models/).
    if let Ok(model_dir) = std::env::var("ENZYME_MODEL_DIR") {
        if !model_dir.is_empty() {
            candidates.push(PathBuf::from(model_dir));
        }
    }

    // Primary locations — inside the given enzyme_home.
    candidates.push(enzyme_home.join("models"));

    // Fallback — the global ~/.enzyme directory, but only when it is a
    // different path from the one we already checked.
    if enzyme_home != global_enzyme {
        candidates.push(global_enzyme.join("models"));
    }

    for models_dir in &candidates {
        for model_name in &["model.rten", "model.onnx"] {
            let model_path = models_dir.join(model_name);
            let tokenizer_path = models_dir.join("tokenizer.json");

            if model_path.exists() && tokenizer_path.exists() {
                info!(
                    model = %model_path.display(),
                    tokenizer = %tokenizer_path.display(),
                    "Found embedding model files"
                );
                return Ok(ModelFiles {
                    model_path,
                    tokenizer_path,
                });
            }
        }
    }

    let global_models = home_dir.join(".enzyme").join("models");

    bail!(
        "Could not find embedding model files. Please place model files in one of:\n  \
         - {enzyme_home}/models/model.rten + tokenizer.json\n  \
         - {enzyme_home}/models/model.onnx + tokenizer.json\n  \
         - {global}/models/model.rten + tokenizer.json\n  \
         - {global}/models/model.onnx + tokenizer.json",
        enzyme_home = enzyme_home.display(),
        global = global_models.parent().unwrap_or(&global_models).display(),
    );
}
