use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::info;

use enzyme_core::config::enzyme_config::EnzymeConfig;
use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::llm::{DEFAULT_FORMAT_KEY, DEFAULT_PROFILE_KEY, LlmClient};
use enzyme_core::pipeline::catalysts::{CatalystConfig, CatalystPipeline, calculate_allocations};
use enzyme_core::pipeline::context::ContextBuilder;
use enzyme_core::pipeline::selection::SelectionPipeline;

/// Dump catalyst prompts to JSONL for training data generation.
pub async fn run_dump_prompts(
    vault_path: &Path,
    output: &PathBuf,
    guide: Option<String>,
    min_top_catalysts: Option<usize>,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;

    let enzyme_config = EnzymeConfig::load(vault_path)?.unwrap_or_default();

    let selection = SelectionPipeline::new(&db, vault_path).run()?;
    let entities = selection.entities;

    info!(
        entities = entities.len(),
        "selected entities for prompt dump"
    );

    // Vault context
    let vault_guide = if let Some(g) = guide {
        Some(g)
    } else {
        let ctx_builder = ContextBuilder::new(&db);
        match ctx_builder.build(&entities, 5, 300) {
            Ok(ctx) => Some(ctx),
            Err(_) => None,
        }
    };

    let effective_min_top = min_top_catalysts.or_else(|| {
        enzyme_config
            .trending
            .as_ref()
            .and_then(|t| t.min_top_catalysts)
    });
    let allocations = calculate_allocations(&entities, effective_min_top);

    let cat_config = CatalystConfig {
        entity_profiles: &selection.entity_profiles,
        catalysts_per_entity: 7,
        default_profile: DEFAULT_PROFILE_KEY,
        format_key: enzyme_config
            .catalyst_format
            .as_deref()
            .unwrap_or(DEFAULT_FORMAT_KEY),
        vault_guide: vault_guide.as_deref(),
        model: "dump",
        per_entity_allocations: Some(&allocations),
        max_context_chars: 0,
        max_samples: 20,
        concurrency: None,
    };

    let llm = LlmClient::with_base_url("dummy", "http://localhost:0");
    let cat_pipeline = CatalystPipeline::new(&db, &llm, None);

    let count = cat_pipeline.dump_prompts(&entities, &cat_config, output)?;

    eprintln!("Wrote {} entity prompts to {}", count, output.display());

    Ok(())
}
