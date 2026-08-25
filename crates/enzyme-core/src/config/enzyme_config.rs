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
# https://memory.enzyme.garden/docs/config
#
# This file configures how Enzyme processes your knowledge workspaces.
# Edit it directly \u{2014} changes take effect on the next `enzyme refresh`.
#
# The [defaults] section below contains global settings used by every workspace.
# max_embedding_files caps the number of docs embedded per run, newest first.
# Set max_embedding_files = 0 to embed all docs.
#
# [llm]
# mode = \"auto\"                    # auto, local, or hosted
# local_model = \"catalyst-135m-v3\" # selected by `enzyme model use`
#
# [update]
# auto = false                      # opt out of background binary updates
#
# Named workspaces use the same name as `enzyme --collection <name>`.
# Markdown is an optional source alongside SQLite and other structured sources:
#
# [workspaces.notes]
# entities = [\"folder:projects\", \"[[Design Principles]]\", \"log:journal\"]
# [workspaces.notes.sources.notes]
# path = \"/Users/you/notes\"
# exclusions = [\"private\"]
# writable = true
# Additional named Markdown roots use sibling source tables; with multiple
# roots their names prefix source refs and preserve provenance.
# [workspaces.notes.sources.references]
# path = \"/Users/you/references\"
# writable = false
#
# SQLite participant roles accept scalar/array columns or a multi-value shape:
# who = { format = \"json_array\", column = \"participants\" }
# who = { format = \"delimited\", column = \"participants\", delimiter = \"|\" }
# An optional `weight = \"importance\"` role contributes additive evidence;
# NULL/omitted weights default to one, like one Markdown mention.
#
# Existing path-addressed Markdown vaults remain supported under [vaults].
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
    /// Read-only SQLite sources indexed alongside Markdown files.
    #[serde(default)]
    pub sources: HashMap<String, SqliteSourceConfig>,
    /// Optional Markdown root for a named workspace. Path-addressed legacy
    /// vaults implicitly use their vault path and therefore leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_path: Option<PathBuf>,
    /// Named Markdown roots for a workspace. A single legacy `notes` source is
    /// retained in `notes_path`; this list is the public multi-root view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes_roots: Vec<NotesRootConfig>,
    /// Maximum number of documents to embed per run.
    /// `None` uses the built-in default (1024). `Some(0)` means unlimited.
    #[serde(default)]
    pub max_embedding_files: Option<usize>,
}

/// A read-only SQLite query contract for one named source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteSourceConfig {
    pub db: PathBuf,
    pub query: String,
    pub roles: SqliteSourceRoles,
    pub timestamp: SqliteTimestampConfig,
}

/// Result columns assigned to Enzyme's source roles.
///
/// Composite roles are concatenated in configured order. A scalar is accepted
/// as a one-column shorthand when parsing; serialization always emits arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteSourceRoles {
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub id: Vec<String>,
    #[serde(default)]
    pub who: SqliteWhoRole,
    pub when: String,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub what: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub r#where: Vec<String>,
    /// Optional numeric result column. Each emitted entity occurrence receives
    /// this additive weight; omitted/NULL values default to `1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<String>,
}

/// Public Markdown-root contract for named workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesRootConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub exclusions: Vec<String>,
    #[serde(default = "default_true")]
    pub writable: bool,
}

/// SQLite `who` may be scalar/composite, a JSON array, or delimited text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqliteWhoRole {
    Columns(OneOrManyStrings),
    MultiValue(SqliteMultiValueWhoRole),
}

impl Default for SqliteWhoRole {
    fn default() -> Self {
        Self::Columns(OneOrManyStrings::Many(Vec::new()))
    }
}

impl SqliteWhoRole {
    pub fn columns(&self) -> Vec<&str> {
        match self {
            Self::Columns(OneOrManyStrings::One(column)) => vec![column],
            Self::Columns(OneOrManyStrings::Many(columns)) => {
                columns.iter().map(String::as_str).collect()
            }
            Self::MultiValue(role) => vec![role.column()],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.columns().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "format")]
pub enum SqliteMultiValueWhoRole {
    JsonArray { column: String },
    Delimited { column: String, delimiter: String },
}

impl SqliteMultiValueWhoRole {
    pub fn column(&self) -> &str {
        match self {
            Self::JsonArray { column } | Self::Delimited { column, .. } => column,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrManyStrings {
    One(String),
    Many(Vec<String>),
}

fn deserialize_one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => vec![value],
        OneOrMany::Many(values) => values,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteTimestampConfig {
    pub unit: SqliteTimestampUnit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqliteTimestampUnit {
    S,
    Ms,
    Us,
    Ns,
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
    update: UpdateTomlSection,
    #[serde(default)]
    workspaces: HashMap<String, WorkspaceTomlSection>,
    /// Backward-compatible path-addressed Markdown vault configuration.
    #[serde(default)]
    vaults: HashMap<String, VaultTomlSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateTomlSection {
    #[serde(default = "default_true")]
    auto: bool,
}

impl Default for UpdateTomlSection {
    fn default() -> Self {
        Self { auto: true }
    }
}

fn default_true() -> bool {
    true
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
    #[serde(default)]
    sources: HashMap<String, SqliteSourceConfig>,
    /// Per-vault override for max embedding files. `0` = unlimited.
    #[serde(default)]
    max_embedding_files: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkspaceTomlSection {
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
    #[serde(default)]
    sources: HashMap<String, WorkspaceSourceToml>,
    #[serde(default)]
    catalyst_format: Option<String>,
    #[serde(default)]
    max_embedding_files: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum WorkspaceSourceToml {
    Notes(NotesSourceToml),
    Sqlite(SqliteSourceConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesSourceToml {
    path: PathBuf,
    #[serde(default)]
    exclusions: Vec<String>,
    #[serde(default = "default_true")]
    writable: bool,
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
        if let Ok(Some(name)) = EnzymePaths::bound_workspace(vault_path) {
            return name;
        }
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
        let mut keys: Vec<_> = global.workspaces.keys().cloned().collect();
        keys.extend(global.vaults.keys().cloned());
        keys.sort();
        keys.dedup();
        Ok(keys)
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

    /// Whether background binary updates are enabled in the global config.
    /// Missing config and missing `[update]` sections both default to enabled.
    pub fn automatic_updates_enabled() -> Result<bool> {
        let path = Self::config_path()?;
        Self::automatic_updates_enabled_at(&path)
    }

    pub fn automatic_updates_enabled_at(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(true);
        }
        Ok(load_global_toml(path)?.update.auto)
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
            let home = config_path.parent().unwrap_or_else(|| Path::new("."));
            if let Some(name) = EnzymePaths::bound_workspace_with_home(vault_path, home)? {
                return Ok(Some(Self::from_workspace_config(&name, &global)));
            }
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
            sources: vault.map(|v| v.sources.clone()).unwrap_or_default(),
            notes_path: None,
            notes_roots: Vec::new(),
            max_embedding_files: vault
                .and_then(|v| v.max_embedding_files)
                .or(global.defaults.max_embedding_files),
        }
    }

    fn from_workspace_config(name: &str, global: &GlobalTomlConfig) -> Self {
        let workspace = global.workspaces.get(name);
        let trending = SelectionConfig {
            minimum_tags: global.defaults.minimum_tags,
            minimum_links: global.defaults.minimum_links,
            minimum_folders: global.defaults.minimum_folders,
            total_limit: global.defaults.total_limit,
            min_top_catalysts: workspace.and_then(|w| w.min_top_catalysts),
        };

        let mut notes_roots: Vec<NotesRootConfig> = workspace
            .map(|w| {
                w.sources
                    .iter()
                    .filter_map(|(name, source)| match source {
                        WorkspaceSourceToml::Notes(notes) => Some(NotesRootConfig {
                            name: name.clone(),
                            path: notes.path.clone(),
                            exclusions: notes.exclusions.clone(),
                            writable: notes.writable,
                        }),
                        WorkspaceSourceToml::Sqlite(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        notes_roots.sort_by(|left, right| left.name.cmp(&right.name));
        let notes_path = notes_roots
            .iter()
            .find(|root| root.name == "notes")
            .map(|root| root.path.clone())
            .or_else(|| (notes_roots.len() == 1).then(|| notes_roots[0].path.clone()));
        let sources = workspace
            .map(|w| {
                w.sources
                    .iter()
                    .filter_map(|(source_name, source)| match source {
                        WorkspaceSourceToml::Sqlite(sqlite) => {
                            Some((source_name.clone(), sqlite.clone()))
                        }
                        WorkspaceSourceToml::Notes(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            catalyzed_entities: workspace.map(|w| w.entities.clone()).unwrap_or_default(),
            trending: Some(trending),
            catalyst_format: workspace
                .and_then(|w| w.catalyst_format.clone())
                .or_else(|| global.defaults.catalyst_format.clone()),
            excluded_tags: workspace
                .map(|w| w.excluded_tags.clone())
                .unwrap_or_default(),
            excluded_links: workspace
                .map(|w| w.excluded_links.clone())
                .unwrap_or_default(),
            excluded_folders: workspace
                .map(|w| w.excluded_folders.clone())
                .unwrap_or_default(),
            targets: workspace.map(|w| w.targets.clone()).unwrap_or_default(),
            sources,
            notes_path,
            notes_roots,
            max_embedding_files: workspace
                .and_then(|w| w.max_embedding_files)
                .or(global.defaults.max_embedding_files),
        }
    }

    /// Legacy single Markdown root for this target. Named workspaces with
    /// several roots should use [`Self::markdown_roots`]; legacy vaults
    /// implicitly use the path by which they were addressed.
    pub fn markdown_root(vault_path: &Path) -> Result<Option<PathBuf>> {
        if EnzymePaths::bound_workspace(vault_path)?.is_some() {
            return Ok(Self::load(vault_path)?.and_then(|config| config.notes_path));
        }
        Ok(Some(vault_path.to_path_buf()))
    }

    /// Markdown roots for this target. Legacy/path-addressed vaults and the
    /// historic single `notes` source preserve un-namespaced source refs.
    pub fn markdown_roots(vault_path: &Path) -> Result<Vec<NotesRootConfig>> {
        if EnzymePaths::bound_workspace(vault_path)?.is_some() {
            return Ok(Self::load(vault_path)?
                .map(|config| config.notes_roots)
                .unwrap_or_default());
        }
        Ok(vec![NotesRootConfig {
            name: "notes".to_string(),
            path: vault_path.to_path_buf(),
            exclusions: Vec::new(),
            writable: true,
        }])
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
            sources: HashMap::new(),
            notes_path: None,
            notes_roots: Vec::new(),
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

    /// Persist automatically selected entities to a caller-owned config file.
    ///
    /// The explicit path is also the ownership root for named workspaces, and
    /// `header` is used when initializing a file. This path never consults or
    /// mutates `ENZYME_HOME`.
    pub fn set_auto_selected_entities_at(
        &mut self,
        vault_path: &Path,
        config_path: &Path,
        entity_refs: Vec<String>,
        header: &str,
    ) -> Result<()> {
        self.catalyzed_entities = entity_refs
            .into_iter()
            .map(CatalyzedEntity::Simple)
            .collect();
        self.save_at(vault_path, config_path, header)
    }

    pub fn save(&self, vault_path: &Path) -> Result<()> {
        let config_path = Self::config_path()?;
        self.save_at(vault_path, &config_path, CONFIG_HEADER)
    }

    fn save_at(&self, vault_path: &Path, config_path: &Path, header: &str) -> Result<()> {
        let mut global = if config_path.exists() {
            load_global_toml(&config_path)?
        } else {
            GlobalTomlConfig::default()
        };

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

        let config_home = config_path.parent().unwrap_or_else(|| Path::new("."));
        let workspace = EnzymePaths::bound_workspace_with_home(vault_path, config_home)?;

        if let Some(name) = workspace.clone() {
            let entry = global.workspaces.entry(name).or_default();
            entry.excluded_tags = self.excluded_tags.clone();
            entry.excluded_links = self.excluded_links.clone();
            entry.excluded_folders = self.excluded_folders.clone();
            entry.min_top_catalysts = self.trending.as_ref().and_then(|t| t.min_top_catalysts);
            entry.entities = self.catalyzed_entities.clone();
            entry.targets = self.targets.clone();
            entry.sources = self
                .sources
                .iter()
                .map(|(source_name, source)| {
                    (
                        source_name.clone(),
                        WorkspaceSourceToml::Sqlite(source.clone()),
                    )
                })
                .collect();
            if self.notes_roots.is_empty()
                && let Some(path) = &self.notes_path
            {
                entry.sources.insert(
                    "notes".to_string(),
                    WorkspaceSourceToml::Notes(NotesSourceToml {
                        path: path.clone(),
                        exclusions: Vec::new(),
                        writable: true,
                    }),
                );
            }
            for root in &self.notes_roots {
                entry.sources.insert(
                    root.name.clone(),
                    WorkspaceSourceToml::Notes(NotesSourceToml {
                        path: root.path.clone(),
                        exclusions: root.exclusions.clone(),
                        writable: root.writable,
                    }),
                );
            }
            entry.catalyst_format = self.catalyst_format.clone();
            entry.max_embedding_files = self.max_embedding_files;
        } else {
            global.defaults.catalyst_format = self.catalyst_format.clone();
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
            entry.sources = self.sources.clone();
            entry.max_embedding_files = self.max_embedding_files;
        }

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        // Fresh file: nothing to preserve, emit the canonical typed serialization.
        if !config_path.exists() {
            let body =
                toml::to_string_pretty(&global).context("failed to serialize enzyme config")?;
            fs::write(config_path, format!("{header}{body}"))
                .with_context(|| format!("failed to write {}", config_path.display()))?;
            debug!(?config_path, "saved enzyme config");
            return Ok(());
        }

        // Existing file: apply only the engine-managed keys in place, so user
        // comments, formatting, unknown keys, and authored `sources` tables (and
        // their comments) survive. Sources round-trip unchanged, so they are not
        // rewritten here; only the derived `notes` markdown source is ensured.
        let original = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut doc: toml_edit::DocumentMut = original.parse().with_context(|| {
            format!(
                "failed to parse {} for in-place update",
                config_path.display()
            )
        })?;

        {
            let defaults = ensure_table(doc.as_table_mut(), "defaults");
            set_optional(
                defaults,
                "catalyst_format",
                &global.defaults.catalyst_format,
            )?;
            set_value(defaults, "minimum_tags", &global.defaults.minimum_tags)?;
            set_value(defaults, "minimum_links", &global.defaults.minimum_links)?;
            set_value(
                defaults,
                "minimum_folders",
                &global.defaults.minimum_folders,
            )?;
            set_value(defaults, "total_limit", &global.defaults.total_limit)?;
            set_optional(
                defaults,
                "max_embedding_files",
                &global.defaults.max_embedding_files,
            )?;
        }

        if let Some(name) = workspace {
            let ws = global
                .workspaces
                .get(&name)
                .expect("workspace entry was just populated");
            let workspaces = ensure_table(doc.as_table_mut(), "workspaces");
            workspaces.set_implicit(true);
            let entry = ensure_table(workspaces, &name);
            apply_common_entry(
                entry,
                &ws.excluded_tags,
                &ws.excluded_links,
                &ws.excluded_folders,
                &ws.min_top_catalysts,
                &ws.entities,
                &ws.targets,
                &ws.max_embedding_files,
            )?;
            set_optional(entry, "catalyst_format", &ws.catalyst_format)?;
            if !self.sources.is_empty() || self.notes_path.is_some() || !self.notes_roots.is_empty()
            {
                let sources = ensure_table(entry, "sources");
                for (source_name, source) in &self.sources {
                    // Only write sources absent from the file; an authored contract
                    // already present is left untouched so its comments survive.
                    if !sources.contains_key(source_name) {
                        set_value(
                            sources,
                            source_name,
                            &WorkspaceSourceToml::Sqlite(source.clone()),
                        )?;
                    }
                }
                if self.notes_roots.is_empty()
                    && let Some(path) = &self.notes_path
                {
                    let notes = ensure_table(sources, "notes");
                    set_value(notes, "path", path)?;
                }
                for root in &self.notes_roots {
                    let notes = ensure_table(sources, &root.name);
                    set_value(notes, "path", &root.path)?;
                    set_value(notes, "exclusions", &root.exclusions)?;
                    set_value(notes, "writable", &root.writable)?;
                }
            }
        } else {
            let key = canonical_vault_key(vault_path);
            let vault = global
                .vaults
                .get(&key)
                .expect("vault entry was just populated");
            let vaults = ensure_table(doc.as_table_mut(), "vaults");
            vaults.set_implicit(true);
            let entry = ensure_table(vaults, &key);
            apply_common_entry(
                entry,
                &vault.excluded_tags,
                &vault.excluded_links,
                &vault.excluded_folders,
                &vault.min_top_catalysts,
                &vault.entities,
                &vault.targets,
                &vault.max_embedding_files,
            )?;
            if !self.sources.is_empty() {
                let sources = ensure_table(entry, "sources");
                for (source_name, source) in &self.sources {
                    if !sources.contains_key(source_name) {
                        set_value(sources, source_name, source)?;
                    }
                }
            }
        }

        let rendered = doc.to_string();
        let contents = if original.trim_start().starts_with('#') {
            rendered
        } else {
            format!("{header}{rendered}")
        };
        fs::write(&config_path, contents)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        debug!(?config_path, "saved enzyme config (in place)");
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

#[derive(Serialize)]
struct SaveWrap<'a, T> {
    v: &'a T,
}

/// Serialize a serde value into a standalone `toml_edit` item, so a single
/// managed key can be replaced in place without reserializing the whole file.
fn serialized_item<T: Serialize>(value: &T) -> Result<toml_edit::Item> {
    let rendered = toml::to_string(&SaveWrap { v: value }).context("serialize config value")?;
    let doc: toml_edit::DocumentMut = rendered.parse().context("reparse config value")?;
    Ok(doc
        .as_table()
        .get("v")
        .cloned()
        .unwrap_or(toml_edit::Item::None))
}

/// Fetch or create a child table, preserving any existing table (and its
/// comments/formatting) at that key.
fn ensure_table<'a>(table: &'a mut toml_edit::Table, key: &str) -> &'a mut toml_edit::Table {
    if !table.contains_key(key) {
        table.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    table
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .expect("ensured key holds a table")
}

fn set_value<T: Serialize>(table: &mut toml_edit::Table, key: &str, value: &T) -> Result<()> {
    let mut replacement = serialized_item(value)?;
    if let Some(existing) = table.get_mut(key) {
        if let (Some(old_value), Some(new_value)) =
            (existing.as_value(), replacement.as_value_mut())
        {
            *new_value.decor_mut() = old_value.decor().clone();
        }
        *existing = replacement;
    } else {
        table.insert(key, replacement);
    }
    Ok(())
}

fn set_optional<T: Serialize>(
    table: &mut toml_edit::Table,
    key: &str,
    value: &Option<T>,
) -> Result<()> {
    match value {
        Some(inner) => {
            set_value(table, key, inner)?;
        }
        None => {
            table.remove(key);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_common_entry(
    entry: &mut toml_edit::Table,
    excluded_tags: &[String],
    excluded_links: &[String],
    excluded_folders: &[String],
    min_top_catalysts: &Option<usize>,
    entities: &[CatalyzedEntity],
    targets: &[String],
    max_embedding_files: &Option<usize>,
) -> Result<()> {
    set_value(entry, "excluded_tags", &excluded_tags.to_vec())?;
    set_value(entry, "excluded_links", &excluded_links.to_vec())?;
    set_value(entry, "excluded_folders", &excluded_folders.to_vec())?;
    set_optional(entry, "min_top_catalysts", min_top_catalysts)?;
    set_value(entry, "entities", &entities.to_vec())?;
    set_value(entry, "targets", &targets.to_vec())?;
    set_optional(entry, "max_embedding_files", max_embedding_files)?;
    Ok(())
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
    fn in_place_entry_update_preserves_comments_and_authored_sources() {
        // A hand-authored workspace config: top comment, an unknown key, and a
        // commented source contract as a subtable of the workspace.
        let authored = r#"# Enzyme configuration — do not delete this line
[workspaces.imessage]
# curated by hand
mystery_key = "keep me"

[workspaces.imessage.sources.messages]
# this contract was written by the skill
db = "/tmp/x.db"
query = "SELECT id, ts, body FROM messages"
roles = { id = "id", when = "ts", what = "body" }
timestamp = { unit = "ms" }
"#;
        let mut doc: toml_edit::DocumentMut = authored.parse().unwrap();
        let workspaces = ensure_table(doc.as_table_mut(), "workspaces");
        workspaces.set_implicit(true);
        let entry = ensure_table(workspaces, "imessage");
        apply_common_entry(
            entry,
            &[],
            &[],
            &[],
            &None,
            &[CatalyzedEntity::Simple("alice".to_string())],
            &[],
            &Some(1024),
        )
        .unwrap();
        set_optional(entry, "catalyst_format", &Some("dialectical".to_string())).unwrap();

        let out = doc.to_string();
        // Must remain valid TOML even though leaf keys were added to a table that
        // already owned a subtable (ordering hazard).
        let _: toml_edit::DocumentMut = out.parse().expect("valid toml after in-place edit");
        let typed: GlobalTomlConfig = toml::from_str(&out).expect("typed parse still works");

        assert!(
            out.contains("# Enzyme configuration"),
            "header lost:\n{out}"
        );
        assert!(
            out.contains("mystery_key = \"keep me\""),
            "unknown key lost:\n{out}"
        );
        assert!(
            out.contains("# this contract was written by the skill"),
            "source comment lost:\n{out}"
        );
        assert!(
            out.contains("SELECT id, ts, body FROM messages"),
            "source contract lost:\n{out}"
        );
        assert!(out.contains("alice"), "entity not written:\n{out}");

        // The authored contract still round-trips through the typed model.
        let ws = typed.workspaces.get("imessage").expect("workspace present");
        assert!(ws.sources.contains_key("messages"), "source dropped");
        assert_eq!(ws.entities.len(), 1);
    }

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
            sources: HashMap::new(),
            notes_path: None,
            notes_roots: Vec::new(),
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
    fn named_workspace_parses_multiple_markdown_roots() {
        let global: GlobalTomlConfig = toml::from_str(
            r#"
[workspaces.margins.sources.sessions]
path = "/notes/sessions"
exclusions = ["exports"]
writable = true

[workspaces.margins.sources.references]
path = "/notes/references"
exclusions = ["cache", "private"]
writable = false
"#,
        )
        .unwrap();
        let config = EnzymeConfig::from_workspace_config("margins", &global);
        assert_eq!(config.notes_roots.len(), 2);
        assert_eq!(config.notes_roots[0].name, "references");
        assert_eq!(config.notes_roots[0].exclusions, vec!["cache", "private"]);
        assert!(!config.notes_roots[0].writable);
        assert_eq!(config.notes_path, None);
    }

    #[test]
    fn sqlite_sources_parse_with_optional_roles_and_timestamp_variants() {
        let without_sources: GlobalTomlConfig = toml::from_str(r#"[vaults."/tmp/plain"]"#).unwrap();
        let plain = EnzymeConfig::from_global_config(Path::new("/tmp/plain"), &without_sources);
        assert!(plain.sources.is_empty());

        let global: GlobalTomlConfig = toml::from_str(
            r#"
[vaults."/tmp/sources".sources.seconds]
db = "/tmp/seconds.db"
query = "SELECT id, ts, body FROM rows"
roles = { id = "id", when = "ts", what = "body" }
timestamp = { unit = "s" }

[vaults."/tmp/sources".sources.millis]
db = "/tmp/millis.db"
query = "SELECT account_id, message_id, sender, ts, subject, body, thread FROM rows"
roles = { id = ["account_id", "message_id"], who = ["sender"], when = "ts", what = ["subject", "body"], where = ["thread"] }
timestamp = { unit = "ms", epoch = "2001-01-01" }

[vaults."/tmp/sources".sources.micros]
db = "/tmp/micros.db"
query = "SELECT id, ts, body FROM rows"
roles = { id = "id", when = "ts", what = "body" }
timestamp = { unit = "us" }

[vaults."/tmp/sources".sources.nanos]
db = "/tmp/nanos.db"
query = "SELECT id, ts, body FROM rows"
roles = { id = "id", when = "ts", what = "body" }
timestamp = { unit = "ns" }
"#,
        )
        .unwrap();
        let config = EnzymeConfig::from_global_config(Path::new("/tmp/sources"), &global);
        assert_eq!(config.sources.len(), 4);

        let seconds = config.sources.get("seconds").unwrap();
        assert_eq!(seconds.roles.id, vec!["id"]);
        assert!(seconds.roles.who.is_empty());
        assert!(seconds.roles.r#where.is_empty());
        assert_eq!(seconds.timestamp.unit, SqliteTimestampUnit::S);

        let millis = config.sources.get("millis").unwrap();
        assert_eq!(millis.roles.id, vec!["account_id", "message_id"]);
        assert_eq!(millis.roles.what, vec!["subject", "body"]);
        assert_eq!(millis.timestamp.unit, SqliteTimestampUnit::Ms);
        assert_eq!(millis.timestamp.epoch.as_deref(), Some("2001-01-01"));
        assert_eq!(
            config.sources.get("micros").unwrap().timestamp.unit,
            SqliteTimestampUnit::Us
        );
        assert_eq!(
            config.sources.get("nanos").unwrap().timestamp.unit,
            SqliteTimestampUnit::Ns
        );
    }

    #[test]
    fn named_workspace_parses_notes_and_sqlite_as_peer_sources() {
        let global: GlobalTomlConfig = toml::from_str(
            r#"
[workspaces.imessage]
entities = ["folder:conversations"]

[workspaces.imessage.sources.notes]
path = "/Users/alice/notes"

[workspaces.imessage.sources.messages]
db = "/Users/alice/Library/Messages/chat.db"
query = "SELECT id, ts, body FROM messages"
roles = { id = "id", when = "ts", what = "body" }
timestamp = { unit = "s" }
"#,
        )
        .unwrap();

        let config = EnzymeConfig::from_workspace_config("imessage", &global);
        assert_eq!(config.notes_path, Some(PathBuf::from("/Users/alice/notes")));
        assert_eq!(config.sources.len(), 1);
        assert_eq!(
            config.sources.get("messages").unwrap().db,
            PathBuf::from("/Users/alice/Library/Messages/chat.db")
        );
        assert_eq!(config.catalyzed_entities.len(), 1);
    }

    #[test]
    fn home_rooted_workspace_selects_named_config_over_legacy_path_config() {
        let temp = TempDir::new().unwrap();
        let store = temp.path().join("workspaces/imessage");
        fs::create_dir_all(&store).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
[workspaces.imessage.sources.notes]
path = "/named/notes"

[vaults."{}"]
excluded_tags = ["legacy"]
"#,
                store.display()
            ),
        )
        .unwrap();

        let config = EnzymeConfig::load_at(&store, &config_path)
            .unwrap()
            .unwrap();
        assert_eq!(config.notes_path, Some(PathBuf::from("/named/notes")));
        assert!(config.excluded_tags.is_empty());
        assert_eq!(
            EnzymePaths::bound_workspace_with_home(&store, temp.path())
                .unwrap()
                .as_deref(),
            Some("imessage")
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
    fn automatic_updates_default_on_and_support_global_opt_out() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing.toml");
        assert!(EnzymeConfig::automatic_updates_enabled_at(&missing).unwrap());

        let config = temp.path().join("config.toml");
        fs::write(&config, "[update]\nauto = false\n").unwrap();
        assert!(!EnzymeConfig::automatic_updates_enabled_at(&config).unwrap());

        fs::write(&config, "[defaults]\nminimum_tags = 1\n").unwrap();
        assert!(EnzymeConfig::automatic_updates_enabled_at(&config).unwrap());
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
            sources: HashMap::from([(
                "messages".to_string(),
                SqliteSourceConfig {
                    db: PathBuf::from("/tmp/messages.db"),
                    query: "SELECT id, sender, sent_at, subject, body, thread FROM messages"
                        .to_string(),
                    roles: SqliteSourceRoles {
                        id: vec!["id".to_string()],
                        who: SqliteWhoRole::Columns(OneOrManyStrings::Many(vec![
                            "sender".to_string(),
                        ])),
                        when: "sent_at".to_string(),
                        what: vec!["subject".to_string(), "body".to_string()],
                        r#where: vec!["thread".to_string()],
                        weight: None,
                    },
                    timestamp: SqliteTimestampConfig {
                        unit: SqliteTimestampUnit::Ms,
                        epoch: Some("2001-01-01".to_string()),
                    },
                },
            )]),
            notes_path: None,
            notes_roots: Vec::new(),
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

        let mut loaded = EnzymeConfig::load(&vault).unwrap().unwrap();
        assert_eq!(loaded.catalyst_format.as_deref(), Some("question"));
        assert_eq!(loaded.catalyzed_entities.len(), 2);
        assert_eq!(loaded.targets, vec!["/Users/alice/code/my-app"]);
        let source = loaded.sources.get("messages").unwrap();
        assert_eq!(source.roles.id, vec!["id"]);
        assert_eq!(source.roles.who.columns(), vec!["sender"]);
        assert_eq!(source.roles.what, vec!["subject", "body"]);
        assert_eq!(source.roles.r#where, vec!["thread"]);
        assert_eq!(source.timestamp.unit, SqliteTimestampUnit::Ms);
        assert_eq!(source.timestamp.epoch.as_deref(), Some("2001-01-01"));
        let llm = EnzymeConfig::catalyst_llm_config().unwrap();
        assert_eq!(llm.mode, CatalystMode::Local);
        assert_eq!(llm.local_model.as_deref(), Some("catalyst-135m-v3"));
        assert_eq!(
            loaded.trending.as_ref().and_then(|t| t.min_top_catalysts),
            Some(13)
        );
        let expected_sources = loaded.sources.clone();
        loaded
            .set_auto_selected_entities(&vault, vec!["[[Rewritten]]".to_string()])
            .unwrap();
        let rewritten = EnzymeConfig::load(&vault).unwrap().unwrap();
        assert_eq!(rewritten.sources, expected_sources);

        // SAFETY: test-only, single-threaded
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("ENZYME_HOME", v),
                None => std::env::remove_var("ENZYME_HOME"),
            }
        }
    }
}
