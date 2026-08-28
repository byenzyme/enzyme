use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const CONTEXT_OPEN: &str = "```enzyme-context";
const FENCE_CLOSE: &str = "```";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCatalystText {
    pub hypothesis: String,
    pub source_refs: Vec<String>,
}

/// Render a self-contained catalyst artifact with its generation context.
///
/// The hypothesis stays ordinary text. A JSON array in the context fence makes
/// the source set portable without pretending to prove each claim
/// or duplicating catalyst-row and epoch state in every artifact.
pub fn render_catalyst_text(hypothesis: &str, source_refs: &[String]) -> anyhow::Result<String> {
    anyhow::ensure!(!hypothesis.trim().is_empty(), "empty catalyst hypothesis");
    anyhow::ensure!(!source_refs.is_empty(), "catalyst has no source references");
    Ok(format!(
        "{}\n\n{CONTEXT_OPEN}\n{}\n{FENCE_CLOSE}",
        hypothesis.trim(),
        serde_json::to_string(source_refs)?
    ))
}

/// Parse catalyst prose and its durable generation-context sources.
pub fn parse_catalyst_text(text: &str) -> anyhow::Result<ParsedCatalystText> {
    let marker = format!("\n\n{CONTEXT_OPEN}\n");
    let (hypothesis, context_block) = text
        .rsplit_once(&marker)
        .ok_or_else(|| anyhow::anyhow!("missing catalyst context fence"))?;
    let context_jsonl = context_block
        .strip_suffix(&format!("\n{FENCE_CLOSE}"))
        .ok_or_else(|| anyhow::anyhow!("unterminated catalyst context fence"))?;
    anyhow::ensure!(!hypothesis.trim().is_empty(), "empty catalyst hypothesis");
    let source_refs: Vec<String> = serde_json::from_str(context_jsonl)?;
    anyhow::ensure!(!source_refs.is_empty(), "catalyst has no source references");
    Ok(ParsedCatalystText {
        hypothesis: hypothesis.trim().to_string(),
        source_refs,
    })
}

/// Text used for semantic embedding. Legacy catalyst chunks fall back to their
/// full text, while context-bearing chunks embed only their hypothesis.
pub fn catalyst_embedding_text(text: &str) -> String {
    parse_catalyst_text(text)
        .map(|parsed| parsed.hypothesis)
        .unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod catalyst_text_tests {
    use super::*;

    #[test]
    fn catalyst_text_round_trips_and_embedding_is_hypothesis_only() {
        let source = "sqlite:mail/thread-1".to_string();
        let text = render_catalyst_text(
            "What might the concrete next step leave unresolved?",
            &[source.clone()],
        )
        .unwrap();
        let parsed = parse_catalyst_text(&text).unwrap();
        assert_eq!(parsed.source_refs, vec![source]);
        assert_eq!(
            catalyst_embedding_text(&text),
            "What might the concrete next step leave unresolved?"
        );
        assert!(!catalyst_embedding_text(&text).contains("sqlite:mail/thread-1"));
    }
}

/// A catalyst — an LLM-generated thematic handle for approximate search.
///
/// Catalysts are the bridge between user queries and document collections.
/// They represent distilled themes or questions derived from vault entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalyst {
    pub id: String,
    pub text: String,
    /// The entity this catalyst was generated from.
    pub entity: String,
    /// The context (vault partition) this catalyst belongs to.
    pub context: String,
    pub generated_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

/// Pre-computed similarity between a catalyst and a document chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalystSimilarity {
    pub id: Option<i64>,
    pub catalyst_id: String,
    pub catalyst_text: String,
    pub file_path: String,
    pub chunk_index: i64,
    /// Cosine similarity score in [0.0, 1.0].
    pub similarity: f64,
    pub chunk_metadata: serde_json::Value,
    pub timeline_data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}
