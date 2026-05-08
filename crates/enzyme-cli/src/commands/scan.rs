use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::document::{DocumentProcessor, FileDiscovery, ProcessedDocument};
use enzyme_core::models::ROOT_FOLDER_ENTITY;

/// Filesystem-only vault scan used before init.
///
/// This command intentionally does not read the enzyme DB. Its job is to
/// propose the config that init should later consume.
pub fn run_scan(vault_path: &Path, write_config: bool) -> Result<()> {
    let docs = process_docs(vault_path)?;
    let mut suggestion = build_scan_suggestion(vault_path, &docs)?;

    if write_config {
        let mut config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
        if config.has_curated_entities() {
            suggestion.status = ScanStatus::AlreadyConfigured;
            suggestion.message =
                Some("config already has entities for this vault; not overwriting".to_string());
        } else {
            config.excluded_folders = suggestion.excluded_folders.clone();
            config.set_auto_selected_entities(vault_path, suggestion.entities.clone())?;
            suggestion.status = ScanStatus::Written;
            suggestion.config_path =
                Some(EnzymeConfig::config_path()?.to_string_lossy().to_string());
        }
    }

    println!("{}", serde_json::to_string(&suggestion)?);
    Ok(())
}

#[derive(Debug, Serialize)]
struct ScanSuggestion {
    files: usize,
    entities: Vec<String>,
    excluded_folders: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_path: Option<String>,
    status: ScanStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScanStatus {
    Suggested,
    Written,
    AlreadyConfigured,
}

fn process_docs(vault_path: &Path) -> Result<Vec<ProcessedDocument>> {
    let discovery = FileDiscovery::new(vault_path.to_path_buf());
    let paths = discovery.discover()?;
    let processor = DocumentProcessor::new(vault_path.to_path_buf());
    Ok(processor
        .process_all(&paths)
        .into_iter()
        .flatten()
        .collect())
}

fn build_scan_suggestion(vault_path: &Path, docs: &[ProcessedDocument]) -> Result<ScanSuggestion> {
    let mut folder_counts: HashMap<String, usize> = HashMap::new();
    let mut tag_counts: HashMap<String, usize> = HashMap::new();
    let mut link_counts: HashMap<String, usize> = HashMap::new();

    for doc in docs {
        for folder in &doc.folders {
            *folder_counts.entry(folder.clone()).or_default() += 1;
        }
        for tag in &doc.tags {
            *tag_counts.entry(tag.clone()).or_default() += 1;
        }
        for link in &doc.links {
            *link_counts.entry(link.clone()).or_default() += 1;
        }
    }

    let entities = suggested_entities(docs, &folder_counts, &tag_counts, &link_counts);

    let existing_configured = EnzymeConfig::load(vault_path)?
        .map(|config| config.has_curated_entities())
        .unwrap_or(false);

    Ok(ScanSuggestion {
        files: docs.len(),
        entities,
        excluded_folders: default_excluded_folders(),
        config_path: None,
        status: ScanStatus::Suggested,
        message: existing_configured.then(|| {
            "config already has entities for this vault; scan is a preview only".to_string()
        }),
    })
}

fn suggested_folders(folder_counts: &HashMap<String, usize>) -> Vec<String> {
    sorted_counts(folder_counts)
        .into_iter()
        .filter(|(folder, _)| !is_structural_folder(folder) && should_include_folder(folder))
        .map(|(folder, _)| folder)
        .collect()
}

fn suggested_entities(
    docs: &[ProcessedDocument],
    folder_counts: &HashMap<String, usize>,
    tag_counts: &HashMap<String, usize>,
    link_counts: &HashMap<String, usize>,
) -> Vec<String> {
    let mut entities = Vec::new();
    let mut seen = HashSet::new();

    if is_flat_vault(docs) {
        push_entity(&mut entities, &mut seen, "folder:.".to_string());
    } else {
        for folder in suggested_folders(folder_counts) {
            push_entity(&mut entities, &mut seen, format!("folder:{folder}"));
        }
    }
    for (tag, _) in sorted_counts(tag_counts)
        .into_iter()
        .filter(|(tag, _)| !is_structural_tag(tag))
        .take(20)
    {
        push_entity(&mut entities, &mut seen, format!("#{tag}"));
    }
    for (link, _) in sorted_counts(link_counts)
        .into_iter()
        .filter(|(link, count)| *count >= 3 && !link.contains('/'))
        .take(8)
    {
        push_entity(&mut entities, &mut seen, format!("[[{link}]]"));
    }

    entities
}

fn is_flat_vault(docs: &[ProcessedDocument]) -> bool {
    !docs.is_empty()
        && docs.iter().all(|doc| {
            doc.folders
                .iter()
                .all(|folder| folder == ROOT_FOLDER_ENTITY)
        })
}

fn should_include_folder(folder: &str) -> bool {
    let depth = folder.split('/').count();
    if depth <= 2 {
        return true;
    }

    let parent = folder.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let parent_name = parent.rsplit('/').next().unwrap_or("");
    if matches!(parent_name, "docs" | "projects" | "clients" | "work") {
        return true;
    }

    false
}

fn sorted_counts(map: &HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut entries: Vec<_> = map
        .iter()
        .map(|(name, count)| (name.clone(), *count))
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

fn push_entity(entities: &mut Vec<String>, seen: &mut HashSet<String>, entity: String) {
    if seen.insert(entity.clone()) {
        entities.push(entity);
    }
}

fn default_excluded_folders() -> Vec<String> {
    vec![
        ".claude".to_string(),
        ".conversations".to_string(),
        ".enzyme".to_string(),
        ".git".to_string(),
        ".obsidian".to_string(),
        ".trash".to_string(),
        "node_modules".to_string(),
        "templates".to_string(),
    ]
}

fn is_structural_folder(folder: &str) -> bool {
    folder.split('/').any(is_structural_folder_component)
}

fn is_structural_folder_component(name: &str) -> bool {
    matches!(
        name,
        ".claude"
            | ".conversations"
            | ".enzyme"
            | ".git"
            | ".obsidian"
            | ".trash"
            | "node_modules"
            | "templates"
            | "templater"
    )
}

fn is_structural_tag(tag: &str) -> bool {
    matches!(tag, "todo" | "template" | "archived" | "archive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_core::models::DateSource;

    fn doc(folders: Vec<&str>) -> ProcessedDocument {
        ProcessedDocument {
            source_ref: "note.md".to_string(),
            title: None,
            content: String::new(),
            content_hash: "hash".to_string(),
            word_count: 0,
            char_count: 0,
            created_at: 0,
            created_source: DateSource::Filesystem,
            modified_at: 0,
            modified_source: DateSource::Filesystem,
            tags: Vec::new(),
            links: Vec::new(),
            folders: folders.into_iter().map(str::to_string).collect(),
            chunks: Vec::new(),
            frontmatter: None,
        }
    }

    #[test]
    fn structural_folder_excludes_descendants() {
        assert!(is_structural_folder(".claude"));
        assert!(is_structural_folder(".claude/commands"));
        assert!(is_structural_folder("projects/.obsidian/cache"));
        assert!(is_structural_folder("work/templates/client"));
        assert!(!is_structural_folder("people"));
        assert!(!is_structural_folder("projects/client-a"));
    }

    #[test]
    fn suggested_folders_filters_structural_descendants() {
        let mut counts = HashMap::new();
        counts.insert("emails".to_string(), 5);
        counts.insert(".claude/commands".to_string(), 10);
        counts.insert("people".to_string(), 3);

        let folders = suggested_folders(&counts);

        assert_eq!(folders, vec!["emails".to_string(), "people".to_string()]);
    }

    #[test]
    fn flat_vault_suggests_root_folder_entity() {
        let docs = vec![doc(Vec::new()), doc(Vec::new())];

        let entities = suggested_entities(&docs, &HashMap::new(), &HashMap::new(), &HashMap::new());

        assert_eq!(entities.first().map(String::as_str), Some("folder:."));
    }
}
