use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::Database;
use crate::pipeline::selection_steps;

/// How entities were selected.
#[derive(Debug, Clone)]
pub enum SelectionSource {
    /// Entities came from `enzyme-config.yaml` catalyzedEntities.
    Config,
    /// Entities came from coverage-aware shortlist (no config present).
    Shortlist,
}

/// Result of entity selection.
#[derive(Debug, Clone)]
pub struct SelectionResult {
    /// The entities selected for catalyst generation.
    pub entities: Vec<crate::models::Entity>,
    /// Per-entity catalyst profile overrides (entity name -> profile name).
    pub entity_profiles: HashMap<String, String>,
    /// Folders whose page entities should get individual catalysts.
    pub expandable_folders: HashMap<String, Vec<String>>,
    /// How the entities were selected.
    pub source: SelectionSource,
}

/// Selects entities for catalyst generation, either from user config or shortlist.
pub struct SelectionPipeline<'a> {
    db: &'a Database,
    vault_path: &'a Path,
}

impl<'a> SelectionPipeline<'a> {
    pub fn new(db: &'a Database, vault_path: &'a Path) -> Self {
        Self { db, vault_path }
    }

    /// Run entity selection.
    ///
    /// 1. Try loading `enzyme-config.yaml` — if it has curated entities, use those
    ///    (profiles come from the config).
    /// 2. Otherwise, use the deterministic step pipeline:
    ///    shortlist -> derive expandable folders -> expand child links
    ///    -> assign profiles.
    pub fn run(&self) -> Result<SelectionResult> {
        let config = EnzymeConfig::load(self.vault_path)?;

        if let Some(ref cfg) = config {
            if cfg.has_curated_entities() {
                info!("selecting entities from enzyme-config.yaml");
                return self.select_from_config(cfg);
            }
        }

        info!("selecting entities from coverage-aware shortlist");
        self.select_shortlist()
    }

    /// Select entities from user-curated config.
    fn select_from_config(&self, config: &EnzymeConfig) -> Result<SelectionResult> {
        let prefs = config.parse_preferences();

        let lookup_pairs: Vec<(&str, crate::models::EntityType)> = prefs
            .entities
            .iter()
            .map(|e| (e.name.as_str(), e.entity_type.clone()))
            .collect();

        let mut found = self.db.get_entities_by_names(&lookup_pairs)?;

        let excluded: std::collections::HashSet<String> = prefs
            .excluded_names
            .iter()
            .map(|e| e.name.clone())
            .collect();

        found.retain(|e| !excluded.contains(&e.name));

        let found_names: std::collections::HashSet<String> =
            found.iter().map(|e| e.name.clone()).collect();

        for parsed in &prefs.entities {
            if !found_names.contains(&parsed.name) && !excluded.contains(&parsed.name) {
                debug!(
                    name = parsed.name.as_str(),
                    "config entity not found in DB, skipping"
                );
            }
        }

        let materialized = selection_steps::build_curated_selection(
            self.db,
            found,
            &prefs.expandable_folders,
            prefs.entity_profiles,
        )?;

        info!(
            count = materialized.final_entities.len(),
            profiles = materialized.entity_profiles.len(),
            expandable = materialized.expandable_folders.len(),
            "selected entities from config"
        );

        Ok(SelectionResult {
            entities: materialized.final_entities,
            entity_profiles: materialized.entity_profiles,
            expandable_folders: materialized.expandable_folders,
            source: SelectionSource::Config,
        })
    }

    /// Select entities using coverage-aware shortlist + deterministic profiles.
    fn select_shortlist(&self) -> Result<SelectionResult> {
        let materialized = selection_steps::build_shortlist_selection(self.db, 20)?;

        info!(
            count = materialized.final_entities.len(),
            profiles = materialized.entity_profiles.len(),
            expandable = materialized.expandable_folders.len(),
            "selected entities from shortlist"
        );

        Ok(SelectionResult {
            entities: materialized.final_entities,
            entity_profiles: materialized.entity_profiles,
            expandable_folders: materialized.expandable_folders,
            source: SelectionSource::Shortlist,
        })
    }
}
