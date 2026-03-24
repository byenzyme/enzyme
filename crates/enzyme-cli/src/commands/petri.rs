use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};
use enzyme_core::models::EntityType;

/// Build the petri JSON value from the vault database.
/// Assumes the DB exists — callers must guard this.
pub fn build_petri_json(vault_path: &Path, top: usize) -> Result<serde_json::Value> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;
    let repo = Repository::new(&db);

    // Load config for expandable folder preferences
    let config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let prefs = config.parse_preferences();

    // Single query to get the set of entity names that have catalysts,
    // then filter top entities against that set. Only fetch full catalyst
    // records for the entities we'll actually display.
    let catalyzed_names = repo.get_catalyzed_entity_names()?;
    let candidates = repo.get_top_entities(top * 3, "recency_score")?;
    let selected: Vec<_> = candidates
        .into_iter()
        .filter(|e| catalyzed_names.contains(&e.name))
        .take(top)
        .collect();
    let mut entities: Vec<(enzyme_core::models::Entity, Vec<enzyme_core::models::Catalyst>)> = Vec::new();
    for entity in selected {
        let catalysts = repo.get_catalysts_for_entity(&entity.name)?;
        entities.push((entity, catalysts));
    }

    let mut entity_list = Vec::new();

    for (entity, catalysts) in &entities {

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
                    obj.as_object_mut().unwrap().insert("era".to_string(), json!(era));
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

        // Enrich expandable folders with sampled children
        if entity.entity_type == EntityType::Folder {
            if let Some(configured_children) = prefs.expandable_folders.get(&entity.name) {
                let (children, total_count) =
                    repo.get_folder_children_by_references(&entity.name, 10)?;

                let sampled: Vec<serde_json::Value> = children
                    .iter()
                    .map(|c| json!({
                        "entity": format!("[[{}]]", c.name),
                        "reference_count": c.reference_count,
                        "last_seen": c.last_seen,
                    }))
                    .collect();

                // Catalyzed children: configured children that actually have catalysts
                let catalyzed: Vec<String> = configured_children
                    .iter()
                    .filter(|child| {
                        repo.get_catalysts_for_entity(&child.name)
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
        }

        entity_list.push(entry);
    }

    let applied_targets = repo.get_all_applied_targets()?;
    let targets_json: Vec<serde_json::Value> = applied_targets
        .iter()
        .map(|t| json!({
            "path": t.target_path,
            "doc_count": t.doc_count,
            "catalyst_count": t.catalyst_count,
            "applied_at": t.applied_at.to_rfc3339(),
        }))
        .collect();

    Ok(json!({
        "entities": entity_list,
        "total_entities": entity_list.len(),
        "applied_targets": targets_json,
    }))
}

pub async fn run_petri(vault_path: &Path, top: usize) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        println!("Vault not initialized. Run 'enzyme init' first.");
        return Ok(());
    }

    let output = build_petri_json(vault_path, top)?;
    println!("{}", serde_json::to_string_pretty(&output)?);

    Ok(())
}
