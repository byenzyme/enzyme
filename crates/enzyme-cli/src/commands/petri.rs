use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tracing::debug;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::models::{Embedding, Entity, Catalyst, EntityType};
use enzyme_core::pipeline::entity_map::EntityMap;

/// Build the petri JSON value from the vault database.
/// Assumes the DB exists — callers must guard this.
pub fn build_petri_json(
    vault_path: &Path,
    top: usize,
    query: Option<&str>,
    catalyst_budget: Option<usize>,
) -> Result<serde_json::Value> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;

    // Load config for expandable folder preferences
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let prefs = config.parse_preferences();

    // Select entities: query-aware or recency-based
    let mut entity_relevances: HashMap<String, f64> = HashMap::new();
    let mut catalyst_relevances: HashMap<String, f64> = HashMap::new();

    let entities = if let Some(query_text) = query {
        match build_query_aware(
            &paths,
            &db,
            top,
            query_text,
            catalyst_budget.unwrap_or(3),
            &mut entity_relevances,
            &mut catalyst_relevances,
        ) {
            Ok(ents) if !ents.is_empty() => ents,
            Ok(_) => {
                debug!("query-aware petri returned no results, falling back to recency");
                entity_relevances.clear();
                catalyst_relevances.clear();
                build_recency_based(&db, top)?
            }
            Err(e) => {
                debug!("query-aware petri failed: {}, falling back to recency", e);
                build_recency_based(&db, top)?
            }
        }
    } else {
        build_recency_based(&db, top)?
    };

    // ── Fold page-entity links under their parent folder ───────────────
    // Use EntityMap to detect which links are subsumed by folders.
    // Their catalysts get nested under the folder instead of appearing
    // as separate top-level entities.
    let page_entity_map: HashMap<String, String> = EntityMap::build(&db, 20)
        .map(|em| {
            let mut map = HashMap::new();
            for fg in &em.folders {
                for pe in &fg.page_entities {
                    map.insert(pe.clone(), fg.entity.name.clone());
                }
            }
            map
        })
        .unwrap_or_default();

    // Partition: top-level entities vs subsumed links grouped by folder
    let mut top_level: Vec<&(Entity, Vec<Catalyst>)> = Vec::new();
    let mut folder_children: HashMap<String, Vec<&(Entity, Vec<Catalyst>)>> = HashMap::new();

    for entry in &entities {
        if entry.0.entity_type == EntityType::Link {
            if let Some(folder_name) = page_entity_map.get(&entry.0.name) {
                folder_children.entry(folder_name.clone()).or_default().push(entry);
                continue;
            }
        }
        top_level.push(entry);
    }

    // Ensure folders with children appear in top_level even if they weren't selected
    let top_level_folder_names: std::collections::HashSet<&str> = top_level.iter()
        .filter(|(e, _)| e.entity_type == EntityType::Folder)
        .map(|(e, _)| e.name.as_str())
        .collect();

    // Build JSON
    let mut entity_list = Vec::new();

    for (entity, catalysts) in &top_level {
        let mut entry = build_entity_json(entity, catalysts, &entity_relevances, &catalyst_relevances);

        // Enrich expandable folders with sampled children (from config)
        if entity.entity_type == EntityType::Folder {
            if let Some(configured_children) = prefs.expandable_folders.get(&entity.name) {
                let (children, total_count) =
                    db.get_folder_children_by_references(&entity.name, 10)?;

                let sampled: Vec<serde_json::Value> = children
                    .iter()
                    .map(|c| {
                        json!({
                            "entity": format!("[[{}]]", c.name),
                            "reference_count": c.reference_count,
                            "last_seen": c.last_seen,
                        })
                    })
                    .collect();

                let catalyzed: Vec<String> = configured_children
                    .iter()
                    .filter(|child| {
                        db.get_catalysts_for_entity(&child.name)
                            .map(|cats| !cats.is_empty())
                            .unwrap_or(false)
                    })
                    .map(|child| format!("[[{}]]", child.name))
                    .collect();

                let sampled_count = sampled.len() as i64;
                let obj = entry.as_object_mut().unwrap();
                obj.insert("expandable".to_string(), json!(true));
                obj.insert("total_children".to_string(), json!(total_count));
                obj.insert("sampled_children".to_string(), json!(sampled));
                obj.insert("catalyzed_children".to_string(), json!(catalyzed));
                obj.insert(
                    "additional_children_available".to_string(),
                    json!(total_count - sampled_count),
                );
            }

            // Nest subsumed page-entity children with their catalysts
            if let Some(children) = folder_children.get(&entity.name) {
                let nested: Vec<serde_json::Value> = children.iter()
                    .map(|(child_entity, child_catalysts)| {
                        build_entity_json(child_entity, child_catalysts, &entity_relevances, &catalyst_relevances)
                    })
                    .collect();
                if !nested.is_empty() {
                    entry.as_object_mut().unwrap()
                        .insert("page_entity_children".to_string(), json!(nested));
                }
            }
        }

        entity_list.push(entry);
    }

    // Add synthetic folder entries for folders that have children but weren't in the entity list
    for (folder_name, children) in &folder_children {
        if top_level_folder_names.contains(folder_name.as_str()) {
            continue; // already handled above
        }
        let nested: Vec<serde_json::Value> = children.iter()
            .map(|(child_entity, child_catalysts)| {
                build_entity_json(child_entity, child_catalysts, &entity_relevances, &catalyst_relevances)
            })
            .collect();
        let entry = json!({
            "name": folder_name,
            "type": "folder",
            "catalysts": [],
            "page_entity_children": nested,
        });
        entity_list.push(entry);
    }

    let applied_targets = db.get_all_applied_targets()?;
    let targets_json: Vec<serde_json::Value> = applied_targets
        .iter()
        .map(|t| {
            json!({
                "path": t.target_path,
                "doc_count": t.doc_count,
                "catalyst_count": t.catalyst_count,
                "applied_at": t.applied_at.to_rfc3339(),
            })
        })
        .collect();

    let mut output = json!({
        "entities": entity_list,
        "total_entities": entity_list.len(),
        "applied_targets": targets_json,
    });

    if let Some(q) = query {
        output
            .as_object_mut()
            .unwrap()
            .insert("query".to_string(), json!(q));
    }

    Ok(output)
}

/// Build JSON for a single entity with its catalysts.
fn build_entity_json(
    entity: &Entity,
    catalysts: &[Catalyst],
    entity_relevances: &HashMap<String, f64>,
    catalyst_relevances: &HashMap<String, f64>,
) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp_millis();
    let last_seen = entity.last_seen.unwrap_or(0);
    let days_since_last = ((now - last_seen) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);

    let activity_trend = if days_since_last <= 7.0 {
        "active"
    } else if days_since_last <= 30.0 {
        "moderate"
    } else {
        "dormant"
    };

    let catalyst_list: Vec<serde_json::Value> = catalysts
        .iter()
        .map(|c| {
            let mut obj = json!({
                "text": c.text,
                "context": c.context,
            });
            if let Some(era) = c.metadata.get("era").and_then(|v| v.as_str()) {
                obj.as_object_mut()
                    .unwrap()
                    .insert("era".to_string(), json!(era));
            }
            if let Some(&rel) = catalyst_relevances.get(&c.id) {
                obj.as_object_mut().unwrap().insert(
                    "relevance".to_string(),
                    json!((rel * 1000.0).round() / 1000.0),
                );
            }
            obj
        })
        .collect();

    let mut entry = json!({
        "name": entity.name,
        "type": entity.entity_type.as_str(),
        "frequency": entity.frequency,
        "frequency_12m": entity.trends.frequency_recent,
        "recency_score": entity.trends.recency_score,
        "last_seen": last_seen,
        "days_since_last_seen": days_since_last.round() as i64,
        "activity_trend": activity_trend,
        "catalysts": catalyst_list,
    });

    if let Some(&rel) = entity_relevances.get(&entity.name) {
        entry.as_object_mut().unwrap().insert(
            "relevance".to_string(),
            json!((rel * 1000.0).round() / 1000.0),
        );
    }

    entry
}

/// Select entities by recency score (default behavior when no query is provided).
fn build_recency_based(
    db: &Database,
    top: usize,
) -> Result<Vec<(enzyme_core::models::Entity, Vec<enzyme_core::models::Catalyst>)>> {
    let catalyzed_names = db.get_catalyzed_entity_names()?;
    let candidates = db.get_top_entities(top.saturating_mul(3), "recency_score")?;
    let selected: Vec<_> = candidates
        .into_iter()
        .filter(|e| catalyzed_names.contains(&e.name))
        .take(top)
        .collect();
    let mut entities = Vec::new();
    for entity in selected {
        let catalysts = db.get_catalysts_for_entity(&entity.name)?;
        entities.push((entity, catalysts));
    }
    Ok(entities)
}

/// Select entities by query relevance blended with recency.
///
/// 1. Embed the query
/// 2. Load cached catalyst embeddings and compute cosine similarity per catalyst
/// 3. Group catalysts by entity, compute entity relevance (max catalyst sim)
/// 4. Rank entities by: relevance × 0.7 + normalized_recency × 0.3
/// 5. Return top N entities with top K catalysts per entity (by similarity)
fn build_query_aware(
    paths: &EnzymePaths,
    db: &Database,
    top: usize,
    query: &str,
    catalyst_budget: usize,
    entity_rel: &mut HashMap<String, f64>,
    catalyst_rel: &mut HashMap<String, f64>,
) -> Result<Vec<(enzyme_core::models::Entity, Vec<enzyme_core::models::Catalyst>)>> {
    let service = get_embedding_service(&paths.home)?;
    let query_vec = service.embed_query(query)?;

    let catalysts = db.get_all_catalysts()?;
    if catalysts.is_empty() {
        return Ok(vec![]);
    }

    // Load cached catalyst embeddings
    let cached = db.get_all_catalyst_embeddings()?;
    if cached.is_empty() {
        anyhow::bail!("no cached catalyst embeddings");
    }
    let cache_map: HashMap<&str, &Vec<f32>> = cached
        .iter()
        .map(|(id, vec)| (id.as_str(), vec))
        .collect();

    // Compute per-catalyst cosine similarity (use indices to avoid borrow issues)
    let cat_sims: Vec<(usize, f32)> = catalysts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            cache_map
                .get(c.id.as_str())
                .map(|vec| (i, Embedding::cosine_similarity(&query_vec, vec)))
        })
        .collect();

    // Group by entity
    let mut entity_groups: HashMap<String, Vec<(usize, f32)>> = HashMap::new();
    for &(idx, sim) in &cat_sims {
        entity_groups
            .entry(catalysts[idx].entity.clone())
            .or_default()
            .push((idx, sim));
    }

    // Load entity metadata for recency scores
    let all_entities =
        db.get_top_entities(entity_groups.len().max(100), "recency_score")?;
    let entity_map: HashMap<&str, &enzyme_core::models::Entity> = all_entities
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let max_recency = all_entities
        .iter()
        .map(|e| e.trends.recency_score)
        .fold(0.0f64, f64::max);

    // Compute blended entity scores: relevance × 0.7 + normalized_recency × 0.3
    let mut scored: Vec<(String, f64, f64)> = entity_groups
        .iter()
        .filter_map(|(name, sims)| {
            let entity = entity_map.get(name.as_str())?;
            let max_sim = sims.iter().map(|(_, s)| *s).fold(0.0f32, f32::max) as f64;
            let norm_recency = if max_recency > 0.0 {
                entity.trends.recency_score / max_recency
            } else {
                0.0
            };
            let blended = max_sim * 0.7 + norm_recency * 0.3;
            Some((name.clone(), max_sim, blended))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(top);

    // Build result with top K catalysts per entity
    let mut result = Vec::new();
    for (name, relevance, _) in &scored {
        if let Some(entity) = entity_map.get(name.as_str()).copied() {
            entity_rel.insert(name.clone(), *relevance);

            let mut group = entity_groups.remove(name.as_str()).unwrap_or_default();
            group.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            group.truncate(catalyst_budget);

            let cats: Vec<enzyme_core::models::Catalyst> = group
                .iter()
                .map(|&(idx, sim)| {
                    catalyst_rel.insert(catalysts[idx].id.clone(), sim as f64);
                    catalysts[idx].clone()
                })
                .collect();

            result.push((entity.clone(), cats));
        }
    }

    Ok(result)
}

pub async fn run_petri(
    vault_path: &Path,
    top: usize,
    query: Option<&str>,
    catalyst_budget: Option<usize>,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        println!("Vault not initialized. Run 'enzyme init' first.");
        return Ok(());
    }

    let output = build_petri_json(vault_path, top, query, catalyst_budget)?;
    println!("{}", serde_json::to_string_pretty(&output)?);

    Ok(())
}
