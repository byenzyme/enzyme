use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};
use enzyme_core::pipeline::staleness::check_catalyst_staleness;

pub async fn run_petri(vault_path: &Path, top: usize) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        println!("Vault not initialized. Run 'enzyme init' first.");
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;
    let stale = check_catalyst_staleness(&db, vault_path)?.is_stale();
    let repo = Repository::new(&db);

    let entities = repo.get_top_entities(top, "recency_score")?;

    let mut entity_list = Vec::new();

    for entity in &entities {
        let catalysts = repo.get_catalysts_for_entity(&entity.name)?;

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
            .map(|c| json!({
                "id": c.id,
                "text": c.text,
                "context": c.context,
            }))
            .collect();

        entity_list.push(json!({
            "name": entity.name,
            "type": entity.entity_type.as_str(),
            "frequency": entity.frequency,
            "frequency_12m": entity.trends.frequency_recent,
            "recency_score": entity.trends.recency_score,
            "last_seen": last_seen,
            "days_since_last_seen": days_since_last.round() as i64,
            "activity_trend": activity_trend,
            "catalysts": catalyst_list,
        }));
    }

    let output = json!({
        "entities": entity_list,
        "total_entities": entity_list.len(),
        "stale": stale,
    });

    println!("{}", serde_json::to_string_pretty(&output)?);

    Ok(())
}
