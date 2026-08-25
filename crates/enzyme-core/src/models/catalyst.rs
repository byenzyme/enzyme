use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const CATALYST_TEXT_FORMAT_VERSION: u32 = 1;
const HEADER_OPEN: &str = "```enzyme-catalyst-header";
const RECEIPTS_OPEN: &str = "```receipts";
const FENCE_CLOSE: &str = "```";

/// Auditable evidence supplied to the model for a generated catalyst.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EvidenceAnchor {
    /// Request-local stable label cited by the model (for example `E1`).
    pub anchor_id: String,
    /// Canonical document/source reference used by the index.
    pub source_ref: String,
    /// Entity-occurrence rows connecting this evidence to the entity.
    pub occurrence_ids: Vec<i64>,
    /// Effective evidence timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: i64,
    /// Chunk index when the evidence came from a stored chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<u32>,
    /// Short verbatim span supplied to the generator.
    pub quote: String,
}

/// A rebuildable era index entry carried by every catalyst in a generation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalystTextEra {
    pub label: String,
    pub start_month: String,
    pub end_month: String,
    pub distinct_documents: usize,
    pub substantive_tokens: usize,
    pub occurrence_count: usize,
    pub catalyst_budget: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Stable, self-contained provenance header embedded in catalyst text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalystTextHeader {
    pub format_version: u32,
    /// Stable source identity for the entity (currently its canonical name).
    pub entity_id: String,
    pub entity_type: String,
    pub era: String,
    pub generation_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_id: Option<String>,
    pub generated_at_ms: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub participant_scoped: bool,
    /// All era allocations for this entity/run, duplicated deliberately so the
    /// derived era index can be rebuilt from any surviving catalyst chunk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eras: Vec<CatalystTextEra>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCatalystText {
    pub header: CatalystTextHeader,
    pub hypothesis: String,
    pub receipts: Vec<EvidenceAnchor>,
}

/// Render a canonical catalyst chunk. Header and receipts use JSON lines inside
/// named Markdown fences: machine-stable while remaining readable in raw text.
pub fn render_catalyst_text(
    header: &CatalystTextHeader,
    hypothesis: &str,
    receipts: &[EvidenceAnchor],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        header.format_version == CATALYST_TEXT_FORMAT_VERSION,
        "unsupported catalyst text format version {}",
        header.format_version
    );
    anyhow::ensure!(!hypothesis.trim().is_empty(), "empty catalyst hypothesis");
    anyhow::ensure!(!receipts.is_empty(), "catalyst has no receipts");

    let mut text = format!(
        "{HEADER_OPEN}\n{}\n{FENCE_CLOSE}\n\n{}\n\n{RECEIPTS_OPEN}\n",
        serde_json::to_string(header)?,
        hypothesis.trim()
    );
    for receipt in receipts {
        text.push_str(&serde_json::to_string(receipt)?);
        text.push('\n');
    }
    text.push_str(FENCE_CLOSE);
    Ok(text)
}

/// Parse the canonical header, hypothesis prose, and receipt JSON lines.
pub fn parse_catalyst_text(text: &str) -> anyhow::Result<ParsedCatalystText> {
    let header_prefix = format!("{HEADER_OPEN}\n");
    let rest = text
        .strip_prefix(&header_prefix)
        .ok_or_else(|| anyhow::anyhow!("missing catalyst header fence"))?;
    let header_end = format!("\n{FENCE_CLOSE}\n\n");
    let (header_json, rest) = rest
        .split_once(&header_end)
        .ok_or_else(|| anyhow::anyhow!("unterminated catalyst header fence"))?;
    let receipts_marker = format!("\n\n{RECEIPTS_OPEN}\n");
    let (hypothesis, receipts_block) = rest
        .split_once(&receipts_marker)
        .ok_or_else(|| anyhow::anyhow!("missing catalyst receipts fence"))?;
    let receipts_jsonl = receipts_block
        .strip_suffix(&format!("\n{FENCE_CLOSE}"))
        .ok_or_else(|| anyhow::anyhow!("unterminated catalyst receipts fence"))?;
    let header: CatalystTextHeader = serde_json::from_str(header_json)?;
    anyhow::ensure!(
        header.format_version == CATALYST_TEXT_FORMAT_VERSION,
        "unsupported catalyst text format version {}",
        header.format_version
    );
    anyhow::ensure!(!hypothesis.trim().is_empty(), "empty catalyst hypothesis");
    let receipts = receipts_jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<Vec<EvidenceAnchor>, _>>()?;
    anyhow::ensure!(!receipts.is_empty(), "catalyst has no receipts");
    Ok(ParsedCatalystText {
        header,
        hypothesis: hypothesis.trim().to_string(),
        receipts,
    })
}

/// Text used for semantic embedding. Legacy catalyst chunks fall back to their
/// full text, while canonical chunks exclude provenance header and receipts.
pub fn catalyst_embedding_text(text: &str) -> String {
    parse_catalyst_text(text)
        .map(|parsed| parsed.hypothesis)
        .unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod canonical_text_tests {
    use super::*;

    #[test]
    fn canonical_text_round_trips_and_embedding_is_hypothesis_only() {
        let header = CatalystTextHeader {
            format_version: CATALYST_TEXT_FORMAT_VERSION,
            entity_id: "person:alice".into(),
            entity_type: "link".into(),
            era: "2026-08".into(),
            generation_run_id: "run-1".into(),
            epoch_id: Some("epoch-1".into()),
            generated_at_ms: 1_777_000_000_000,
            participant_scoped: true,
            eras: vec![],
        };
        let receipt = EvidenceAnchor {
            anchor_id: "E1".into(),
            source_ref: "mail:thread-1".into(),
            occurrence_ids: vec![7],
            timestamp_ms: 1_777_000_000_000,
            chunk_index: Some(2),
            quote: "Alice proposed the concrete next step.".into(),
        };
        let text = render_catalyst_text(
            &header,
            "Alice may be testing a next step [E1].",
            &[receipt.clone()],
        )
        .unwrap();
        let parsed = parse_catalyst_text(&text).unwrap();
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.receipts, vec![receipt]);
        assert_eq!(
            catalyst_embedding_text(&text),
            "Alice may be testing a next step [E1]."
        );
        assert!(!catalyst_embedding_text(&text).contains("mail:thread-1"));
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
