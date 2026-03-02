use std::path::Path;

use anyhow::Result;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::{Database, Repository};

pub async fn run_status(vault_path: &Path) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    // Check if database exists
    if !paths.vault_db.exists() {
        println!("Vault: {}", vault_path.display());
        println!("Status: not initialized");
        println!();
        println!("Run 'enzyme init' to initialize.");
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;
    let repo = Repository::new(&db);

    let doc_count = repo.get_document_count()?;
    let entities = repo.get_all_entities()?;
    let entity_count = entities.len();
    let catalysts = repo.get_all_catalysts()?;
    let catalyst_count = catalysts.len();

    // Check how many docs are embedded
    let docs_needing = repo.get_documents_needing_embedding()?;
    let embedded_count = doc_count - docs_needing.len() as i64;

    println!("Vault: {}", vault_path.display());
    println!("Database: {}", paths.vault_db.display());
    println!("Embeddings: {}", paths.embeddings_dir.display());
    println!();
    println!("Documents:  {}", doc_count);
    println!("Embedded:   {}/{}", embedded_count, doc_count);
    println!("Entities:   {}", entity_count);
    println!("Catalysts:  {}", catalyst_count);

    // Check for model files
    match enzyme_core::embedding::resolve_model_files(&paths.home) {
        Ok(mf) => println!("Model:      {}", mf.model_path.display()),
        Err(_) => println!("Model:      not found"),
    }

    // Check for API key
    match enzyme_core::llm::resolve_api_key() {
        Ok(_) => println!("API key:    configured"),
        Err(_) => println!("API key:    not configured"),
    }

    Ok(())
}
