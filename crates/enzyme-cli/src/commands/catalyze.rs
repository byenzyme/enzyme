use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::search::context::ContextSearch;

pub async fn run_catalyze(vault_path: &Path, query: &str, limit: usize) -> Result<()> {
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

    let search = ContextSearch::new(&db, service, &paths.embeddings_dir);

    let start = Instant::now();
    let response = search.catalyze_search(query, limit)?;
    let processing_time = start.elapsed().as_secs_f64();

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
