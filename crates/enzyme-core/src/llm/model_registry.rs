//! Compiled-in registry of local GGUF models plus on-disk resolution.
//!
//! Weights live at `<ENZYME_HOME>/models/<name>.gguf` (flat, one file per
//! registry name) — the same `~/.enzyme` global state directory the rest of
//! the CLI uses. `enzyme model install` writes them; the in-process engine
//! (feature `local-llm`) reads them.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::enzyme_config::EnzymeConfig;
use crate::config::paths::EnzymePaths;

/// One blessed model. Registry entries are compiled in; `--url` on
/// `enzyme model install` bypasses this table entirely.
#[derive(Debug, Clone, Copy)]
pub struct ModelEntry {
    /// Short install name (`enzyme model install <name>`).
    pub name: &'static str,
    /// Download URL. GitHub release-asset URLs are resolved through the API
    /// when `GITHUB_TOKEN`/`GH_TOKEN` is set, so private releases work too.
    pub url: &'static str,
    /// Expected SHA-256 of the finished file (lowercase hex).
    pub sha256: &'static str,
    /// Expected size in bytes (progress display only; sha256 is the gate).
    pub size: u64,
    /// Training/serving context window in tokens.
    pub context_window: u32,
    /// One-line description for `enzyme model list`.
    pub description: &'static str,
}

/// Model installed by `enzyme model install` with no arguments.
pub const DEFAULT_MODEL: &str = "catalyst-135m-v3";

/// Blessed models.
///
/// Model metadata must match the tracked `models/<name>.release.json` manifest.
/// `.github/workflows/publish-model.yml` checksum-gates the cleared private
/// source artifact and publishes it to the public release URL below.
pub const REGISTRY: &[ModelEntry] = &[ModelEntry {
    name: DEFAULT_MODEL,
    // IQ4_NL + in-domain imatrix: matches Q4_K_M quality (paired lexical
    // -0.003 on the 60-example GGUF gate) at Q4_0-class speed (+48% prefill
    // via AVX-VNNI/ARM repack paths, which K-quants miss on this model's
    // 576-dim tensors) and 19% smaller. See score-report-iq4nl.json.
    url: "https://github.com/useenzyme/enzyme/releases/download/models-catalyst-v3/enzyme-catalyst-135m-v3.gguf",
    sha256: "710a79eea9a84b22cc18b8626b9d893e1a5a2de61f8852fcef839099e8138eab",
    size: 91_727_072,
    context_window: 8192,
    description: "SmolLM2-135M catalyst fine-tune (v3 DPO, IQ4_NL imatrix)",
}];

/// Look up a registry entry by name.
pub fn lookup(name: &str) -> Option<&'static ModelEntry> {
    REGISTRY.iter().find(|m| m.name == name)
}

/// Directory holding installed weights (`<ENZYME_HOME>/models`).
pub fn models_dir() -> Result<PathBuf> {
    Ok(EnzymePaths::enzyme_home()?.join("models"))
}

/// Path an installed model occupies, whether or not it exists.
pub fn model_path(name: &str) -> Result<PathBuf> {
    Ok(model_path_in(&models_dir()?, name))
}

/// Path for a named model in an explicitly selected model store.
pub fn model_path_in(models_dir: &Path, name: &str) -> PathBuf {
    models_dir.join(format!("{name}.gguf"))
}

/// Names of every `.gguf` in the models directory, sorted.
pub fn installed() -> Vec<String> {
    let Ok(dir) = models_dir() else {
        return Vec::new();
    };
    installed_in(&dir)
}

/// Names of installed GGUF models in an explicit model store.
pub fn installed_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            (path.extension().is_some_and(|x| x == "gguf"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

/// The installed model the local engine would load.
///
/// `ENZYME_LOCAL_MODEL_NAME` wins, followed by the persisted `[llm]` selection,
/// the default model, and finally the first installed model for backward
/// compatibility with pre-mode custom installs.
pub fn active() -> Option<(String, PathBuf)> {
    let models_dir = models_dir().ok()?;
    let config_path = EnzymeConfig::config_path().ok()?;
    let env_pick = std::env::var("ENZYME_LOCAL_MODEL_NAME")
        .ok()
        .filter(|n| !n.is_empty());
    active_with_selection(&models_dir, &config_path, env_pick)
}

/// Resolve the active local model using independent model-store and config
/// paths. Unlike [`active`], this never consults Enzyme path environment
/// overrides or any other environment variables.
pub fn active_in(models_dir: &Path, config_path: &Path) -> Option<(String, PathBuf)> {
    active_with_selection(models_dir, config_path, None)
}

fn active_with_selection(
    models_dir: &Path,
    config_path: &Path,
    explicit_pick: Option<String>,
) -> Option<(String, PathBuf)> {
    let installed = installed_in(models_dir);
    let configured_pick = EnzymeConfig::catalyst_llm_config_at(config_path)
        .ok()
        .and_then(|config| config.local_model);
    let explicit_pick = explicit_pick.or(configured_pick);
    let pick = match explicit_pick {
        Some(name) if installed.iter().any(|installed| installed == &name) => name,
        Some(_) => return None,
        None => installed
            .iter()
            .find(|name| *name == DEFAULT_MODEL)
            .or_else(|| installed.first())
            .cloned()?,
    };
    let path = model_path_in(models_dir, &pick);
    Some((pick, path))
}

/// Selected model name even when its weights are not currently installed.
pub fn selected_name() -> String {
    std::env::var("ENZYME_LOCAL_MODEL_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            EnzymeConfig::catalyst_llm_config()
                .ok()
                .and_then(|config| config.local_model)
        })
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Context window for a name, defaulting to 8k for off-registry models.
pub fn context_window(name: &str) -> u32 {
    lookup(name).map_or(8192, |m| m.context_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_in_registry() {
        assert!(lookup(DEFAULT_MODEL).is_some());
    }

    #[test]
    fn registry_checksums_are_lowercase_hex_sha256() {
        for m in REGISTRY {
            assert_eq!(m.sha256.len(), 64, "{}", m.name);
            assert!(
                m.sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{}",
                m.name
            );
        }
    }

    #[test]
    fn default_registry_entry_matches_release_manifest() {
        let manifest: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/models/catalyst-135m-v3.release.json"
        )))
        .unwrap();
        let entry = lookup(DEFAULT_MODEL).unwrap();
        assert_eq!(manifest["name"], entry.name);
        assert_eq!(manifest["sha256"], entry.sha256);
        assert_eq!(manifest["size"], entry.size);
        let expected_url = format!(
            "https://github.com/useenzyme/enzyme/releases/download/{}/{}",
            manifest["public_tag"].as_str().unwrap(),
            manifest["source_asset"].as_str().unwrap()
        );
        assert_eq!(entry.url, expected_url);
    }

    #[test]
    fn unknown_name_falls_back_to_8k_context() {
        assert_eq!(context_window("not-a-model"), 8192);
    }

    #[test]
    fn explicit_model_store_and_config_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("weights");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(model_path_in(&models, "chosen"), b"fixture").unwrap();
        let config = root.path().join("settings").join("enzyme.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "[llm]\nmode = \"local\"\nlocal_model = \"chosen\"\n",
        )
        .unwrap();

        assert_eq!(
            active_in(&models, &config),
            Some(("chosen".into(), model_path_in(&models, "chosen")))
        );
        assert!(!root.path().join("settings/models/chosen.gguf").exists());
    }
}
