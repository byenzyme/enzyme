use std::collections::HashMap;

use anyhow::Result;
use chrono::{TimeZone, Utc};

use crate::db::{Database, Repository};
use crate::db::repository::FolderStats;
use crate::models::Entity;

/// Full vault context built from DB, with per-entity fragments.
pub struct VaultContext {
    /// Complete XML string for the system prompt.
    pub xml: String,
    /// Per-entity context fragments keyed by entity name.
    pub entity_contexts: HashMap<String, EntityContext>,
}

/// Context gathered for a single entity.
pub struct EntityContext {
    pub entity_name: String,
    pub entity_type: String,
    pub file_count: i64,
    pub recent_12mo: i64,
    pub most_recent_date: Option<String>,
    pub children: Vec<(String, i64)>,
    pub excerpts: Vec<(String, String, String)>, // (file, date, snippet)
}

pub struct ContextBuilder<'a> {
    repo: Repository<'a>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self {
            repo: Repository::new(db),
        }
    }

    /// Build vault context XML from DB for the given entities.
    pub fn build(
        &self,
        entities: &[Entity],
        excerpts_per_entity: usize,
        max_excerpt_chars: usize,
    ) -> Result<VaultContext> {
        let folders = self.repo.get_folder_structure()?;
        let mut entity_contexts = HashMap::new();

        for entity in entities {
            let entity_id = entity.id.unwrap_or(0);
            let entity_name = &entity.name;
            let entity_type = entity.entity_type.as_str();

            // Stats from Entity fields
            let file_count = entity.frequency;
            let recent_12mo = entity.trends.frequency_recent;
            let most_recent_date = entity.last_seen.map(|ts| format_timestamp(ts));

            // Children
            let children_entities = self.repo.get_child_entities(
                entity_name,
                entity_type,
                5,
            )?;
            let children: Vec<(String, i64)> = children_entities
                .iter()
                .map(|e| (e.name.clone(), e.frequency))
                .collect();

            // Recent excerpts
            let occurrences = self.repo.get_recent_occurrences(entity_id, excerpts_per_entity)?;
            let excerpts: Vec<(String, String, String)> = occurrences
                .iter()
                .filter_map(|occ| {
                    let context = occ.context.as_ref()?;
                    let snippet = truncate_excerpt(context, max_excerpt_chars);
                    let date = occ.effective_date.map(format_timestamp).unwrap_or_default();
                    Some((occ.file_path.clone(), date, snippet))
                })
                .collect();

            entity_contexts.insert(
                entity_name.clone(),
                EntityContext {
                    entity_name: entity_name.clone(),
                    entity_type: entity_type.to_string(),
                    file_count,
                    recent_12mo,
                    most_recent_date,
                    children,
                    excerpts,
                },
            );
        }

        let xml = build_xml(&folders, &entity_contexts);

        Ok(VaultContext {
            xml,
            entity_contexts,
        })
    }
}

fn build_xml(
    folders: &[FolderStats],
    entity_contexts: &HashMap<String, EntityContext>,
) -> String {
    let mut xml = String::with_capacity(4096);
    xml.push_str("<vault>\n");

    // Folder structure
    xml.push_str("  <structure>\n");
    for folder in folders {
        let most_recent = folder
            .most_recent
            .map(format_timestamp)
            .unwrap_or_default();
        xml.push_str(&format!(
            "    <folder name=\"{}\" file_count=\"{}\" most_recent=\"{}\">\n",
            escape_xml(&folder.name),
            folder.file_count,
            most_recent,
        ));
        for sub in &folder.subfolders {
            xml.push_str(&format!(
                "      <subfolder name=\"{}\" file_count=\"{}\"/>\n",
                escape_xml(&sub.name),
                sub.file_count,
            ));
        }
        xml.push_str("    </folder>\n");
    }
    xml.push_str("  </structure>\n");

    // Trending entities
    xml.push_str("  <trending_entities>\n");
    for ctx in entity_contexts.values() {
        xml.push_str(&format!(
            "    <entity type=\"{}\">\n",
            escape_xml(&ctx.entity_type),
        ));
        xml.push_str(&format!(
            "      <name>{}</name>\n",
            escape_xml(&ctx.entity_name),
        ));

        // Stats
        xml.push_str("      <stats>");
        xml.push_str(&format!("<file_count>{}</file_count>", ctx.file_count));
        xml.push_str(&format!("<recent_12mo>{}</recent_12mo>", ctx.recent_12mo));
        if let Some(ref date) = ctx.most_recent_date {
            xml.push_str(&format!("<most_recent>{}</most_recent>", date));
        }
        xml.push_str("</stats>\n");

        // Children
        if !ctx.children.is_empty() {
            xml.push_str("      <children>");
            for (name, freq) in &ctx.children {
                xml.push_str(&format!(
                    "<child name=\"{}\" file_count=\"{}\"/>",
                    escape_xml(name),
                    freq,
                ));
            }
            xml.push_str("</children>\n");
        }

        // Recent content
        if !ctx.excerpts.is_empty() {
            xml.push_str("      <recent_content>\n");
            for (file, date, snippet) in &ctx.excerpts {
                xml.push_str(&format!(
                    "        <excerpt file=\"{}\" date=\"{}\">{}</excerpt>\n",
                    escape_xml(file),
                    date,
                    escape_xml(snippet),
                ));
            }
            xml.push_str("      </recent_content>\n");
        }

        xml.push_str("    </entity>\n");
    }
    xml.push_str("  </trending_entities>\n");
    xml.push_str("</vault>");

    xml
}

fn format_timestamp(ts: i64) -> String {
    match Utc.timestamp_millis_opt(ts) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        format!("{}...", &text[..max_chars])
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
