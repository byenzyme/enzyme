//! Compiled-in registry of local GGUF models plus on-disk resolution.
//!
//! Weights live at `<ENZYME_HOME>/models/<name>.gguf` (flat, one file per
//! registry name) — the same `~/.enzyme` global state directory the rest of
//! the CLI uses. `enzyme model install` writes them; the in-process engine
//! (feature `local-llm`) reads them.

use std::path::PathBuf;

use anyhow::Result;

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
/// The catalyst champion is currently hosted as a **private** release asset on
/// `useenzyme/enzyme-rust` pending the training-data pseudonym sweep; installing it
/// needs `GITHUB_TOKEN`/`GH_TOKEN`. Once the weights are cleared for public
/// hosting this URL moves to a public `useenzyme/enzyme` release and no token is
/// required.
pub const REGISTRY: &[ModelEntry] = &[ModelEntry {
    name: DEFAULT_MODEL,
    url: "https://github.com/useenzyme/enzyme-rust/releases/download/models-catalyst-v3/enzyme-catalyst-smollm135-v3-dpo-8ep-q4_k_m.gguf",
    sha256: "c1853769c45470e74992826c78ab4de7d8ca3876da3eb15ee232d761932ba62d",
    size: 105_454_080,
    context_window: 8192,
    description: "SmolLM2-135M catalyst fine-tune (v3 DPO, Q4_K_M)",
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
    Ok(models_dir()?.join(format!("{name}.gguf")))
}

/// Names of every `.gguf` in the models directory, sorted.
pub fn installed() -> Vec<String> {
    let Ok(dir) = models_dir() else {
        return Vec::new();
    };
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
/// `ENZYME_LOCAL_MODEL_NAME` wins; otherwise the default if installed;
/// otherwise the first installed model.
pub fn active() -> Option<(String, PathBuf)> {
    let installed = installed();
    let pick = std::env::var("ENZYME_LOCAL_MODEL_NAME")
        .ok()
        .filter(|n| !n.is_empty() && installed.iter().any(|i| i == n))
        .or_else(|| {
            installed
                .iter()
                .find(|n| *n == DEFAULT_MODEL)
                .or_else(|| installed.first())
                .cloned()
        })?;
    let path = model_path(&pick).ok()?;
    Some((pick, path))
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
    fn unknown_name_falls_back_to_8k_context() {
        assert_eq!(context_window("not-a-model"), 8192);
    }
}
