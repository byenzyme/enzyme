use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::enzyme_config::{CatalystMode, EnzymeConfig};
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::db::update_sentinel::{self, UpdateStatus};
use enzyme_core::llm::model_registry;

use super::init::BINARY_VERSION;
use super::refresh_outcome::{BackgroundRefreshOutcome, OutcomeStatus, load_outcome};
use super::update::{
    BinaryUpdateOutcome, BinaryUpdateStatus, load_outcome as load_binary_update, update_available,
};

/// Build structured JSON with vault status info.
/// Returns None if the vault is not initialized.
pub async fn build_status_json(vault_path: &Path) -> Result<Option<serde_json::Value>> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        return Ok(None);
    }

    // Status must stay read-only and offline: report provider intent from the
    // persisted mode rather than resolving hosted credentials over the network.
    let llm = EnzymeConfig::catalyst_llm_config()?;
    let local_model = model_registry::selected_name();
    let local_model_installed = model_registry::model_path(&local_model)?.exists();
    let catalyst_mode = match llm.mode {
        CatalystMode::Auto if local_model_installed => "local",
        CatalystMode::Auto => "hosted",
        CatalystMode::Local if local_model_installed => "local",
        CatalystMode::Local => "local-missing",
        CatalystMode::Hosted => "hosted",
    };
    let api_key_configured = ["OPENAI_API_KEY", "OPENROUTER_API_KEY"]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty()));

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

    // Status stays read-only: peek at the update sentinel without advancing
    // it, so agents can see whether a regen is pending before it happens.
    let binary_update = load_binary_update(&db);
    let mut update = match update_sentinel::peek(&db, BINARY_VERSION)? {
        UpdateStatus::UpToDate => json!({
            "status": "up_to_date", "regen_required": false, "reason": null
        }),
        UpdateStatus::SentinelRefreshed { previous_version } => json!({
            "status": "sentinel_stale", "regen_required": false,
            "reason": format!(
                "binary changed since {}; schema compatible, sentinel will advance on next refresh",
                previous_version.as_deref().unwrap_or("unknown")
            ),
        }),
        UpdateStatus::RegenRequired { reason } => json!({
            "status": "regen_required", "regen_required": true, "reason": reason
        }),
    };
    update["binary"] = json!({
        "current": BINARY_VERSION,
        "last_outcome": binary_update,
    });

    Ok(Some(json!({
        "vault": vault_path.display().to_string(),
        "database": paths.vault_db.display().to_string(),
        "documents": doc_count,
        "embedded": embedded_count,
        "entities": entity_count,
        "catalysts": catalyst_count,
        "model": model_path,
        "catalyst_mode_configured": llm.mode.to_string(),
        "catalyst_mode": catalyst_mode,
        "catalyst_model": local_model,
        "catalyst_model_installed": local_model_installed,
        "api_key_configured": api_key_configured,
        "last_background_refresh": last_background_refresh,
        "update": update,
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
            let binary_update: Option<BinaryUpdateOutcome> =
                serde_json::from_value(json["update"]["binary"]["last_outcome"].clone()).ok();

            println!("Vault: {}", json["vault"].as_str().unwrap_or(""));
            println!("Database: {}", json["database"].as_str().unwrap_or(""));
            println!();
            println!("Documents:  {}", json["documents"]);
            println!("Embedded:   {}/{}", json["embedded"], json["documents"]);
            println!("Entities:   {}", json["entities"]);
            println!("Catalysts:  {}", json["catalysts"]);
            if let Some(outcome) = &outcome {
                if outcome.status == OutcomeStatus::Succeeded {
                    println!("Refreshed:  {}", format_finished_at(outcome.finished_at_ms));
                }
            }
            println!("Catalysts:  {}", json["catalyst_mode"]);
            if json["catalyst_mode"] == "local" || json["catalyst_mode"] == "local-missing" {
                println!(
                    "Local LLM:  {} ({})",
                    json["catalyst_model"].as_str().unwrap_or("unknown"),
                    if json["catalyst_model_installed"].as_bool().unwrap_or(false) {
                        "installed"
                    } else {
                        "not installed"
                    }
                );
            }
            match json["model"].as_str() {
                Some(path) => println!("Embedding:  {}", path),
                None => println!("Embedding:  not found"),
            }
            if json["api_key_configured"].as_bool().unwrap_or(false) {
                println!("API key:    configured");
            } else {
                println!("API key:    not configured");
            }
            if json["update"]["regen_required"].as_bool().unwrap_or(false) {
                println!();
                println!(
                    "⚠ Update: regen required — {}. The next `enzyme init` or agent refresh will rebuild the index automatically (auth and config are preserved).",
                    json["update"]["reason"].as_str().unwrap_or("schema drift"),
                );
            }
            if let Some(binary) = &binary_update {
                let staged = binary.status == BinaryUpdateStatus::Updated
                    && binary.to.as_deref() != Some(BINARY_VERSION);
                if staged {
                    println!();
                    println!(
                        "⚠ Binary update to {} is staged and takes effect on the next invocation.",
                        binary.to.as_deref().unwrap_or("the latest version")
                    );
                } else if !binary.channel.is_self_swappable()
                    && binary.to.as_deref().is_some_and(|latest| {
                        update_available(BINARY_VERSION, latest).unwrap_or(false)
                    })
                {
                    println!();
                    println!(
                        "⚠ Binary update available: {}.",
                        binary.channel.advisory_action()
                    );
                }
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
