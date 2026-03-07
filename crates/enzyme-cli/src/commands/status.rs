use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};

/// Build structured JSON with vault status info.
/// Returns None if the vault is not initialized.
pub async fn build_status_json(vault_path: &Path) -> Result<Option<serde_json::Value>> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        return Ok(None);
    }

    // Resolve async data first, before opening non-Send DB
    let api_key_configured = enzyme_core::llm::resolve_api_key().await.is_ok();

    let model_path = match enzyme_core::embedding::resolve_model_files(&paths.home) {
        Ok(mf) => Some(mf.model_path.display().to_string()),
        Err(_) => None,
    };

    let db = Database::open(&paths.vault_db)?;
    let repo = Repository::new(&db);

    let doc_count = repo.get_document_count()?;
    let entities = repo.get_all_entities()?;
    let entity_count = entities.len();
    let catalysts = repo.get_all_catalysts()?;
    let catalyst_count = catalysts.len();

    let docs_needing = repo.get_documents_needing_embedding(None)?;
    let embedded_count = doc_count - docs_needing.len() as i64;

    Ok(Some(json!({
        "vault": vault_path.display().to_string(),
        "database": paths.vault_db.display().to_string(),
        "embeddings_dir": paths.embeddings_dir.display().to_string(),
        "documents": doc_count,
        "embedded": embedded_count,
        "entities": entity_count,
        "catalysts": catalyst_count,
        "model": model_path,
        "api_key_configured": api_key_configured,
    })))
}

pub async fn run_status(vault_path: &Path) -> Result<()> {
    match build_status_json(vault_path).await? {
        None => {
            println!("Vault: {}", vault_path.display());
            println!("Status: not initialized");
            println!();
            println!("Run 'enzyme init' to initialize.");
        }
        Some(json) => {
            println!("Vault: {}", json["vault"].as_str().unwrap_or(""));
            println!("Database: {}", json["database"].as_str().unwrap_or(""));
            println!("Embeddings: {}", json["embeddings_dir"].as_str().unwrap_or(""));
            println!();
            println!("Documents:  {}", json["documents"]);
            println!("Embedded:   {}/{}", json["embedded"], json["documents"]);
            println!("Entities:   {}", json["entities"]);
            println!("Catalysts:  {}", json["catalysts"]);
            match json["model"].as_str() {
                Some(path) => println!("Model:      {}", path),
                None => println!("Model:      not found"),
            }
            if json["api_key_configured"].as_bool().unwrap_or(false) {
                println!("API key:    configured");
            } else {
                println!("API key:    not configured");
            }
        }
    }

    Ok(())
}
