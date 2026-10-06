use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::debug;

use super::catalyst_profiles::{
    CatalystFormat, CatalystProfile, DEFAULT_FORMAT_KEY, ENABLED_PROFILE_KEYS, full_guidance_lines,
    get_format, get_profile,
};
use crate::models::Entity;
use crate::pipeline::eras::Era;

// ---------------------------------------------------------------------------
// JSON response types
// ---------------------------------------------------------------------------

/// Legacy array-of-objects format.
#[derive(Deserialize)]
struct CatalystEntry {
    catalyst: String,
    topic: String,
    #[serde(default, rename = "eraLabel", alias = "era_label")]
    era_label: Option<String>,
}

/// Streaming-friendly object format: `{ "entries": [{ "catalyst": ..., "topic": ... }] }`.
#[derive(Deserialize)]
struct CatalystEntriesPayload {
    entries: Vec<CatalystEntry>,
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
pub fn catalyst_json_schema(include_era_labels: bool) -> serde_json::Value {
    let mut schema = serde_json::json!({
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
        },
        "required": ["catalysts", "topicNames"],
        "additionalProperties": false
    });

    if include_era_labels {
        schema["properties"]["eraLabels"] = serde_json::json!({
            "type": "array",
            "items": { "type": "string" }
        });
        schema["required"]
            .as_array_mut()
            .expect("schema required is an array")
            .push(serde_json::json!("eraLabels"));
    }

    schema
}

// ---------------------------------------------------------------------------
// System prompt (built once, cached across entities)
// ---------------------------------------------------------------------------

/// Build the system prompt for catalyst generation.
///
/// Includes:
/// - Base catalyst generation instructions (format-aware)
/// - All catalyst profiles (for prompt caching — actual profile selected per-entity)
/// - Additional prompt context for the vault (if available)
pub fn build_system_prompt(format_key: Option<&str>, prompt_context: Option<&str>) -> String {
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
GROUNDING AND SAFETY: Treat every interpretation as tentative. Do not make clinical, diagnostic, \
or personality claims. Ground every catalyst in the supplied excerpts.\n\n\
Always respond with valid JSON that includes catalysts, topic names, and era labels.",
        fmt.format_prompt, surface_items, examples_section,
    );

    // All profiles section
    let profiles_section = build_profiles_section();

    let mut prompt = format!("{base}\n\n{profiles_section}");

    // Optional vault context
    if let Some(context) = prompt_context {
        prompt.push_str(&format!(
            "\n\n<vault_context>\n\
The following context summarizes tracked entities and the vault's current terrain. \
Use this to understand how the current entity relates to the broader landscape of inquiry.\n\n\
{context}\n\
</vault_context>"
        ));
    }

    prompt
}

/// Build the neutral system contract used when every selected entity has a
/// custom readable profile. The per-entity profile owns semantic intent, so
/// injecting the built-in reflective catalog and its examples would create a
/// competing instruction surface.
pub fn build_custom_reading_system_prompt(
    format_key: Option<&str>,
    prompt_context: Option<&str>,
) -> String {
    let fmt = format_key
        .and_then(get_format)
        .unwrap_or_else(|| get_format(DEFAULT_FORMAT_KEY).unwrap());
    let mut prompt = format!(
        "You generate source-grounded semantic retrieval handles for a knowledge base.\n\n\
{}\n\n\
The user prompt supplies a SELECTED READING PROFILE. Its seek, notice, ask, and note fields \n\
determine what relationship each catalyst should expose. Follow that profile directly. Do not \n\
substitute generic questions about cost, tension, assumptions, or what was left unsaid unless the \n\
selected profile and supplied evidence call for them. Direct factual retrieval questions are valid.\n\n\
Preserve concrete identity, scope, time, identifiers, and source vocabulary when they determine \n\
which evidence is relevant. Leave the requested relation open rather than inserting an unsupported \n\
answer. Treat interpretations as tentative, make no clinical, diagnostic, or personality claims, \n\
and ground every catalyst in the supplied excerpts.\n\n\
Always respond with valid JSON that includes catalysts, topic names, and era labels.",
        fmt.format_prompt,
    );
    if let Some(context) = prompt_context {
        prompt.push_str(&format!(
            "\n\n<vault_context>\n\
The following context summarizes tracked entities and the vault's current terrain. \n\
Use it only to understand scope and relationships around the current entity.\n\n\
{context}\n\
</vault_context>"
        ));
    }
    prompt
}

/// Build the `<catalyst_profiles>` section listing all enabled profiles.
fn build_profiles_section() -> String {
    let mut lines: Vec<String> = vec!["<catalyst_profiles>".into(), String::new()];

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
fn build_practice_shape(entity: &Entity, now: DateTime<Utc>) -> String {
    let mut parts: Vec<String> = Vec::new();

    let freq_recent = entity.trends.frequency_recent;
    let freq_all = entity.frequency;

    // Engagement trajectory from timeline buckets
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
            // A last_seen after the reference time (a future-dated note) is
            // active at that time, never a negative age.
            let days_since = (now - last_seen_dt).num_days().max(0);
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
/// entity-specific context and instructions. `reference_ms` is the generation
/// clock (a vault's `as of`, else wall time) that practice-shape recency uses.
pub fn build_user_prompt(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
    profile: &CatalystProfile,
    format_key: &str,
    entity: Option<&Entity>,
    era_context: Option<&EraContext>,
    reference_ms: i64,
) -> String {
    build_user_prompt_body(
        entity_name,
        context_samples,
        num_catalysts,
        Some(profile),
        format_key,
        entity,
        era_context,
        reference_ms,
    )
}

/// Build the user prompt body for a custom readable profile.
///
/// Pairs with [`build_custom_reading_system_prompt`]: the caller prepends the
/// SELECTED READING PROFILE guidance. The body carries only the dated evidence,
/// the focus entity, and the grounding/format/count/JSON output contract — no
/// built-in profile header, practice shape, register/engagement steering, or
/// default periphery probe, all of which would compete with the profile.
pub fn build_custom_reading_user_prompt(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
    format_key: &str,
    era_context: Option<&EraContext>,
) -> String {
    build_user_prompt_body(
        entity_name,
        context_samples,
        num_catalysts,
        None,
        format_key,
        None,
        era_context,
        0,
    )
}

/// Shared user prompt builder. `builtin_profile` is `None` for custom readings,
/// which drops every built-in steering surface while keeping evidence and the
/// output contract byte-identical.
/// Whether a tag name is a synthetic body-cluster handle (`auto-cluster-*` or
/// `auto/body/*`). Such names are functional handles only; nothing may infer
/// the cluster's meaning from them.
pub fn is_synthetic_body_cluster(entity_name: &str) -> bool {
    entity_name.starts_with("auto-cluster-") || entity_name.starts_with("auto/body/")
}

#[allow(clippy::too_many_arguments)]
fn build_user_prompt_body(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
    builtin_profile: Option<&CatalystProfile>,
    format_key: &str,
    entity: Option<&Entity>,
    era_context: Option<&EraContext>,
    reference_ms: i64,
) -> String {
    let fmt = get_format(format_key).unwrap_or_else(|| get_format(DEFAULT_FORMAT_KEY).unwrap());
    let synthetic_body_cluster = is_synthetic_body_cluster(entity_name);

    // Profile selection header (custom readings get theirs from the caller)
    let mut prompt = match builtin_profile {
        Some(profile) => format!(
            "SELECTED PROFILE: {} (`{}`)\n\n\
Apply the guidance and approach from the {} profile defined in the system prompt.",
            profile.display_name, profile.key, profile.display_name,
        ),
        None => String::new(),
    };

    if let Some(ec) = era_context {
        for (era_idx, era) in ec.eras.iter().enumerate() {
            let samples = ec
                .samples
                .iter()
                .filter(|(idx, _)| *idx == era_idx)
                .map(|(_, sample)| sample.as_str())
                .collect::<Vec<_>>();
            if !samples.is_empty() {
                prompt.push_str(&format!(
                    "\n\n<vault_context era=\"{}\">\n{}\n</vault_context>",
                    era.label,
                    samples.join("\n\n")
                ));
            }
        }
    } else {
        let max_samples = context_samples.len().min(20);
        let context_section: String = context_samples[..max_samples].join("\n\n");
        prompt.push_str(&format!(
            "\n\n<vault_context>\n{context_section}\n</vault_context>"
        ));
    }

    // Entity focus
    if synthetic_body_cluster {
        prompt.push_str(&format!(
            "\n\n<entity>\nFocus entity: {entity_name}\n\
This is an automatically generated body-similarity cluster ID. The words after auto-cluster \
are a weak display hint, not evidence. Treat the excerpts as the source of meaning for this \
cluster.\n</entity>"
        ));
    } else {
        prompt.push_str(&format!(
            "\n\n<entity>\nFocus entity: {entity_name}\n</entity>"
        ));
    }

    // Practice shape from entity trends (built-in profiles only)
    if let Some(e) = entity.filter(|_| builtin_profile.is_some()) {
        let reference = DateTime::from_timestamp_millis(reference_ms).unwrap_or_else(Utc::now);
        let practice = build_practice_shape(e, reference);
        if !practice.is_empty() {
            prompt.push_str(&format!("\n\n{practice}"));
        }
    }

    // Register detection + engagement shape preamble (built-in profiles only)
    if builtin_profile.is_none() {
        prompt.push_str(
            "\n\n\
Catalysts must be retrieval-functional — each one should embed well against vault passages and \
surface relevant content when used as a search query.",
        );
    } else {
        prompt.push_str(
            "\n\n\
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
embed well against vault passages and surface relevant content when used as a search query.",
        );
    }

    // Format-specific instructions
    let (mut periphery_instruction, format_instruction, placeholder_label) =
        format_instructions(fmt);
    if builtin_profile.is_none() && periphery_instruction == QUESTION_PERIPHERY_INSTRUCTION {
        periphery_instruction = CUSTOM_READING_QUESTION_INSTRUCTION;
    }

    // Catalyst / topic placeholders
    let catalyst_placeholders: String = (0..num_catalysts)
        .map(|i| format!("    \"[{} {}]\"", placeholder_label, i + 1))
        .collect::<Vec<_>>()
        .join(",\n");
    let topic_placeholders: String = (0..num_catalysts)
        .map(|i| format!("    \"[specific concept relationship {}]\"", i + 1))
        .collect::<Vec<_>>()
        .join(",\n");
    let era_placeholders: String = (0..num_catalysts)
        .map(|i| format!("    \"[era label {}]\"", i + 1))
        .collect::<Vec<_>>()
        .join(",\n");

    prompt.push_str(&format!(
        "\n\n\
Now attend to the excerpts and:\n\
1. Ground each catalyst in a specific phrase from the excerpts — what does this entity actually mean in THIS vault? If the entity is an auto-cluster ID, use the keyword slug only as a weak hint and infer the cluster's meaning from the excerpts. Then {periphery_instruction}\n\
2. {format_instruction}\n\
3. For topicNames: 3-5 lowercase words capturing each inquiry (e.g. \"craft negotiation perfection\", \"tools reveal conceal needs\").\n\
4. For eraLabels: copy the era label that most shaped each catalyst. Use an empty string only if no era label is available.\n\n\
Return a JSON object with:\n\
{{\n\
  \"catalysts\": [\n\
{catalyst_placeholders}\n\
  ],\n\
  \"topicNames\": [\n\
{topic_placeholders}\n\
  ],\n\
  \"eraLabels\": [\n\
{era_placeholders}\n\
  ]\n\
}}"
    ));

    prompt
}

/// Simplified user prompt for local fine-tuned models.
///
/// Strips profile selection, register detection, engagement shape, and format
/// template — all baked into the model via SFT. Just context + entity + count.
pub fn build_local_user_prompt(
    entity_name: &str,
    context_samples: &[String],
    num_catalysts: usize,
    profile: Option<&CatalystProfile>,
    era_context: Option<&EraContext>,
) -> String {
    let mut prompt = String::new();

    if let Some(profile) = profile {
        let guidance = full_guidance_lines(profile).join("\n- ");
        prompt.push_str(&format!(
            "SELECTED PROFILE: {} (`{}`)\n\
             Apply this optional profile as the generation lens.\n\
             {}\n\
             Guidance:\n- {}\n\n",
            profile.display_name, profile.key, profile.description, guidance
        ));
    }

    // Context section
    if let Some(ec) = era_context {
        for (era_idx, era) in ec.eras.iter().enumerate() {
            let era_samples: Vec<&str> = ec
                .samples
                .iter()
                .filter(|(idx, _)| *idx == era_idx)
                .map(|(_, s)| s.as_str())
                .collect();
            if !era_samples.is_empty() {
                prompt.push_str(&format!(
                    "<vault_context era=\"{}\">\n{}\n</vault_context>\n\n",
                    era.label,
                    era_samples.join("\n\n")
                ));
            }
        }
    } else if !context_samples.is_empty() {
        let context_section = context_samples.join("\n\n");
        prompt.push_str(&format!(
            "<vault_context>\n{context_section}\n</vault_context>\n\n"
        ));
    }

    prompt.push_str(&format!(
        "<entity>Focus entity: {entity_name}</entity>\n\n\
         Generate {num_catalysts} catalyst questions grounded in these excerpts. \
         Each catalyst should probe a different facet — different tensions, angles, \
         or time periods. Return JSON:\n\
         {{\"catalysts\": [...], \"topicNames\": [...]}}."
    ));

    prompt
}

/// Default semantic instruction for question-form catalysts. Custom readable
/// profiles replace this clause while retaining the question output contract.
pub const QUESTION_PERIPHERY_INSTRUCTION: &str = "Then probe AROUND the grounded meaning, not about it directly. Ask about what it costs, what it forecloses, what it's in friction with, or what it quietly assumes. Name what's at stake rather than describing the concept.";

/// Question-form replacement for [`QUESTION_PERIPHERY_INSTRUCTION`] when a
/// custom readable profile owns the semantic aim.
pub const CUSTOM_READING_QUESTION_INSTRUCTION: &str = "Then formulate the retrieval question requested by the selected reading profile. Preserve the concrete identity, scope, time, and source vocabulary needed to route back to this evidence; leave the profile's requested relation open.";

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
            QUESTION_PERIPHERY_INSTRUCTION,
            "Each catalyst must be a single sentence question. Keep every catalyst to one sentence and avoid second-person framing. IMPORTANT: Vary your sentence openings — do NOT start multiple catalysts with the same word (e.g., don't start 3 catalysts with \"What\").",
            "Retrieval question",
        ),
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// A single parsed catalyst with text, topic, and optional era label.
#[derive(Debug, Clone, PartialEq, Eq)]
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
///
/// If both full-parse attempts fail (e.g. truncated response), falls back to partial
/// recovery: extracts however many complete catalyst strings exist before the cutoff.
pub fn parse_catalyst_response(response: &str) -> Result<CatalystResponse> {
    let s = strip_code_fences(response);
    // Normalize Unicode smart quotes → escaped ASCII so they don't break JSON string boundaries.
    let s = normalize_smart_quotes(s);
    let s = strip_trailing_commas(&s);

    // Try the object format first
    if let Ok(payload) = serde_json::from_str::<CatalystPayload>(&s) {
        return Ok(drop_empty_catalysts(build_from_payload(payload)));
    }

    if let Ok(payload) = serde_json::from_str::<CatalystEntriesPayload>(&s) {
        return Ok(drop_empty_catalysts(build_from_entries(payload.entries)));
    }

    // Fall back to legacy array format
    if let Ok(entries) = serde_json::from_str::<Vec<CatalystEntry>>(&s) {
        return Ok(drop_empty_catalysts(build_from_entries(entries)));
    }

    // Partial recovery: salvage complete catalysts from a truncated response.
    if let Some(recovered) = try_recover_partial(&s).map(drop_empty_catalysts) {
        if !recovered.catalysts.is_empty() {
            return Ok(recovered);
        }
    }

    bail!("Failed to parse catalyst response as JSON")
}

/// Drop catalysts whose text is empty or whitespace-only.
///
/// Neither the JSON schema nor the local GBNF grammar can require a non-empty
/// string, and small local models do emit `""`. Filtering after topics and era
/// labels are paired keeps the remaining catalysts aligned with their metadata.
fn drop_empty_catalysts(response: CatalystResponse) -> CatalystResponse {
    let before = response.catalysts.len();
    let catalysts: Vec<ParsedCatalyst> = response
        .catalysts
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .collect();
    if catalysts.len() < before {
        debug!(
            dropped = before - catalysts.len(),
            kept = catalysts.len(),
            "dropped empty catalyst hypotheses from model response"
        );
    }
    let pairs = catalysts
        .iter()
        .map(|c| (c.text.clone(), c.topic.clone()))
        .collect();
    CatalystResponse { pairs, catalysts }
}

fn build_from_payload(payload: CatalystPayload) -> CatalystResponse {
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
            ParsedCatalyst {
                text,
                topic,
                era_label,
            }
        })
        .collect();
    let pairs = catalysts
        .iter()
        .map(|c| (c.text.clone(), c.topic.clone()))
        .collect();
    CatalystResponse { pairs, catalysts }
}

fn build_from_entries(entries: Vec<CatalystEntry>) -> CatalystResponse {
    let catalysts: Vec<ParsedCatalyst> = entries
        .into_iter()
        .map(|entry| ParsedCatalyst {
            text: entry.catalyst,
            topic: entry.topic,
            era_label: entry.era_label,
        })
        .collect();
    let pairs = catalysts
        .iter()
        .map(|c| (c.text.clone(), c.topic.clone()))
        .collect();
    CatalystResponse { pairs, catalysts }
}

/// Replace Unicode left/right double quotation marks with escaped ASCII quotes
/// so they don't accidentally break JSON string boundaries.
fn normalize_smart_quotes(s: &str) -> String {
    s.replace('\u{201C}', "\\\"").replace('\u{201D}', "\\\"")
}

/// Attempt to salvage complete catalysts from a truncated JSON response.
///
/// Finds the `"catalysts"` array in the (possibly incomplete) JSON and extracts
/// every fully-formed string before the truncation point. Does the same for
/// `"topicNames"` and `"eraLabels"`, generating fallback topics where needed.
fn try_recover_partial(json: &str) -> Option<CatalystResponse> {
    let catalysts = find_and_extract_strings(json, "catalysts")?;
    if catalysts.is_empty() {
        return None;
    }
    let topic_names = find_and_extract_strings(json, "topicNames").unwrap_or_default();
    let era_labels = find_and_extract_strings(json, "eraLabels").unwrap_or_default();

    let parsed: Vec<ParsedCatalyst> = catalysts
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let topic = topic_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| generate_fallback_topic(&text));
            let era_label = era_labels.get(i).cloned();
            ParsedCatalyst {
                text,
                topic,
                era_label,
            }
        })
        .collect();

    let pairs = parsed
        .iter()
        .map(|c| (c.text.clone(), c.topic.clone()))
        .collect();
    Some(CatalystResponse {
        pairs,
        catalysts: parsed,
    })
}

/// Locate a named key in JSON and extract its complete string array values.
///
/// Returns `None` if the key or array opener is not found. Returns a
/// (possibly empty) `Vec` of all complete strings before the truncation point.
fn find_and_extract_strings(json: &str, key: &str) -> Option<Vec<String>> {
    let needle = format!("\"{}\"", key);
    let key_pos = json.find(&needle)?;
    let after_key = &json[key_pos + needle.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start_matches(|c: char| c.is_whitespace());
    if !after_colon.starts_with('[') {
        return None;
    }
    Some(extract_complete_strings_from_array(after_colon))
}

/// Walk a JSON array literal (starting with `[`) and return every fully-formed
/// string value encountered before truncation or the closing `]`.
fn extract_complete_strings_from_array(array_json: &str) -> Vec<String> {
    let rest = match array_json
        .trim_start_matches(|c: char| c.is_whitespace())
        .strip_prefix('[')
    {
        Some(r) => r,
        None => return vec![],
    };

    let mut results = Vec::new();
    let mut chars = rest.char_indices().peekable();

    loop {
        // Skip whitespace and commas between elements.
        while let Some(&(_, c)) = chars.peek() {
            if c == ',' || c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        match chars.peek() {
            None | Some(&(_, ']')) => break,
            Some(&(_, '"')) => {
                chars.next(); // consume opening quote
                let mut value = String::new();
                let mut complete = false;

                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => match chars.next() {
                            Some((_, '"')) => value.push('"'),
                            Some((_, '\\')) => value.push('\\'),
                            Some((_, '/')) => value.push('/'),
                            Some((_, 'n')) => value.push('\n'),
                            Some((_, 'r')) => value.push('\r'),
                            Some((_, 't')) => value.push('\t'),
                            Some((_, 'b')) => value.push('\x08'),
                            Some((_, 'f')) => value.push('\x0C'),
                            Some((_, 'u')) => {
                                let hex: String = (0..4)
                                    .filter_map(|_| chars.next().map(|(_, ch)| ch))
                                    .collect();
                                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(n) {
                                        value.push(ch);
                                    }
                                }
                            }
                            Some((_, c)) => value.push(c),
                            None => break,
                        },
                        '"' => {
                            complete = true;
                            break;
                        }
                        c => value.push(c),
                    }
                }

                if complete {
                    results.push(value);
                } else {
                    // Truncation — stop here, don't include the partial string.
                    break;
                }
            }
            _ => break, // unexpected token
        }
    }

    results
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
            while j < bytes.len()
                && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r')
            {
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
    let cleaned = text.to_lowercase().replace('?', "").replace('.', "");
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "how"
                    | "what"
                    | "why"
                    | "when"
                    | "where"
                    | "who"
                    | "which"
                    | "does"
                    | "do"
                    | "is"
                    | "are"
                    | "can"
                    | "could"
                    | "would"
                    | "should"
                    | "the"
                    | "a"
                    | "an"
                    | "in"
                    | "of"
                    | "to"
                    | "and"
                    | "or"
                    | "for"
                    | "with"
                    | "that"
                    | "this"
            )
        })
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
    use crate::llm::catalyst_profiles::{PROFILE_OPERATIONAL, PROFILE_TENSION_TRACE};

    fn dated_entity(last_seen: &str) -> Entity {
        Entity {
            id: Some(1),
            name: "topic".into(),
            entity_type: crate::models::EntityType::Link,
            frequency: 4,
            ranking_weight: 4.0,
            first_seen: None,
            last_seen: Some(
                chrono::DateTime::parse_from_rfc3339(last_seen)
                    .unwrap()
                    .timestamp_millis(),
            ),
            trends: Default::default(),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn practice_shape_treats_future_last_seen_as_active_today() {
        let now = DateTime::parse_from_rfc3339("2026-09-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let future = build_practice_shape(&dated_entity("2100-06-01T00:00:00Z"), now);
        assert!(future.contains("Last active today."), "{future}");
        assert!(!future.contains('-'), "{future}");
        let past = build_practice_shape(&dated_entity("2026-09-20T12:00:00Z"), now);
        assert!(past.contains("Last active 3 days ago."), "{past}");
    }

    #[test]
    fn test_parse_object_format() {
        let json =
            r#"{"catalysts":["Where does X?","What about Y?"],"topicNames":["topic a","topic b"]}"#;
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
    fn test_parse_entries_object_format() {
        let json =
            r#"{"entries":[{"catalyst":"Where does X?","topic":"topic a","eraLabel":"spring"}]}"#;
        let resp = parse_catalyst_response(json).unwrap();
        assert_eq!(resp.pairs.len(), 1);
        assert_eq!(resp.catalysts[0].text, "Where does X?");
        assert_eq!(resp.catalysts[0].topic, "topic a");
        assert_eq!(resp.catalysts[0].era_label.as_deref(), Some("spring"));
    }

    #[test]
    fn test_parse_with_code_fences() {
        let response = "```json\n[{\"catalyst\":\"A?\",\"topic\":\"t\"}]\n```";
        let resp = parse_catalyst_response(response).unwrap();
        assert_eq!(resp.pairs.len(), 1);
    }

    #[test]
    fn test_build_system_prompt_default() {
        let prompt = build_system_prompt(None, None);
        assert!(prompt.contains("single sentence questions"));
        assert!(prompt.contains("<catalyst_profiles>"));
        assert!(prompt.contains("Tension Trace"));
        assert!(prompt.contains("Preference Evidence"));
        assert!(prompt.contains("Reflective"));
        assert!(prompt.contains("</catalyst_profiles>"));
    }

    #[test]
    fn test_build_system_prompt_thesis_format() {
        let prompt = build_system_prompt(Some("thesis"), None);
        assert!(prompt.contains("investment theses"));
        assert!(prompt.contains("Where value is accruing"));
    }

    #[test]
    fn test_build_system_prompt_with_prompt_context() {
        let prompt = build_system_prompt(None, Some("My vault context content"));
        assert!(prompt.contains("<vault_context>"));
        assert!(prompt.contains("My vault context content"));
        assert!(prompt.contains("</vault_context>"));
    }

    #[test]
    fn test_custom_reading_system_prompt_leaves_semantic_intent_to_profile() {
        let prompt = build_custom_reading_system_prompt(
            Some("question"),
            Some("Organization and source context"),
        );
        assert!(prompt.contains("SELECTED READING PROFILE"));
        assert!(prompt.contains("Direct factual retrieval questions are valid"));
        assert!(prompt.contains("Organization and source context"));
        assert!(!prompt.contains("<catalyst_profiles>"));
        assert!(!prompt.contains("What does the commitment to craft cost"));
    }

    #[test]
    fn test_build_user_prompt_question_format() {
        let samples = vec!["excerpt one".into(), "excerpt two".into()];
        let prompt = build_user_prompt(
            "enzyme",
            &samples,
            3,
            &PROFILE_TENSION_TRACE,
            "question",
            None,
            None,
            Utc::now().timestamp_millis(),
        );
        assert!(prompt.contains("SELECTED PROFILE: Tension Trace"));
        assert!(prompt.contains("<vault_context>"));
        assert!(prompt.contains("Focus entity: enzyme"));
        assert!(prompt.contains("probe AROUND the grounded meaning"));
        assert!(prompt.contains("\"catalysts\""));
    }

    const CUSTOM_READING_STEERING: [&str; 8] = [
        "<practice_shape>",
        "Engagement shape",
        "Register —",
        "probe what would reopen",
        "tensions, emergence",
        "probe AROUND",
        "what it costs",
        "defined in the system prompt",
    ];

    #[test]
    fn custom_reading_user_prompt_keeps_evidence_and_contract_without_steering() {
        let samples = vec!["EVIDENCE ONE [notes/a.md]".into(), "EVIDENCE TWO".into()];
        let prompt = build_custom_reading_user_prompt("enzyme", &samples, 3, "question", None);
        for banned in CUSTOM_READING_STEERING {
            assert!(!prompt.contains(banned), "{banned:?} in {prompt}");
        }
        for kept in [
            "<vault_context>",
            "EVIDENCE ONE [notes/a.md]",
            "EVIDENCE TWO",
            "Focus entity: enzyme",
            "Ground each catalyst in a specific phrase",
            CUSTOM_READING_QUESTION_INSTRUCTION,
            "single sentence question",
            "[Retrieval question 3]",
            "\"topicNames\"",
            "\"eraLabels\"",
            "retrieval-functional",
        ] {
            assert!(prompt.contains(kept), "{kept:?} missing from {prompt}");
        }
        assert!(!prompt.contains("SELECTED PROFILE"));
    }

    #[test]
    fn builtin_user_prompt_keeps_practice_shape_and_engagement_block() {
        let reference = DateTime::parse_from_rfc3339("2026-09-23T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        let prompt = build_user_prompt(
            "topic",
            &["excerpt".into()],
            3,
            &PROFILE_TENSION_TRACE,
            "question",
            Some(&dated_entity("2026-01-01T00:00:00Z")),
            None,
            reference,
        );
        for kept in CUSTOM_READING_STEERING {
            assert!(prompt.contains(kept), "{kept:?} missing from {prompt}");
        }
    }

    #[test]
    fn test_build_local_user_prompt_includes_optional_profile() {
        let samples = vec!["excerpt one".into()];
        let prompt =
            build_local_user_prompt("recipes", &samples, 3, Some(&PROFILE_OPERATIONAL), None);

        assert!(prompt.contains("SELECTED PROFILE: Operational"));
        assert!(prompt.contains("`operational`"));
        assert!(prompt.contains("Focus entity: recipes"));
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
            Utc::now().timestamp_millis(),
        );
        assert!(prompt.contains("SELECTED PROFILE: Operational"));
        assert!(prompt.contains("articulate what's at stake"));
        assert!(prompt.contains("Investment thesis statement"));
    }

    #[test]
    fn test_parse_partial_recovery_truncated_array() {
        // Simulates a response truncated mid-sentence in the last catalyst.
        let response = r#"{
  "catalysts": [
    "What does the cost of assuming users want introspection reveal about early product bets?",
    "How does the tension between local and remote compute shape the memory layer?",
    "What does"#;
        let resp = parse_catalyst_response(response).unwrap();
        // Should recover 2 complete catalysts, drop the truncated one.
        assert_eq!(resp.catalysts.len(), 2);
        assert!(resp.catalysts[0].text.contains("introspection"));
        assert!(resp.catalysts[1].text.contains("memory layer"));
    }

    #[test]
    fn test_parse_partial_recovery_with_topics() {
        // Truncated after catalysts array but topics partially present.
        let response = r#"{
  "catalysts": [
    "What does X cost?",
    "How does Y work?"
  ],
  "topicNames": [
    "cost assumption tradeoff",
    "tension local rem"#;
        let resp = parse_catalyst_response(response).unwrap();
        assert_eq!(resp.catalysts.len(), 2);
        // First topic complete, second truncated → fallback generated for second.
        assert_eq!(resp.catalysts[0].topic, "cost assumption tradeoff");
        assert!(!resp.catalysts[1].topic.is_empty()); // fallback generated
    }

    #[test]
    fn test_parse_partial_recovery_empty_fails() {
        // Only the key came through — no complete strings. Should error, not return empty.
        let response = r#"{"catalysts": ["#;
        assert!(parse_catalyst_response(response).is_err());
    }

    #[test]
    fn test_parse_smart_quote_normalization() {
        // Unicode left/right double quotes inside a string value.
        let response =
            r#"{"catalysts":["\u201CWhat does it cost?\u201D"],"topicNames":["cost framing"]}"#;
        let resp = parse_catalyst_response(response).unwrap();
        assert_eq!(resp.catalysts.len(), 1);
    }

    #[test]
    fn test_parse_drops_empty_and_whitespace_catalysts_keeping_alignment() {
        let response = r#"{"catalysts":["", "What does X cost?", "  \n\t", "How does Y work?"],
            "topicNames":["empty", "cost framing", "blank", "mechanism"],
            "eraLabels":["a", "b", "c", "d"]}"#;
        let resp = parse_catalyst_response(response).unwrap();
        assert_eq!(resp.catalysts.len(), 2);
        assert_eq!(resp.catalysts[0].text, "What does X cost?");
        assert_eq!(resp.catalysts[0].topic, "cost framing");
        assert_eq!(resp.catalysts[0].era_label.as_deref(), Some("b"));
        assert_eq!(resp.catalysts[1].topic, "mechanism");
        assert_eq!(resp.pairs.len(), 2);

        let legacy = r#"[{"catalyst":" ","topic":"blank"},{"catalyst":"Why Z?","topic":"z"}]"#;
        assert_eq!(parse_catalyst_response(legacy).unwrap().catalysts.len(), 1);

        let all_empty = r#"{"catalysts":["", " "],"topicNames":["a","b"]}"#;
        assert!(
            parse_catalyst_response(all_empty)
                .unwrap()
                .catalysts
                .is_empty()
        );

        let truncated_empty = r#"{"catalysts": ["", "  ", "Whe"#;
        assert!(parse_catalyst_response(truncated_empty).is_err());
    }
}
