use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::search::context::ContextSearch;

pub async fn run_catalyze(vault_path: &Path, query: &str, limit: usize, target: Option<&Path>) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        let output = json!({
            "error": "Vault not initialized. Run 'enzyme init' first.",
            "query": query,
            "results": []
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;

    let service = get_embedding_service(&paths.home)?;

    // When a target is provided, open its DB for similarity/content lookups
    let target_db;
    let target_paths;
    let search = if let Some(target_path) = target {
        target_paths = EnzymePaths::new(target_path)?;
        if !target_paths.vault_db.exists() {
            let output = json!({
                "error": format!("Target not initialized. Run 'enzyme apply {}' first.", target_path.display()),
                "query": query,
                "results": []
            });
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
        target_db = Database::open(&target_paths.vault_db)?;
        ContextSearch::with_target(&db, &target_db, service, &paths.embeddings_dir)
    } else {
        ContextSearch::new(&db, service, &paths.embeddings_dir)
    };

    let start = Instant::now();
    let response = search.catalyze_search(query, limit)?;
    let processing_time = start.elapsed().as_secs_f64();

    // Enrich top catalysts with profile-specific presentation guidance
    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let prefs = enzyme_config.parse_preferences();
    let response = response.with_presentation_guidance(
        &prefs.entity_profiles,
        enzyme_config.catalyst_profile.as_deref(),
    );

    let output = json!({
        "query": query,
        "search_strategy": "catalyze",
        "processing_time": (processing_time * 1000.0).round() / 1000.0,
        "total_results": response.results.len(),
        "results": response.results,
        "top_contributing_catalysts": response.top_contributing_catalysts,
    });

    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}
