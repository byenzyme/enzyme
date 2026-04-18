use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::db::Database;
use crate::llm::{
    ExpandableFolder, GenerationParams, LlmClient, SelectionResponse,
    build_selection_system_prompt, build_selection_user_prompt, parse_selection_response,
    selection_json_schema,
};
use crate::models::Entity;
use crate::pipeline::context::ContextBuilder;
use crate::pipeline::entity_map::EntityMap;
use crate::pipeline::selection::{SelectionPipeline, SelectionResult};
use crate::pipeline::selection_steps::{
    CATALYZED_FOLDER_CHILD_LIMIT, select_folder_children,
};

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
            folders_with_page_entities = ctx_result.entity_map.folder_page_entity_map().len(),
            "built vault context XML for LLM selection"
        );

        // Build prompts
        let system_prompt = build_selection_system_prompt();
        let mut user_prompt = build_selection_user_prompt(&ctx_result.xml);
        if let Some(guide) = vault_guide {
            user_prompt.push_str(&format!("\n\n<vault_guide>\n{}\n</vault_guide>", guide));
        }

        // Call LLM — selection responses are larger than catalyst responses,
        // so we need more tokens than the default 1024.
        let structured_output = self.llm.structured_output_support();
        let response_format =
            structured_output.build_response_format("selection_response", selection_json_schema());
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
        post_process_expandable_folders(self.db, &mut selection, &ctx_result.entity_map);

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
    db: &Database,
    selection: &mut SelectionResponse,
    entity_map: &EntityMap,
) {
    let folders_with_page_entities = entity_map.folder_page_entity_map();
    if folders_with_page_entities.is_empty() {
        return;
    }

    let folder_profiles: std::collections::HashMap<String, Option<String>> = selection
        .catalyzed_entities
        .iter()
        .filter_map(|entity| {
            entity
                .entity_ref
                .strip_prefix("folder:")
                .map(|name| (format!("folder:{name}"), entity.profile.clone()))
        })
        .collect();

    let mut folder_refs: Vec<String> = folder_profiles.keys().cloned().collect();
    for folder in &selection.expandable_folders {
        if !folder_refs.contains(&folder.folder_ref) {
            folder_refs.push(folder.folder_ref.clone());
        }
    }

    // For each selected expandable folder, deterministically replace its children.
    for folder_ref in folder_refs {
        let Some(folder_name) = folder_ref.strip_prefix("folder:") else {
            continue;
        };
        let Some(page_entities) = folders_with_page_entities.get(folder_name) else {
            continue;
        };

        let children =
            select_folder_children(db, folder_name, page_entities, CATALYZED_FOLDER_CHILD_LIMIT);
        let profile = folder_profiles.get(&folder_ref).cloned().flatten();

        if let Some(existing) = selection
            .expandable_folders
            .iter_mut()
            .find(|f| f.folder_ref == folder_ref)
        {
            existing.children = children;
            if existing.profile.is_none() {
                existing.profile = profile;
            }
        } else {
            selection.expandable_folders.push(ExpandableFolder {
                folder_ref,
                children,
                profile,
            });
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
    std::fs::create_dir_all(&enzyme_dir).context("failed to create .enzyme directory")?;

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

        yaml.push_str(&format!(
            "  - \"{}\":\n      expandable: true{}\n",
            yaml_escape(&folder.folder_ref),
            profile_str,
        ));
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
        let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
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
    use crate::db::Database;
    use crate::llm::{ExcludedEntity, ExpandableFolder, SelectedEntity, SelectionResponse};
    use crate::models::{Entity, EntityTrends, EntityType};
    use crate::pipeline::entity_map::EntityMap;
    use libsql::Value;
    use std::collections::HashMap;

    fn make_entity(name: &str, entity_type: EntityType, frequency: i64) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type,
            frequency,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: frequency,
                recency_score: frequency as f64,
                timeline_buckets: HashMap::new(),
            },
            metadata: serde_json::Value::Null,
        }
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
             VALUES (?1, ?2, '', 'hash', ?3, 'test', ?3, 'test', ?3)",
            vec![
                Value::Text(source_ref.to_string()),
                Value::Text(source_ref.to_string()),
                Value::Integer(now),
            ],
        )
        .unwrap();
    }

    fn insert_entity(db: &Database, name: &str, entity_type: EntityType, frequency: i64) {
        let now = chrono::Utc::now().timestamp_millis();
        let trends = serde_json::to_string(&EntityTrends {
            frequency_recent: frequency,
            recency_score: frequency as f64,
            timeline_buckets: HashMap::new(),
        })
        .unwrap();
        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends, metadata)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, '{}')",
            vec![
                Value::Text(name.to_string()),
                Value::Text(entity_type.as_str().to_string()),
                Value::Integer(frequency),
                Value::Integer(now),
                Value::Text(trends),
            ],
        )
        .unwrap();
    }

    fn insert_link_reference(db: &Database, source_ref: &str, link: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
             VALUES (?1, ?2, '', 'hash', ?3, 'test', ?3, 'test', ?3)",
            vec![
                Value::Text(source_ref.to_string()),
                Value::Text(source_ref.to_string()),
                Value::Integer(now),
            ],
        )
        .unwrap();

        let doc_id: i64 = db
            .query_read(
                "SELECT id FROM docs WHERE source_ref = ?1",
                vec![Value::Text(source_ref.to_string())],
                |row| row.get::<i64>(0),
            )
            .unwrap()[0];

        db.execute_write(
            "INSERT INTO doc_links (doc_id, link) VALUES (?1, ?2)",
            vec![Value::Integer(doc_id), Value::Text(link.to_string())],
        )
        .unwrap();
    }

    fn make_entity_map() -> (Database, EntityMap) {
        let db = Database::open_in_memory().unwrap();
        insert_doc(&db, "people/Alice.md");
        insert_doc(&db, "people/Bob.md");
        insert_doc(&db, "people/Carol.md");

        insert_entity(&db, "alice", EntityType::Link, 8);
        insert_entity(&db, "bob", EntityType::Link, 6);
        insert_entity(&db, "carol", EntityType::Link, 4);

        insert_link_reference(&db, "notes/ref-a.md", "alice");
        insert_link_reference(&db, "notes/ref-b.md", "alice");
        insert_link_reference(&db, "notes/ref-c.md", "bob");

        let map = EntityMap::from_entities(
            &db,
            &[
                make_entity("people", EntityType::Folder, 12),
                make_entity("alice", EntityType::Link, 8),
                make_entity("bob", EntityType::Link, 6),
                make_entity("carol", EntityType::Link, 4),
            ],
        )
        .unwrap();

        (db, map)
    }

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
        assert!(!contents.contains("children:"));
        assert!(!contents.contains("[[ari]]"));
        assert!(!contents.contains("[[jonah]]"));

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
                ExcludedEntity {
                    entity_ref: "#todo".to_string(),
                },
                ExcludedEntity {
                    entity_ref: "#note".to_string(),
                },
                ExcludedEntity {
                    entity_ref: "[[template]]".to_string(),
                },
                ExcludedEntity {
                    entity_ref: "folder:templates".to_string(),
                },
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

    #[test]
    fn test_post_process_expandable_folders_promotes_and_deduplicates_children() {
        let mut selection = SelectionResponse {
            catalyzed_entities: vec![
                SelectedEntity {
                    entity_ref: "folder:people".to_string(),
                    profile: Some("relational".to_string()),
                },
                SelectedEntity {
                    entity_ref: "[[alice]]".to_string(),
                    profile: None,
                },
            ],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![],
            excluded_entities: vec![],
        };

        let (db, entity_map) = make_entity_map();
        post_process_expandable_folders(&db, &mut selection, &entity_map);

        assert_eq!(selection.expandable_folders.len(), 1);
        assert_eq!(selection.expandable_folders[0].folder_ref, "folder:people");
        assert_eq!(
            selection.expandable_folders[0].children,
            vec!["[[alice]]", "[[bob]]", "[[carol]]"]
        );
        assert_eq!(
            selection.expandable_folders[0].profile.as_deref(),
            Some("relational")
        );
        assert_eq!(selection.catalyzed_entities.len(), 1);
        assert_eq!(selection.catalyzed_entities[0].entity_ref, "folder:people");
    }

    #[test]
    fn test_post_process_expandable_folders_merges_without_duplicate_children() {
        let mut selection = SelectionResponse {
            catalyzed_entities: vec![SelectedEntity {
                entity_ref: "folder:people".to_string(),
                profile: Some("relational".to_string()),
            }],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![ExpandableFolder {
                folder_ref: "folder:people".to_string(),
                children: vec!["[[carol]]".to_string()],
                profile: Some("relational".to_string()),
            }],
            excluded_entities: vec![],
        };

        let (db, entity_map) = make_entity_map();
        post_process_expandable_folders(&db, &mut selection, &entity_map);

        assert_eq!(selection.expandable_folders.len(), 1);
        assert_eq!(
            selection.expandable_folders[0].children,
            vec!["[[alice]]", "[[bob]]", "[[carol]]"]
        );
    }

    #[test]
    fn test_post_process_expandable_folders_replaces_llm_children_for_expandable_only_folder() {
        let mut selection = SelectionResponse {
            catalyzed_entities: vec![],
            catalyst_profile: None,
            guide_signal: None,
            expandable_folders: vec![ExpandableFolder {
                folder_ref: "folder:people".to_string(),
                children: vec![
                    "[[stale one]]".to_string(),
                    "[[stale two]]".to_string(),
                    "[[stale three]]".to_string(),
                    "[[stale four]]".to_string(),
                ],
                profile: Some("relational".to_string()),
            }],
            excluded_entities: vec![],
        };

        let (db, entity_map) = make_entity_map();
        post_process_expandable_folders(&db, &mut selection, &entity_map);

        assert_eq!(selection.expandable_folders.len(), 1);
        assert_eq!(selection.expandable_folders[0].folder_ref, "folder:people");
        assert_eq!(
            selection.expandable_folders[0].children,
            vec!["[[alice]]", "[[bob]]", "[[carol]]"]
        );
        assert_eq!(
            selection.expandable_folders[0].profile.as_deref(),
            Some("relational")
        );
    }
}
