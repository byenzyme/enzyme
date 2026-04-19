use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// File include/exclude patterns for vault indexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePatterns {
    /// Glob patterns for files to include. Default: `["**/*.md"]`.
    pub include: Vec<String>,
    /// Glob patterns for files to exclude.
    pub exclude: Vec<String>,
}

impl Default for FilePatterns {
    fn default() -> Self {
        Self {
            include: vec!["**/*.md".to_string()],
            exclude: vec![
                ".enzyme/**".to_string(),
                ".enzyme-embeddings/**".to_string(),
                ".obsidian/**".to_string(),
                ".trash/**".to_string(),
                ".git/**".to_string(),
                "node_modules/**".to_string(),
                "ENZYME_GUIDE.md".to_string(),
                "enzyme-config.yaml".to_string(),
            ],
        }
    }
}

/// Per-vault configuration.
///
/// Stored at `<vault>/.enzyme/config.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Absolute path to the vault root.
    #[serde(skip)]
    pub vault_path: PathBuf,
    /// Named context for this vault (allows multiple indexes per vault in the future).
    #[serde(default = "default_context_name")]
    pub context_name: String,
    /// File include/exclude patterns.
    #[serde(default)]
    pub file_patterns: FilePatterns,
}

fn default_context_name() -> String {
    "default".to_string()
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            vault_path: PathBuf::new(),
            context_name: default_context_name(),
            file_patterns: FilePatterns::default(),
        }
    }
}

impl VaultConfig {
    /// Load vault config from `<vault_path>/.enzyme/config.yaml`.
    ///
    /// Returns sensible defaults if the file does not exist.
    pub fn load(vault_path: impl Into<PathBuf>) -> Result<Self> {
        let vault_path = vault_path.into();
        let config_path = vault_path.join(".enzyme").join("config.yaml");
        Self::load_from(&config_path, vault_path)
    }

    /// Load from a specific file path (useful for testing).
    pub fn load_from(config_path: &Path, vault_path: PathBuf) -> Result<Self> {
        if !config_path.exists() {
            return Ok(Self {
                vault_path,
                ..Self::default()
            });
        }

        let contents = fs::read_to_string(config_path).with_context(|| {
            format!("failed to read vault config from {}", config_path.display())
        })?;

        if contents.trim().is_empty() {
            return Ok(Self {
                vault_path,
                ..Self::default()
            });
        }

        let mut config: Self = serde_yaml::from_str(&contents).with_context(|| {
            format!("failed to parse vault config at {}", config_path.display())
        })?;

        config.vault_path = vault_path;
        Ok(config)
    }

    /// Save vault config to `<vault_path>/.enzyme/config.yaml`.
    pub fn save(&self) -> Result<()> {
        let config_path = self.vault_path.join(".enzyme").join("config.yaml");
        self.save_to(&config_path)
    }

    /// Save to a specific path (useful for testing).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create vault config directory {}",
                    parent.display()
                )
            })?;
        }

        let yaml =
            serde_yaml::to_string(self).context("failed to serialize vault config to YAML")?;

        fs::write(path, yaml)
            .with_context(|| format!("failed to write vault config to {}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_defaults() {
        let config = VaultConfig::default();
        assert_eq!(config.context_name, "default");
        assert_eq!(config.file_patterns.include, vec!["**/*.md"]);
        assert!(!config.file_patterns.exclude.is_empty());
    }

    #[test]
    fn test_load_missing_file_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let vault_path = tmp.path().to_path_buf();
        let config = VaultConfig::load(&vault_path).unwrap();

        assert_eq!(config.vault_path, vault_path);
        assert_eq!(config.context_name, "default");
    }

    #[test]
    fn test_load_empty_file_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".enzyme").join("config.yaml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, "").unwrap();

        let config = VaultConfig::load(tmp.path()).unwrap();
        assert_eq!(config.context_name, "default");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let vault_path = tmp.path().to_path_buf();

        let config = VaultConfig {
            vault_path: vault_path.clone(),
            context_name: "research".to_string(),
            file_patterns: FilePatterns {
                include: vec!["**/*.md".to_string(), "**/*.txt".to_string()],
                exclude: vec![".git/**".to_string()],
            },
        };

        config.save().unwrap();

        let loaded = VaultConfig::load(&vault_path).unwrap();
        assert_eq!(loaded.vault_path, vault_path);
        assert_eq!(loaded.context_name, "research");
        assert_eq!(loaded.file_patterns.include.len(), 2);
        assert_eq!(loaded.file_patterns.exclude, vec![".git/**"]);
    }

    #[test]
    fn test_yaml_deserialization_partial() {
        let yaml = r#"
context_name: notes
"#;
        let config: VaultConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.context_name, "notes");
        assert_eq!(config.file_patterns, FilePatterns::default());
    }

    #[test]
    fn test_yaml_deserialization_minimal() {
        // An empty YAML document should produce all defaults.
        let yaml = "{}";
        let config: VaultConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.context_name, "default");
        assert_eq!(config.file_patterns, FilePatterns::default());
    }

    #[test]
    fn test_vault_path_not_serialized() {
        let config = VaultConfig {
            vault_path: PathBuf::from("/secret/path"),
            ..VaultConfig::default()
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        // vault_path is #[serde(skip)], so it must not appear in the output.
        assert!(!yaml.contains("/secret/path"));
    }

    #[test]
    fn test_file_patterns_default_excludes() {
        let patterns = FilePatterns::default();
        assert!(patterns.exclude.contains(&".obsidian/**".to_string()));
        assert!(patterns.exclude.contains(&".git/**".to_string()));
        assert!(patterns.exclude.contains(&".enzyme/**".to_string()));
        assert!(
            patterns
                .exclude
                .contains(&".enzyme-embeddings/**".to_string())
        );
    }
}
