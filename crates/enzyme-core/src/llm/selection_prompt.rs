use anyhow::{Context, Result};
use serde::Deserialize;

use super::catalyst_prompt::strip_code_fences;

// ---------------------------------------------------------------------------
// JSON response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionResponse {
    pub catalyzed_entities: Vec<SelectedEntity>,
    #[serde(default)]
    pub catalyst_profile: Option<String>,
    #[serde(default)]
    pub guide_signal: Option<String>,
    // Future: add voice_signal here — a tonal description of how the user writes
    // (sentence rhythm, certainty level, specificity, register). Extract from
    // vault excerpts or user guide files passed as <user_voice> context.
    #[serde(default)]
    pub expandable_folders: Vec<ExpandableFolder>,
    #[serde(default)]
    pub excluded_entities: Vec<ExcludedEntity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExcludedEntity {
    pub entity_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedEntity {
    pub entity_ref: String,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandableFolder {
    pub folder_ref: String,
    pub children: Vec<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

/// JSON Schema for the `SelectionResponse` response type.
///
/// Used with `response_format: json_schema` for strict schema enforcement.
pub fn selection_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "catalyzedEntities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "entityRef": { "type": "string" },
                        "profile": { "type": ["string", "null"] }
                    },
                    "required": ["entityRef", "profile"],
                    "additionalProperties": false
                }
            },
            "catalystProfile": { "type": ["string", "null"] },
            "guideSignal": { "type": ["string", "null"] },
            "expandableFolders": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "folderRef": { "type": "string" },
                        "children": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "profile": { "type": ["string", "null"] }
                    },
                    "required": ["folderRef", "children", "profile"],
                    "additionalProperties": false
                }
            },
            "excludedEntities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "entityRef": { "type": "string" }
                    },
                    "required": ["entityRef"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["catalyzedEntities", "catalystProfile", "guideSignal", "expandableFolders", "excludedEntities"],
        "additionalProperties": false
    })
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

/// Build the system prompt for LLM entity selection.
///
/// Describes the 6 profiles compactly, entity type syntax, expandable folder
/// semantics, and asks the LLM to select entities from the trending candidates.
pub fn build_selection_system_prompt() -> String {
    r##"You are an entity selector for Enzyme, a system that generates thought-provoking catalysts for an Obsidian vault.

Your task: Read the vault's structure and trending entities, then identify the **intellectual architecture** — the folders, tags, and recurring concepts that organize the owner's thinking. Select the fewest entities that cover the most ground.

## Core principle: coverage over activity

Every entity you select will have catalysts generated for it. A folder containing 200 notes produces far richer catalysts than any single note inside it. A tag used across 30 files reveals a thinking pattern; a wikilink referenced once is just a document title.

**Use the `file_count` stat as your primary signal.** High file_count = high coverage = good catalyst target. Prefer 8 high-coverage entities over 20 low-coverage ones.

## Entity types and syntax
- Tags: "#tag" (e.g. "#enzyme", "#philosophy/epistemology")
- Links: "[[Note Name]]" (e.g. "[[product-market fit]]", "[[deep work]]")
- Folders: "folder:name" (e.g. "folder:inbox", "folder:people")

## Catalyst profiles (assign the best-fit profile per entity)
- **dialectical** — surfaces intellectual tensions, conflicting frameworks, unresolved contradictions. Default for general thinking entities.
- **synthesis** — bridges imported/read content to personal thinking. For highlights, articles, readwise content.
- **relational** — surfaces relationship value and dynamics. For people, contacts, network entities.
- **operational** — finds what's stuck, blocked, or being avoided. For projects, tasks, active work.
- **strategic** — names positions, bets, and leverage. For strategy, investment, decision-making entities.
- **reflective** — creates space for personal pattern recognition. For journaling, self-reflection entities.

## What to select and what to skip

**Good catalyst targets:**
- Folders with many files (content collections, people, projects)
- Tags used across many notes (recurring themes, interests, life areas)
- Wikilinks referenced across 5+ files (recurring concepts, people, ideas)

**Skip these — they waste catalyst budget:**
- Individual note titles, article names, or dated entries (e.g. "[[Some Article Highlights]]", "convos/Person - 2026-03-05")
- Tooling/config folders (e.g. .claude, templates, config)
- Meta/structural tags (#todo, #note, #template)
- Wikilinks referenced in only 1-2 files (document titles, not concepts)

## Expandable folders

Some folder entities include a `<page_entities>` section listing wikilinks with source files in that folder. These folders can be marked as expandable.

**Important:** Expandable folder children should only be **recurring concepts or people** — entities that appear across multiple files. Never list individual dated entries, article titles, or single-file documents as children. If a folder's page entities are mostly document titles (articles, conversation logs, highlights), select the folder but leave children empty.

Good children: `["[[alice]]", "[[bob]]"]` (people referenced across the vault)
Bad children: `["[[convos/Person - 2026-03-05]]", "[[Articles/Some Article Highlights]]"]` (dated entries, individual documents)

## Vault guide
A `<vault_guide>` may appear in the user message with directives from the vault owner. Treat it as **authoritative intent** — it overrides default heuristics. If the guide names specific folders or tags, include them. If it calls something structural, exclude it.

## Exclusions
Identify entities that should be excluded — structural/meta entities like #todo, #note, #template, tooling folders, or anything organizational rather than thematic.

## Guide signal
Write a 1-2 sentence read of what's intellectually alive in this vault right now. This calibrates future catalyst generation.

## How many to select
There is no minimum. A vault with 3 clear pillars needs only 3 entities. A complex vault with many active threads might need 20. **Select only what would produce meaningful catalysts.** Typical range: 5-20 entities.

## Response format
Return a JSON object:
```json
{
  "catalyzedEntities": [
    {"entityRef": "#tag", "profile": "dialectical"},
    {"entityRef": "folder:name", "profile": "synthesis"}
  ],
  "catalystProfile": "dialectical",
  "guideSignal": "...",
  "expandableFolders": [
    {
      "folderRef": "folder:people",
      "children": ["[[person1]]", "[[person2]]"],
      "profile": "relational"
    }
  ],
  "excludedEntities": [
    {"entityRef": "#todo"},
    {"entityRef": "#note"}
  ]
}
```"##.to_string()
}

// ---------------------------------------------------------------------------
// User prompt
// ---------------------------------------------------------------------------

/// Build the user prompt wrapping vault context XML.
pub fn build_selection_user_prompt(vault_context_xml: &str) -> String {
    format!(
        "Here is the vault's structure and trending entities. \
Select entities for catalyst generation — focus on high file_count entities that cover the most ground. \
Fewer high-coverage entities are better than many low-coverage ones.\n\n{vault_context_xml}"
    )
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the LLM selection response from JSON.
pub fn parse_selection_response(response: &str) -> Result<SelectionResponse> {
    let json_str = strip_code_fences(response);
    serde_json::from_str(json_str).context("failed to parse entity selection response as JSON")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_selection_response() {
        let json = r##"{
            "catalyzedEntities": [
                {"entityRef": "#enzyme", "profile": "dialectical"},
                {"entityRef": "[[deep work]]", "profile": "dialectical"}
            ],
            "catalystProfile": "dialectical",
            "guideSignal": "Thinking about craft and tools",
            "expandableFolders": [
                {
                    "folderRef": "folder:people",
                    "children": ["[[ari]]", "[[jonah]]"],
                    "profile": "relational"
                }
            ],
            "excludedEntities": [
                {"entityRef": "#todo"},
                {"entityRef": "#note"}
            ]
        }"##;
        let resp = parse_selection_response(json).unwrap();
        assert_eq!(resp.catalyzed_entities.len(), 2);
        assert_eq!(resp.catalyzed_entities[0].entity_ref, "#enzyme");
        assert_eq!(resp.catalyzed_entities[0].profile.as_deref(), Some("dialectical"));
        assert_eq!(resp.catalyst_profile.as_deref(), Some("dialectical"));
        assert_eq!(resp.guide_signal.as_deref(), Some("Thinking about craft and tools"));
        assert_eq!(resp.expandable_folders.len(), 1);
        assert_eq!(resp.expandable_folders[0].children.len(), 2);
        assert_eq!(resp.excluded_entities.len(), 2);
        assert_eq!(resp.excluded_entities[0].entity_ref, "#todo");
    }

    #[test]
    fn test_parse_selection_response_minimal() {
        let json = r##"{"catalyzedEntities": [{"entityRef": "#tag"}]}"##;
        let resp = parse_selection_response(json).unwrap();
        assert_eq!(resp.catalyzed_entities.len(), 1);
        assert!(resp.catalyst_profile.is_none());
        assert!(resp.guide_signal.is_none());
        assert!(resp.expandable_folders.is_empty());
    }

    #[test]
    fn test_parse_selection_response_with_fences() {
        let json = "```json\n{\"catalyzedEntities\": [{\"entityRef\": \"#test\"}]}\n```";
        let resp = parse_selection_response(json).unwrap();
        assert_eq!(resp.catalyzed_entities.len(), 1);
    }

    #[test]
    fn test_system_prompt_contains_profiles() {
        let prompt = build_selection_system_prompt();
        assert!(prompt.contains("dialectical"));
        assert!(prompt.contains("synthesis"));
        assert!(prompt.contains("relational"));
        assert!(prompt.contains("operational"));
        assert!(prompt.contains("strategic"));
        assert!(prompt.contains("reflective"));
    }
}
