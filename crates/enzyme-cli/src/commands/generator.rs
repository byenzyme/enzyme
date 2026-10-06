//! `--llm`: which catalyst generator a command may use for this call.

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};

use enzyme_core::llm::{LlmCredentialMode, resolve_api_config_for_mode};

/// Catalyst generator chosen for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum LlmMode {
    /// No `--llm`: the persisted `enzyme model` preference (an installed local
    /// model when selected or available), otherwise Enzyme hosted.
    #[default]
    #[value(skip)]
    Auto,
    /// OpenAI-compatible endpoint from OPENAI_API_KEY (required),
    /// OPENAI_BASE_URL and OPENAI_MODEL only; never Enzyme auth or caches.
    Env,
    /// The selected local model in $ENZYME_HOME/models; fails if not installed.
    Local,
    /// Enzyme hosted generation.
    Hosted,
    /// Index, embed, and select only; no catalyst generation or network.
    None,
}

/// Generator flags shared by every command that can generate catalysts.
#[derive(Debug, Clone, Copy, Args)]
pub struct LlmArgs {
    /// Catalyst generator for this call. `env` reads only OPENAI_API_KEY
    /// (required), OPENAI_BASE_URL (default https://api.openai.com/v1) and
    /// OPENAI_MODEL (default gpt-4o-mini). Omit to use the configured default.
    #[arg(long, value_enum, value_name = "MODE")]
    llm: Option<LlmMode>,

    /// Alias for `--llm env`.
    #[arg(long, conflicts_with = "llm")]
    use_env_llm: bool,
}

impl LlmArgs {
    pub fn mode(self) -> LlmMode {
        if self.use_env_llm {
            LlmMode::Env
        } else {
            self.llm.unwrap_or_default()
        }
    }
}

impl LlmMode {
    /// Credentials to resolve, or `None` when generation is disabled.
    pub fn credential_mode(self) -> Option<LlmCredentialMode> {
        match self {
            Self::Auto => Some(LlmCredentialMode::Auto),
            Self::Env => Some(LlmCredentialMode::Env),
            Self::Local => Some(LlmCredentialMode::Local),
            Self::Hosted => Some(LlmCredentialMode::Hosted),
            Self::None => None,
        }
    }

    pub fn generates(self) -> bool {
        self != Self::None
    }

    /// Whether this call may fall through to Enzyme hosted generation.
    pub fn may_use_hosted(self) -> bool {
        matches!(self, Self::Auto | Self::Hosted)
    }

    /// The flag value that reproduces this mode in a spawned worker.
    pub fn flag_value(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Env => Some("env"),
            Self::Local => Some("local"),
            Self::Hosted => Some("hosted"),
            Self::None => Some("none"),
        }
    }

    /// Append `--llm <mode>` for a spawned worker when one was chosen.
    pub fn push_flag(self, cmd: &mut std::process::Command) {
        if let Some(value) = self.flag_value() {
            cmd.arg("--llm").arg(value);
        }
    }

    /// Fail an explicit `env` or `local` choice before any work starts.
    ///
    /// Local checks only: neither mode makes a network call here, and `local`
    /// only confirms the model file is installed.
    pub async fn preflight(self) -> Result<()> {
        match self {
            Self::Env => resolve_api_config_for_mode(LlmCredentialMode::Env)
                .await
                .map(drop)
                .context("--llm env needs OPENAI_API_KEY in the environment"),
            Self::Local => resolve_api_config_for_mode(LlmCredentialMode::Local)
                .await
                .map(drop)
                .context("--llm local needs an installed local model in $ENZYME_HOME/models"),
            Self::Auto | Self::Hosted | Self::None => Ok(()),
        }
    }
}
