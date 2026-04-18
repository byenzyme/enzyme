use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tracing::debug;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;

use enzyme_core::models::{Catalyst, Embedding, Entity, EntityType};
use enzyme_core::pipeline::entity_map::{
    EntityDisposition, EntityKey, EntityMap, FolderGroup, FolderOrigin,
};
use enzyme_core::pipeline::llm_selection::select_folder_children;

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

    // Canonical structural facts for the selected entity set.
    let entity_list_only: Vec<Entity> = entities.iter().map(|(e, _)| e.clone()).collect();
    let entity_map = EntityMap::from_entities(&db, &entity_list_only)?;

    let mut tag_children: HashMap<String, Vec<&(Entity, Vec<Catalyst>)>> = HashMap::new();
    let mut top_level: Vec<&(Entity, Vec<Catalyst>)> = Vec::new();
    let mut folder_children: HashMap<String, Vec<&(Entity, Vec<Catalyst>)>> = HashMap::new();

    for entry in &entities {
        match entity_map.disposition_for(&entry.0) {
            EntityDisposition::TopLevel => top_level.push(entry),
            EntityDisposition::FoldedIntoTag { parent_name } => {
                tag_children.entry(parent_name).or_default().push(entry);
            }
            EntityDisposition::FoldedIntoFolder { folder_name } => {
                folder_children.entry(folder_name).or_default().push(entry);
            }
            EntityDisposition::ShadowedByTag { .. } => {
                continue;
            }
        }
    }

    let top_level_keys: HashMap<EntityKey, &(Entity, Vec<Catalyst>)> = top_level
        .iter()
        .map(|entry| (EntityKey::from(&entry.0), *entry))
        .collect();

    // Build JSON
    let mut entity_list = Vec::new();

    for (entity, catalysts) in &top_level {
        let mut entry =
            build_entity_json(entity, catalysts, &entity_relevances, &catalyst_relevances);

        if entity.entity_type == EntityType::Tag {
            if let Some(children) = tag_children.get(&entity.name) {
                let nested: Vec<serde_json::Value> = children
                    .iter()
                    .map(|(child_entity, child_catalysts)| {
                        build_entity_json(
                            child_entity,
                            child_catalysts,
                            &entity_relevances,
                            &catalyst_relevances,
                        )
                    })
                    .collect();
                if !nested.is_empty() {
                    entry
                        .as_object_mut()
                        .unwrap()
                        .insert("significant_children".to_string(), json!(nested));
                }
            }
        }

        if entity.entity_type == EntityType::Folder {
            if let Some(folder_group) = entity_map.folder_group(&entity.name) {
                enrich_folder_entry(
                    &db,
                    folder_group,
                    folder_children.get(&entity.name),
                    &mut entry,
                    &entity_relevances,
                    &catalyst_relevances,
                )?;
            }
        }

        entity_list.push(entry);
    }

    for folder_group in &entity_map.folders {
        let key = EntityKey::from(&folder_group.entity);
        if top_level_keys.contains_key(&key) || folder_group.origin != FolderOrigin::Synthetic {
            continue;
        }

        let mut entry = build_entity_json(
            &folder_group.entity,
            &[],
            &entity_relevances,
            &catalyst_relevances,
        );
        enrich_folder_entry(
            &db,
            folder_group,
            folder_children.get(&folder_group.entity.name),
            &mut entry,
            &entity_relevances,
            &catalyst_relevances,
        )?;
        entry
            .as_object_mut()
            .unwrap()
            .insert("synthetic".to_string(), json!(true));
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

fn enrich_folder_entry(
    db: &Database,
    folder_group: &FolderGroup,
    page_entity_children: Option<&Vec<&(Entity, Vec<Catalyst>)>>,
    entry: &mut serde_json::Value,
    entity_relevances: &HashMap<String, f64>,
    catalyst_relevances: &HashMap<String, f64>,
) -> Result<()> {
    if !folder_group.page_entities.is_empty() {
        let (children, total_count) =
            db.get_folder_children_by_references(&folder_group.entity.name, 10)?;

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

        let catalyzed =
            select_folder_children(db, &folder_group.entity.name, &folder_group.page_entities);

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

    if let Some(children) = page_entity_children {
        let nested: Vec<serde_json::Value> = children
            .iter()
            .map(|(child_entity, child_catalysts)| {
                build_entity_json(
                    child_entity,
                    child_catalysts,
                    entity_relevances,
                    catalyst_relevances,
                )
            })
            .collect();
        if !nested.is_empty() {
            entry
                .as_object_mut()
                .unwrap()
                .insert("page_entity_children".to_string(), json!(nested));
        }
    }

    Ok(())
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
) -> Result<
    Vec<(
        enzyme_core::models::Entity,
        Vec<enzyme_core::models::Catalyst>,
    )>,
> {
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
) -> Result<
    Vec<(
        enzyme_core::models::Entity,
        Vec<enzyme_core::models::Catalyst>,
    )>,
> {
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
    let cache_map: HashMap<&str, &Vec<f32>> =
        cached.iter().map(|(id, vec)| (id.as_str(), vec)).collect();

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
    let all_entities = db.get_top_entities(entity_groups.len().max(100), "recency_score")?;
    let entity_map: HashMap<&str, &enzyme_core::models::Entity> =
        all_entities.iter().map(|e| (e.name.as_str(), e)).collect();

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

    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top);

    // Build result with top K catalysts per entity
    let mut result = Vec::new();
    for (name, relevance, _) in &scored {
        if let Some(entity) = entity_map.get(name.as_str()).copied() {
            entity_rel.insert(name.clone(), *relevance);

            let mut group = entity_groups.remove(name.as_str()).unwrap_or_default();
            group.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use enzyme_core::db::Database;
    use enzyme_core::models::{Catalyst, EntityTrends};
    use tempfile::TempDir;

    fn create_test_vault() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".enzyme")).unwrap();
        dir
    }

    fn open_test_db(vault_path: &Path) -> Database {
        let paths = EnzymePaths::new(vault_path).unwrap();
        Database::open(&paths.vault_db).unwrap()
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        let now = Utc::now().timestamp_millis();
        db.execute_write(
            &format!(
                "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
                 VALUES ('{source_ref}', '{source_ref}', '', 'hash', {now}, 'test', {now}, 'test', {now})"
            ),
            vec![],
        )
        .unwrap();
    }

    fn insert_entity(
        db: &Database,
        name: &str,
        entity_type: EntityType,
        frequency: i64,
        recency_score: f64,
    ) {
        let now = Utc::now().timestamp_millis();
        let trends = serde_json::to_string(&EntityTrends {
            frequency_recent: frequency,
            recency_score,
            timeline_buckets: HashMap::new(),
        })
        .unwrap();

        db.execute_write(
            &format!(
                "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends, metadata)
                 VALUES ('{name}', '{}', {frequency}, {now}, {now}, '{}', '{{}}')",
                entity_type.as_str(),
                trends.replace('\'', "''"),
            ),
            vec![],
        )
        .unwrap();
    }

    fn insert_catalyst(db: &Database, entity: &str, id: &str, text: &str) {
        db.insert_catalysts(&[Catalyst {
            id: id.to_string(),
            text: text.to_string(),
            entity: entity.to_string(),
            context: "test".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
    }

    #[test]
    fn petri_nests_significant_tag_children() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100, 100.0);
        insert_entity(&db, "enzyme/pmf", EntityType::Tag, 40, 80.0);
        insert_catalyst(&db, "enzyme", "cat-parent", "parent catalyst");
        insert_catalyst(&db, "enzyme/pmf", "cat-child", "child catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["name"], "enzyme");
        let children = entities[0]["significant_children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "enzyme/pmf");
        assert_eq!(children[0]["type"], "tag");
    }

    #[test]
    fn petri_surfaces_synthetic_folder_page_entity_children() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_doc(&db, "people/Alice Smith.md");
        insert_entity(&db, "alice smith", EntityType::Link, 30, 90.0);
        insert_catalyst(&db, "alice smith", "cat-alice", "alice catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["name"], "people");
        assert_eq!(entities[0]["type"], "folder");
        assert_eq!(entities[0]["synthetic"], true);

        let children = entities[0]["page_entity_children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "alice smith");
        assert_eq!(children[0]["type"], "link");
    }

    #[test]
    fn petri_does_not_duplicate_folded_or_shadowed_entities_at_top_level() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_doc(&db, "people/Alice.md");
        insert_entity(&db, "enzyme", EntityType::Tag, 100, 100.0);
        insert_entity(&db, "enzyme/pmf", EntityType::Tag, 40, 80.0);
        insert_entity(&db, "alice", EntityType::Link, 30, 70.0);
        insert_entity(&db, "shadow", EntityType::Tag, 25, 60.0);
        insert_entity(&db, "shadow", EntityType::Link, 20, 50.0);

        insert_catalyst(&db, "enzyme", "cat-enzyme", "parent catalyst");
        insert_catalyst(&db, "enzyme/pmf", "cat-pmf", "child catalyst");
        insert_catalyst(&db, "alice", "cat-alice", "alice catalyst");
        insert_catalyst(&db, "shadow", "cat-shadow", "shadow catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        let top_level_names: Vec<&str> = entities
            .iter()
            .filter_map(|entity| entity["name"].as_str())
            .collect();

        assert!(top_level_names.contains(&"enzyme"));
        assert!(top_level_names.contains(&"people"));
        assert!(top_level_names.contains(&"shadow"));
        assert!(!top_level_names.contains(&"enzyme/pmf"));
        assert!(!top_level_names.contains(&"alice"));

        let enzyme = entities
            .iter()
            .find(|entity| entity["name"] == "enzyme")
            .unwrap();
        let tag_children = enzyme["significant_children"].as_array().unwrap();
        assert_eq!(tag_children.len(), 1);
        assert_eq!(tag_children[0]["name"], "enzyme/pmf");

        let people = entities
            .iter()
            .find(|entity| entity["name"] == "people")
            .unwrap();
        let page_children = people["page_entity_children"].as_array().unwrap();
        assert_eq!(page_children.len(), 1);
        assert_eq!(page_children[0]["name"], "alice");
    }

    #[test]
    fn petri_derives_catalyzed_children_for_expandable_folder_from_runtime_structure() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        std::fs::write(
            vault.path().join(".enzyme").join("enzyme-config.yaml"),
            r#"# Auto-generated by enzyme init (LLM selection)
# Edit this file to customize which entities get catalysts.

catalyzedEntities:
  - "folder:people":
      expandable: true
      profile: relational
"#,
        )
        .unwrap();

        insert_doc(&db, "people/Maren Holt.md");
        insert_doc(&db, "people/Odessa Lark.md");
        insert_doc(&db, "people/Ivo.md");
        insert_doc(&db, "people/Tamsin Vey.md");

        insert_entity(&db, "maren holt", EntityType::Link, 53, 72.0);
        insert_entity(&db, "odessa lark", EntityType::Link, 61, 39.0);
        insert_entity(&db, "ivo", EntityType::Link, 52, 20.0);
        insert_entity(&db, "tamsin vey", EntityType::Link, 8, 20.0);

        let now = Utc::now().timestamp_millis();
        for (target, count) in [
            ("odessa lark", 61),
            ("maren holt", 53),
            ("ivo", 52),
            ("tamsin vey", 8),
        ] {
            for i in 0..count {
                let source = format!("notes/{}-{i}.md", target.replace(' ', "-"));
                db.execute_write(
                    &format!(
                        "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                         VALUES ('{source}', 'h-{source}', {now}, {now}, {now})"
                    ),
                    vec![],
                )
                .unwrap();

                let doc_id: i64 = db
                    .query_read(
                        &format!("SELECT id FROM docs WHERE source_ref = '{source}'"),
                        vec![],
                        |r| r.get::<i64>(0),
                    )
                    .unwrap()[0];

                db.execute_write(
                    &format!("INSERT INTO doc_links (doc_id, link) VALUES ({doc_id}, '{target}')"),
                    vec![],
                )
                .unwrap();
            }
        }

        insert_catalyst(&db, "maren holt", "cat-maren", "maren catalyst");
        insert_catalyst(&db, "odessa lark", "cat-odessa", "odessa catalyst");
        insert_catalyst(&db, "ivo", "cat-ivo", "ivo catalyst");
        insert_catalyst(&db, "tamsin vey", "cat-tamsin", "tamsin catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();
        let people = entities
            .iter()
            .find(|entity| entity["name"] == "people")
            .unwrap();

        let catalyzed_children = people["catalyzed_children"].as_array().unwrap();
        let child_names: Vec<&str> = catalyzed_children
            .iter()
            .filter_map(|child| child.as_str())
            .collect();

        assert_eq!(
            child_names,
            vec!["[[odessa lark]]", "[[maren holt]]", "[[ivo]]"]
        );
    }
}
