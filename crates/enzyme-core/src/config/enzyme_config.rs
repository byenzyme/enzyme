use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::models::EntityType;

// ---------------------------------------------------------------------------
// YAML schema types
// ---------------------------------------------------------------------------

/// Top-level `enzyme-config.yaml` that lives at `<vault>/.enzyme/enzyme-config.yaml`
/// (or `<vault>/enzyme-config.yaml` for backwards compat).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnzymeConfig {
    /// User-curated entities for catalyst generation.
    #[serde(default)]
    pub catalyzed_entities: Vec<CatalyzedEntity>,

    /// Default catalyst generation profile for the vault.
    #[serde(default)]
    pub catalyst_profile: Option<String>,

    /// Trending distribution overrides.
    #[serde(default)]
    pub trending: Option<TrendingConfig>,

    /// Catalyst output format (question, thesis, claim).
    #[serde(default)]
    pub catalyst_format: Option<String>,

    /// Custom profile guidance from vault guide generator.
    #[serde(default)]
    pub custom_profile_guidance: Option<CustomProfileGuidance>,

    /// Optional guide signal for calibrating catalyst generation.
    #[serde(default)]
    pub guide_signal: Option<String>,

    /// Tags to exclude from trending selection.
    #[serde(default)]
    pub excluded_tags: Vec<String>,

    /// Links to exclude from trending selection.
    #[serde(default)]
    pub excluded_links: Vec<String>,

    /// Folders to exclude from trending selection.
    #[serde(default)]
    pub excluded_folders: Vec<String>,
}

/// A single entry in `catalyzedEntities`.
///
/// Supports two YAML forms:
/// - Simple string: `"#tag"` or `"folder:inbox"` or `"[[Note]]"`
/// - Map with options: `"#tag": { profile: "strategic" }`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CatalyzedEntity {
    Simple(String),
    WithOptions(HashMap<String, EntityOptions>),
}

/// Per-entity options in the config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityOptions {
    pub profile: Option<String>,
    #[serde(default)]
    pub expandable: bool,
    #[serde(default, deserialize_with = "deserialize_null_as_empty_vec")]
    pub children: Vec<String>,
}

/// Deserialize a `Vec<String>` that tolerates YAML `null` (treats it as `[]`).
fn deserialize_null_as_empty_vec<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<String>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Custom profile guidance from vault guide generator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomProfileGuidance {
    #[serde(default)]
    pub base_profile: Option<String>,
    #[serde(default)]
    pub guidance_lines: Vec<String>,
    #[serde(default)]
    pub domain_vocabulary: Option<DomainVocabulary>,
    #[serde(default)]
    pub theoretical_frame: Option<String>,
}

/// Domain-specific vocabulary for custom guidance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainVocabulary {
    #[serde(default)]
    pub metaphors: Vec<String>,
    #[serde(default)]
    pub named_concepts: Vec<String>,
    #[serde(default)]
    pub structural_patterns: Vec<String>,
}

/// Override trending distribution minimums.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingConfig {
    #[serde(default = "default_min_tags")]
    pub minimum_tags: usize,
    #[serde(default = "default_min_links")]
    pub minimum_links: usize,
    #[serde(default = "default_min_folders")]
    pub minimum_folders: usize,
    #[serde(default = "default_total_limit")]
    pub total_limit: usize,
}

fn default_min_tags() -> usize { 8 }
fn default_min_links() -> usize { 6 }
fn default_min_folders() -> usize { 6 }
fn default_total_limit() -> usize { 30 }

impl Default for TrendingConfig {
    fn default() -> Self {
        Self {
            minimum_tags: default_min_tags(),
            minimum_links: default_min_links(),
            minimum_folders: default_min_folders(),
            total_limit: default_total_limit(),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed output
// ---------------------------------------------------------------------------

/// A parsed entity reference from the config.
#[derive(Debug, Clone)]
pub struct ParsedEntity {
    /// The entity name as stored in the DB (e.g. `"enzyme"`, `"maren holt"`, `"inbox"`).
    pub name: String,
    /// The entity type inferred from the prefix.
    pub entity_type: EntityType,
}

/// Normalized output from parsing the config.
#[derive(Debug, Clone, Default)]
pub struct EntityPreferences {
    /// Entities to generate catalysts for.
    pub entities: Vec<ParsedEntity>,
    /// Per-entity catalyst profile overrides (entity name -> profile).
    pub entity_profiles: HashMap<String, String>,
    /// Expandable folders and their children (folder name -> child names).
    pub expandable_folders: HashMap<String, Vec<ParsedEntity>>,
    /// Entity names to exclude.
    pub excluded_names: Vec<ParsedEntity>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl EnzymeConfig {
    /// Load the config from the vault. Tries `<vault>/.enzyme/enzyme-config.yaml`
    /// first, then `<vault>/enzyme-config.yaml` for backwards compatibility.
    /// Returns `None` if no config file exists.
    pub fn load(vault_path: &Path) -> Result<Option<Self>> {
        let enzyme_dir_path = vault_path.join(".enzyme").join("enzyme-config.yaml");
        let root_path = vault_path.join("enzyme-config.yaml");

        let config_path = if enzyme_dir_path.exists() {
            enzyme_dir_path
        } else if root_path.exists() {
            root_path
        } else {
            debug!("no enzyme-config.yaml found");
            return Ok(None);
        };

        debug!(?config_path, "loading enzyme config");
        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;

        let config: EnzymeConfig = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;

        Ok(Some(config))
    }

    /// Parse the config into normalized entity preferences.
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
                        let Some(parsed) = parse_entity_ref(raw) else {
                            continue;
                        };

                        // Record profile override
                        if let Some(ref profile) = options.profile {
                            prefs
                                .entity_profiles
                                .insert(parsed.name.clone(), profile.clone());
                        }

                        // Handle expandable folders
                        if options.expandable {
                            let children: Vec<ParsedEntity> = options
                                .children
                                .iter()
                                .filter_map(|c| parse_entity_ref(c))
                                .collect();

                            // Also add children as top-level entities
                            for child in &children {
                                prefs.entities.push(child.clone());
                                // Inherit parent's profile if child doesn't have its own
                                if let Some(ref profile) = options.profile {
                                    prefs
                                        .entity_profiles
                                        .entry(child.name.clone())
                                        .or_insert_with(|| profile.clone());
                                }
                            }

                            prefs
                                .expandable_folders
                                .insert(parsed.name.clone(), children);
                        }

                        prefs.entities.push(parsed);
                    }
                }
            }
        }

        // Parse excluded entities
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

    /// Whether the config has any user-curated entities.
    pub fn has_curated_entities(&self) -> bool {
        !self.catalyzed_entities.is_empty()
    }
}

/// Parse a raw entity reference string into a name and type.
///
/// Formats:
/// - `#tag` or `#tag/subtag` -> Tag, name = `"tag"` / `"tag/subtag"`
/// - `[[Note Name]]` -> Link, name = `"note name"` (lowercased)
/// - `folder:path` -> Folder, name = `"path"`
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
    } else if let Some(path) = trimmed.strip_prefix("folder:") {
        let name = path.trim().to_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(ParsedEntity {
            name,
            entity_type: EntityType::Folder,
        })
    } else {
        // Unknown format — skip
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let p = parse_entity_ref("[[Maren Holt]]").unwrap();
        assert_eq!(p.name, "maren holt");
        assert_eq!(p.entity_type, EntityType::Link);
    }

    #[test]
    fn test_parse_entity_ref_folder() {
        let p = parse_entity_ref("folder:inbox").unwrap();
        assert_eq!(p.name, "inbox");
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
    fn test_parse_config_yaml() {
        let yaml = r##"
catalyzedEntities:
  - "#enzyme":
      profile: operational
  - folder:inbox
  - "[[Maren Holt]]"
  - folder:people:
      expandable: true
      profile: relational
      children:
        - "[[ari]]"
        - "[[milo strand]]"
catalystProfile: dialectical
excludedTags:
  - note
  - todo
"##;

        let config: EnzymeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.catalyzed_entities.len(), 4);
        assert_eq!(config.catalyst_profile, Some("dialectical".to_string()));
        assert_eq!(config.excluded_tags.len(), 2);

        let prefs = config.parse_preferences();
        // 4 top-level + 2 children from expandable folder
        assert_eq!(prefs.entities.len(), 6);
        assert_eq!(prefs.entity_profiles.get("enzyme"), Some(&"operational".to_string()));
        assert_eq!(prefs.entity_profiles.get("people"), Some(&"relational".to_string()));
        // Children inherit parent profile
        assert_eq!(prefs.entity_profiles.get("ari"), Some(&"relational".to_string()));
        assert!(prefs.expandable_folders.contains_key("people"));
    }
}
