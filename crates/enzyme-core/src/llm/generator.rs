use std::path::Path;

use anyhow::{Context, Result};

use super::{LlmClient, model_registry};

/// An explicitly selected catalyst generator.
///
/// Construction never consults Enzyme config, provider environment variables,
/// cached credentials, or the enzyme.garden hosted bootstrap endpoint.
pub struct Generator {
    client: LlmClient,
    model: String,
    kind: GeneratorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorKind {
    OpenAiCompatible,
    Local,
}

impl Generator {
    /// Use an OpenAI-compatible endpoint with an explicit key/base/model.
    pub fn openai_compatible(api_key: &str, base_url: &str, model: &str) -> Self {
        Self {
            client: LlmClient::with_base_url(api_key, base_url),
            model: model.to_owned(),
            kind: GeneratorKind::OpenAiCompatible,
        }
    }

    /// Load exactly `<model_store>/<model>.gguf` for in-process generation.
    pub fn local(model_store: &Path, model: &str) -> Result<Self> {
        let path = model_registry::model_path_in(model_store, model);
        if !path.is_file() {
            anyhow::bail!("local model '{}' not found at {}", model, path.display());
        }
        let client = LlmClient::from_local_model(&path, model)
            .with_context(|| format!("loading explicitly selected local generator '{model}'"))?;
        Ok(Self {
            client,
            model: model.to_owned(),
            kind: GeneratorKind::Local,
        })
    }

    /// Resolve the configured local selection using only the two supplied
    /// paths. Returns `None` when the selection is absent or not installed;
    /// there is deliberately no environment or hosted fallback.
    pub fn local_from_paths(model_store: &Path, config_path: &Path) -> Result<Option<Self>> {
        let Some((model, _path)) = model_registry::active_in(model_store, config_path) else {
            return Ok(None);
        };
        Self::local(model_store, &model).map(Some)
    }

    pub fn client(&self) -> &LlmClient {
        &self.client
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn kind(&self) -> GeneratorKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_hosted_generator_has_no_free_tier_fallback() {
        let generator =
            Generator::openai_compatible("fixture-key", "http://127.0.0.1:9/v1", "fixture-model");
        assert_eq!(generator.kind(), GeneratorKind::OpenAiCompatible);
        assert_eq!(generator.model(), "fixture-model");
        assert!(!generator.client().is_openrouter());
    }

    #[test]
    fn missing_explicit_local_model_does_not_fall_back() {
        let store = tempfile::tempdir().unwrap();
        let error = Generator::local(store.path(), "missing").err().unwrap();
        let message = error.to_string();
        assert!(message.contains("missing.gguf"));
        assert!(!message.contains("hosted"));
        assert!(!message.contains("enzyme.garden"));
    }

    #[test]
    fn path_resolution_does_not_fall_back_to_an_unrelated_store() {
        let root = tempfile::tempdir().unwrap();
        let selected_store = root.path().join("selected-store");
        let unrelated_store = root.path().join("models");
        std::fs::create_dir_all(&selected_store).unwrap();
        std::fs::create_dir_all(&unrelated_store).unwrap();
        std::fs::write(unrelated_store.join("chosen.gguf"), b"not selected").unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            "[llm]\nmode = \"local\"\nlocal_model = \"chosen\"\n",
        )
        .unwrap();

        assert!(
            Generator::local_from_paths(&selected_store, &config)
                .unwrap()
                .is_none()
        );
    }
}
