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
    #[serde(default)]
    pub expandable_folders: Vec<ExpandableFolder>,
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

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

/// Build the system prompt for LLM entity selection.
///
/// Describes the 6 profiles compactly, entity type syntax, expandable folder
/// semantics, and asks the LLM to select 15–25 from the trending candidates.
pub fn build_selection_system_prompt() -> String {
    r##"You are an entity selector for Enzyme, a system that generates thought-provoking catalysts for an Obsidian vault.

Your task: From the trending entities below, select 15–25 that would produce the richest catalysts. You're curating the vault's intellectual surface area.

## Entity types and syntax
- Tags: "#tag" (e.g. "#enzyme", "#philosophy/epistemology")
- Links: "[[Note Name]]" (e.g. "[[Maren Holt]]", "[[product-market fit]]")
- Folders: "folder:name" (e.g. "folder:inbox", "folder:people")

## Catalyst profiles (assign the best-fit profile per entity)
- **dialectical** — surfaces intellectual tensions, conflicting frameworks, unresolved contradictions. Default for general thinking entities.
- **synthesis** — bridges imported/read content to personal thinking. For highlights, articles, readwise content.
- **relational** — surfaces relationship value and dynamics. For people, contacts, network entities.
- **operational** — finds what's stuck, blocked, or being avoided. For projects, tasks, active work.
- **strategic** — names positions, bets, and leverage. For strategy, investment, decision-making entities.
- **reflective** — creates space for personal pattern recognition. For journaling, self-reflection entities.

## Expandable folders
If a folder contains meaningful sub-entities (e.g. folder:people contains specific people), return it as an expandable folder with its most active children listed. Children inherit the folder's profile unless overridden.

## Selection criteria
- Prefer entities with recent activity and rich context
- Ensure diversity: mix of tags, links, and folders
- Avoid meta/structural entities (e.g. #todo, #note, generic folder names)
- Group related entities under expandable folders when natural

## Guide signal
Write a 1-2 sentence "guide signal" — what's intellectually alive in this vault right now. This calibrates future catalyst generation.

## Response format
Return a JSON object:
```json
{
  "catalyzedEntities": [
    {"entityRef": "#tag", "profile": "dialectical"},
    {"entityRef": "[[Note Name]]", "profile": "synthesis"},
    {"entityRef": "folder:name", "profile": "operational"}
  ],
  "catalystProfile": "dialectical",
  "guideSignal": "...",
  "expandableFolders": [
    {
      "folderRef": "folder:people",
      "children": ["[[person1]]", "[[person2]]"],
      "profile": "relational"
    }
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
        "Here is the vault's structure and trending entities with recent content excerpts. \
Select 15–25 entities for catalyst generation.\n\n{vault_context_xml}"
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
                {"entityRef": "[[Maren Holt]]", "profile": "relational"}
            ],
            "catalystProfile": "dialectical",
            "guideSignal": "Thinking about craft and tools",
            "expandableFolders": [
                {
                    "folderRef": "folder:people",
                    "children": ["[[ari]]", "[[jonah]]"],
                    "profile": "relational"
                }
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
