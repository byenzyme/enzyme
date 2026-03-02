use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::EnzymePaths;

/// Per-vault settings stored in the global config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VaultEntry {
    /// Optional catalyst profile name (e.g. "research", "work").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalyst_profile: Option<String>,
    /// When this vault was last opened / used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
}

/// Global Enzyme configuration.
///
/// Lives at `~/.enzyme/config.yaml` (or `$ENZYME_HOME/config.yaml`).
/// Tracks the default vault and per-vault overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobalConfig {
    /// The vault that is used when no `--vault` flag is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_vault: Option<PathBuf>,
    /// Per-vault settings keyed by canonical vault path.
    #[serde(default)]
    pub vaults: HashMap<PathBuf, VaultEntry>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            default_vault: None,
            vaults: HashMap::new(),
        }
    }
}

impl GlobalConfig {
    /// Resolve the path to the global config file.
    ///
    /// Uses `$ENZYME_HOME/config.yaml` if `ENZYME_HOME` is set,
    /// otherwise `~/.enzyme/config.yaml`.
    pub fn config_path() -> Result<PathBuf> {
        let home = EnzymePaths::enzyme_home()?;
        Ok(home.join("config.yaml"))
    }

    /// Load the global config from disk.
    ///
    /// Returns `Default` if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Load from a specific path (useful for testing).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read global config from {}", path.display()))?;

        if contents.trim().is_empty() {
            return Ok(Self::default());
        }

        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse global config at {}", path.display()))?;

        Ok(config)
    }

    /// Persist the config back to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    /// Save to a specific path (useful for testing).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create config directory {}",
                    parent.display()
                )
            })?;
        }

        let yaml = serde_yaml::to_string(self)
            .context("failed to serialize global config to YAML")?;

        fs::write(path, yaml)
            .with_context(|| format!("failed to write global config to {}", path.display()))?;

        Ok(())
    }

    /// Register or update a vault entry and optionally set it as default.
    pub fn register_vault(
        &mut self,
        vault_path: PathBuf,
        set_default: bool,
    ) {
        self.vaults
            .entry(vault_path.clone())
            .or_insert_with(|| VaultEntry {
                catalyst_profile: None,
                last_used: Some(Utc::now()),
            })
            .last_used = Some(Utc::now());

        if set_default {
            self.default_vault = Some(vault_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = GlobalConfig::default();
        assert!(config.default_vault.is_none());
        assert!(config.vaults.is_empty());
    }

    #[test]
    fn test_load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.yaml");
        let config = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(config, GlobalConfig::default());
    }

    #[test]
    fn test_load_empty_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(&path, "").unwrap();
        let config = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(config, GlobalConfig::default());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");

        let mut config = GlobalConfig::default();
        config.default_vault = Some(PathBuf::from("/Users/alice/vault"));
        config.vaults.insert(
            PathBuf::from("/Users/alice/vault"),
            VaultEntry {
                catalyst_profile: Some("research".to_string()),
                last_used: Some(
                    DateTime::parse_from_rfc3339("2025-06-15T10:30:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
            },
        );
        config.vaults.insert(
            PathBuf::from("/Users/alice/work-vault"),
            VaultEntry {
                catalyst_profile: None,
                last_used: None,
            },
        );

        config.save_to(&path).unwrap();

        let loaded = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(loaded.default_vault, config.default_vault);
        assert_eq!(loaded.vaults.len(), 2);
        assert_eq!(
            loaded.vaults[&PathBuf::from("/Users/alice/vault")]
                .catalyst_profile,
            Some("research".to_string()),
        );
        assert!(
            loaded.vaults[&PathBuf::from("/Users/alice/work-vault")]
                .last_used
                .is_none()
        );
    }

    #[test]
    fn test_register_vault_sets_default() {
        let mut config = GlobalConfig::default();
        let vault = PathBuf::from("/tmp/my-vault");
        config.register_vault(vault.clone(), true);

        assert_eq!(config.default_vault, Some(vault.clone()));
        assert!(config.vaults.contains_key(&vault));
        assert!(config.vaults[&vault].last_used.is_some());
    }

    #[test]
    fn test_register_vault_without_default() {
        let mut config = GlobalConfig::default();
        let vault = PathBuf::from("/tmp/my-vault");
        config.register_vault(vault.clone(), false);

        assert!(config.default_vault.is_none());
        assert!(config.vaults.contains_key(&vault));
    }

    #[test]
    fn test_register_vault_updates_last_used() {
        let mut config = GlobalConfig::default();
        let vault = PathBuf::from("/tmp/my-vault");

        config.vaults.insert(
            vault.clone(),
            VaultEntry {
                catalyst_profile: Some("old-profile".to_string()),
                last_used: Some(
                    DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
            },
        );

        config.register_vault(vault.clone(), false);

        // Profile should be preserved, last_used should be updated.
        let entry = &config.vaults[&vault];
        assert_eq!(entry.catalyst_profile, Some("old-profile".to_string()));
        assert!(entry.last_used.unwrap().year() >= 2025);
    }

    #[test]
    fn test_yaml_deserialization() {
        let yaml = r#"
default_vault: /home/user/notes
vaults:
  /home/user/notes:
    catalyst_profile: personal
    last_used: "2025-03-01T12:00:00Z"
  /home/user/work:
    catalyst_profile: null
"#;
        let config: GlobalConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.default_vault,
            Some(PathBuf::from("/home/user/notes"))
        );
        assert_eq!(config.vaults.len(), 2);
        assert_eq!(
            config.vaults[&PathBuf::from("/home/user/notes")]
                .catalyst_profile,
            Some("personal".to_string()),
        );
    }

    #[test]
    fn test_save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("config.yaml");

        let config = GlobalConfig::default();
        config.save_to(&path).unwrap();

        assert!(path.exists());
    }
}
