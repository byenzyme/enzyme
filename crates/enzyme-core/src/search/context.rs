use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::db::Database;
use crate::embedding::EmbeddingService;
use crate::models::{Catalyst, Embedding, Entity, catalyst_embedding_text};
use crate::pipeline::eras::{Era, apply_timeline_weights, cluster_eras_periodic};
use crate::pipeline::similarity::CATALYST_TOP_K;

pub const TARGET_PROJECTION_NOT_READY: &str = "target projection is not ready for active epoch";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EraKey {
    entity_id: i64,
    era_index: usize,
}

#[derive(Debug, Clone)]
struct ScoredChunk {
    file_path: String,
    chunk_index: i64,
    score: f64,
    best_catalyst_id: String,
    content: String,
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn has_name_boundaries(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|ch| !is_word_char(ch)) && after.is_none_or(|ch| !is_word_char(ch))
}

/// Detect complete known entity names in a query, preferring longer overlapping
/// names so a known "Will Ashby" does not degrade into a match for "Will".
fn match_query_entity_names(query: &str, entities: &[Entity]) -> Vec<String> {
    let query = query.to_lowercase();
    let mut names = entities
        .iter()
        .map(|entity| entity.name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
    names.dedup();

    let mut occupied = Vec::<(usize, usize)>::new();
    let mut matched = Vec::new();
    for name in names {
        let found = query.match_indices(&name).find_map(|(start, _)| {
            let end = start + name.len();
            (has_name_boundaries(&query, start, end)
                && occupied
                    .iter()
                    .all(|(used_start, used_end)| end <= *used_start || start >= *used_end))
            .then_some((start, end))
        });
        if let Some(span) = found {
            occupied.push(span);
            matched.push(name);
        }
    }
    matched
}

/// Route an explicit link-entity (person) query through that identity's own
/// catalysts when available. For other named entities, keep cosine order
/// within the owned and global pools while putting owned catalysts first.
fn prioritize_anchored_catalysts(
    global_ranking: &[(usize, f32)],
    catalysts: &[Catalyst],
    matched_entity_names: &HashSet<String>,
    matched_identity_names: &HashSet<String>,
    limit: usize,
) -> Vec<(usize, f32)> {
    let entity_name = |catalyst: &Catalyst| catalyst.entity.to_lowercase();
    if !matched_identity_names.is_empty() {
        let owned = global_ranking
            .iter()
            .filter(|(idx, _)| matched_identity_names.contains(&entity_name(&catalysts[*idx])))
            .copied()
            .take(limit)
            .collect::<Vec<_>>();
        // An explicit person query routes through that person's own catalyst
        // set when it exists. Other catalysts remain a fallback only when the
        // identity has no catalysts at all, rather than winning because they
        // happen to mention the person in higher-cosine generic prose.
        if !owned.is_empty() {
            return owned;
        }
    }
    if matched_entity_names.is_empty() {
        return global_ranking.iter().copied().take(limit).collect();
    }

    global_ranking
        .iter()
        .filter(|(idx, _)| matched_entity_names.contains(&entity_name(&catalysts[*idx])))
        .chain(
            global_ranking
                .iter()
                .filter(|(idx, _)| !matched_entity_names.contains(&entity_name(&catalysts[*idx]))),
        )
        .copied()
        .take(limit)
        .collect()
}

fn era_index_for_timestamp(eras: &[Era], timestamp: i64) -> Option<usize> {
    let month = chrono::DateTime::from_timestamp_millis(timestamp)?
        .format("%Y-%m")
        .to_string();
    eras.iter()
        .position(|era| month.as_str() >= era.start.as_str() && month.as_str() <= era.end.as_str())
}

fn build_anchored_era_index(
    entities: &[&Entity],
    occurrence_dates: &[(i64, String, i64)],
    era_budget: usize,
) -> (Vec<EraKey>, HashMap<String, Vec<EraKey>>) {
    let entities = entities
        .iter()
        .filter_map(|entity| entity.id.map(|id| (id, *entity)))
        .collect::<Vec<_>>();
    let mut era_order = Vec::new();
    let mut eras_by_entity = HashMap::<i64, Vec<Era>>::new();

    for (position, (entity_id, entity)) in entities.iter().enumerate() {
        let budget = if entities.is_empty() {
            0
        } else {
            era_budget / entities.len() + usize::from(position < era_budget % entities.len())
        };
        if budget == 0 {
            continue;
        }
        let mut eras = cluster_eras_periodic(&entity.trends.timeline_buckets, 3, budget);
        apply_timeline_weights(&mut eras, &entity.trends.timeline_weights);
        era_order.extend((0..eras.len()).map(|era_index| EraKey {
            entity_id: *entity_id,
            era_index,
        }));
        eras_by_entity.insert(*entity_id, eras);
    }

    let mut path_eras = HashMap::<String, Vec<EraKey>>::new();
    for (entity_id, file_path, timestamp) in occurrence_dates {
        let Some(eras) = eras_by_entity.get(entity_id) else {
            continue;
        };
        let Some(era_index) = era_index_for_timestamp(eras, *timestamp) else {
            continue;
        };
        let key = EraKey {
            entity_id: *entity_id,
            era_index,
        };
        let keys = path_eras.entry(file_path.clone()).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    (era_order, path_eras)
}

/// Seed the best-scoring chunk for every era, then fill from the weighted
/// ranking. The input must already be sorted by descending score.
fn prioritize_era_chunks(
    scored: Vec<ScoredChunk>,
    era_order: &[EraKey],
    path_eras: &HashMap<String, Vec<EraKey>>,
) -> Vec<ScoredChunk> {
    let mut selected = Vec::with_capacity(scored.len());
    let mut used = HashSet::<(String, i64)>::new();

    for era in era_order {
        if let Some(chunk) = scored.iter().find(|chunk| {
            !used.contains(&(chunk.file_path.clone(), chunk.chunk_index))
                && path_eras
                    .get(&chunk.file_path)
                    .is_some_and(|keys| keys.contains(era))
        }) {
            used.insert((chunk.file_path.clone(), chunk.chunk_index));
            selected.push(chunk);
        }
    }

    for chunk in &scored {
        if used.insert((chunk.file_path.clone(), chunk.chunk_index)) {
            selected.push(chunk);
        }
    }

    selected.into_iter().cloned().collect()
}

/// Structured response from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResponse {
    pub results: Vec<CatalyzeResultItem>,
    pub top_contributing_catalysts: Vec<TopCatalyst>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingBackend;
    use crate::models::{EntityTrends, EntityType};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ActivatingBackend {
        db_path: PathBuf,
        calls: Arc<AtomicUsize>,
        change_epoch: bool,
    }

    impl EmbeddingBackend for ActivatingBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let db = Database::open(&self.db_path)?;
                let token = db.begin_continuous_projection()?;
                if self.change_epoch {
                    db.execute_write(
                        "UPDATE catalyst_epoch_state SET active_epoch_id='new-epoch' WHERE singleton=1",
                        vec![],
                    )?;
                }
                db.finish_continuous_projection(&token)?;
            }
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }

        fn dimension(&self) -> usize {
            2
        }

        fn name(&self) -> &str {
            "activating-test"
        }
    }

    #[test]
    fn catalyst_search_retries_if_epoch_activates_between_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("search.db");
        let db = Database::open(&db_path).unwrap();
        db.insert_catalysts(&[Catalyst {
            id: "cat".into(),
            text: "question".into(),
            entity: "entity".into(),
            context: "default".into(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
        db.upsert_catalyst_embeddings(&[("cat", &[1.0_f32, 0.0][..])])
            .unwrap();
        db.execute_write(
            "UPDATE catalyst_epoch_state SET active_epoch_id='old-epoch' WHERE singleton=1",
            vec![],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: calls.clone(),
            change_epoch: true,
        }));

        ContextSearch::new(&db, &service)
            .catalyze_search("query", 5)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn catalyst_search_retries_if_same_epoch_projection_changes_between_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("search.db");
        let db = Database::open(&db_path).unwrap();
        db.insert_catalysts(&[Catalyst {
            id: "cat".into(),
            text: "question".into(),
            entity: "entity".into(),
            context: "default".into(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
        db.upsert_catalyst_embeddings(&[("cat", &[1.0_f32, 0.0][..])])
            .unwrap();
        db.execute_write(
            "UPDATE catalyst_epoch_state SET active_epoch_id='same-epoch' WHERE singleton=1",
            vec![],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: calls.clone(),
            change_epoch: false,
        }));

        ContextSearch::new(&db, &service)
            .catalyze_search("query", 5)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn target_search_reports_missing_active_epoch_projection_as_retriable() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("source.db");
        let source = Database::open(&db_path).unwrap();
        let target = Database::open(temp.path().join("target.db")).unwrap();
        source
            .insert_catalysts(&[Catalyst {
                id: "cat".into(),
                text: "question".into(),
                entity: "entity".into(),
                context: "default".into(),
                generated_at: chrono::Utc::now(),
                metadata: serde_json::json!({}),
            }])
            .unwrap();
        source
            .execute_write(
                "UPDATE catalyst_epoch_state SET active_epoch_id='new-epoch' WHERE singleton=1",
                vec![],
            )
            .unwrap();
        let service = EmbeddingService::new(Box::new(ActivatingBackend {
            db_path,
            calls: Arc::new(AtomicUsize::new(1)),
            change_epoch: false,
        }));

        let error = ContextSearch::with_target(&source, &target, &service)
            .catalyze_search("query", 5)
            .unwrap_err();
        assert!(error.to_string().contains(TARGET_PROJECTION_NOT_READY));
    }

    fn entity(name: &str) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type: EntityType::Link,
            frequency: 1,
            ranking_weight: 1.0,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends::default(),
            metadata: serde_json::json!({}),
        }
    }

    fn catalyst(id: &str, entity: &str) -> Catalyst {
        Catalyst {
            id: id.to_string(),
            text: format!("question {id}"),
            entity: entity.to_string(),
            context: "default".to_string(),
            generated_at: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    fn scored_chunk(path: &str, score: f64) -> ScoredChunk {
        ScoredChunk {
            file_path: path.to_string(),
            chunk_index: 0,
            score,
            best_catalyst_id: "cat".to_string(),
            content: String::new(),
        }
    }

    #[test]
    fn entity_matching_is_case_insensitive_bounded_and_longest_first() {
        let entities = vec![
            entity("Will"),
            entity("Will Ashby"),
            entity("Ashby"),
            entity("Books"),
            entity("Art"),
        ];

        assert_eq!(
            match_query_entity_names("What changed with WILL ASHBY in books?", &entities),
            vec!["will ashby", "books"]
        );
        assert!(match_query_entity_names("articles about cartography", &entities).is_empty());
    }

    #[test]
    fn anchored_catalysts_precede_higher_cosine_global_catalysts() {
        let catalysts = vec![
            catalyst("global-high", "Other"),
            catalyst("anchor-high", "Will Ashby"),
            catalyst("anchor-low", "Will Ashby"),
            catalyst("global-low", "Another"),
        ];
        let global = vec![(0, 0.9), (1, 0.8), (2, 0.7), (3, 0.6)];
        let matched = HashSet::from(["will ashby".to_string()]);

        assert_eq!(
            prioritize_anchored_catalysts(&global, &catalysts, &matched, &HashSet::new(), 3),
            vec![(1, 0.8), (2, 0.7), (0, 0.9)]
        );
    }

    #[test]
    fn person_routes_only_through_owned_catalysts_when_available() {
        let catalysts = vec![
            catalyst("mention-high", "Other"),
            catalyst("own-high", "Will Ashby"),
            catalyst("own-low", "Will Ashby"),
        ];
        let global = vec![(0, 0.99), (1, 0.7), (2, 0.4)];
        let matched = HashSet::from(["will ashby".to_string()]);

        assert_eq!(
            prioritize_anchored_catalysts(&global, &catalysts, &matched, &matched, 10),
            vec![(1, 0.7), (2, 0.4)]
        );
    }

    #[test]
    fn era_stratified_selection_seeds_each_era_then_fills_by_score() {
        let old = EraKey {
            entity_id: 1,
            era_index: 0,
        };
        let recent = EraKey {
            entity_id: 1,
            era_index: 1,
        };
        let scored = vec![
            scored_chunk("recent-best", 0.9),
            scored_chunk("old-best", 0.8),
            scored_chunk("old-runner-up", 0.7),
            scored_chunk("global", 0.6),
        ];
        let path_eras = HashMap::from([
            ("recent-best".to_string(), vec![recent]),
            ("old-best".to_string(), vec![old]),
            ("old-runner-up".to_string(), vec![old]),
        ]);

        let selected = prioritize_era_chunks(scored, &[old, recent], &path_eras);
        assert_eq!(
            selected
                .iter()
                .take(3)
                .map(|chunk| chunk.file_path.as_str())
                .collect::<Vec<_>>(),
            vec!["old-best", "recent-best", "old-runner-up"]
        );
    }

    #[test]
    fn unanchored_catalyst_selection_preserves_global_fallthrough() {
        let catalysts = vec![
            catalyst("first", "One"),
            catalyst("second", "Two"),
            catalyst("third", "Three"),
        ];
        let global = vec![(2, 0.9), (0, 0.8), (1, 0.7)];

        assert_eq!(
            prioritize_anchored_catalysts(&global, &catalysts, &HashSet::new(), &HashSet::new(), 2,),
            vec![(2, 0.9), (0, 0.8)]
        );
    }
}

/// A single result from catalyst-based search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalyzeResultItem {
    pub file_path: String,
    pub content: String,
    pub similarity: f64,
    /// Catalyst bridge that contributed the strongest score for this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_catalyst_id: Option<String>,
    /// Portable catalyst text, including sources used as generation context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_catalyst_text: Option<String>,
}

/// A catalyst that contributed to search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopCatalyst {
    pub id: String,
    pub text: String,
    pub entity: String,
    pub topic_name: Option<String>,
    pub relevance_score: f64,
    pub contribution_count: usize,
}

/// A single result from direct (non-catalyst) search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectSearchResult {
    pub file_path: String,
    pub chunk_index: i64,
    pub content: String,
    pub similarity: f64,
}

/// Catalyst-based search over pre-computed similarities.
///
/// Supports an optional **target** database for split-DB mode (`enzyme catalyze --target`):
/// - Catalysts come from the **source** DB
/// - Catalyst embeddings, similarities, and chunk content come from the **target** DB
pub struct ContextSearch<'a> {
    /// Source vault: catalysts live here.
    db: &'a Database,
    /// Target DB for embeddings/similarities/content. Falls back to `db` when `None`.
    target_db: Option<&'a Database>,
    service: &'a EmbeddingService,
}

impl<'a> ContextSearch<'a> {
    pub fn new(db: &'a Database, service: &'a EmbeddingService) -> Self {
        Self {
            db,
            target_db: None,
            service,
        }
    }

    /// Create a split-DB search: catalysts from `source_db`, everything else from `target_db`.
    pub fn with_target(
        source_db: &'a Database,
        target_db: &'a Database,
        service: &'a EmbeddingService,
    ) -> Self {
        Self {
            db: source_db,
            target_db: Some(target_db),
            service,
        }
    }

    /// Catalyst-based search (used by `enzyme catalyze`).
    ///
    /// 1. Embed query
    /// 2. Find top relevant catalysts (from cached embeddings)
    /// 3. For each relevant catalyst, get pre-computed chunk similarities
    /// 4. Aggregate, boost by multi-catalyst and recency, deduplicate, sort
    /// 5. Compute top contributing catalysts with contribution counts
    pub fn catalyze_search(&self, query: &str, limit: usize) -> Result<CatalyzeResponse> {
        // Source activation spans several materialized tables. Read an epoch
        // fence before and after the autocommit reads; if activation landed in
        // between, discard the mixed attempt and retry against the new epoch.
        // If activation lands after the final fence, this is a valid old-epoch
        // snapshot (the read logically preceded activation).
        for _ in 0..4 {
            let source_projection_fence = self.db.continuous_projection_fence()?;
            let result_db = self.target_db.unwrap_or(self.db);
            let result_projection_fence = result_db.continuous_projection_fence()?;
            if source_projection_fence.dirty || result_projection_fence.dirty {
                std::thread::yield_now();
                continue;
            }
            let source_epoch = self.db.get_epoch_state()?.active_epoch_id;
            let response = self.catalyze_search_at_epoch(query, limit, source_epoch.as_deref())?;
            if self.db.get_epoch_state()?.active_epoch_id == source_epoch
                && self.db.continuous_projection_fence()? == source_projection_fence
                && result_db.continuous_projection_fence()? == result_projection_fence
            {
                return Ok(response);
            }
        }
        anyhow::bail!(
            "catalyst epoch or document projection changed during search; retry the query"
        )
    }

    fn catalyze_search_at_epoch(
        &self,
        query: &str,
        limit: usize,
        source_epoch: Option<&str>,
    ) -> Result<CatalyzeResponse> {
        let query_vec = self.service.embed_query(query)?;

        // Get all catalysts
        let catalysts = self.db.get_all_catalysts()?;
        if catalysts.is_empty() {
            return Ok(CatalyzeResponse {
                results: Vec::new(),
                top_contributing_catalysts: Vec::new(),
            });
        }
        let entities = self.db.get_all_entities()?;
        let matched_entity_names = match_query_entity_names(query, &entities);
        let matched_entity_name_set = matched_entity_names.iter().cloned().collect::<HashSet<_>>();
        let matched_entities = matched_entity_names
            .iter()
            .flat_map(|name| {
                entities
                    .iter()
                    .filter(move |entity| entity.name.eq_ignore_ascii_case(name))
            })
            .collect::<Vec<_>>();
        let matched_identity_name_set = matched_entities
            .iter()
            .filter(|entity| entity.entity_type == crate::models::EntityType::Link)
            .map(|entity| entity.name.to_lowercase())
            .collect::<HashSet<_>>();
        let anchored = !matched_entity_name_set.is_empty();

        // Load cached catalyst embeddings from DB, falling back to re-embedding
        let emb_db = self.target_db.unwrap_or(self.db);
        let cached = if self.target_db.is_some() {
            let epoch_id = source_epoch
                .ok_or_else(|| anyhow::anyhow!("source has no active catalyst epoch"))?;
            let cached = emb_db.get_epoch_catalyst_embeddings(epoch_id)?;
            anyhow::ensure!(
                !cached.is_empty(),
                "{TARGET_PROJECTION_NOT_READY}: {epoch_id}"
            );
            cached
        } else {
            emb_db.get_all_catalyst_embeddings()?
        };
        let cat_embeddings = if !cached.is_empty() {
            let cache_map: HashMap<&str, &Vec<f32>> =
                cached.iter().map(|(id, vec)| (id.as_str(), vec)).collect();

            let mut vecs = Vec::with_capacity(catalysts.len());
            let mut all_hit = true;
            for cat in &catalysts {
                if let Some(vec) = cache_map.get(cat.id.as_str()) {
                    vecs.push((*vec).clone());
                } else {
                    all_hit = false;
                    break;
                }
            }

            if all_hit {
                debug!("using cached catalyst embeddings ({} vectors)", vecs.len());
                vecs
            } else {
                debug!(
                    "catalyst cache stale, re-embedding {} catalysts",
                    catalysts.len()
                );
                let embedding_texts: Vec<String> = catalysts
                    .iter()
                    .map(|c| catalyst_embedding_text(&c.text))
                    .collect();
                let cat_texts: Vec<&str> = embedding_texts.iter().map(String::as_str).collect();
                self.service.embed_documents(&cat_texts)?
            }
        } else {
            debug!("no catalyst cache, embedding {} catalysts", catalysts.len());
            let embedding_texts: Vec<String> = catalysts
                .iter()
                .map(|c| catalyst_embedding_text(&c.text))
                .collect();
            let cat_texts: Vec<&str> = embedding_texts.iter().map(String::as_str).collect();
            self.service.embed_documents(&cat_texts)?
        };

        // Find top relevant catalysts (top 10 — over-retrieve for better coverage)
        let max_catalysts = 10usize;
        let mut global_cat_sims: Vec<(usize, f32)> = cat_embeddings
            .iter()
            .enumerate()
            .map(|(i, vec)| (i, Embedding::cosine_similarity(&query_vec, vec)))
            .collect();
        global_cat_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let cat_sims = prioritize_anchored_catalysts(
            &global_cat_sims,
            &catalysts,
            &matched_entity_name_set,
            &matched_identity_name_set,
            max_catalysts,
        );

        // Build catalyst metadata lookup for the top catalysts
        let catalyst_meta: HashMap<String, (String, String, f64)> = cat_sims
            .iter()
            .map(|(idx, sim)| {
                let catalyst = &catalysts[*idx];
                (
                    catalyst.id.clone(),
                    (catalyst.text.clone(), catalyst.entity.clone(), *sim as f64),
                )
            })
            .collect();

        // For each relevant catalyst, get pre-computed chunk similarities.
        // Weight each chunk's similarity by how relevant its catalyst is to the query.
        // Track per-chunk: (max weighted score, catalyst_count, best_catalyst_id)
        struct ChunkScore {
            max_weighted_score: f64,
            catalyst_count: usize,
            best_catalyst_id: String,
        }

        let mut chunk_scores: HashMap<String, ChunkScore> = HashMap::new();
        let context_per_catalyst = if anchored {
            CATALYST_TOP_K
        } else {
            (limit * 3 / max_catalysts.max(1)).max(10)
        };

        let sim_repo = self.target_db.unwrap_or(self.db);

        // Batch-load similarities for all top catalysts in one query
        let top_catalyst_ids: Vec<&str> = cat_sims
            .iter()
            .map(|(idx, _)| catalysts[*idx].id.as_str())
            .collect();
        let all_sims = if self.target_db.is_some() {
            sim_repo.get_epoch_similarities_for_catalysts(
                source_epoch.expect("checked active source epoch"),
                &top_catalyst_ids,
                context_per_catalyst,
            )?
        } else {
            sim_repo.get_similarities_for_catalysts(&top_catalyst_ids, context_per_catalyst)?
        };

        let mut sims_by_catalyst: HashMap<&str, Vec<&crate::models::CatalystSimilarity>> =
            HashMap::new();
        for sim in &all_sims {
            sims_by_catalyst
                .entry(sim.catalyst_id.as_str())
                .or_default()
                .push(sim);
        }

        for (cat_idx, cat_sim) in &cat_sims {
            let catalyst = &catalysts[*cat_idx];
            let relevance = *cat_sim as f64;

            if let Some(sims) = sims_by_catalyst.get(catalyst.id.as_str()) {
                for sim in sims {
                    let key = format!("{}:{}", sim.file_path, sim.chunk_index);
                    let weighted_score = sim.similarity * relevance;
                    let entry = chunk_scores.entry(key).or_insert(ChunkScore {
                        // Anchored catalysts can have a negative query cosine;
                        // the first real contribution must still win and retain
                        // its catalyst id instead of leaving an empty bridge.
                        max_weighted_score: f64::NEG_INFINITY,
                        catalyst_count: 0,
                        best_catalyst_id: String::new(),
                    });
                    if weighted_score > entry.max_weighted_score {
                        entry.max_weighted_score = weighted_score;
                        entry.best_catalyst_id = catalyst.id.clone();
                    }
                    entry.catalyst_count += 1;
                }
            }
        }

        let mut scored: Vec<ScoredChunk> = Vec::new();

        // Unanchored queries retain the existing modified_at recency behavior.
        // Anchored queries use effective dates only for timeline stratification.
        let unique_paths: Vec<&str> = chunk_scores
            .keys()
            .filter_map(|key| key.rsplitn(2, ':').last())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let modified_at_map = if anchored {
            HashMap::new()
        } else {
            sim_repo
                .get_modified_at_batch(&unique_paths)
                .unwrap_or_default()
        };
        let now = chrono::Utc::now().timestamp_millis();

        for (key, cs) in &chunk_scores {
            let parts: Vec<&str> = key.rsplitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let chunk_index: i64 = parts[0].parse().unwrap_or(0);
            let file_path = parts[1].to_string();

            // Multi-catalyst boost
            let multi_boost = 1.0 + (cs.catalyst_count as f64 - 1.0) * 0.2;

            // Recency boost (from document modified_at) remains byte-for-byte
            // equivalent for unanchored broad-topic queries.
            let recency_boost = if anchored {
                1.0
            } else if let Some(&modified_at) = modified_at_map.get(&file_path) {
                1.0 + crate::models::activity_decay(now, modified_at)
            } else {
                1.0
            };

            scored.push(ScoredChunk {
                file_path,
                chunk_index,
                score: cs.max_weighted_score * multi_boost * recency_boost,
                best_catalyst_id: cs.best_catalyst_id.clone(),
                content: String::new(), // filled below
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if anchored {
            let entity_ids = matched_entities
                .iter()
                .filter_map(|entity| entity.id)
                .collect::<Vec<_>>();
            let occurrence_dates = self.db.get_entity_occurrence_effective_dates(&entity_ids)?;
            let (era_order, path_eras) =
                build_anchored_era_index(&matched_entities, &occurrence_dates, limit);
            scored = prioritize_era_chunks(scored, &era_order, &path_eras);
        }

        // Fetch content for top limit*3 candidates BEFORE dedup/truncation
        let fetch_count = (limit * 3).min(scored.len());
        for s in scored.iter_mut().take(fetch_count) {
            s.content = sim_repo
                .get_chunk_content(&s.file_path, s.chunk_index)?
                .unwrap_or_default();
        }
        // Drop candidates without content (beyond fetch_count)
        scored.truncate(fetch_count);

        // Content-based deduplication: keep highest-scoring chunk per normalized content
        {
            let mut seen: HashMap<String, usize> = HashMap::new();
            let mut keep = vec![false; scored.len()];
            for (i, s) in scored.iter().enumerate() {
                let normalized: String = s.content.split_whitespace().collect::<Vec<_>>().join(" ");
                match seen.get(&normalized) {
                    Some(&existing_idx) => {
                        // Already have this content — keep whichever has higher score
                        if s.score > scored[existing_idx].score {
                            keep[existing_idx] = false;
                            keep[i] = true;
                            seen.insert(normalized, i);
                        }
                        // else: existing is better, skip this one
                    }
                    None => {
                        keep[i] = true;
                        seen.insert(normalized, i);
                    }
                }
            }
            let mut idx = 0;
            scored.retain(|_| {
                let k = keep[idx];
                idx += 1;
                k
            });
        }

        // Now truncate to requested limit
        scored.truncate(limit);

        // Count catalyst contributions across final results
        let mut contribution_counts: HashMap<&str, usize> = HashMap::new();
        for s in &scored {
            *contribution_counts.entry(&s.best_catalyst_id).or_insert(0) += 1;
        }

        // Build top contributing catalysts (top 3 by contribution count)
        let mut top_catalysts: Vec<TopCatalyst> = catalyst_meta
            .iter()
            .filter_map(|(id, (text, entity, relevance_score))| {
                let count = contribution_counts.get(id.as_str()).copied().unwrap_or(0);
                if count == 0 {
                    return None;
                }
                Some(TopCatalyst {
                    id: id.clone(),
                    text: text.clone(),
                    entity: entity.clone(),
                    topic_name: None,
                    relevance_score: (*relevance_score * 1000.0).round() / 1000.0,
                    contribution_count: count,
                })
            })
            .collect();
        top_catalysts.sort_by(|a, b| {
            b.contribution_count
                .cmp(&a.contribution_count)
                .then_with(|| {
                    b.relevance_score
                        .partial_cmp(&a.relevance_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        // Build result items using already-fetched content
        let results: Vec<CatalyzeResultItem> = scored
            .iter()
            .map(|s| CatalyzeResultItem {
                file_path: s.file_path.clone(),
                content: s.content.clone(),
                similarity: (s.score * 1000.0).round() / 1000.0,
                via_catalyst_id: Some(s.best_catalyst_id.clone()),
                via_catalyst_text: catalyst_meta
                    .get(&s.best_catalyst_id)
                    .map(|(text, _, _)| text.clone()),
            })
            .collect();

        Ok(CatalyzeResponse {
            results,
            top_contributing_catalysts: top_catalysts,
        })
    }

    /// Direct search: pure cosine similarity between query and all chunk embeddings.
    ///
    /// Skips catalyst indirection entirely — baseline for benchmarking.
    pub fn direct_search(&self, query: &str, limit: usize) -> Result<Vec<DirectSearchResult>> {
        let query_vec = self.service.embed_query(query)?;

        let repo = self.target_db.unwrap_or(self.db);
        let all_chunks = repo.get_all_chunk_embeddings()?;

        if all_chunks.is_empty() {
            return Ok(Vec::new());
        }

        // Score all chunks by cosine similarity
        let mut scored: Vec<(usize, f32)> = all_chunks
            .iter()
            .enumerate()
            .map(|(i, (_fp, _ci, vec))| (i, Embedding::cosine_similarity(&query_vec, vec)))
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        // Fetch content for top results
        let mut results = Vec::with_capacity(scored.len());
        for (idx, sim) in &scored {
            let (ref file_path, chunk_index, _) = all_chunks[*idx];
            let content = repo
                .get_chunk_content(file_path, chunk_index as i64)?
                .unwrap_or_default();
            results.push(DirectSearchResult {
                file_path: file_path.clone(),
                chunk_index: chunk_index as i64,
                content,
                similarity: (*sim as f64 * 1000.0).round() / 1000.0,
            });
        }

        Ok(results)
    }

    /// Direct search with externally-provided embeddings (for cross-model benchmarking).
    ///
    /// Takes pre-computed chunk embeddings and a query vector, bypassing the
    /// service's backend entirely. Used when comparing ESE vs Arctic on the same corpus.
    pub fn direct_search_with_embeddings(
        query_vec: &[f32],
        chunk_embeddings: &[(String, u32, Vec<f32>)],
        db: &Database,
        limit: usize,
    ) -> Result<Vec<DirectSearchResult>> {
        if chunk_embeddings.is_empty() {
            return Ok(Vec::new());
        }

        let mut scored: Vec<(usize, f32)> = chunk_embeddings
            .iter()
            .enumerate()
            .map(|(i, (_fp, _ci, vec))| (i, Embedding::cosine_similarity(query_vec, vec)))
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let mut results = Vec::with_capacity(scored.len());
        for (idx, sim) in &scored {
            let (ref file_path, chunk_index, _) = chunk_embeddings[*idx];
            let content = db
                .get_chunk_content(file_path, chunk_index as i64)?
                .unwrap_or_default();
            results.push(DirectSearchResult {
                file_path: file_path.clone(),
                chunk_index: chunk_index as i64,
                content,
                similarity: (*sim as f64 * 1000.0).round() / 1000.0,
            });
        }

        Ok(results)
    }
}
