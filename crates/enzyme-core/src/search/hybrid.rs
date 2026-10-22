//! Hybrid note search: lexical BM25 plus dense chunk similarity, rolled up to
//! notes. Catalysts annotate results ("why this") instead of ranking them.
//!
//! On a real Obsidian vault this recipe beat catalyst routing on agent queries
//! (mean judged relevance 1.17 vs at most 0.75) and on cross-domain questions
//! (2.55 vs at most 1.3 strong links per top 10). Mixing catalyst routes into
//! the ranking made it worse, so catalysts only attach to results here.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::{Embedding, parse_catalyst_text};
use crate::search::bm25::Bm25Index;
use crate::search::context::{CatalyzeResponse, CatalyzeResultItem, TopCatalyst};

/// Weight of the dense side; the lexical side gets `1 - DENSE_WEIGHT`.
pub(crate) const DENSE_WEIGHT: f64 = 0.4;
/// Notes taken from each side before fusing.
const CANDIDATES_PER_SIDE: usize = 30;
/// Chunks pulled from BM25 before rolling up to notes.
const BM25_CHUNK_DEPTH: usize = 400;

/// Question words dropped from the lexical query so they don't outvote topic
/// words ("how does the soil plan..." should match on soil, plan).
pub(crate) const QUERY_STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "about", "what", "is",
    "my", "me", "we", "our", "it", "its", "as", "at", "by", "from", "that", "this", "not", "vs",
    "over", "rather", "than", "you", "your", "how", "why", "when", "where", "who", "which", "do",
    "does", "did", "be", "are", "was", "were",
];

/// Best chunk of a note on one side: (chunk index, raw score).
type NoteHit = (i64, f64);

pub struct HybridSearch<'a> {
    db: &'a Database,
    service: &'a EmbeddingService,
}

impl<'a> HybridSearch<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self { db, service }
    }

    /// Search notes for `query` and return the top `limit`, each with its best
    /// passage and, when catalysts exist, the catalyst that best connects it to
    /// the query.
    pub fn search(&self, query: &str, limit: usize) -> Result<CatalyzeResponse> {
        let empty = CatalyzeResponse {
            results: Vec::new(),
            top_contributing_catalysts: Vec::new(),
        };
        if limit == 0 {
            return Ok(empty);
        }
        let start = Instant::now();
        let query_vec = self.service.embed_query(query)?;
        let embedding_ms = start.elapsed().as_secs_f64() * 1000.0;
        let dense = top_notes(dense_hits(self.db, &query_vec)?, CANDIDATES_PER_SIDE);
        let dense_ms = start.elapsed().as_secs_f64() * 1000.0 - embedding_ms;
        let lexical = top_notes(lexical_hits(self.db, query)?, CANDIDATES_PER_SIDE);
        let lexical_ms = start.elapsed().as_secs_f64() * 1000.0 - embedding_ms - dense_ms;
        let fused = fuse(&dense, &lexical, limit);
        if fused.is_empty() {
            return Ok(empty);
        }

        let mut results = Vec::with_capacity(fused.len());
        for (path, score, chunk_index) in &fused {
            let content = self
                .db
                .get_chunk_content(path, *chunk_index)?
                .unwrap_or_default();
            results.push(CatalyzeResultItem {
                file_path: path.clone(),
                content,
                similarity: (score * 1000.0).round() / 1000.0,
                via_catalyst_id: None,
                via_catalyst_text: None,
            });
        }
        let annotation_start = Instant::now();
        let top_contributing_catalysts = self.annotate(&query_vec, &mut results)?;
        if std::env::var_os("ENZYME_HYBRID_PROFILE").is_some() {
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            let annotation_ms = annotation_start.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "HYBRID_PROFILE embedding_ms={embedding_ms:.3} dense_ms={dense_ms:.3} lexical_ms={lexical_ms:.3} fetch_and_fuse_ms={:.3} annotation_ms={annotation_ms:.3} total_ms={total_ms:.3}",
                total_ms - embedding_ms - dense_ms - lexical_ms - annotation_ms
            );
        }
        Ok(CatalyzeResponse {
            results,
            top_contributing_catalysts,
        })
    }

    /// Attach to each result the linked catalyst that is closest to both the
    /// query and that note, and summarize which catalysts were attached.
    /// Scoring by the query alone gives every note the same catalyst whenever
    /// one catalyst links to them all, as it does in small vaults.
    fn annotate(
        &self,
        query_vec: &[f32],
        results: &mut [CatalyzeResultItem],
    ) -> Result<Vec<TopCatalyst>> {
        let catalysts: HashMap<String, _> = self
            .db
            .get_all_catalysts()?
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        if catalysts.is_empty() {
            return Ok(Vec::new());
        }
        // Only current catalysts may annotate; stale embeddings or links from
        // an earlier generation are skipped.
        let query_sims: HashMap<String, f64> = self
            .db
            .get_all_catalyst_embeddings()?
            .iter()
            .filter(|(id, _)| catalysts.contains_key(id))
            .map(|(id, vec)| {
                (
                    id.clone(),
                    Embedding::cosine_similarity(query_vec, vec) as f64,
                )
            })
            .collect();
        let paths: Vec<String> = results.iter().map(|r| r.file_path.clone()).collect();
        // path -> (catalyst id, fit to query and note, query similarity)
        let mut best: HashMap<String, (String, f64, f64)> = HashMap::new();
        for (catalyst_id, path, link_sim) in self.db.get_catalyst_links_for_paths(&paths)? {
            let Some(&query_sim) = query_sims.get(&catalyst_id) else {
                continue;
            };
            let fit = query_sim + link_sim;
            let slot = best
                .entry(path)
                .or_insert((catalyst_id.clone(), f64::MIN, query_sim));
            if fit > slot.1 {
                *slot = (catalyst_id, fit, query_sim);
            }
        }
        if best.is_empty() {
            return Ok(Vec::new());
        }
        let mut counts: HashMap<String, (usize, f64)> = HashMap::new();
        for result in results.iter_mut() {
            let Some((id, _, sim)) = best.get(&result.file_path) else {
                continue;
            };
            let Some(catalyst) = catalysts.get(id) else {
                continue;
            };
            result.via_catalyst_id = Some(id.clone());
            result.via_catalyst_text = Some(question_text(&catalyst.text));
            let entry = counts.entry(id.clone()).or_insert((0, *sim));
            entry.0 += 1;
        }
        let mut top: Vec<TopCatalyst> = counts
            .into_iter()
            .filter_map(|(id, (count, sim))| {
                catalysts.get(&id).map(|c| TopCatalyst {
                    id,
                    text: question_text(&c.text),
                    entity: c.entity.clone(),
                    topic_name: None,
                    relevance_score: (sim * 1000.0).round() / 1000.0,
                    contribution_count: count,
                })
            })
            .collect();
        top.sort_by(|a, b| {
            b.contribution_count
                .cmp(&a.contribution_count)
                .then(b.relevance_score.total_cmp(&a.relevance_score))
        });
        Ok(top)
    }
}

/// The catalyst question without its stored source-reference block.
fn question_text(text: &str) -> String {
    parse_catalyst_text(text)
        .map(|parsed| parsed.hypothesis)
        .unwrap_or_else(|_| text.trim().to_string())
}

/// Best dense-similarity chunk per note.
fn dense_hits(db: &Database, query_vec: &[f32]) -> Result<HashMap<String, NoteHit>> {
    let start = Instant::now();
    let hits = db.best_chunk_embedding_hits(query_vec)?;
    if std::env::var_os("ENZYME_HYBRID_PROFILE").is_some() {
        eprintln!(
            "HYBRID_DENSE stream_and_scan_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(hits)
}

/// Best lexical chunk per note. New vaults use persistent FTS5; older vaults
/// opened read-only fall back to the original in-memory BM25 path.
fn lexical_hits(db: &Database, query: &str) -> Result<HashMap<String, NoteHit>> {
    if db.has_chunk_fts()? {
        let mut hits: HashMap<String, NoteHit> = HashMap::new();
        for (path, chunk_index, score) in
            db.search_chunks_fts(&fts_query(query), BM25_CHUNK_DEPTH, CANDIDATES_PER_SIDE)?
        {
            let slot = hits.entry(path).or_insert((chunk_index, f64::MIN));
            if score > slot.1 {
                *slot = (chunk_index, score);
            }
        }
        return Ok(hits);
    }
    let start = Instant::now();
    let mut index = Bm25Index::new();
    let chunks = db.get_all_chunks_with_embeddings()?;
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    for (path, chunk_index, content) in chunks {
        let text = format!("{path} {path} {content}");
        index.add_document(format!("{chunk_index}\t{path}"), &text);
    }
    index.build();
    let build_ms = start.elapsed().as_secs_f64() * 1000.0 - load_ms;
    let mut hits: HashMap<String, NoteHit> = HashMap::new();
    for result in index.search(&lexical_query(query), BM25_CHUNK_DEPTH) {
        let Some((chunk, path)) = result.doc_id.split_once('\t') else {
            continue;
        };
        let chunk_index: i64 = chunk.parse().unwrap_or(0);
        let score = result.score as f64;
        let slot = hits
            .entry(path.to_string())
            .or_insert((chunk_index, f64::MIN));
        if score > slot.1 {
            *slot = (chunk_index, score);
        }
    }
    if std::env::var_os("ENZYME_HYBRID_PROFILE").is_some() {
        eprintln!(
            "HYBRID_LEXICAL load_ms={load_ms:.3} build_ms={build_ms:.3} search_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0 - load_ms - build_ms
        );
    }
    Ok(hits)
}

/// Translate arbitrary user text to quoted FTS5 atoms joined by OR. Quoting
/// every atom prevents operators, field selectors, and punctuation in the
/// user's query from becoming FTS syntax. ID-like spans stay adjacent phrases.
fn fts_query(query: &str) -> String {
    let lower = query.to_lowercase();
    let mut atoms = Vec::new();
    let mut span = String::new();
    let mut push_span = |span: &mut String| {
        if span.is_empty() {
            return;
        }
        let parts: Vec<&str> = span
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        let id_like = parts.len() > 1
            && parts.iter().any(|p| p.chars().any(|c| c.is_numeric()))
            && span.chars().any(|c| c == '-' || c == '_');
        if id_like {
            let phrase = parts.join(" ");
            if !atoms.contains(&phrase) {
                atoms.push(phrase);
            }
        } else {
            for part in parts {
                if part.len() > 1
                    && !QUERY_STOPWORDS.contains(&part)
                    && !atoms.iter().any(|a| a == part)
                {
                    atoms.push(part.to_string());
                }
            }
        }
        span.clear();
    };
    for c in lower.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            span.push(c);
        } else {
            push_span(&mut span);
        }
    }
    push_span(&mut span);
    atoms
        .into_iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Distinct topic words of the query, in order.
fn lexical_query(query: &str) -> String {
    let lower = query.to_lowercase();
    let mut terms: Vec<&str> = Vec::new();
    for term in lower.split(|c: char| !c.is_alphanumeric()) {
        if term.len() > 1 && !QUERY_STOPWORDS.contains(&term) && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms.join(" ")
}

/// The `k` highest-scoring notes, best first.
fn top_notes(hits: HashMap<String, NoteHit>, k: usize) -> Vec<(String, NoteHit)> {
    let mut notes: Vec<(String, NoteHit)> = hits.into_iter().collect();
    notes.sort_by(|a, b| b.1.1.total_cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
    notes.truncate(k);
    notes
}

/// Min-max normalize each side over its candidates, then blend. Returns
/// `(path, fused score, chunk index)` for the top `limit` notes; the passage
/// comes from whichever side contributed more to the note's score.
fn fuse(
    dense: &[(String, NoteHit)],
    lexical: &[(String, NoteHit)],
    limit: usize,
) -> Vec<(String, f64, i64)> {
    let dense_n = normalize(dense);
    let lexical_n = normalize(lexical);
    let mut paths: Vec<&String> = dense_n.keys().chain(lexical_n.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut fused: Vec<(String, f64, i64)> = paths
        .into_iter()
        .map(|path| {
            let d = dense_n.get(path).copied();
            let l = lexical_n.get(path).copied();
            let d_part = DENSE_WEIGHT * d.map_or(0.0, |(_, s)| s);
            let l_part = (1.0 - DENSE_WEIGHT) * l.map_or(0.0, |(_, s)| s);
            let chunk = match (d, l) {
                (Some((dc, _)), Some((lc, _))) => {
                    if d_part >= l_part {
                        dc
                    } else {
                        lc
                    }
                }
                (Some((dc, _)), None) => dc,
                (None, Some((lc, _))) => lc,
                (None, None) => 0,
            };
            (path.clone(), d_part + l_part, chunk)
        })
        .collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused.truncate(limit);
    fused
}

fn normalize(side: &[(String, NoteHit)]) -> HashMap<String, (i64, f64)> {
    let (lo, hi) = side
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), (_, (_, s))| {
            (lo.min(*s), hi.max(*s))
        });
    side.iter()
        .map(|(path, (chunk, s))| {
            let n = if hi > lo { (s - lo) / (hi - lo) } else { 1.0 };
            (path.clone(), (*chunk, n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(items: &[(&str, i64, f64)]) -> Vec<(String, NoteHit)> {
        items
            .iter()
            .map(|(p, c, s)| (p.to_string(), (*c, *s)))
            .collect()
    }

    #[test]
    fn fts_query_escapes_syntax_and_keeps_ids_as_phrases() {
        assert_eq!(
            fts_query("What happened to ENG-4902?"),
            "\"happened\" OR \"eng 4902\""
        );
        assert_eq!(
            fts_query("title:foo OR bar* \"baz\" (qux)"),
            "\"title\" OR \"foo\" OR \"bar\" OR \"baz\" OR \"qux\""
        );
        assert_eq!(fts_query("' OR NOT * : ( )"), "");
    }

    #[test]
    fn fuse_weights_lexical_above_dense_and_unions_both_sides() {
        let dense = side(&[("a.md", 0, 0.9), ("b.md", 1, 0.5)]);
        let lexical = side(&[("b.md", 2, 12.0), ("c.md", 0, 3.0)]);
        let fused = fuse(&dense, &lexical, 10);
        let order: Vec<&str> = fused.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(order, vec!["b.md", "a.md", "c.md"]);
        assert_eq!(
            fused[0].2, 2,
            "b.md takes the lexical chunk, the larger contribution"
        );
        assert!((fused[0].1 - 0.6).abs() < 1e-9);
        assert!((fused[1].1 - 0.4).abs() < 1e-9);
    }

    #[test]
    fn fuse_respects_limit_and_handles_one_empty_side() {
        let dense = side(&[("a.md", 0, 0.9), ("b.md", 0, 0.8), ("c.md", 0, 0.1)]);
        let fused = fuse(&dense, &[], 2);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "a.md");
    }

    #[test]
    fn lexical_query_drops_question_words() {
        assert_eq!(
            lexical_query("How does the garden's soil plan fit into the garden budget?"),
            "garden soil plan fit into budget"
        );
    }

    #[test]
    fn question_text_drops_the_source_block() {
        let stored = "Why did I keep the monolith?\n\n```enzyme-context\n[\"adrs/005.md\"]\n```";
        assert_eq!(question_text(stored), "Why did I keep the monolith?");
        assert_eq!(question_text("  Plain question?  "), "Plain question?");
    }

    #[test]
    fn top_notes_orders_by_score_then_path() {
        let mut hits = HashMap::new();
        hits.insert("b.md".to_string(), (0, 1.0));
        hits.insert("a.md".to_string(), (0, 1.0));
        hits.insert("c.md".to_string(), (0, 2.0));
        let top = top_notes(hits, 2);
        assert_eq!(top[0].0, "c.md");
        assert_eq!(top[1].0, "a.md");
    }
}
