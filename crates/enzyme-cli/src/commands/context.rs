use std::path::Path;

use anyhow::Result;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::search::context::ContextSearch;

pub async fn run_context(vault_path: &Path, query: &str, limit: usize) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        println!("Vault not initialized. Run 'enzyme init' first.");
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;

    let service = get_embedding_service(&paths.home)?;

    let emb_pipeline = EmbeddingPipeline::new(&db, service, paths.embeddings_dir.clone());
    let (hnsw, bm25) = emb_pipeline.build_search_indices()?;

    let search = ContextSearch::new(&db, &hnsw, &bm25, service);
    let results = search.hybrid_search(query, limit)?;

    if results.is_empty() {
        println!("No results found for: {:?}", query);
        return Ok(());
    }

    for (i, result) in results.iter().enumerate() {
        println!("{}. {} (score: {:.4})", i + 1, result.file_path, result.score);
        let snippet: String = result.content.chars().take(200).collect();
        println!("   {}", snippet.replace('\n', " "));
        println!();
    }

    Ok(())
}
