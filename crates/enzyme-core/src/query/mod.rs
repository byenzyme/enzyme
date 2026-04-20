//! Query-time utilities: petri context formatting for LLM prompts.

use anyhow::Result;

use crate::config::paths::EnzymePaths;
use crate::db::Database;

/// Format the top entities and their catalysts as a compact context string
/// for LLM decomposition and synthesis prompts.
///
/// Output format:
/// ```text
/// - entity_name: catalyst1; catalyst2; catalyst3
/// ```
pub fn format_petri_context(vault_path: &std::path::Path, top: usize) -> Result<String> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;

    let entities = db.get_top_entities(top, "recency_score")?;
    let mut lines = Vec::new();

    for entity in &entities {
        let catalysts = db.get_catalysts_for_entity(&entity.name)?;
        if catalysts.is_empty() {
            continue;
        }
        let cat_texts: Vec<&str> = catalysts.iter().map(|c| c.text.as_str()).collect();
        lines.push(format!("- {}: {}", entity.name, cat_texts.join("; ")));
    }

    Ok(lines.join("\n"))
}

/// Format petri context with topic names for the steering prompt.
///
/// Matches Python's `_load_steering_petri_context()` format:
/// ```text
/// - entity_name:
///   * [topic: topic_label] catalyst question text
/// ```
pub fn format_steering_petri_context(vault_path: &std::path::Path, top: usize) -> Result<String> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;

    let entities = db.get_top_entities(top, "recency_score")?;
    let mut lines = Vec::new();

    for entity in &entities {
        let catalysts = db.get_catalysts_for_entity(&entity.name)?;
        if catalysts.is_empty() {
            continue;
        }
        lines.push(format!("- {}:", entity.name));
        for cat in &catalysts {
            // cat.context holds the topic_name (from LLM topicNames)
            if cat.context.is_empty() || cat.context == "default" {
                lines.push(format!("  * {}", cat.text));
            } else {
                lines.push(format!("  * [topic: {}] {}", cat.context, cat.text));
            }
        }
    }

    Ok(lines.join("\n"))
}
