use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::db::Database;
use crate::models::{Entity, EntityType};
use crate::pipeline::entity_map::EntityMap;
use crate::pipeline::profile_assignment;

/// Maximum page-entity children to materialize per expandable folder.
pub const MAX_FOLDER_CHILDREN: usize = 10;
/// Number of page-entity children to surface as catalyzed/expandable hints.
pub const CATALYZED_FOLDER_CHILD_LIMIT: usize = 3;

/// Minimum fraction of a folder's files that must map to active page-entity
/// links before the folder is treated as expandable by auto-detection.
pub const EXPANDABLE_DENSITY_THRESHOLD: f64 = 0.05;

/// Shared deterministic selection result after running the step pipeline.
pub struct MaterializedSelection {
    /// Base entities before folder-child expansion.
    pub base_entities: Vec<Entity>,
    /// Structural view over the final entity set.
    pub entity_map: EntityMap,
    /// Expandable folders and their capped child link names.
    pub expandable_folders: HashMap<String, Vec<String>>,
    /// Final entity list after folder-child expansion.
    pub final_entities: Vec<Entity>,
    /// Final per-entity profile assignment map.
    pub entity_profiles: HashMap<String, String>,
}

/// Build the default deterministic selection pipeline:
/// shortlist -> detect expandable folders -> expand child links -> assign profiles.
pub fn build_shortlist_selection(db: &Database, limit: usize) -> Result<MaterializedSelection> {
    let base_entities = EntityMap::select_shortlist(db, limit)?;
    let expandable_folders = auto_expandable_folders(db, &base_entities)?;
    let final_entities = expand_with_folder_children(db, &base_entities, &expandable_folders)?;
    let entity_map = EntityMap::from_entities(db, &final_entities)?;
    let entity_profiles =
        profile_assignment::assign_profiles(&final_entities, &HashMap::new(), None);

    Ok(MaterializedSelection {
        base_entities,
        entity_map,
        expandable_folders,
        final_entities,
        entity_profiles,
    })
}

/// Build the curated selection pipeline:
/// curated entities -> detect expandable folders -> expand child links -> assign profiles.
pub fn build_curated_selection(
    db: &Database,
    base_entities: Vec<Entity>,
) -> Result<MaterializedSelection> {
    let expandable_folders = auto_expandable_folders(db, &base_entities)?;
    let final_entities = expand_with_folder_children(db, &base_entities, &expandable_folders)?;
    let entity_map = EntityMap::from_entities(db, &final_entities)?;
    let entity_profiles =
        profile_assignment::assign_profiles(&final_entities, &HashMap::new(), None);

    Ok(MaterializedSelection {
        base_entities,
        entity_map,
        expandable_folders,
        final_entities,
        entity_profiles,
    })
}

/// Deterministically rank a folder's page entities for catalyst/display use.
///
/// Ranking prefers children that are highly referenced from within the folder,
/// then falls back to overall frequency and recency. Results are returned as
/// wikilinks for direct reuse in config/petri surfaces.
pub fn select_folder_children(
    db: &Database,
    folder_name: &str,
    page_entities: &[String],
    limit: usize,
) -> Vec<String> {
    if page_entities.is_empty() || limit == 0 {
        return Vec::new();
    }

    let page_entity_set: HashSet<&str> = page_entities.iter().map(|name| name.as_str()).collect();

    let lookup_pairs: Vec<(&str, EntityType)> = page_entities
        .iter()
        .map(|name| (name.as_str(), EntityType::Link))
        .collect();
    let entities = db.get_entities_by_names(&lookup_pairs).unwrap_or_default();
    let entity_by_name: HashMap<String, Entity> = entities
        .into_iter()
        .map(|entity| (entity.name.clone(), entity))
        .collect();

    let reference_lookup_limit = page_entities.len().max(limit);
    let referenced_children = db
        .get_folder_children_by_references(folder_name, reference_lookup_limit)
        .map(|(children, _)| children)
        .unwrap_or_default();

    let mut ranked: Vec<(String, i64, i64, i64)> = referenced_children
        .into_iter()
        .filter_map(|child| {
            let normalized = child.name.to_lowercase();
            if !page_entity_set.contains(normalized.as_str()) {
                return None;
            }
            let entity = entity_by_name.get(&normalized)?;
            Some((
                normalized,
                child.reference_count,
                entity.frequency,
                entity.last_seen.unwrap_or(0),
            ))
        })
        .collect();

    let mut ranked_names: HashSet<String> =
        ranked.iter().map(|(name, _, _, _)| name.clone()).collect();

    for page_entity in page_entities {
        if ranked_names.contains(page_entity) {
            continue;
        }
        if let Some(entity) = entity_by_name.get(page_entity) {
            ranked.push((
                page_entity.clone(),
                0,
                entity.frequency,
                entity.last_seen.unwrap_or(0),
            ));
            ranked_names.insert(page_entity.clone());
        }
    }

    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(limit);

    ranked
        .into_iter()
        .map(|(name, _, _, _)| format!("[[{}]]", name))
        .collect()
}

fn auto_expandable_folders(
    db: &Database,
    entities: &[Entity],
) -> Result<HashMap<String, Vec<String>>> {
    let folders = folder_scope(entities);
    load_expandable_folder_children(db, &folders, true)
}

fn folder_scope(entities: &[Entity]) -> HashMap<String, i64> {
    entities
        .iter()
        .filter(|entity| entity.entity_type == EntityType::Folder)
        .map(|entity| (entity.name.clone(), entity.frequency.max(1)))
        .collect()
}

fn load_expandable_folder_children(
    db: &Database,
    folders: &HashMap<String, i64>,
    enforce_density: bool,
) -> Result<HashMap<String, Vec<String>>> {
    if folders.is_empty() {
        return Ok(HashMap::new());
    }

    let folder_names: HashSet<&str> = folders.keys().map(|name| name.as_str()).collect();
    let links_by_folder = db.get_folder_page_entity_links_grouped(&folder_names)?;

    let mut result = HashMap::new();
    for (folder, mut links) in links_by_folder {
        let file_count = folders.get(&folder).copied().unwrap_or(1) as f64;
        let density = links.len() as f64 / file_count;
        if enforce_density && density < EXPANDABLE_DENSITY_THRESHOLD {
            continue;
        }

        links.sort_by(|a, b| {
            b.trends
                .recency_score
                .partial_cmp(&a.trends.recency_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.frequency.cmp(&a.frequency))
                .then_with(|| a.name.cmp(&b.name))
        });
        links.truncate(MAX_FOLDER_CHILDREN);

        let names: Vec<String> = links.into_iter().map(|entity| entity.name).collect();
        if !names.is_empty() {
            result.insert(folder, names);
        }
    }

    Ok(result)
}

fn expand_with_folder_children(
    db: &Database,
    base_entities: &[Entity],
    expandable_folders: &HashMap<String, Vec<String>>,
) -> Result<Vec<Entity>> {
    let mut final_entities = base_entities.to_vec();
    let mut existing: HashSet<(String, EntityType)> = final_entities
        .iter()
        .map(|entity| (entity.name.clone(), entity.entity_type.clone()))
        .collect();

    let mut ordered_missing = Vec::new();
    let mut folder_names: Vec<&String> = expandable_folders.keys().collect();
    folder_names.sort();

    for folder_name in folder_names {
        if let Some(children) = expandable_folders.get(folder_name) {
            for child_name in children {
                let key = (child_name.clone(), EntityType::Link);
                if existing.insert(key) {
                    ordered_missing.push(child_name.clone());
                }
            }
        }
    }

    if ordered_missing.is_empty() {
        return Ok(final_entities);
    }

    let lookup: Vec<(&str, EntityType)> = ordered_missing
        .iter()
        .map(|name| (name.as_str(), EntityType::Link))
        .collect();
    let found = db.get_entities_by_names(&lookup)?;
    let found_by_name: HashMap<String, Entity> = found
        .into_iter()
        .map(|entity| (entity.name.clone(), entity))
        .collect();

    for child_name in ordered_missing {
        if let Some(entity) = found_by_name.get(&child_name) {
            final_entities.push(entity.clone());
        }
    }

    Ok(final_entities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::EntityTrends;

    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        use libsql::Value as V;
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
             VALUES (?1, ?2, '', 'hash', ?3, 'test', ?3, 'test', ?3)",
            vec![
                V::Text(source_ref.to_string()),
                V::Text(source_ref.to_string()),
                V::Integer(now),
            ],
        )
        .unwrap();
    }

    fn insert_entity(db: &Database, name: &str, etype: &str, frequency: i64, recent: i64) {
        use libsql::Value as V;
        let now = chrono::Utc::now().timestamp_millis();
        let trends = serde_json::json!({
            "frequency_recent": recent,
            "recency_score": frequency as f64,
            "timeline_buckets": {},
        });
        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            vec![
                V::Text(name.to_string()),
                V::Text(etype.to_string()),
                V::Integer(frequency),
                V::Integer(now),
                V::Text(trends.to_string()),
            ],
        )
        .unwrap();
    }

    fn make_entity(name: &str, entity_type: EntityType, frequency: i64) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type,
            frequency,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: frequency,
                recency_score: frequency as f64,
                timeline_buckets: HashMap::new(),
            },
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn auto_expandable_folder_loads_page_entities_from_db() {
        let db = test_db();
        insert_doc(&db, "people/Alice Smith.md");
        insert_entity(&db, "alice smith", "link", 12, 4);

        let selected = vec![make_entity("people", EntityType::Folder, 20)];

        let result = build_curated_selection(&db, selected).unwrap();

        assert_eq!(
            result.expandable_folders.get("people"),
            Some(&vec!["alice smith".to_string()])
        );
        assert!(
            result.final_entities.iter().any(
                |entity| entity.name == "alice smith" && entity.entity_type == EntityType::Link
            )
        );
    }

    #[test]
    fn folder_child_expansion_deduplicates_shared_links() {
        let db = test_db();
        insert_entity(&db, "alice smith", "link", 12, 4);

        let base_entities = vec![
            make_entity("people", EntityType::Folder, 20),
            make_entity("team", EntityType::Folder, 20),
        ];
        let expandable_folders = HashMap::from([
            ("people".to_string(), vec!["alice smith".to_string()]),
            ("team".to_string(), vec!["alice smith".to_string()]),
        ]);

        let final_entities =
            expand_with_folder_children(&db, &base_entities, &expandable_folders).unwrap();
        let alice_count = final_entities
            .iter()
            .filter(|entity| entity.name == "alice smith" && entity.entity_type == EntityType::Link)
            .count();

        assert_eq!(alice_count, 1);
    }

}
