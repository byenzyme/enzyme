use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::db::Database;
use crate::llm::{
    build_selection_system_prompt, build_selection_user_prompt, parse_selection_response,
    selection_json_schema,
    ExpandableFolder, GenerationParams, LlmClient, SelectionResponse,
};
use crate::models::Entity;
use crate::pipeline::context::ContextBuilder;
use crate::pipeline::selection::{SelectionPipeline, SelectionResult};

pub struct LlmSelectionPipeline<'a> {
    db: &'a Database,
    llm: &'a LlmClient,
}

impl<'a> LlmSelectionPipeline<'a> {
    pub fn new(db: &'a Database, llm: &'a LlmClient) -> Self {
        Self { db, llm }
    }

    /// Run LLM-assisted entity selection.
    ///
    /// 1. Build vault context XML from trending candidates (with page entity detection)
    /// 2. Ask the LLM to select and profile entities
    /// 3. Post-process: deterministically set expandable folders based on page entities
    /// 4. Write enzyme-config.yaml
    /// 5. Re-run SelectionPipeline (now picks up the written config)
    pub async fn run(
        &self,
        candidates: &[Entity],
        vault_path: &Path,
        model: &str,
        vault_guide: Option<&str>,
    ) -> Result<SelectionResult> {
        // Build vault context XML with page entity detection
        let ctx_builder = ContextBuilder::new(self.db);
        let ctx_result = ctx_builder
            .build_with_page_entities(candidates, 5, 300)
            .context("failed to build vault context for LLM selection")?;

        debug!(
            context_bytes = ctx_result.xml.len(),
            folders_with_page_entities = ctx_result.folders_with_page_entities.len(),
            "built vault context XML for LLM selection"
        );

        // Build prompts
        let system_prompt = build_selection_system_prompt();
        let mut user_prompt = build_selection_user_prompt(&ctx_result.xml);
        if let Some(guide) = vault_guide {
            user_prompt.push_str(&format!(
                "\n\n<vault_guide>\n{}\n</vault_guide>",
                guide
            ));
        }

        // Call LLM — selection responses are larger than catalyst responses,
        // so we need more tokens than the default 1024.
        let structured_output = self.llm.structured_output_support();
        let response_format = structured_output
            .build_response_format("selection_response", selection_json_schema());
        let params = GenerationParams {
            max_tokens: 4096,
            response_format,
            ..GenerationParams::default()
        };

        let response_text = self
            .llm
            .chat_completion_cached(model, &system_prompt, &user_prompt, params)
            .await
            .context("LLM entity selection call failed")?;

        debug!(
            response_len = response_text.len(),
            "received LLM selection response"
        );

        // Parse response
        let mut selection = match parse_selection_response(&response_text) {
            Ok(s) => s,
            Err(e) => {
                debug!(
                    response_preview = &response_text[..response_text.len().min(500)],
                    "LLM selection response that failed to parse"
                );
                return Err(e).context("failed to parse LLM selection response");
            }
        };

        // Post-process: deterministically set expandable folders based on page entities
        post_process_expandable_folders(&mut selection, &ctx_result.folders_with_page_entities);

        info!(
            entities = selection.catalyzed_entities.len(),
            expandable_folders = selection.expandable_folders.len(),
            "LLM selected entities (after post-processing)"
        );

        // Write config
        write_config(vault_path, &selection)?;

        // Re-run selection pipeline with the new config
        let pipeline = SelectionPipeline::new(self.db, vault_path);
        pipeline.run()
    }
}

/// Deterministically set expandable folders based on page entity detection.
///
/// This is more reliable than depending on the LLM to correctly identify
/// expandable folders. We know structurally which folders have trending
/// wikilinks as source files, so we set expandable programmatically.
///
/// Also deduplicates: removes wikilinks from `catalyzed_entities` that are
/// children of an expandable folder (the folder subsumes them).
fn post_process_expandable_folders(
    selection: &mut SelectionResponse,
    folders_with_page_entities: &HashMap<String, Vec<String>>,
) {
    if folders_with_page_entities.is_empty() {
        return;
    }

    // Build a set of existing expandable folder refs for merging
    let existing_expandable: std::collections::HashSet<String> = selection
        .expandable_folders
        .iter()
        .map(|f| f.folder_ref.clone())
        .collect();

    // For each folder entity in the selection that has page entities,
    // ensure it's in expandable_folders with its children
    for entity in &selection.catalyzed_entities {
        let folder_name = if let Some(name) = entity.entity_ref.strip_prefix("folder:") {
            name
        } else {
            continue;
        };

        if let Some(page_entities) = folders_with_page_entities.get(folder_name) {
            let folder_ref = entity.entity_ref.clone();

            if existing_expandable.contains(&folder_ref) {
                // Already expandable from LLM — merge children
                if let Some(existing) = selection
                    .expandable_folders
                    .iter_mut()
                    .find(|f| f.folder_ref == folder_ref)
                {
                    // Add any page entities not already in children
                    let existing_children: std::collections::HashSet<String> =
                        existing.children.iter().cloned().collect();
                    for pe in page_entities {
                        let wikilink = format!("[[{}]]", pe);
                        if !existing_children.contains(&wikilink) {
                            existing.children.push(wikilink);
                        }
                    }
                }
            } else {
                // Not yet expandable — promote it
                let children: Vec<String> =
                    page_entities.iter().map(|pe| format!("[[{}]]", pe)).collect();
                selection.expandable_folders.push(ExpandableFolder {
                    folder_ref,
                    children,
                    profile: entity.profile.clone(),
                });
            }
        }
    }

    // Deduplicate: collect all children of expandable folders as a set
    let expandable_children: std::collections::HashSet<String> = selection
        .expandable_folders
        .iter()
        .flat_map(|f| f.children.iter().cloned())
        .collect();

    // Remove wikilinks from catalyzed_entities that are children of expandable folders
    let before = selection.catalyzed_entities.len();
    selection
        .catalyzed_entities
        .retain(|e| !expandable_children.contains(&e.entity_ref));

    let removed = before - selection.catalyzed_entities.len();
    if removed > 0 {
        debug!(
            removed,
            "deduplicated wikilinks subsumed by expandable folders"
        );
    }
}

/// Write the LLM selection result as enzyme-config.yaml.
///
/// Uses a template approach for human-readable YAML output rather than
/// serde_yaml serialization (which doesn't handle the untagged CatalyzedEntity
/// enum well).
fn write_config(vault_path: &Path, selection: &SelectionResponse) -> Result<()> {
    let enzyme_dir = vault_path.join(".enzyme");
    std::fs::create_dir_all(&enzyme_dir)
        .context("failed to create .enzyme directory")?;

    let config_path = enzyme_dir.join("enzyme-config.yaml");

    let mut yaml = String::with_capacity(2048);
    yaml.push_str("# Auto-generated by enzyme init (LLM selection)\n");
    yaml.push_str("# Edit this file to customize which entities get catalysts.\n\n");

    // catalyzedEntities
    yaml.push_str("catalyzedEntities:\n");

    // First, write expandable folders
    let expandable_refs: std::collections::HashSet<&str> = selection
        .expandable_folders
        .iter()
        .map(|f| f.folder_ref.as_str())
        .collect();

    for folder in &selection.expandable_folders {
        let profile_str = folder
            .profile
            .as_deref()
            .map(|p| format!("\n      profile: {p}"))
            .unwrap_or_default();

        if folder.children.is_empty() {
            // No children — write expandable folder without children key
            yaml.push_str(&format!(
                "  - \"{}\":\n      expandable: true{}\n",
                yaml_escape(&folder.folder_ref),
                profile_str,
            ));
        } else {
            let children_str: String = folder
                .children
                .iter()
                .map(|c| format!("\n        - \"{}\"", yaml_escape(c)))
                .collect();

            yaml.push_str(&format!(
                "  - \"{}\":\n      expandable: true\n      children:{}{}\n",
                yaml_escape(&folder.folder_ref),
                children_str,
                profile_str,
            ));
        }
    }

    // Then write non-folder entities
    for entity in &selection.catalyzed_entities {
        // Skip entities that are expandable folders (already written above)
        if expandable_refs.contains(entity.entity_ref.as_str()) {
            continue;
        }

        if let Some(ref profile) = entity.profile {
            yaml.push_str(&format!(
                "  - \"{}\":\n      profile: {}\n",
                yaml_escape(&entity.entity_ref),
                profile,
            ));
        } else {
            yaml.push_str(&format!("  - \"{}\"\n", yaml_escape(&entity.entity_ref)));
        }
    }

    // catalystProfile
    if let Some(ref profile) = selection.catalyst_profile {
        yaml.push_str(&format!("\ncatalystProfile: {}\n", profile));
    }

    // guideSignal
    if let Some(ref signal) = selection.guide_signal {
        yaml.push_str(&format!("\nguideSignal: \"{}\"\n", yaml_escape(signal)));
    }

    // excludedTags / excludedLinks / excludedFolders
    let mut excluded_tags = Vec::new();
    let mut excluded_links = Vec::new();
    let mut excluded_folders = Vec::new();
    for entity in &selection.excluded_entities {
        let r = entity.entity_ref.trim();
        if let Some(tag) = r.strip_prefix('#') {
            excluded_tags.push(tag.to_string());
        } else if r.starts_with("[[") && r.ends_with("]]") {
            excluded_links.push(r[2..r.len() - 2].to_string());
        } else if let Some(folder) = r.strip_prefix("folder:") {
            excluded_folders.push(folder.to_string());
        }
    }
    if !excluded_tags.is_empty() {
        yaml.push_str("\nexcludedTags:\n");
        for tag in &excluded_tags {
            yaml.push_str(&format!("  - \"{}\"\n", yaml_escape(tag)));
        }
    }
    if !excluded_links.is_empty() {
        yaml.push_str("\nexcludedLinks:\n");
        for link in &excluded_links {
            yaml.push_str(&format!("  - \"{}\"\n", yaml_escape(link)));
        }
    }
    if !excluded_folders.is_empty() {
        yaml.push_str("\nexcludedFolders:\n");
        for folder in &excluded_folders {
            yaml.push_str(&format!("  - \"{}\"\n", yaml_escape(folder)));
        }
    }

    // Check for existing config — don't overwrite user-edited config
    if config_path.exists() {
        let existing = std::fs::read_to_string(&config_path)
            .unwrap_or_default();
        if !existing.contains("Auto-generated by enzyme init") {
            debug!("existing enzyme-config.yaml appears user-edited, skipping overwrite");
            return Ok(());
        }
    }

    std::fs::write(&config_path, &yaml)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    info!(?config_path, "wrote enzyme-config.yaml from LLM selection");
    Ok(())
}

/// Escape a string for YAML double-quoted values.
fn yaml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::enzyme_config::EnzymeConfig;
    use crate::llm::{ExcludedEntity, ExpandableFolder, SelectedEntity, SelectionResponse};

    #[test]
    fn test_write_config_creates_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path();

        let selection = SelectionResponse {
            catalyzed_entities: vec![
                SelectedEntity {
                    entity_ref: "#enzyme".to_string(),
                    profile: Some("dialectical".to_string()),
                },
                SelectedEntity {
                    entity_ref: "[[deep work]]".to_string(),
                    profile: Some("relational".to_string()),
                },
                SelectedEntity {
                    entity_ref: "folder:inbox".to_string(),
                    profile: None,
                },
            ],
            catalyst_profile: Some("dialectical".to_string()),
            guide_signal: Some("Thinking about craft".to_string()),
            expandable_folders: vec![],
            excluded_entities: vec![],
        };

        write_config(vault_path, &selection).unwrap();

        let config_path = vault_path.join(".enzyme").join("enzyme-config.yaml");
        assert!(config_path.exists());

        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("catalyzedEntities:"));
        assert!(contents.contains("#enzyme"));
        assert!(contents.contains("[[deep work]]"));
        assert!(contents.contains("catalystProfile: dialectical"));
        assert!(contents.contains("guideSignal:"));

        // Verify it parses back
        let config: EnzymeConfig = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(config.catalyzed_entities.len(), 3);
        assert_eq!(config.catalyst_profile.as_deref(), Some("dialectical"));
    }

    #[test]
    fn test_write_config_with_expandable_folders() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path();

        let selection = SelectionResponse {
            catalyzed_entities: vec![
                SelectedEntity {
                    entity_ref: "#tag".to_string(),
                    profile: None,
                },
                SelectedEntity {
                    entity_ref: "folder:people".to_string(),
                    profile: Some("relational".to_string()),
                },
            ],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![ExpandableFolder {
                folder_ref: "folder:people".to_string(),
                children: vec!["[[ari]]".to_string(), "[[jonah]]".to_string()],
                profile: Some("relational".to_string()),
            }],
            excluded_entities: vec![],
        };

        write_config(vault_path, &selection).unwrap();

        let config_path = vault_path.join(".enzyme").join("enzyme-config.yaml");
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("expandable: true"));
        assert!(contents.contains("[[ari]]"));
        assert!(contents.contains("[[jonah]]"));

        // Verify it parses back
        let config: EnzymeConfig = serde_yaml::from_str(&contents).unwrap();
        assert!(!config.catalyzed_entities.is_empty());
    }

    #[test]
    fn test_write_config_with_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path();

        let selection = SelectionResponse {
            catalyzed_entities: vec![SelectedEntity {
                entity_ref: "#enzyme".to_string(),
                profile: Some("dialectical".to_string()),
            }],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![],
            excluded_entities: vec![
                ExcludedEntity { entity_ref: "#todo".to_string() },
                ExcludedEntity { entity_ref: "#note".to_string() },
                ExcludedEntity { entity_ref: "[[template]]".to_string() },
                ExcludedEntity { entity_ref: "folder:templates".to_string() },
            ],
        };

        write_config(vault_path, &selection).unwrap();

        let config_path = vault_path.join(".enzyme").join("enzyme-config.yaml");
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("excludedTags:"));
        assert!(contents.contains("todo"));
        assert!(contents.contains("note"));
        assert!(contents.contains("excludedLinks:"));
        assert!(contents.contains("template"));
        assert!(contents.contains("excludedFolders:"));
        assert!(contents.contains("templates"));

        // Verify it parses back
        let config: EnzymeConfig = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(config.excluded_tags.len(), 2);
        assert_eq!(config.excluded_links.len(), 1);
        assert_eq!(config.excluded_folders.len(), 1);
    }

    #[test]
    fn test_write_config_does_not_overwrite_user_edited() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path();
        let enzyme_dir = vault_path.join(".enzyme");
        std::fs::create_dir_all(&enzyme_dir).unwrap();

        // Write a user-edited config (no auto-generated comment)
        let config_path = enzyme_dir.join("enzyme-config.yaml");
        std::fs::write(&config_path, "catalyzedEntities:\n  - \"#custom\"\n").unwrap();

        let selection = SelectionResponse {
            catalyzed_entities: vec![SelectedEntity {
                entity_ref: "#new".to_string(),
                profile: None,
            }],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![],
            excluded_entities: vec![],
        };

        write_config(vault_path, &selection).unwrap();

        // Should not have been overwritten
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("#custom"));
        assert!(!contents.contains("#new"));
    }
}
