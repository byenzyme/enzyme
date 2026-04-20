use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::llm::ContentRegister;
use enzyme_core::search::context::ContextSearch;

pub async fn run_catalyze(
    vault_path: &Path,
    query: &str,
    limit: usize,
    target: Option<&Path>,
    register_str: &str,
) -> Result<()> {
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

    let register = ContentRegister::from_str_opt(register_str).unwrap_or_else(|| {
        tracing::warn!("unknown register '{}', defaulting to explore", register_str);
        ContentRegister::Explore
    });

    let db = Database::open(&paths.vault_db)?;

    let service = get_embedding_service(&paths.home)?;

    // When a target is provided, open its DB from source's targets/ directory
    let target_db;
    let search = if let Some(target_path) = target {
        let target_db_path = paths.target_db(target_path);
        if !target_db_path.exists() {
            let output = json!({
                "error": format!("Target not initialized. Run 'enzyme apply {}' first.", target_path.display()),
                "query": query,
                "results": []
            });
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
        target_db = Database::open(&target_db_path)?;
        ContextSearch::with_target(&db, &target_db, service)
    } else {
        ContextSearch::new(&db, service)
    };

    let start = Instant::now();
    let response = search.catalyze_search(query, limit)?;
    let processing_time = start.elapsed().as_secs_f64();

    // Enrich with register-appropriate presentation guidance
    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
    let response = response.with_presentation_guidance(
        register,
        &std::collections::HashMap::new(),
        enzyme_config.catalyst_profile.as_deref(),
    );

    // Merge response fields with metadata into a single JSON object
    let mut output = serde_json::to_value(&response)?;
    if let Some(obj) = output.as_object_mut() {
        obj.insert("query".into(), json!(query));
        obj.insert("search_strategy".into(), json!("catalyze"));
        obj.insert(
            "processing_time".into(),
            json!((processing_time * 1000.0).round() / 1000.0),
        );
        obj.insert("total_results".into(), json!(response.results.len()));
    }

    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}
