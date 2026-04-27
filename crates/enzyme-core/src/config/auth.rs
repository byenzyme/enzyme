use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Authentication token stored at `~/.enzyme/auth.json`.
///
/// Contains an API key obtained via the device flow during `enzyme login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    /// The API key (prefixed `enz_`).
    pub api_key: String,
    /// User ID from the server.
    pub user_id: String,
    /// User's email address.
    pub email: String,
}

impl AuthToken {
    /// Load a saved auth token from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read auth token from {}", path.display()))?;
        let token: Self = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse auth token from {}", path.display()))?;
        Ok(token)
    }

    /// Save the auth token to disk, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        let content =
            serde_json::to_string_pretty(self).context("failed to serialize auth token")?;
        fs::write(path, content)
            .with_context(|| format!("failed to write auth token to {}", path.display()))?;
        Ok(())
    }

    /// Delete the auth token file.
    pub fn delete(path: &Path) -> Result<()> {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to delete auth token at {}", path.display()))?;
        }
        Ok(())
    }
}

/// Check if the user is logged in (auth token file exists and is parseable).
pub fn is_logged_in(auth_path: &Path) -> bool {
    AuthToken::load(auth_path).is_ok()
}

/// Load the current login token, returning the standard CLI login prompt on failure.
pub fn require_login(auth_path: &Path) -> Result<AuthToken> {
    AuthToken::load(auth_path)
        .map_err(|_| anyhow::anyhow!("Not logged in. Run `enzyme login` first."))
}
