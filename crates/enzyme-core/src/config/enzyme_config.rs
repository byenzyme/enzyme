use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::config::paths::EnzymePaths;
use crate::models::{EntityType, ROOT_FOLDER_ENTITY};

/// Comment header prepended to config.toml on every save.
const CONFIG_HEADER: &str = "\
# Enzyme configuration
# https://enzyme.garden/docs/config
#
# This file configures how Enzyme processes your Obsidian vault(s).
# Edit it directly \u{2014} changes take effect on the next `enzyme refresh`.
#
# The [defaults] section below contains global settings used by every vault.
# max_embedding_files caps the number of docs embedded per run, newest first.
# Set max_embedding_files = 0 to embed all docs.
#
# [llm]
# mode = \"auto\"                    # auto, local, or hosted
# local_model = \"catalyst-135m-v3\" # selected by `enzyme model use`
#
# Each vault gets its own section, keyed by absolute path:
#
# [vaults.\"/Users/you/notes\"]
# entities = [\"folder:projects\", \"[[Design Principles]]\", \"log:journal\"]
# # log: marks a single file of dated entries (a journal that grows by
# # appending) as its own entity, so retrieval sees its internal timeline.
# # `enzyme scan --write-config` records the logs it recognizes here; add or
# # remove lines to correct it. The name is the file's title or filename stem.
# # Flat vault fallback: if notes live directly at the vault root, Enzyme may use \"folder:.\"
# excluded_tags = [\"todo\", \"template\"]         # skip these from trending selection
# targets = [\"/Users/you/code/my-app\"]         # project catalysts onto external directories
# min_top_catalysts = 13                       # catalyst budget for the largest entity (default 15)
# max_embedding_files = 0                      # override: embed all docs in this vault
#
# min_top_catalysts sets how many catalysts the largest entity (by document
# count) receives. Every other entity scales down linearly by its share of
# documents — an entity with half as many docs gets about half as many
# catalysts — down to a floor of 3 for entities that are tiny relative to the
# largest. Coverage tracks how much material each entity holds, so similarly
# sized entities get similar budgets. Raise it for broader coverage of the top
# entity (at higher indexing cost); the default when unset is 15.
#

";

/// Top-level config view for a specific vault.
///
/// Data is loaded from the global `~/.enzyme/config.toml`, merged with the
/// current vault's section. For backward compatibility, if that TOML file does
/// not exist we still read legacy vault-local `enzyme-config.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnzymeConfig {
    #[serde(default)]
    pub catalyzed_entities: Vec<CatalyzedEntity>,
    #[serde(default)]
    pub trending: Option<SelectionConfig>,
    #[serde(default)]
    pub catalyst_format: Option<String>,
    #[serde(default)]
    pub excluded_tags: Vec<String>,
    #[serde(default)]
    pub excluded_links: Vec<String>,
    #[serde(default)]
    pub excluded_folders: Vec<String>,
    /// External directories to project this vault's catalysts onto.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Maximum number of documents to embed per run.
    /// `None` uses the built-in default (1024). `Some(0)` means unlimited.
    #[serde(default)]
    pub max_embedding_files: Option<usize>,
}

/// A single entry in the config.
///
/// Supports:
/// - Simple string: `"#tag"` or `"folder:inbox"` or `"folder:."` or `"[[Note]]"`
/// - Legacy YAML map with options: `"#tag": { profile: "decision_trace" }`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CatalyzedEntity {
    Simple(String),
    WithOptions(HashMap<String, EntityOptions>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityOptions {
    pub profile: Option<String>,
    #[serde(default)]
    pub expandable: bool,
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_null_as_empty_vec"
    )]
    pub children: Vec<String>,
}

fn deserialize_null_as_empty_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<String>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionConfig {
    #[serde(default = "default_min_tags")]
    pub minimum_tags: usize,
    #[serde(default = "default_min_links")]
    pub minimum_links: usize,
    #[serde(default = "default_min_folders")]
    pub minimum_folders: usize,
    #[serde(default = "default_total_limit")]
    pub total_limit: usize,
    #[serde(default)]
    pub min_top_catalysts: Option<usize>,
}

fn default_min_tags() -> usize {
    8
}
fn default_min_links() -> usize {
    6
}
fn default_min_folders() -> usize {
    6
}
fn default_total_limit() -> usize {
    30
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            minimum_tags: default_min_tags(),
            minimum_links: default_min_links(),
            minimum_folders: default_min_folders(),
            total_limit: default_total_limit(),
            min_top_catalysts: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedEntity {
    pub name: String,
    pub entity_type: EntityType,
}

#[derive(Debug, Clone, Default)]
pub struct EntityPreferences {
    pub entities: Vec<ParsedEntity>,
    pub excluded_names: Vec<ParsedEntity>,
    pub entity_profiles: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GlobalTomlConfig {
    #[serde(default)]
    defaults: DefaultsTomlSection,
    #[serde(default)]
    llm: CatalystLlmConfig,
    #[serde(default)]
    vaults: HashMap<String, VaultTomlSection>,
}

/// Machine-wide catalyst-generation provider preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalystMode {
    /// Prefer the selected installed model, otherwise use Enzyme hosted.
    #[default]
    Auto,
    /// Require the selected installed local model.
    Local,
    /// Always use Enzyme hosted generation.
    Hosted,
}

impl std::fmt::Display for CatalystMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Local => f.write_str("local"),
            Self::Hosted => f.write_str("hosted"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalystLlmConfig {
    #[serde(default)]
    pub mode: CatalystMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DefaultsTomlSection {
    #[serde(default)]
    catalyst_format: Option<String>,
    #[serde(default = "default_min_tags")]
    minimum_tags: usize,
    #[serde(default = "default_min_links")]
    minimum_links: usize,
    #[serde(default = "default_min_folders")]
    minimum_folders: usize,
    #[serde(default = "default_total_limit")]
    total_limit: usize,
    /// Maximum documents to embed per run. `0` = unlimited. Omitted = 1024.
    #[serde(default)]
    max_embedding_files: Option<usize>,
}

impl Default for DefaultsTomlSection {
    fn default() -> Self {
        Self {
            catalyst_format: None,
            minimum_tags: default_min_tags(),
            minimum_links: default_min_links(),
            minimum_folders: default_min_folders(),
            total_limit: default_total_limit(),
            max_embedding_files: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VaultTomlSection {
    #[serde(default)]
    excluded_tags: Vec<String>,
    #[serde(default)]
    excluded_links: Vec<String>,
    #[serde(default)]
    excluded_folders: Vec<String>,
    #[serde(default)]
    min_top_catalysts: Option<usize>,
    #[serde(default)]
    entities: Vec<CatalyzedEntity>,
    #[serde(default)]
    targets: Vec<String>,
    /// Per-vault override for max embedding files. `0` = unlimited.
    #[serde(default)]
    max_embedding_files: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyYamlConfig {
    #[serde(default)]
    catalyzed_entities: Vec<CatalyzedEntity>,
    #[serde(default)]
    trending: Option<SelectionConfig>,
    #[serde(default)]
    catalyst_format: Option<String>,
    #[serde(default)]
    excluded_tags: Vec<String>,
    #[serde(default)]
    excluded_links: Vec<String>,
    #[serde(default)]
    excluded_folders: Vec<String>,
}

impl EnzymeConfig {
    pub fn config_path() -> Result<PathBuf> {
        Ok(EnzymePaths::enzyme_home()?.join("config.toml"))
    }

    /// Canonical key used for this vault in the global TOML config.
    pub fn vault_key(vault_path: &Path) -> String {
        canonical_vault_key(vault_path)
    }

    /// Return the configured vault keys from `~/.enzyme/config.toml`.
    ///
    /// This intentionally ignores legacy vault-local YAML because the loginless
    /// first-init gate is about the machine-global TOML state: once a machine
    /// has configured multiple vaults, hosted usage should be attached to a
    /// signed-in account.
    pub fn configured_vault_keys() -> Result<Vec<String>> {
        let config_path = Self::config_path()?;
        Self::configured_vault_keys_at(&config_path)
    }

    pub fn configured_vault_keys_at(config_path: &Path) -> Result<Vec<String>> {
        if !config_path.exists() {
            return Ok(Vec::new());
        }

        let global = load_global_toml(&config_path)?;
        Ok(global.vaults.keys().cloned().collect())
    }

    /// Load machine-wide catalyst provider preferences.
    pub fn catalyst_llm_config() -> Result<CatalystLlmConfig> {
        let path = Self::config_path()?;
        Self::catalyst_llm_config_at(&path)
    }

    /// Load machine-wide catalyst preferences from an explicit file.
    pub fn catalyst_llm_config_at(path: &Path) -> Result<CatalystLlmConfig> {
        if !path.exists() {
            return Ok(CatalystLlmConfig::default());
        }
        Ok(load_global_toml(&path)?.llm)
    }

    /// Persist machine-wide catalyst provider preferences without changing vault settings.
    pub fn save_catalyst_llm_config(llm: CatalystLlmConfig) -> Result<()> {
        let path = Self::config_path()?;
        Self::save_catalyst_llm_config_at(&path, llm)
    }

    pub fn save_catalyst_llm_config_at(path: &Path, llm: CatalystLlmConfig) -> Result<()> {
        let mut global = if path.exists() {
            load_global_toml(&path)?
        } else {
            GlobalTomlConfig::default()
        };
        global.llm = llm;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(&global).context("failed to serialize enzyme config")?;
        fs::write(&path, format!("{CONFIG_HEADER}{body}"))
            .with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn load(vault_path: &Path) -> Result<Option<Self>> {
        let config_path = Self::config_path()?;
        Self::load_at(vault_path, &config_path)
    }

    /// Load a vault view from an explicit global TOML path.
    ///
    /// Legacy vault-local YAML remains the fallback only when that explicit
    /// file does not exist, preserving the CLI's compatibility behavior.
    pub fn load_at(vault_path: &Path, config_path: &Path) -> Result<Option<Self>> {
        if config_path.exists() {
            debug!(?config_path, "loading enzyme config from global TOML");
            let global = load_global_toml(&config_path)?;
            return Ok(Some(Self::from_global_config(vault_path, &global)));
        }

        Self::load_legacy_yaml(vault_path)
    }

    /// The user's `exclude` folder list for a vault, or empty when there is no
    /// readable config. This is the single source every file-discovery site uses
    /// to feed the index gate, so `exclude` is honored consistently (indexing,
    /// doctor, scan, corpus-preservation checks) rather than only where an
    /// `IndexingPipeline` happens to run. A load error resolves to empty so
    /// discovery never fails on a bad/absent config.
    pub fn excluded_folders_at(vault_path: &Path) -> Vec<String> {
        match Self::load(vault_path) {
            Ok(Some(cfg)) => cfg.excluded_folders,
            _ => Vec::new(),
        }
    }

    pub fn excluded_folders_with_config(vault_path: &Path, config_path: &Path) -> Vec<String> {
        match Self::load_at(vault_path, config_path) {
            Ok(Some(cfg)) => cfg.excluded_folders,
            _ => Vec::new(),
        }
    }

    fn from_global_config(vault_path: &Path, global: &GlobalTomlConfig) -> Self {
        let key = canonical_vault_key(vault_path);
        let vault = global
            .vaults
            .get(&key)
            .or_else(|| global.vaults.get(&vault_path.to_string_lossy().to_string()));

        let trending = SelectionConfig {
            minimum_tags: global.defaults.minimum_tags,
            minimum_links: global.defaults.minimum_links,
            minimum_folders: global.defaults.minimum_folders,
            total_limit: global.defaults.total_limit,
            min_top_catalysts: vault.and_then(|v| v.min_top_catalysts),
        };

        Self {
            catalyzed_entities: vault.map(|v| v.entities.clone()).unwrap_or_default(),
            trending: Some(trending),
            catalyst_format: global.defaults.catalyst_format.clone(),
            excluded_tags: vault.map(|v| v.excluded_tags.clone()).unwrap_or_default(),
            excluded_links: vault.map(|v| v.excluded_links.clone()).unwrap_or_default(),
            excluded_folders: vault
                .map(|v| v.excluded_folders.clone())
                .unwrap_or_default(),
            targets: vault.map(|v| v.targets.clone()).unwrap_or_default(),
            max_embedding_files: vault
                .and_then(|v| v.max_embedding_files)
                .or(global.defaults.max_embedding_files),
        }
    }

    fn load_legacy_yaml(vault_path: &Path) -> Result<Option<Self>> {
        let enzyme_dir_path = vault_path.join(".enzyme").join("enzyme-config.yaml");
        let root_path = vault_path.join("enzyme-config.yaml");

        let config_path = if enzyme_dir_path.exists() {
            enzyme_dir_path
        } else if root_path.exists() {
            root_path
        } else {
            debug!("no config.toml or legacy enzyme-config.yaml found");
            return Ok(None);
        };

        let contents = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;

        let legacy: LegacyYamlConfig = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;

        Ok(Some(Self {
            catalyzed_entities: legacy.catalyzed_entities,
            trending: legacy.trending,
            catalyst_format: legacy.catalyst_format,
            excluded_tags: legacy.excluded_tags,
            excluded_links: legacy.excluded_links,
            excluded_folders: legacy.excluded_folders,
            targets: Vec::new(),
            max_embedding_files: None,
        }))
    }

    pub fn parse_preferences(&self) -> EntityPreferences {
        let mut prefs = EntityPreferences::default();

        for entry in &self.catalyzed_entities {
            match entry {
                CatalyzedEntity::Simple(raw) => {
                    if let Some(parsed) = parse_entity_ref(raw) {
                        prefs.entities.push(parsed);
                    }
                }
                CatalyzedEntity::WithOptions(map) => {
                    for (raw, options) in map {
                        if let Some(parsed) = parse_entity_ref(raw) {
                            if let Some(profile) = &options.profile {
                                prefs
                                    .entity_profiles
                                    .insert(parsed.name.clone(), profile.clone());
                            }
                            prefs.entities.push(parsed);
                        }
                    }
                }
            }
        }

        for tag in &self.excluded_tags {
            if let Some(parsed) = parse_entity_ref(&format!("#{tag}")) {
                prefs.excluded_names.push(parsed);
            }
        }
        for link in &self.excluded_links {
            if let Some(parsed) = parse_entity_ref(&format!("[[{link}]]")) {
                prefs.excluded_names.push(parsed);
            }
        }
        for folder in &self.excluded_folders {
            if let Some(parsed) = parse_entity_ref(&format!("folder:{folder}")) {
                prefs.excluded_names.push(parsed);
            }
        }

        prefs
    }

    /// Resolve the effective max-embedding-files limit.
    ///
    /// - `None` in config → `Some(DEFAULT)` (built-in 1024)
    /// - `Some(0)` in config → `None` (unlimited)
    /// - `Some(n)` in config → `Some(n)`
    pub fn effective_max_embedding_files(&self) -> Option<usize> {
        use crate::pipeline::embedding::MAX_EMBEDDING_FILES;
        match self.max_embedding_files {
            None => Some(MAX_EMBEDDING_FILES),
            Some(0) => None,
            Some(n) => Some(n),
        }
    }

    pub fn has_curated_entities(&self) -> bool {
        !self.catalyzed_entities.is_empty()
    }

    pub fn persist_min_top_catalysts(&mut self, vault_path: &Path, value: usize) -> Result<()> {
        let trending = self.trending.get_or_insert_with(SelectionConfig::default);
        trending.min_top_catalysts = Some(value);
        self.save(vault_path)
    }

    /// Add a target path to the config if not already present. Canonicalizes the path.
    pub fn add_target(&mut self, vault_path: &Path, target_path: &Path) -> Result<()> {
        let canonical = target_path
            .canonicalize()
            .unwrap_or_else(|_| target_path.to_path_buf())
            .to_string_lossy()
            .to_string();
        if !self.targets.contains(&canonical) {
            self.targets.push(canonical);
            self.save(vault_path)?;
        }
        Ok(())
    }

    pub fn set_auto_selected_entities(
        &mut self,
        vault_path: &Path,
        entity_refs: Vec<String>,
    ) -> Result<()> {
        self.catalyzed_entities = entity_refs
            .into_iter()
            .map(CatalyzedEntity::Simple)
            .collect();
        self.save(vault_path)
    }

    pub fn save(&self, vault_path: &Path) -> Result<()> {
        let config_path = Self::config_path()?;
        let mut global = if config_path.exists() {
            load_global_toml(&config_path)?
        } else {
            GlobalTomlConfig::default()
        };

        global.defaults.catalyst_format = self.catalyst_format.clone();
        if let Some(ref trending) = self.trending {
            global.defaults.minimum_tags = trending.minimum_tags;
            global.defaults.minimum_links = trending.minimum_links;
            global.defaults.minimum_folders = trending.minimum_folders;
            global.defaults.total_limit = trending.total_limit;
        }
        if global.defaults.max_embedding_files.is_none() {
            global.defaults.max_embedding_files =
                Some(crate::pipeline::embedding::MAX_EMBEDDING_FILES);
        }

        let entry = global
            .vaults
            .entry(canonical_vault_key(vault_path))
            .or_default();
        entry.excluded_tags = self.excluded_tags.clone();
        entry.excluded_links = self.excluded_links.clone();
        entry.excluded_folders = self.excluded_folders.clone();
        entry.min_top_catalysts = self.trending.as_ref().and_then(|t| t.min_top_catalysts);
        entry.entities = self.catalyzed_entities.clone();
        entry.targets = self.targets.clone();
        entry.max_embedding_files = self.max_embedding_files;

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let body = toml::to_string_pretty(&global).context("failed to serialize enzyme config")?;
        let contents = format!("{CONFIG_HEADER}{body}");
        fs::write(&config_path, contents)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        debug!(?config_path, "saved enzyme config");
        Ok(())
    }
}

fn load_global_toml(path: &Path) -> Result<GlobalTomlConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(GlobalTomlConfig::default());
    }
    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn canonical_vault_key(vault_path: &Path) -> String {
    vault_path
        .canonicalize()
        .unwrap_or_else(|_| vault_path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn parse_entity_ref(raw: &str) -> Option<ParsedEntity> {
    let trimmed = raw.trim();

    if let Some(tag) = trimmed.strip_prefix('#') {
        let name = tag.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(ParsedEntity {
            name,
            entity_type: EntityType::Tag,
        })
    } else if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
        let inner = &trimmed[2..trimmed.len() - 2];
        let name = inner.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(ParsedEntity {
            name,
            entity_type: EntityType::Link,
        })
    } else if let Some(log) = trimmed.strip_prefix("log:") {
        let name = log.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(ParsedEntity {
            name,
            entity_type: EntityType::AppendLog,
        })
    } else if let Some(path) = trimmed.strip_prefix("folder:") {
        let name = path.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        let name = if name == "." {
            ROOT_FOLDER_ENTITY.to_string()
        } else {
            name
        };
        Some(ParsedEntity {
            name,
            entity_type: EntityType::Folder,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_entity_ref_tag() {
        let p = parse_entity_ref("#enzyme").unwrap();
        assert_eq!(p.name, "enzyme");
        assert_eq!(p.entity_type, EntityType::Tag);
    }

    #[test]
    fn test_parse_entity_ref_nested_tag() {
        let p = parse_entity_ref("#enzyme/pmf").unwrap();
        assert_eq!(p.name, "enzyme/pmf");
        assert_eq!(p.entity_type, EntityType::Tag);
    }

    #[test]
    fn test_parse_entity_ref_link() {
        let p = parse_entity_ref("[[Test Note]]").unwrap();
        assert_eq!(p.name, "test note");
        assert_eq!(p.entity_type, EntityType::Link);
    }

    #[test]
    fn test_parse_entity_ref_folder() {
        let p = parse_entity_ref("folder:inbox").unwrap();
        assert_eq!(p.name, "inbox");
        assert_eq!(p.entity_type, EntityType::Folder);
    }

    #[test]
    fn test_parse_entity_ref_root_folder() {
        let p = parse_entity_ref("folder:.").unwrap();
        assert_eq!(p.name, ROOT_FOLDER_ENTITY);
        assert_eq!(p.entity_type, EntityType::Folder);
    }

    #[test]
    fn test_parse_entity_ref_invalid() {
        assert!(parse_entity_ref("").is_none());
        assert!(parse_entity_ref("#").is_none());
        assert!(parse_entity_ref("[[]]").is_none());
        assert!(parse_entity_ref("folder:").is_none());
        assert!(parse_entity_ref("random text").is_none());
    }

    #[test]
    fn effective_max_embedding_files_defaults_caps_and_unlimited() {
        let mut config = EnzymeConfig::default();
        assert_eq!(
            config.effective_max_embedding_files(),
            Some(crate::pipeline::embedding::MAX_EMBEDDING_FILES)
        );

        config.max_embedding_files = Some(32);
        assert_eq!(config.effective_max_embedding_files(), Some(32));

        config.max_embedding_files = Some(0);
        assert_eq!(config.effective_max_embedding_files(), None);
    }

    #[test]
    fn test_parse_legacy_yaml_config() {
        let yaml = r##"
catalyzedEntities:
  - "#enzyme":
      profile: operational
  - folder:inbox
  - "[[Test Note]]"
  - folder:people:
      expandable: true
      profile: relational
      children:
        - "[[Alice]]"
        - "[[Bob Chen]]"
catalystProfile: tension_trace
excludedTags:
  - note
  - todo
"##;

        let config: LegacyYamlConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.catalyzed_entities.len(), 4);
        let merged = EnzymeConfig {
            catalyzed_entities: config.catalyzed_entities,
            trending: config.trending,
            catalyst_format: config.catalyst_format,
            excluded_tags: config.excluded_tags,
            excluded_links: config.excluded_links,
            excluded_folders: config.excluded_folders,
            targets: Vec::new(),
            max_embedding_files: None,
        };
        let prefs = merged.parse_preferences();
        assert_eq!(prefs.entities.len(), 4);
        assert_eq!(prefs.entity_profiles.get("enzyme").unwrap(), "operational");
        assert_eq!(prefs.entity_profiles.get("people").unwrap(), "relational");
    }

    #[test]
    fn test_parse_global_toml_entity_profile_options() {
        let toml = r##"
[vaults."/tmp/recipe-vault"]
entities = [
  { "folder:user-recipe-comment" = { profile = "preference_evidence" } },
  "folder:agent-observed-preference",
  { "folder:preference-substitution" = { profile = "operational" } },
]
"##;

        let global: GlobalTomlConfig = toml::from_str(toml).unwrap();
        let config = EnzymeConfig::from_global_config(Path::new("/tmp/recipe-vault"), &global);
        let prefs = config.parse_preferences();

        assert_eq!(prefs.entities.len(), 3);
        assert_eq!(
            prefs.entity_profiles.get("user-recipe-comment").unwrap(),
            "preference_evidence"
        );
        assert_eq!(
            prefs
                .entity_profiles
                .get("preference-substitution")
                .unwrap(),
            "operational"
        );
        assert!(
            !prefs
                .entity_profiles
                .contains_key("agent-observed-preference")
        );
    }

    #[test]
    fn catalyst_llm_config_defaults_and_parses() {
        let defaults: GlobalTomlConfig = toml::from_str("").unwrap();
        assert_eq!(defaults.llm.mode, CatalystMode::Auto);
        assert_eq!(defaults.llm.local_model, None);

        let configured: GlobalTomlConfig = toml::from_str(
            r#"
[llm]
mode = "local"
local_model = "catalyst-135m-v3"
"#,
        )
        .unwrap();
        assert_eq!(configured.llm.mode, CatalystMode::Local);
        assert_eq!(
            configured.llm.local_model.as_deref(),
            Some("catalyst-135m-v3")
        );
    }

    #[test]
    fn test_save_and_load_global_toml() {
        let tmp = TempDir::new().unwrap();
        let old_home = std::env::var("ENZYME_HOME").ok();
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("ENZYME_HOME", tmp.path()) };

        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();

        EnzymeConfig::save_catalyst_llm_config(CatalystLlmConfig {
            mode: CatalystMode::Local,
            local_model: Some("catalyst-135m-v3".to_string()),
        })
        .unwrap();

        let config = EnzymeConfig {
            catalyzed_entities: vec![
                CatalyzedEntity::Simple("#enzyme".to_string()),
                CatalyzedEntity::Simple("[[Test Note]]".to_string()),
            ],
            trending: Some(SelectionConfig {
                minimum_tags: 9,
                minimum_links: 7,
                minimum_folders: 5,
                total_limit: 21,
                min_top_catalysts: Some(13),
            }),
            catalyst_format: Some("question".to_string()),
            excluded_tags: vec!["todo".to_string()],
            excluded_links: vec!["Template".to_string()],
            excluded_folders: vec!["templates".to_string()],
            targets: vec!["/Users/alice/code/my-app".to_string()],
            max_embedding_files: None,
        };

        config.save(&vault).unwrap();

        // Verify the header comment is present in the written file
        let raw = fs::read_to_string(EnzymeConfig::config_path().unwrap()).unwrap();
        assert!(raw.starts_with("# Enzyme configuration"));
        assert!(raw.contains("[defaults]"));
        assert!(raw.contains("[llm]"));
        assert!(raw.contains("mode = \"local\""));
        assert!(raw.contains("max_embedding_files = 1024"));

        let loaded = EnzymeConfig::load(&vault).unwrap().unwrap();
        assert_eq!(loaded.catalyst_format.as_deref(), Some("question"));
        assert_eq!(loaded.catalyzed_entities.len(), 2);
        assert_eq!(loaded.targets, vec!["/Users/alice/code/my-app"]);
        let llm = EnzymeConfig::catalyst_llm_config().unwrap();
        assert_eq!(llm.mode, CatalystMode::Local);
        assert_eq!(llm.local_model.as_deref(), Some("catalyst-135m-v3"));
        assert_eq!(
            loaded.trending.as_ref().and_then(|t| t.min_top_catalysts),
            Some(13)
        );

        // SAFETY: test-only, single-threaded
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("ENZYME_HOME", v),
                None => std::env::remove_var("ENZYME_HOME"),
            }
        }
    }
}
