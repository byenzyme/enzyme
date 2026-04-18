use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::Database;
use crate::models::EntityType;
use crate::pipeline::entity_map::EntityMap;
use crate::pipeline::profile_assignment;

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
    /// 2. Otherwise, use coverage-aware shortlist from `EntityMap::build` and
    ///    assign profiles deterministically.
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

        let lookup_pairs: Vec<(&str, EntityType)> = prefs
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

        // If config has profile assignments, use those. Otherwise, assign deterministically.
        let entity_profiles = if prefs.entity_profiles.is_empty() {
            profile_assignment::assign_profiles(&found, &HashMap::new(), None)
        } else {
            prefs.entity_profiles
        };

        // Detect expandable folders: use config if specified, otherwise detect from DB.
        let expandable_folders = if !prefs.expandable_folders.is_empty() {
            let entity_map = EntityMap::from_entities(self.db, &found)?;
            prefs
                .expandable_folders
                .iter()
                .filter_map(|name| {
                    entity_map
                        .folder_group(name)
                        .map(|fg| (name.clone(), fg.page_entities.clone()))
                })
                .collect()
        } else {
            Self::detect_expandable_folders(self.db, &found)?
        };

        // Expand: add page entity children to the entity list
        Self::expand_folder_children(self.db, &expandable_folders, &mut found)?;

        info!(
            count = found.len(),
            profiles = entity_profiles.len(),
            expandable = expandable_folders.len(),
            "selected entities from config"
        );

        Ok(SelectionResult {
            entities: found,
            entity_profiles,
            expandable_folders,
            source: SelectionSource::Config,
        })
    }

    /// Select entities using coverage-aware shortlist + deterministic profiles.
    fn select_shortlist(&self) -> Result<SelectionResult> {
        // EntityMap::build handles shortlist + scoped page entity detection
        let entity_map = EntityMap::build(self.db, 20)?;
        let mut entities: Vec<_> = entity_map.all_entities().into_iter().cloned().collect();

        let expandable_folders: HashMap<String, Vec<String>> = entity_map
            .folders
            .iter()
            .filter(|fg| !fg.page_entities.is_empty())
            .map(|fg| (fg.entity.name.clone(), fg.page_entities.clone()))
            .collect();

        // Expand: add page entity children to the entity list
        Self::expand_folder_children(self.db, &expandable_folders, &mut entities)?;

        let profiles = profile_assignment::assign_profiles(&entities, &HashMap::new(), None);

        info!(
            count = entities.len(),
            profiles = profiles.len(),
            expandable = expandable_folders.len(),
            "selected entities from shortlist"
        );

        Ok(SelectionResult {
            entities,
            entity_profiles: profiles,
            expandable_folders,
            source: SelectionSource::Shortlist,
        })
    }

    /// Detect folders with page entities (wikilinks that have source files inside).
    fn detect_expandable_folders(
        db: &Database,
        entities: &[crate::models::Entity],
    ) -> Result<HashMap<String, Vec<String>>> {
        let entity_map = EntityMap::from_entities(db, entities)?;
        Ok(entity_map
            .folders
            .iter()
            .filter(|fg| !fg.page_entities.is_empty())
            .map(|fg| (fg.entity.name.clone(), fg.page_entities.clone()))
            .collect())
    }

    /// Maximum page entity children to catalyze per expandable folder.
    const MAX_FOLDER_CHILDREN: usize = 10;

    /// Add page entity children from expandable folders to the entity list.
    /// Caps at the top N most recent children per folder to control LLM cost.
    fn expand_folder_children(
        db: &Database,
        expandable: &HashMap<String, Vec<String>>,
        entities: &mut Vec<crate::models::Entity>,
    ) -> Result<()> {
        let existing: HashSet<String> = entities.iter().map(|e| e.name.clone()).collect();

        for children in expandable.values() {
            let lookup: Vec<(&str, EntityType)> = children
                .iter()
                .filter(|name| !existing.contains(name.as_str()))
                .map(|name| (name.as_str(), EntityType::Link))
                .collect();

            if !lookup.is_empty() {
                let mut found = db.get_entities_by_names(&lookup)?;
                // Keep only the most recently active children
                found.sort_by(|a, b| {
                    b.trends
                        .recency_score
                        .partial_cmp(&a.trends.recency_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                found.truncate(Self::MAX_FOLDER_CHILDREN);
                entities.extend(found);
            }
        }

        Ok(())
    }
}
