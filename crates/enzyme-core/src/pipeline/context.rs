use std::fmt::Write;

use anyhow::Result;
use chrono::{TimeZone, Utc};

use crate::db::{Database, Repository};
use crate::models::Entity;

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
    /// Returns the XML string for the system prompt.
    pub fn build(
        &self,
        entities: &[Entity],
        excerpts_per_entity: usize,
        max_excerpt_chars: usize,
    ) -> Result<String> {
        let folders = self.repo.get_folder_structure()?;

        let mut xml = String::with_capacity(4096);
        xml.push_str("<vault>\n");

        // Folder structure
        xml.push_str("  <structure>\n");
        for folder in &folders {
            let most_recent = folder
                .most_recent
                .map(format_timestamp)
                .unwrap_or_default();
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
            let _ = write!(xml, "<recent_12mo>{}</recent_12mo>", entity.trends.frequency_recent);
            if let Some(ref date) = most_recent {
                let _ = write!(xml, "<most_recent>{}</most_recent>", date);
            }
            xml.push_str("</stats>\n");

            // Children
            let children = self.repo.get_child_entities(entity_name, entity_type, 5)?;
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

            // Recent excerpts
            let occurrences = self.repo.get_recent_occurrences(entity_id, excerpts_per_entity)?;
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

        Ok(xml)
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
