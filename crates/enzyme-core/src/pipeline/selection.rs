use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::config::enzyme_config::{EnzymeConfig, TrendingConfig};
use crate::db::{Database, Repository};
use crate::models::{Entity, EntityType};

/// How entities were selected.
#[derive(Debug, Clone)]
pub enum SelectionSource {
    /// Entities came from `enzyme-config.yaml` catalyzedEntities.
    Config,
    /// Entities came from trending analysis with type-based distribution.
    Trending,
}

/// Result of entity selection.
#[derive(Debug, Clone)]
pub struct SelectionResult {
    /// The entities selected for catalyst generation.
    pub entities: Vec<Entity>,
    /// Per-entity catalyst profile overrides (entity name -> profile name).
    pub entity_profiles: HashMap<String, String>,
    /// How the entities were selected.
    pub source: SelectionSource,
}

/// Selects entities for catalyst generation, either from user config or trending.
pub struct SelectionPipeline<'a> {
    repo: Repository<'a>,
    vault_path: &'a Path,
}

impl<'a> SelectionPipeline<'a> {
    pub fn new(db: &'a Database, vault_path: &'a Path) -> Self {
        Self {
            repo: Repository::new(db),
            vault_path,
        }
    }

    /// Run entity selection.
    ///
    /// 1. Try loading `enzyme-config.yaml` — if it has curated entities, use those.
    /// 2. Otherwise, use trending entities with type-based distribution.
    pub fn run(&self) -> Result<SelectionResult> {
        let config = EnzymeConfig::load(self.vault_path)?;

        if let Some(ref cfg) = config {
            if cfg.has_curated_entities() {
                info!("selecting entities from enzyme-config.yaml");
                return self.select_from_config(cfg);
            }
        }

        let trending_config = config
            .as_ref()
            .and_then(|c| c.trending.clone())
            .unwrap_or_default();

        info!("selecting entities from trending analysis");
        self.select_trending(&trending_config)
    }

    /// Select entities from user-curated config.
    fn select_from_config(&self, config: &EnzymeConfig) -> Result<SelectionResult> {
        let prefs = config.parse_preferences();

        // Build lookup pairs
        let lookup_pairs: Vec<(&str, EntityType)> = prefs
            .entities
            .iter()
            .map(|e| (e.name.as_str(), e.entity_type.clone()))
            .collect();

        let mut found = self.repo.get_entities_by_names(&lookup_pairs)?;

        // Build a set of excluded names for filtering
        let excluded: std::collections::HashSet<String> = prefs
            .excluded_names
            .iter()
            .map(|e| e.name.clone())
            .collect();

        found.retain(|e| !excluded.contains(&e.name));

        // For entities not found in DB, create placeholder entries so they still
        // get passed to catalyst generation (they may have occurrences but just
        // weren't in the top entities).
        let found_names: std::collections::HashSet<String> =
            found.iter().map(|e| e.name.clone()).collect();

        for parsed in &prefs.entities {
            if !found_names.contains(&parsed.name) && !excluded.contains(&parsed.name) {
                debug!(name = parsed.name.as_str(), "config entity not found in DB, skipping");
            }
        }

        info!(
            count = found.len(),
            profiles = prefs.entity_profiles.len(),
            "selected entities from config"
        );

        Ok(SelectionResult {
            entities: found,
            entity_profiles: prefs.entity_profiles,
            source: SelectionSource::Config,
        })
    }

    /// Select trending entities with guaranteed type diversity.
    fn select_trending(&self, config: &TrendingConfig) -> Result<SelectionResult> {
        let entities = self.repo.get_trending_entities(
            config.minimum_tags,
            config.minimum_links,
            config.minimum_folders,
            config.total_limit,
        )?;

        info!(count = entities.len(), "selected trending entities");

        Ok(SelectionResult {
            entities,
            entity_profiles: HashMap::new(),
            source: SelectionSource::Trending,
        })
    }
}
