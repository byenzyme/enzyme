use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct CatalystEntry {
    catalyst: String,
    topic: String,
}

pub fn build_system_prompt() -> String {
    r#"You are analyzing a personal knowledge vault. Given an entity (tag, link, or folder) and excerpts from documents where it appears, generate thematic catalysts — short, evocative questions or framings that capture the intellectual threads running through this entity's usage.

Each catalyst should:
- Be a question or thematic framing (not a statement of fact)
- Capture a pattern or tension visible across multiple excerpts
- Be specific enough to distinguish this entity from others
- Be useful for approximate search (someone searching for this theme would find relevant documents)

Return a JSON array of objects, each with:
- "catalyst": the catalyst text (a question or thematic framing)
- "topic": a short topic label (2-4 words)

Return ONLY the JSON array, no other text."#
        .to_string()
}

pub fn build_user_prompt(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
) -> String {
    let mut prompt = format!("Entity: {}\n\n", entity_name);
    prompt.push_str("Context excerpts from documents where this entity appears:\n\n");

    for sample in context_samples {
        prompt.push_str("---\n");
        prompt.push_str(sample);
        prompt.push('\n');
    }
    prompt.push_str("---\n\n");

    prompt.push_str(&format!("Generate {} catalysts for this entity.\n", num_catalysts));

    prompt
}

pub fn parse_catalyst_response(response: &str) -> Result<Vec<(String, String)>> {
    // Strip markdown code fences if present
    let trimmed = response.trim();
    let json_str = if trimmed.starts_with("```") {
        let without_opening = if let Some(after_first_line) = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
        {
            after_first_line
        } else {
            trimmed
        };

        without_opening
            .trim()
            .strip_suffix("```")
            .unwrap_or(without_opening)
            .trim()
    } else {
        trimmed
    };

    let entries: Vec<CatalystEntry> =
        serde_json::from_str(json_str).context("Failed to parse catalyst response as JSON")?;

    Ok(entries
        .into_iter()
        .map(|entry| (entry.catalyst, entry.topic))
        .collect())
}
