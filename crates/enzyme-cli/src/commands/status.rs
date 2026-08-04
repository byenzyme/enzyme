use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;

use super::refresh_outcome::{BackgroundRefreshOutcome, OutcomeStatus, load_outcome};

/// Build structured JSON with vault status info.
/// Returns None if the vault is not initialized.
pub async fn build_status_json(vault_path: &Path) -> Result<Option<serde_json::Value>> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        return Ok(None);
    }

    // Resolve async data first, before opening non-Send DB
    let api_key_configured = enzyme_core::llm::resolve_api_key().await.is_ok();

    let model_path = enzyme_core::embedding::model_description(&paths.home);

    let db = Database::open(&paths.vault_db)?;

    let doc_count = db.get_document_count()?;
    let entities = db.get_all_entities()?;
    let entity_count = entities.len();
    let catalysts = db.get_all_catalysts()?;
    let catalyst_count = catalysts.len();

    let docs_needing = db.get_documents_needing_embedding(None, None)?;
    let embedded_count = doc_count - docs_needing.len() as i64;

    // Terminal outcome of the most recent detached background epoch build
    // (see `refresh_outcome`). `null` when no background build has finished
    // yet (or the vault predates outcome recording).
    let last_background_refresh = load_outcome(&db);

    Ok(Some(json!({
        "vault": vault_path.display().to_string(),
        "database": paths.vault_db.display().to_string(),
        "documents": doc_count,
        "embedded": embedded_count,
        "entities": entity_count,
        "catalysts": catalyst_count,
        "model": model_path,
        "api_key_configured": api_key_configured,
        "last_background_refresh": last_background_refresh,
    })))
}

/// Render `finished_at_ms` for humans, falling back to the raw value if it is
/// not a representable timestamp.
fn format_finished_at(finished_at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(finished_at_ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| format!("{finished_at_ms} ms"))
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
            let outcome: Option<BackgroundRefreshOutcome> =
                serde_json::from_value(json["last_background_refresh"].clone()).ok();

            println!("Vault: {}", json["vault"].as_str().unwrap_or(""));
            println!("Database: {}", json["database"].as_str().unwrap_or(""));
            println!();
            println!("Documents:  {}", json["documents"]);
            println!("Embedded:   {}/{}", json["embedded"], json["documents"]);
            println!("Entities:   {}", json["entities"]);
            println!("Catalysts:  {}", json["catalysts"]);
            if let Some(outcome) = &outcome {
                if outcome.status == OutcomeStatus::Succeeded {
                    println!(
                        "Refreshed:  {}",
                        format_finished_at(outcome.finished_at_ms)
                    );
                }
            }
            match json["model"].as_str() {
                Some(path) => println!("Model:      {}", path),
                None => println!("Model:      not found"),
            }
            if json["api_key_configured"].as_bool().unwrap_or(false) {
                println!("API key:    configured");
            } else {
                println!("API key:    not configured");
            }
            if let Some(outcome) = &outcome {
                if outcome.status == OutcomeStatus::Failed {
                    println!();
                    println!(
                        "⚠ Last background refresh failed ({}): {} — previous catalysts carried forward; see .enzyme/background_refresh.log",
                        format_finished_at(outcome.finished_at_ms),
                        outcome.error.as_deref().unwrap_or("unknown error"),
                    );
                }
            }
        }
    }

    Ok(())
}
