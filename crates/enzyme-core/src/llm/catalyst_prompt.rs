use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::CustomProfileGuidance;
use super::catalyst_profiles::{
    CatalystFormat, CatalystProfile, ENABLED_PROFILE_KEYS,
    full_guidance_lines, get_format, get_profile, DEFAULT_FORMAT_KEY,
};

// ---------------------------------------------------------------------------
// JSON response types
// ---------------------------------------------------------------------------

/// Legacy array-of-objects format.
#[derive(Deserialize)]
struct CatalystEntry {
    catalyst: String,
    topic: String,
}

/// New object format: `{ "catalysts": [...], "topicNames": [...] }`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalystPayload {
    catalysts: Vec<String>,
    #[serde(default)]
    topic_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// System prompt (built once, cached across entities)
// ---------------------------------------------------------------------------

/// Build the system prompt for catalyst generation.
///
/// Includes:
/// - Base catalyst generation instructions (format-aware)
/// - All catalyst profiles (for prompt caching — actual profile selected per-entity)
/// - Enzyme guide content for vault context (if available)
pub fn build_system_prompt(
    format_key: Option<&str>,
    custom_guidance: Option<&CustomProfileGuidance>,
    vault_guide: Option<&str>,
) -> String {
    let fmt = format_key
        .and_then(get_format)
        .unwrap_or_else(|| get_format(DEFAULT_FORMAT_KEY).unwrap());

    // Surface guidance from the format
    let surface_items: String = fmt
        .surface_guidance
        .iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Examples section
    let examples_section = if fmt.examples.is_empty() {
        String::new()
    } else {
        let list: String = fmt
            .examples
            .iter()
            .map(|e| format!("- {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n\nExamples of good {}:\n{}",
            fmt.display_name.to_lowercase(),
            list,
        )
    };

    let base = format!(
        "You receive what a living archive is already wondering about. Your catalysts aren't \
extractions—they're ways of staying with what's knocking at the door.\n\n\
{}\n\n\
Catalysts should surface:\n\
{}{}\n\n\
Always respond with valid JSON that includes the catalysts and topic names.",
        fmt.format_prompt, surface_items, examples_section,
    );

    // All profiles section
    let profiles_section = build_profiles_section(custom_guidance);

    let mut prompt = format!("{base}\n\n{profiles_section}");

    // Optional vault overview
    if let Some(guide) = vault_guide {
        prompt.push_str(&format!(
            "\n\n<vault_overview>\n\
The following is the ENZYME_GUIDE for this vault, providing an overview of all tracked \
entities and the vault's intellectual terrain. Use this to understand how the current \
entity relates to the broader landscape of inquiry.\n\n\
{guide}\n\
</vault_overview>"
        ));
    }

    prompt
}

/// Build the `<catalyst_profiles>` section listing all 6 profiles.
fn build_profiles_section(custom_guidance: Option<&CustomProfileGuidance>) -> String {
    let mut lines: Vec<String> = vec!["<catalyst_profiles>".into(), String::new()];

    // Custom domain guidance (prepended to all profiles)
    if let Some(cg) = custom_guidance {
        let has_vocab = cg
            .domain_vocabulary
            .as_ref()
            .map_or(false, |v| {
                !v.metaphors.is_empty()
                    || !v.named_concepts.is_empty()
                    || !v.structural_patterns.is_empty()
            });

        if !cg.guidance_lines.is_empty() || has_vocab {
            lines.push("## Domain-Specific Theoretical Frame".into());
            lines.push(String::new());

            if let Some(ref frame) = cg.theoretical_frame {
                lines.push(frame.clone());
                lines.push(String::new());
            }

            if let Some(ref vocab) = cg.domain_vocabulary {
                let any = !vocab.metaphors.is_empty()
                    || !vocab.named_concepts.is_empty()
                    || !vocab.structural_patterns.is_empty();
                if any {
                    lines.push("**Domain Vocabulary (probe around these terms):**".into());
                    if !vocab.metaphors.is_empty() {
                        lines.push(format!(
                            "- Metaphors/imagery: {}",
                            vocab.metaphors.join(", ")
                        ));
                    }
                    if !vocab.named_concepts.is_empty() {
                        lines.push(format!(
                            "- Named concepts: {}",
                            vocab.named_concepts.join(", ")
                        ));
                    }
                    if !vocab.structural_patterns.is_empty() {
                        lines.push(format!(
                            "- Structural patterns: {}",
                            vocab.structural_patterns.join(", ")
                        ));
                    }
                    lines.push(String::new());
                }
            }

            if !cg.guidance_lines.is_empty() {
                lines.push("**Apply this lens across all profiles:**".into());
                for g in &cg.guidance_lines {
                    lines.push(format!("- {g}"));
                }
                lines.push(String::new());
            }
        }
    }

    // Each profile
    for key in ENABLED_PROFILE_KEYS {
        let Some(profile) = get_profile(key) else {
            continue;
        };

        lines.push(format!("## {} (`{}`)", profile.display_name, profile.key));
        lines.push(profile.description.to_string());
        lines.push(String::new());
        lines.push("**Guidance:**".into());

        for g in full_guidance_lines(profile) {
            lines.push(format!("- {g}"));
        }

        // Sample prompts (up to 3)
        if !profile.sample_prompts.is_empty() {
            lines.push(String::new());
            lines.push("**Sample prompts:**".into());
            for sample in profile.sample_prompts.iter().take(3) {
                lines.push(format!("- {sample}"));
            }
        }

        if let Some(footer) = profile.footer {
            lines.push(String::new());
            lines.push(format!("*{footer}*"));
        }

        lines.push(String::new());
    }

    lines.push("</catalyst_profiles>".into());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// User prompt (varies per entity)
// ---------------------------------------------------------------------------

/// Build the user prompt for a specific entity.
///
/// This references the profile selected in the system prompt and provides the
/// entity-specific context and instructions.
pub fn build_user_prompt(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
    profile: &CatalystProfile,
    format_key: &str,
    guide_signal: Option<&str>,
) -> String {
    let fmt = get_format(format_key)
        .unwrap_or_else(|| get_format(DEFAULT_FORMAT_KEY).unwrap());

    // Profile selection header
    let mut prompt = format!(
        "SELECTED PROFILE: {} (`{}`)\n\n\
Apply the guidance and approach from the {} profile defined in the system prompt.",
        profile.display_name, profile.key, profile.display_name,
    );

    // Optional guide signal
    if let Some(signal) = guide_signal {
        prompt.push_str(&format!(
            "\n\nUSER CONTEXT (from vault guide):\n\
{signal}\n\n\
Use this context to calibrate your catalyst questions. The user has expressed intellectual \
preoccupations or tensions — probe around or underneath these rather than asking about them \
directly. Let this inform the angle of your questions without constraining them."
        ));
    }

    // Context section
    let max_samples = context_samples.len().min(20);
    let context_section: String = context_samples[..max_samples].join("\n\n");

    prompt.push_str(&format!(
        "\n\n<vault_context>\n{context_section}\n</vault_context>"
    ));

    // Entity focus
    prompt.push_str(&format!(
        "\n\n<entity>\nFocus entity: {entity_name}\n</entity>"
    ));

    // Format-specific instructions
    let (periphery_instruction, format_instruction, placeholder_label) =
        format_instructions(fmt);

    // Catalyst / topic placeholders
    let catalyst_placeholders: String = (0..num_catalysts)
        .map(|i| format!("    \"[{} {}]\"", placeholder_label, i + 1))
        .collect::<Vec<_>>()
        .join(",\n");
    let topic_placeholders: String = (0..num_catalysts)
        .map(|i| format!("    \"[specific concept relationship {}]\"", i + 1))
        .collect::<Vec<_>>()
        .join(",\n");

    prompt.push_str(&format!(
        "\n\n\
Attend to the excerpts and:\n\
1. For EACH catalyst: First ground in specific excerpts — what does {entity_name} mean in THIS vault based on what you're reading? Quote a phrase. Do NOT default to generic definitions.\n\
2. {periphery_instruction}\n\
3. {format_instruction}\n\
4. Create precise topic names (3-5 words, lowercase, space-separated) capturing each inquiry. Examples: \"craft negotiation perfection\", \"enzyme validation authenticity\", \"tools reveal conceal needs\", \"physical knowledge embodiment\", \"ai human voice missions\"\n\n\
Return a JSON object with:\n\
{{\n\
  \"catalysts\": [\n\
{catalyst_placeholders}\n\
  ],\n\
  \"topicNames\": [\n\
{topic_placeholders}\n\
  ]\n\
}}"
    ));

    prompt
}

/// Return (periphery_instruction, format_instruction, placeholder_label) for a format.
fn format_instructions(fmt: &CatalystFormat) -> (&'static str, &'static str, &'static str) {
    match fmt.key {
        "thesis" => (
            "Then articulate what's at stake — name the position being taken, where value accrues, what bet is being made (explicit or implicit).",
            "Each catalyst must be a single sentence CLAIM or ASSERTION that reads like a conviction statement. These should name positions and stakes, not ask questions. Keep every catalyst to one sentence.",
            "Investment thesis statement",
        ),
        "claim" => (
            "Then name what's actually being said — the position being staked, the commitment embedded in the writing, what the material assumes to be true.",
            "Each catalyst must be a single sentence STATEMENT that stakes a position. These should be assertive claims, not questions or hedged observations. Keep every catalyst to one sentence.",
            "Assertive claim statement",
        ),
        // "question" and any fallback
        _ => (
            "Then probe the PERIPHERY of that grounded meaning — don't ask about the concept directly. Ask about what surrounds it, what it's in friction with, what tensions touch it, what it enables or forecloses.",
            "Each catalyst must be a single impersonal retrieval question that invites philosophical wonder and can surface resonant passages anywhere in the archive. Keep every catalyst to one sentence and avoid second-person framing.",
            "Ideological retrieval question",
        ),
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the LLM response into `(catalyst_text, topic_name)` pairs.
///
/// Supports two formats:
/// 1. **Object format** (preferred): `{ "catalysts": [...], "topicNames": [...] }`
/// 2. **Legacy array format**: `[{ "catalyst": "...", "topic": "..." }, ...]`
pub fn parse_catalyst_response(response: &str) -> Result<Vec<(String, String)>> {
    let json_str = strip_code_fences(response);

    // Try the new object format first
    if let Ok(payload) = serde_json::from_str::<CatalystPayload>(json_str) {
        let pairs: Vec<(String, String)> = payload
            .catalysts
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let topic = payload
                    .topic_names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| generate_fallback_topic(&text));
                (text, topic)
            })
            .collect();
        return Ok(pairs);
    }

    // Fall back to legacy array format
    let entries: Vec<CatalystEntry> =
        serde_json::from_str(json_str).context("Failed to parse catalyst response as JSON")?;

    Ok(entries
        .into_iter()
        .map(|entry| (entry.catalyst, entry.topic))
        .collect())
}

/// Build a strict `json_schema` response format for catalyst generation.
pub fn catalyst_response_format() -> super::client::ResponseFormat {
    use super::client::{JsonSchemaSpec, ResponseFormat};

    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaSpec {
            name: "catalysts_payload".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "catalysts": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "topicNames": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["catalysts", "topicNames"]
            }),
            strict: true,
        },
    }
}

/// Strip markdown code fences from a response.
fn strip_code_fences(response: &str) -> &str {
    let trimmed = response.trim();
    if trimmed.starts_with("```") {
        let without_opening = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);

        without_opening
            .trim()
            .strip_suffix("```")
            .unwrap_or(without_opening)
            .trim()
    } else {
        trimmed
    }
}

/// Generate a simple fallback topic from catalyst text.
fn generate_fallback_topic(text: &str) -> String {
    let cleaned = text
        .to_lowercase()
        .replace('?', "")
        .replace('.', "");
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| !matches!(
            *w,
            "how" | "what" | "why" | "when" | "where" | "who" | "which"
                | "does" | "do" | "is" | "are" | "can" | "could" | "would"
                | "should" | "the" | "a" | "an" | "in" | "of" | "to" | "and"
                | "or" | "for" | "with" | "that" | "this"
        ))
        .take(5)
        .collect();
    words.join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::catalyst_profiles::{PROFILE_DIALECTICAL, PROFILE_OPERATIONAL};
    use crate::config::{DomainVocabulary};

    #[test]
    fn test_parse_object_format() {
        let json = r#"{"catalysts":["Where does X?","What about Y?"],"topicNames":["topic a","topic b"]}"#;
        let pairs = parse_catalyst_response(json).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "Where does X?");
        assert_eq!(pairs[0].1, "topic a");
        assert_eq!(pairs[1].0, "What about Y?");
        assert_eq!(pairs[1].1, "topic b");
    }

    #[test]
    fn test_parse_object_format_missing_topics() {
        let json = r#"{"catalysts":["Where does X?","What about Y?"]}"#;
        let pairs = parse_catalyst_response(json).unwrap();
        assert_eq!(pairs.len(), 2);
        // Should generate fallback topics
        assert!(!pairs[0].1.is_empty());
    }

    #[test]
    fn test_parse_legacy_array_format() {
        let json = r#"[{"catalyst":"Where does X?","topic":"topic a"}]"#;
        let pairs = parse_catalyst_response(json).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "Where does X?");
        assert_eq!(pairs[0].1, "topic a");
    }

    #[test]
    fn test_parse_with_code_fences() {
        let response = "```json\n[{\"catalyst\":\"A?\",\"topic\":\"t\"}]\n```";
        let pairs = parse_catalyst_response(response).unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn test_build_system_prompt_default() {
        let prompt = build_system_prompt(None, None, None);
        assert!(prompt.contains("wondering-alongside"));
        assert!(prompt.contains("<catalyst_profiles>"));
        assert!(prompt.contains("Dialectical"));
        assert!(prompt.contains("Reflective"));
        assert!(prompt.contains("</catalyst_profiles>"));
    }

    #[test]
    fn test_build_system_prompt_thesis_format() {
        let prompt = build_system_prompt(Some("thesis"), None, None);
        assert!(prompt.contains("investment theses"));
        assert!(prompt.contains("Where value is accruing"));
    }

    #[test]
    fn test_build_system_prompt_with_vault_guide() {
        let prompt = build_system_prompt(None, None, Some("My vault guide content"));
        assert!(prompt.contains("<vault_overview>"));
        assert!(prompt.contains("My vault guide content"));
        assert!(prompt.contains("</vault_overview>"));
    }

    #[test]
    fn test_build_system_prompt_with_custom_guidance() {
        let cg = CustomProfileGuidance {
            base_profile: None,
            guidance_lines: vec!["Custom lens one".into()],
            domain_vocabulary: Some(DomainVocabulary {
                metaphors: vec!["gardening".into()],
                named_concepts: vec!["emergence".into()],
                structural_patterns: vec![],
            }),
            theoretical_frame: Some("A custom frame".into()),
        };
        let prompt = build_system_prompt(None, Some(&cg), None);
        assert!(prompt.contains("Domain-Specific Theoretical Frame"));
        assert!(prompt.contains("A custom frame"));
        assert!(prompt.contains("gardening"));
        assert!(prompt.contains("emergence"));
        assert!(prompt.contains("Custom lens one"));
    }

    #[test]
    fn test_build_user_prompt_question_format() {
        let samples = vec!["excerpt one".into(), "excerpt two".into()];
        let prompt = build_user_prompt(
            "enzyme",
            &samples,
            3,
            &PROFILE_DIALECTICAL,
            "question",
            None,
        );
        assert!(prompt.contains("SELECTED PROFILE: Dialectical"));
        assert!(prompt.contains("<vault_context>"));
        assert!(prompt.contains("Focus entity: enzyme"));
        assert!(prompt.contains("probe the PERIPHERY"));
        assert!(prompt.contains("\"catalysts\""));
    }

    #[test]
    fn test_build_user_prompt_thesis_format() {
        let samples = vec!["excerpt".into()];
        let prompt = build_user_prompt(
            "projects",
            &samples,
            2,
            &PROFILE_OPERATIONAL,
            "thesis",
            None,
        );
        assert!(prompt.contains("SELECTED PROFILE: Operational"));
        assert!(prompt.contains("articulate what's at stake"));
        assert!(prompt.contains("Investment thesis statement"));
    }

    #[test]
    fn test_build_user_prompt_with_guide_signal() {
        let samples = vec!["excerpt".into()];
        let prompt = build_user_prompt(
            "test",
            &samples,
            2,
            &PROFILE_DIALECTICAL,
            "question",
            Some("User is interested in X"),
        );
        assert!(prompt.contains("USER CONTEXT (from vault guide):"));
        assert!(prompt.contains("User is interested in X"));
    }
}
