use std::fmt::Write;

use anyhow::Result;
use chrono::{TimeZone, Utc};

use crate::db::Database;
use crate::models::{Entity, EntityType};
use crate::pipeline::entity_map::EntityMap;

/// Result of context building, including the XML and any detected page entities.
pub struct ContextResult {
    /// The vault context XML string.
    pub xml: String,
    /// Canonical structural facts for the entity set used to build this context.
    pub entity_map: EntityMap,
}

pub struct ContextBuilder<'a> {
    db: &'a Database,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Build vault context XML from DB for the given entities.
    /// Returns the XML string for the system prompt.
    pub fn build(
        &self,
        entities: &[Entity],
        excerpts_per_entity: usize,
        max_excerpt_chars: usize,
    ) -> Result<String> {
        self.build_with_page_entities(entities, excerpts_per_entity, max_excerpt_chars)
            .map(|r| r.xml)
    }

    /// Build vault context XML and detect page entities for expandable folder decisions.
    ///
    /// Page entities are wikilinks whose source files live inside a folder.
    /// For example, if `folder:people` is selected and `people/Alice Smith.md` exists,
    /// then `[[alice smith]]` is a page entity of `people`.
    pub fn build_with_page_entities(
        &self,
        entities: &[Entity],
        excerpts_per_entity: usize,
        max_excerpt_chars: usize,
    ) -> Result<ContextResult> {
        let entity_map = EntityMap::from_entities(self.db, entities)?;
        let folders = self.db.get_folder_structure()?;
        let folders_with_page_entities = entity_map.folder_page_entity_map();

        let mut xml = String::with_capacity(4096);
        xml.push_str("<vault>\n");

        // Folder structure
        xml.push_str("  <structure>\n");
        for folder in &folders {
            let most_recent = folder.most_recent.map(format_timestamp).unwrap_or_default();
            let _ = write!(
                xml,
                "    <folder name=\"{}\" file_count=\"{}\" most_recent=\"{}\">\n",
                EscapeXml(&folder.name),
                folder.file_count,
                most_recent,
            );
            for sub in &folder.subfolders {
                let _ = write!(
                    xml,
                    "      <subfolder name=\"{}\" file_count=\"{}\"/>\n",
                    EscapeXml(&sub.name),
                    sub.file_count,
                );
            }
            xml.push_str("    </folder>\n");
        }
        xml.push_str("  </structure>\n");

        // Trending entities
        xml.push_str("  <trending_entities>\n");
        for entity in entities {
            let entity_id = entity.id.unwrap_or(0);
            let entity_name = &entity.name;
            let entity_type = entity.entity_type.as_str();

            let _ = write!(xml, "    <entity type=\"{}\">\n", EscapeXml(entity_type));
            let _ = write!(xml, "      <name>{}</name>\n", EscapeXml(entity_name));

            // Stats
            let most_recent = entity.last_seen.map(format_timestamp);
            xml.push_str("      <stats>");
            let _ = write!(xml, "<file_count>{}</file_count>", entity.frequency);
            let _ = write!(
                xml,
                "<recent_12mo>{}</recent_12mo>",
                entity.trends.frequency_recent
            );
            if let Some(ref date) = most_recent {
                let _ = write!(xml, "<most_recent>{}</most_recent>", date);
            }
            xml.push_str("</stats>\n");

            // Children
            let children = self.db.get_child_entities(entity_name, entity_type, 5)?;
            if !children.is_empty() {
                xml.push_str("      <children>");
                for child in &children {
                    let _ = write!(
                        xml,
                        "<child name=\"{}\" file_count=\"{}\"/>",
                        EscapeXml(&child.name),
                        child.frequency,
                    );
                }
                xml.push_str("</children>\n");
            }

            // Page entities for folder entities
            if entity.entity_type == EntityType::Folder {
                let page_entities = folders_with_page_entities
                    .get(entity_name)
                    .cloned()
                    .unwrap_or_default();
                if !page_entities.is_empty() {
                    xml.push_str("      <page_entities hint=\"These wikilinks have source files in this folder. If selected as expandable, DO NOT also select these wikilinks separately.\">\n");
                    for wikilink in page_entities.iter().take(10) {
                        let _ = write!(
                            xml,
                            "        <wikilink>[[{}]]</wikilink>\n",
                            EscapeXml(wikilink),
                        );
                    }
                    if page_entities.len() > 10 {
                        let _ = write!(
                            xml,
                            "        <!-- and {} more -->\n",
                            page_entities.len() - 10,
                        );
                    }
                    xml.push_str("      </page_entities>\n");
                }
            }

            // Recent excerpts
            let occurrences = self
                .db
                .get_recent_occurrences(entity_id, excerpts_per_entity)?;
            let excerpts: Vec<_> = occurrences
                .iter()
                .filter_map(|occ| {
                    let context = occ.context.as_ref()?;
                    let snippet = truncate_chars(context, max_excerpt_chars);
                    let date = occ.effective_date.map(format_timestamp).unwrap_or_default();
                    Some((occ.file_path.as_str(), date, snippet))
                })
                .collect();

            if !excerpts.is_empty() {
                xml.push_str("      <recent_content>\n");
                for (file, date, snippet) in &excerpts {
                    let _ = write!(
                        xml,
                        "        <excerpt file=\"{}\" date=\"{}\">{}</excerpt>\n",
                        EscapeXml(file),
                        date,
                        EscapeXml(snippet),
                    );
                }
                xml.push_str("      </recent_content>\n");
            }

            xml.push_str("    </entity>\n");
        }
        xml.push_str("  </trending_entities>\n");
        xml.push_str("</vault>");

        Ok(ContextResult { xml, entity_map })
    }
}

fn format_timestamp(ts: i64) -> String {
    match Utc.timestamp_millis_opt(ts) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

/// Truncate to at most `max` characters (not bytes). UTF-8 safe.
fn truncate_chars(text: &str, max: usize) -> String {
    let mut end = 0;
    for (i, (byte_pos, _)) in text.char_indices().enumerate() {
        if i >= max {
            return format!("{}...", &text[..end]);
        }
        end = byte_pos;
    }
    // Reached end of string without hitting max
    text.to_string()
}

/// Display wrapper for XML-escaping strings via `write!`.
struct EscapeXml<'a>(&'a str);

impl std::fmt::Display for EscapeXml<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for c in self.0.chars() {
            match c {
                '&' => f.write_str("&amp;")?,
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
                '"' => f.write_str("&quot;")?,
                '\'' => f.write_str("&apos;")?,
                _ => f.write_char(c)?,
            }
        }
        Ok(())
    }
}
