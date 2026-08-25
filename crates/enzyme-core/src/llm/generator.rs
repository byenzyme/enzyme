use std::path::Path;

use anyhow::Result;

use super::{ConfigProvider, LlmClient, ResolvedConfig, resolve_llm_client, resolve_llm_client_in};

/// A catalyst generator selected by Enzyme's single credential resolver.
pub struct Generator {
    client: LlmClient,
    config: ResolvedConfig,
}

impl Generator {
    #[cfg(test)]
    pub(crate) fn fixture(client: LlmClient, model: &str) -> Self {
        Self {
            config: ResolvedConfig {
                api_key: "fixture-key".to_string(),
                base_url: "fixture".to_string(),
                model: model.to_string(),
                provider: ConfigProvider::Env,
                expires_at: None,
            },
            client,
        }
    }

    /// Resolve env credentials, configured local inference, or the anonymous
    /// hosted fallback using the same policy as the rest of Enzyme core.
    pub async fn resolve() -> Result<Self> {
        let (config, client) = resolve_llm_client().await?;
        Ok(Self { client, config })
    }

    /// Resolve with config, models, and anonymous identity rooted in an embedder-owned home.
    pub async fn resolve_in(enzyme_home: &Path) -> Result<Self> {
        let (config, client) = resolve_llm_client_in(enzyme_home).await?;
        Ok(Self { client, config })
    }

    pub fn client(&self) -> &LlmClient {
        &self.client
    }

    /// Current model for this generator.
    ///
    /// Hosted `resolve_in` generators may rotate model and endpoint after a
    /// rejected lease, so the value is an owned coherent snapshot rather than
    /// a reference to the initial resolution.
    pub fn model(&self) -> String {
        self.client.resolved_model(&self.config.model)
    }

    pub fn provider(&self) -> &ConfigProvider {
        &self.config.provider
    }
}
