use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;

use crate::config::CustomProfileGuidance;
use crate::models::Entity;
use crate::pipeline::eras::Era;
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

/// Object format: `{ "catalysts": [...], "topicNames": [...], "eraLabels": [...] }`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalystPayload {
    catalysts: Vec<String>,
    #[serde(default)]
    topic_names: Vec<String>,
    #[serde(default)]
    era_labels: Vec<String>,
}

/// JSON Schema for the `CatalystPayload` response type.
///
/// Used with `response_format: json_schema` for strict schema enforcement.
pub fn catalyst_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "catalysts": {
                "type": "array",
                "items": { "type": "string" }
            },
            "topicNames": {
                "type": "array",
                "items": { "type": "string" }
            },
            "eraLabels": {
                "type": "array",
                "items": { "type": "string" }
            }
        },
        "required": ["catalysts", "topicNames"],
        "additionalProperties": false
    })
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
VOCABULARY: Avoid analytical vocabulary that distances the question from lived experience. \
Specifically banned: \"reveal,\" \"suggest about,\" \"underlying,\" \"foreclose,\" \"manifest,\" \
\"illuminate,\" \"inherent,\" \"navigate,\" \"in what ways,\" \"intersection of,\" \"the notion of,\" \
\"the pursuit of,\" \"implicitly,\" \"the architecture of.\" Use words that appeared in the \
excerpts or that a person would use in conversation.\n\n\
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

/// Build a `<practice_shape>` section from entity trends data.
///
/// Surfaces temporal engagement patterns so the LLM can use
/// the shape of practice — not just content — to aim its probes.
fn build_practice_shape(entity: &Entity) -> String {
    let mut parts: Vec<String> = Vec::new();

    let freq_recent = entity.trends.frequency_recent;
    let freq_all = entity.frequency;

    // Engagement trajectory from timeline buckets
    let now = Utc::now();
    let cutoff = (now - chrono::Duration::days(90)).date_naive();

    let freq_3m: i64 = entity
        .trends
        .timeline_buckets
        .iter()
        .filter_map(|(month_key, &count)| {
            // Parse "YYYY-MM" keys
            let mut parts = month_key.splitn(2, '-');
            let year: i32 = parts.next()?.parse().ok()?;
            let month: u32 = parts.next()?.parse().ok()?;
            let bucket_date = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
            if bucket_date >= cutoff {
                Some(count)
            } else {
                None
            }
        })
        .sum();

    if freq_all > 0 {
        if freq_3m > 0 && freq_recent > 0 {
            let ratio = freq_3m as f64 / freq_recent.max(1) as f64;
            if ratio > 0.5 {
                parts.push(format!(
                    "Engagement is accelerating: {} notes in the last 3 months out of {} in the past year ({} all time).",
                    freq_3m, freq_recent, freq_all
                ));
            } else if ratio > 0.15 {
                parts.push(format!(
                    "Steady engagement: {} notes in the last 3 months, {} in the past year ({} all time).",
                    freq_3m, freq_recent, freq_all
                ));
            } else {
                parts.push(format!(
                    "Engagement is cooling: {} notes in the last 3 months vs. {} in the past year ({} all time).",
                    freq_3m, freq_recent, freq_all
                ));
            }
        } else {
            parts.push(format!(
                "This entity appears in {} notes total, {} in the last 3 months.",
                freq_all, freq_3m
            ));
        }
    }

    // Recency from last_seen (millis since epoch)
    if let Some(last_seen_ms) = entity.last_seen {
        if let Some(last_seen_dt) = chrono::DateTime::from_timestamp_millis(last_seen_ms) {
            let days_since = (now - last_seen_dt).num_days();
            if days_since == 0 {
                parts.push("Last active today.".into());
            } else if days_since <= 7 {
                parts.push(format!(
                    "Last active {} day{} ago.",
                    days_since,
                    if days_since != 1 { "s" } else { "" }
                ));
            } else if days_since <= 30 {
                parts.push(format!(
                    "Last active {} days ago ({}).",
                    days_since,
                    last_seen_dt.format("%b %d")
                ));
            } else {
                parts.push(format!(
                    "Dormant — last active {} days ago ({}).",
                    days_since,
                    last_seen_dt.format("%b %d")
                ));
            }
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    format!("<practice_shape>\n{}\n</practice_shape>", parts.join(" "))
}

/// Era-aware context: samples grouped by era, with allocation info.
pub struct EraContext {
    pub eras: Vec<Era>,
    /// Per-era minimum catalyst counts (parallel to `eras`).
    pub era_allocations: Vec<usize>,
    /// (era_index, context_string) pairs. era_index indexes into `eras`;
    /// index == eras.len() means unmatched/undated.
    pub samples: Vec<(usize, String)>,
}

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
    entity: Option<&Entity>,
    era_context: Option<&EraContext>,
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

    // Context section — era-grouped if available, flat otherwise
    if let Some(ec) = era_context {
        if ec.eras.len() > 1 {
            // Group samples by era
            for (era_idx, era) in ec.eras.iter().enumerate() {
                let era_samples: Vec<&str> = ec.samples
                    .iter()
                    .filter(|(idx, _)| *idx == era_idx)
                    .map(|(_, s)| s.as_str())
                    .collect();
                if !era_samples.is_empty() {
                    prompt.push_str(&format!(
                        "\n\n<vault_context era=\"{}\" notes=\"{}\">\n{}\n</vault_context>",
                        era.label, era.count, era_samples.join("\n\n")
                    ));
                }
            }
            // Unmatched samples
            let unmatched: Vec<&str> = ec.samples
                .iter()
                .filter(|(idx, _)| *idx == ec.eras.len())
                .map(|(_, s)| s.as_str())
                .collect();
            if !unmatched.is_empty() {
                prompt.push_str(&format!(
                    "\n\n<vault_context era=\"undated\">\n{}\n</vault_context>",
                    unmatched.join("\n\n")
                ));
            }
        } else {
            // Single or no eras — flat context
            let context_section: String = ec.samples
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            prompt.push_str(&format!(
                "\n\n<vault_context>\n{context_section}\n</vault_context>"
            ));
        }
    } else {
        let max_samples = context_samples.len().min(20);
        let context_section: String = context_samples[..max_samples].join("\n\n");
        prompt.push_str(&format!(
            "\n\n<vault_context>\n{context_section}\n</vault_context>"
        ));
    }

    // Entity focus
    prompt.push_str(&format!(
        "\n\n<entity>\nFocus entity: {entity_name}\n</entity>"
    ));

    // Practice shape from entity trends
    if let Some(e) = entity {
        let practice = build_practice_shape(e);
        if !practice.is_empty() {
            prompt.push_str(&format!("\n\n{practice}"));
        }
    }

    // Era coverage instructions
    if let Some(ec) = era_context {
        if ec.eras.len() > 1 {
            let mut era_lines: Vec<String> = Vec::new();
            let mut accounted = 0usize;
            for (i, era) in ec.eras.iter().enumerate() {
                let alloc = ec.era_allocations.get(i).copied().unwrap_or(1);
                era_lines.push(format!(
                    "- \"{}\" ({} notes): min {} catalyst{}",
                    era.label, era.count, alloc,
                    if alloc != 1 { "s" } else { "" }
                ));
                accounted += alloc;
            }
            let free = num_catalysts.saturating_sub(accounted);
            prompt.push_str(&format!(
                "\n\n<era_coverage>\n\
Each era in the context above represents a distinct period of engagement with this entity. \
Ensure temporal coverage — ground at least the minimum number of catalysts in excerpts from each era:\n\
{}\n\
{}\
The differences between eras often hold the most interesting material — probe what shifted, \
what recurred, or what was left behind between periods.\n\
</era_coverage>",
                era_lines.join("\n"),
                if free > 0 {
                    format!("Remaining {} catalyst{}: allocate freely to wherever the richest material is.\n",
                        free, if free != 1 { "s" } else { "" })
                } else {
                    String::new()
                },
            ));
        }
    }

    // Register detection + engagement shape preamble
    prompt.push_str("\n\n\
Before generating catalysts, read the excerpts for register and engagement shape:\n\
\n\
Register — adapt your probing depth to what the content actually is:\n\
- Conversational content (tool calls, Q&A turns, factual queries): treat questions at face value. \
Surface what was being worked on, what information was gathered, what decisions were reached. \
Do not probe beneath factual questions.\n\
- Reflective content (journal entries, long-form prose, personal processing): apply the profile's \
full probing posture — tensions, emergence, what's unresolved.\n\
- Reference material (highlights, annotations, clipped articles): surface what drew attention and \
how it connects to the user's own thinking elsewhere in the vault.\n\
\n\
Engagement shape — if practice_shape data is provided above, let the pattern of engagement \
inform the angle of your probes:\n\
- Accelerating engagement suggests an active conviction cycle — probe what's driving the return.\n\
- Cooling engagement suggests something unresolved that lost energy — probe what was left open.\n\
- Dormant entities with high all-time frequency suggest completed or abandoned threads — \
probe what would reopen them.\n\
\n\
The register and engagement shape determine HOW you aim the probe. The selected profile \
determines WHAT to probe for. Catalysts must still be retrieval-functional — each one should \
embed well against vault passages and surface relevant content when used as a search query.");

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

    let has_eras = era_context.map_or(false, |ec| ec.eras.len() > 1);
    let era_instruction = if has_eras {
        "\n4. For eraLabels: the era label (from the era attributes on vault_context) that each catalyst is grounded in."
    } else {
        ""
    };
    let era_json = if has_eras {
        let era_placeholders: String = (0..num_catalysts)
            .map(|i| format!("    \"[era label {}]\"", i + 1))
            .collect::<Vec<_>>()
            .join(",\n");
        format!(",\n  \"eraLabels\": [\n{era_placeholders}\n  ]")
    } else {
        String::new()
    };

    prompt.push_str(&format!(
        "\n\n\
Now attend to the excerpts and:\n\
1. Ground each catalyst in a specific phrase from the excerpts — what does {entity_name} actually mean in THIS vault? Then {periphery_instruction}\n\
2. {format_instruction}\n\
3. For topicNames: 3-5 lowercase words capturing each inquiry (e.g. \"craft negotiation perfection\", \"tools reveal conceal needs\").{era_instruction}\n\n\
Return a JSON object with:\n\
{{\n\
  \"catalysts\": [\n\
{catalyst_placeholders}\n\
  ],\n\
  \"topicNames\": [\n\
{topic_placeholders}\n\
  ]{era_json}\n\
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
            "Then probe AROUND the grounded meaning, not about it directly. Ask about what it costs, what it forecloses, what it's in friction with, or what it quietly assumes. Name what's at stake rather than describing the concept.",
            "Each catalyst must be a single sentence question. Keep every catalyst to one sentence and avoid second-person framing. IMPORTANT: Vary your sentence openings — do NOT start multiple catalysts with the same word (e.g., don't start 3 catalysts with \"What\").",
            "Retrieval question",
        ),
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// A single parsed catalyst with text, topic, and optional era label.
pub struct ParsedCatalyst {
    pub text: String,
    pub topic: String,
    pub era_label: Option<String>,
}

/// Parsed catalyst response.
pub struct CatalystResponse {
    pub pairs: Vec<(String, String)>,
    pub catalysts: Vec<ParsedCatalyst>,
}

/// Parse the LLM response into catalyst pairs.
///
/// Supports two formats:
/// 1. **Object format** (preferred): `{ "catalysts": [...], "topicNames": [...], "eraLabels": [...] }`
/// 2. **Legacy array format**: `[{ "catalyst": "...", "topic": "..." }, ...]`
pub fn parse_catalyst_response(response: &str) -> Result<CatalystResponse> {
    let cleaned;
    let json_str = {
        let s = strip_code_fences(response);
        if s.contains(",") {
            cleaned = strip_trailing_commas(s);
            &cleaned
        } else {
            s
        }
    };

    // Try the object format first
    if let Ok(payload) = serde_json::from_str::<CatalystPayload>(json_str) {
        let catalysts: Vec<ParsedCatalyst> = payload
            .catalysts
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let topic = payload
                    .topic_names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| generate_fallback_topic(&text));
                let era_label = payload.era_labels.get(i).cloned();
                ParsedCatalyst { text, topic, era_label }
            })
            .collect();
        let pairs = catalysts.iter().map(|c| (c.text.clone(), c.topic.clone())).collect();
        return Ok(CatalystResponse { pairs, catalysts });
    }

    // Fall back to legacy array format
    let entries: Vec<CatalystEntry> =
        serde_json::from_str(json_str).context("Failed to parse catalyst response as JSON")?;

    let catalysts: Vec<ParsedCatalyst> = entries
        .into_iter()
        .map(|entry| ParsedCatalyst {
            text: entry.catalyst,
            topic: entry.topic,
            era_label: None,
        })
        .collect();
    let pairs = catalysts.iter().map(|c| (c.text.clone(), c.topic.clone())).collect();

    Ok(CatalystResponse { pairs, catalysts })
}

/// Remove trailing commas before `]` or `}` so lenient LLM output parses as valid JSON.
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            // Scan forward past whitespace to see if the next non-whitespace is ] or }
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'}') {
                // Skip the comma
                i += 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip markdown code fences from a response.
pub(crate) fn strip_code_fences(response: &str) -> &str {
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
        let resp = parse_catalyst_response(json).unwrap();
        assert_eq!(resp.pairs.len(), 2);
        assert_eq!(resp.pairs[0].0, "Where does X?");
        assert_eq!(resp.pairs[0].1, "topic a");
        assert_eq!(resp.pairs[1].0, "What about Y?");
        assert_eq!(resp.pairs[1].1, "topic b");
    }

    #[test]
    fn test_parse_object_format_missing_topics() {
        let json = r#"{"catalysts":["Where does X?","What about Y?"]}"#;
        let resp = parse_catalyst_response(json).unwrap();
        assert_eq!(resp.pairs.len(), 2);
        // Should generate fallback topics
        assert!(!resp.pairs[0].1.is_empty());
    }

    #[test]
    fn test_parse_legacy_array_format() {
        let json = r#"[{"catalyst":"Where does X?","topic":"topic a"}]"#;
        let resp = parse_catalyst_response(json).unwrap();
        assert_eq!(resp.pairs.len(), 1);
        assert_eq!(resp.pairs[0].0, "Where does X?");
        assert_eq!(resp.pairs[0].1, "topic a");
    }

    #[test]
    fn test_parse_with_code_fences() {
        let response = "```json\n[{\"catalyst\":\"A?\",\"topic\":\"t\"}]\n```";
        let resp = parse_catalyst_response(response).unwrap();
        assert_eq!(resp.pairs.len(), 1);
    }

    #[test]
    fn test_build_system_prompt_default() {
        let prompt = build_system_prompt(None, None, None);
        assert!(prompt.contains("single sentence questions"));
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
            None,
            None,
        );
        assert!(prompt.contains("SELECTED PROFILE: Dialectical"));
        assert!(prompt.contains("<vault_context>"));
        assert!(prompt.contains("Focus entity: enzyme"));
        assert!(prompt.contains("probe AROUND the grounded meaning"));
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
            None,
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
            None,
            None,
        );
        assert!(prompt.contains("USER CONTEXT (from vault guide):"));
        assert!(prompt.contains("User is interested in X"));
    }
}
