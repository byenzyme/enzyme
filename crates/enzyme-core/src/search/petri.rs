//! Query-mode petri: rank catalyzed entities by how well their catalyst
//! questions match a query, by keyword and meaning, with no recency term.
//!
//! 1. Each catalyst with a cached embedding scores
//!    `DENSE_WEIGHT * minmax(cosine) + (1 - DENSE_WEIGHT) * bm25 / max(bm25)`.
//!    BM25 runs over the catalyst question plus its entity name, so a query
//!    that names a tag or folder matches every question filed under it.
//! 2. An entity scores its best catalyst.
//! 3. Entities the query names in full rank ahead of the rest, so a named
//!    scope comes first even when a neighbor's questions score higher.
//!
//! Recency is deliberately absent: blending it in let the busiest, most
//! recent entities outrank the scope a query was about.

use std::collections::{HashMap, HashSet};

use crate::models::{Catalyst, Embedding, Entity, catalyst_embedding_text};
use crate::search::bm25::Bm25Index;
use crate::search::context::match_query_entity_names;
use crate::search::hybrid::{DENSE_WEIGHT, QUERY_STOPWORDS};

/// One entity ranked for a query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryRankedEntity {
    /// Entity name as stored on its catalysts.
    pub name: String,
    /// Blended relevance of the entity's best catalyst, in [0, 1].
    pub relevance: f64,
    /// True when the query names this entity in full.
    pub named: bool,
    /// `(index into the input catalysts, blended relevance)`, best first.
    pub catalysts: Vec<(usize, f64)>,
}

/// Lexical terms for short texts (catalyst questions, entity names, queries):
/// lowercased alphanumeric runs longer than one character, minus question
/// words. A word followed by a number also yields a joined `word_number`
/// term, so an identifier such as `ops-112` outscores texts that share only
/// the word or only the number.
pub fn short_text_terms(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let raw: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let mut terms: Vec<String> = raw
        .iter()
        .filter(|t| t.chars().count() > 1 && !QUERY_STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect();
    terms.extend(
        raw.windows(2)
            .filter(|w| {
                w[0].chars().any(char::is_alphabetic) && w[1].chars().all(|c| c.is_ascii_digit())
            })
            .map(|w| format!("{}_{}", w[0], w[1])),
    );
    terms
}

/// Rank the entities that own catalysts for `query`. Catalysts without a
/// cached embedding are skipped. `entities` supplies the names a query can
/// name; pass the vault's entities. Returns every ranked entity: named ones
/// first, then by relevance, ties by name.
pub fn rank_entities_for_query(
    query: &str,
    query_vec: &[f32],
    catalysts: &[Catalyst],
    catalyst_vectors: &HashMap<&str, &Vec<f32>>,
    entities: &[Entity],
) -> Vec<QueryRankedEntity> {
    let candidates: Vec<(usize, &Vec<f32>)> = catalysts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| catalyst_vectors.get(c.id.as_str()).map(|v| (i, *v)))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let cosines: Vec<f64> = candidates
        .iter()
        .map(|(_, v)| Embedding::cosine_similarity(query_vec, v) as f64)
        .collect();
    let mut lexical = Bm25Index::new();
    for (i, _) in &candidates {
        let catalyst = &catalysts[*i];
        let text = format!(
            "{} {}",
            catalyst_embedding_text(&catalyst.text),
            catalyst.entity
        );
        lexical.add_document_terms(catalyst.id.clone(), &short_text_terms(&text));
    }
    lexical.build();
    let bm25: Vec<f64> = lexical
        .scores_for_terms(&short_text_terms(query))
        .into_iter()
        .map(f64::from)
        .collect();

    let cos_lo = cosines.iter().copied().fold(f64::INFINITY, f64::min);
    let cos_hi = cosines.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let bm25_hi = bm25.iter().copied().fold(0.0, f64::max);

    let mut by_entity: HashMap<&str, Vec<(usize, f64)>> = HashMap::new();
    for (k, (i, _)) in candidates.iter().enumerate() {
        let dense = if cos_hi > cos_lo {
            (cosines[k] - cos_lo) / (cos_hi - cos_lo)
        } else {
            1.0
        };
        let lexical = if bm25_hi > 0.0 {
            bm25[k] / bm25_hi
        } else {
            0.0
        };
        let score = DENSE_WEIGHT * dense + (1.0 - DENSE_WEIGHT) * lexical;
        by_entity
            .entry(catalysts[*i].entity.as_str())
            .or_default()
            .push((*i, score));
    }

    let named: HashSet<String> = match_query_entity_names(query, entities)
        .into_iter()
        .collect();
    let mut ranked: Vec<QueryRankedEntity> = by_entity
        .into_iter()
        .map(|(name, mut scored)| {
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            QueryRankedEntity {
                name: name.to_string(),
                relevance: scored[0].1,
                named: named.contains(&name.trim().to_lowercase()),
                catalysts: scored,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.named
            .cmp(&a.named)
            .then_with(|| b.relevance.total_cmp(&a.relevance))
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EntityTrends, EntityType};

    fn entity(name: &str, entity_type: EntityType) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type,
            frequency: 10,
            ranking_weight: 1.0,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends::default(),
            metadata: serde_json::json!({}),
        }
    }

    fn catalyst(id: &str, entity: &str, text: &str) -> Catalyst {
        Catalyst {
            id: id.to_string(),
            text: text.to_string(),
            entity: entity.to_string(),
            context: "default".to_string(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    /// The busiest entity's questions sit closest to every query embedding,
    /// as a blurry embedding plus recency once made them win every query.
    fn fixture() -> (Vec<Entity>, Vec<Catalyst>, Vec<(String, Vec<f32>)>) {
        let entities = vec![
            entity("journal", EntityType::Folder),
            entity("ops-112", EntityType::Link),
            entity("ops-113", EntityType::Link),
            entity("ada lovelace", EntityType::Link),
            entity("gardening", EntityType::Tag),
        ];
        let catalysts = vec![
            catalyst("j1", "journal", "What keeps the weekly plan slipping?"),
            catalyst("j2", "journal", "Which habits are still unproven?"),
            catalyst(
                "w1",
                "ops-112",
                "What made the circuit breaker a launch blocker?",
            ),
            catalyst(
                "w2",
                "ops-113",
                "Why was the kill switch rolled back twice?",
            ),
            catalyst(
                "p1",
                "ada lovelace",
                "What does Ada weigh when she pushes back?",
            ),
            catalyst(
                "g1",
                "gardening",
                "Which seedlings failed after the frost?\n\n```enzyme-context\n[\"notes/frost.md\"]\n```",
            ),
        ];
        let vectors = vec![
            ("j1".to_string(), vec![1.0, 0.0]),
            ("j2".to_string(), vec![0.95, 0.1]),
            ("w1".to_string(), vec![0.3, 1.0]),
            ("w2".to_string(), vec![0.6, 0.8]),
            ("p1".to_string(), vec![0.2, 1.0]),
            ("g1".to_string(), vec![0.4, 1.0]),
        ];
        (entities, catalysts, vectors)
    }

    fn rank(query: &str) -> Vec<QueryRankedEntity> {
        let (entities, catalysts, vectors) = fixture();
        let map: HashMap<&str, &Vec<f32>> =
            vectors.iter().map(|(id, v)| (id.as_str(), v)).collect();
        rank_entities_for_query(query, &[1.0, 0.0], &catalysts, &map, &entities)
    }

    #[test]
    fn named_entities_rank_first() {
        let ranked = rank("hand off ops-112 to a colleague");
        assert_eq!(ranked[0].name, "ops-112");
        assert!(ranked[0].named);
        assert!(!ranked.iter().any(|e| e.name == "ops-113" && e.named));

        assert_eq!(rank("get oriented on Ada Lovelace")[0].name, "ada lovelace");
        assert_eq!(rank("recent gardening themes")[0].name, "gardening");
    }

    #[test]
    fn keyword_matches_outrank_embedding_proximity() {
        // `journal` questions are closest to the query embedding; a question
        // sharing the query's words still ranks first.
        let ranked = rank("why was the kill switch rolled back");
        assert_eq!(ranked[0].name, "ops-113");
        assert!(!ranked[0].named, "a keyword match, not a named entity");
        // A stored source block is not matched as question text.
        assert_eq!(rank("notes")[0].name, "journal");
        // With no keyword or name signal the cosine order stands.
        assert_eq!(rank("zzz qqq")[0].name, "journal");
    }

    #[test]
    fn entities_take_their_best_catalyst_first() {
        let ranked = rank("unproven habits");
        assert_eq!(ranked[0].name, "journal");
        let first = ranked[0].catalysts[0];
        assert_eq!(first.0, 1, "the matching question leads its entity");
        assert!((ranked[0].relevance - first.1).abs() < 1e-12);
        assert!(ranked[0].catalysts.windows(2).all(|w| w[0].1 >= w[1].1));
    }

    #[test]
    fn catalysts_without_embeddings_are_skipped() {
        let (entities, catalysts, _) = fixture();
        assert!(
            rank_entities_for_query(
                "ops-112",
                &[1.0, 0.0],
                &catalysts,
                &HashMap::new(),
                &entities
            )
            .is_empty()
        );
    }

    #[test]
    fn short_text_terms_join_words_to_following_numbers() {
        assert_eq!(
            short_text_terms("What is OPS-112 about?"),
            vec!["ops", "112", "ops_112"]
        );
        assert_eq!(short_text_terms("plan 2 b"), vec!["plan", "plan_2"]);
        assert!(short_text_terms("2024 12").iter().all(|t| !t.contains('_')));
    }
}
