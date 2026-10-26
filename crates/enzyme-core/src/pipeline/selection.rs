use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::config::enzyme_config::EnzymeConfig;
use crate::db::Database;
use crate::models::EntityType;
use crate::pipeline::entity_map::EntityMap;
use crate::pipeline::exclusions::FolderExclusions;
use crate::pipeline::selection_steps;

/// How entities were selected.
#[derive(Debug, Clone)]
pub enum SelectionSource {
    /// Entities came from `enzyme-config.yaml` catalyzedEntities.
    Config,
    /// Entities came from coverage-aware catalyst-candidate selection (no config present).
    Coverage,
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
    /// Why readable readings resolved as they did (empty for other paths).
    pub diagnostics: selection_steps::SelectionDiagnostics,
}

impl SelectionResult {
    /// Whether automatic selection chose this entity: every entity on the
    /// coverage path, and `learn questions automatically` additions on the
    /// readings path.
    pub fn is_automatic(&self, name: &str, entity_type: &str) -> bool {
        match self.source {
            SelectionSource::Coverage => true,
            SelectionSource::Config => self.diagnostics.automatic.as_ref().is_some_and(|a| {
                a.picked
                    .iter()
                    .chain(&a.expanded)
                    .any(|e| e.name == name && e.entity_type == entity_type)
            }),
        }
    }
}

/// `leave out tags|links` as (lowercase name, type).
fn left_out_names(config: &EnzymeConfig) -> HashSet<(String, EntityType)> {
    config
        .parse_preferences()
        .excluded_names
        .into_iter()
        .filter(|e| e.entity_type != EntityType::Folder)
        .map(|e| (e.name.to_lowercase(), e.entity_type))
        .collect()
}

/// Selects entities for catalyst generation, either from user config or coverage selection.
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
    ///    catalyst-candidate selection -> derive expandable folders -> expand child links
    ///    -> assign profiles.
    pub fn run(&self) -> Result<SelectionResult> {
        let config = EnzymeConfig::load(self.vault_path)?;

        self.run_with_config(config.as_ref())
    }

    /// Run selection against an already-frozen config snapshot.
    pub fn run_with_config(&self, config: Option<&EnzymeConfig>) -> Result<SelectionResult> {
        // Build the surface-gate exclusion set once at the config boundary:
        // enzyme's built-in structural defaults merged with the user's `exclude`
        // list (`config.excluded_folders`). This is the "never a catalyst topic"
        // hard tier; it bites on every selection path (coverage + curated), not
        // just the curated one.
        //
        // This enforces the *surface* gate. The same `exclude` list separately
        // feeds the *index* gate in `document::discovery` (via IndexingPipeline),
        // so `exclude` also means "don't index" — the two gates share the list
        // but are enforced independently.
        let exclusions = match config {
            Some(cfg) => FolderExclusions::with_extra(cfg.excluded_folders.iter()),
            None => FolderExclusions::defaults(),
        };

        if let Some(cfg) = config {
            let automatic = cfg.reading_spec.as_ref().and_then(|s| s.automatic.as_ref());
            if cfg.has_curated_entities() || automatic.is_some() {
                info!("selecting entities from enzyme-config.yaml");
                return self.select_from_config(cfg, &exclusions);
            }
        }

        info!("selecting entities from coverage-aware catalyst-candidate selection");
        let left_out = config.map(left_out_names).unwrap_or_default();
        self.select_catalyst_candidates(
            &exclusions,
            &left_out,
            selection_steps::AUTOMATIC_LIMIT,
        )
    }

    /// Select entities from user-curated config.
    fn select_from_config(
        &self,
        config: &EnzymeConfig,
        exclusions: &FolderExclusions,
    ) -> Result<SelectionResult> {
        let prefs = config.parse_preferences();

        let lookup_pairs: Vec<(&str, crate::models::EntityType)> = prefs
            .entities
            .iter()
            .map(|e| (e.name.as_str(), e.entity_type.clone()))
            .collect();

        let mut found = self.db.get_entities_by_names(&lookup_pairs)?;

        // Keyed by name and type: `leave out links ["writing"]` must not drop a
        // `writing` folder or `#writing` tag.
        let excluded: HashSet<(String, EntityType)> = prefs
            .excluded_names
            .iter()
            .map(|e| (e.name.to_lowercase(), e.entity_type.clone()))
            .collect();
        let is_excluded =
            |name: &str, kind: &EntityType| excluded.contains(&(name.to_lowercase(), kind.clone()));

        // Drop entities the user explicitly excluded, and — via the unified
        // structural set — any folder that should never be surfaced even if the
        // user accidentally curated it (e.g. `templates`, `excalidraw`).
        let indexed: std::collections::HashSet<(String, crate::models::EntityType)> = found
            .iter()
            .map(|e| (e.name.clone(), e.entity_type.clone()))
            .collect();
        found.retain(|e| {
            !is_excluded(&e.name, &e.entity_type)
                && !(e.entity_type == crate::models::EntityType::Folder
                    && exclusions.is_excluded(&e.name))
        });

        let found_keys: HashSet<(String, EntityType)> = found
            .iter()
            .map(|e| (e.name.clone(), e.entity_type.clone()))
            .collect();

        let mut missing = Vec::new();
        let mut left_out = Vec::new();
        for parsed in &prefs.entities {
            if found_keys.contains(&(parsed.name.clone(), parsed.entity_type.clone())) {
                continue;
            }
            let kind = match parsed.entity_type {
                crate::models::EntityType::AppendLog => "log",
                ref other => other.as_str(),
            };
            let selector = enzyme_spec::entity_selector(kind, &parsed.name);
            if indexed.contains(&(parsed.name.clone(), parsed.entity_type.clone())) {
                left_out.push(selector);
            } else {
                if !is_excluded(&parsed.name, &parsed.entity_type) {
                    debug!(
                        name = parsed.name.as_str(),
                        "config entity not found in DB, skipping"
                    );
                }
                missing.push(selector);
            }
        }

        let mut materialized = if let Some(spec) = &config.reading_spec {
            selection_steps::build_readable_selection(self.db, found, &prefs.entity_profiles, spec)?
        } else {
            selection_steps::build_curated_selection(self.db, found, &prefs.entity_profiles)?
        };

        // Folder expansion and patterns add links after the leave-outs above;
        // a left-out link stays out however it was reached.
        let before = materialized.final_entities.len();
        materialized
            .final_entities
            .retain(|e| !is_excluded(&e.name, &e.entity_type));
        if materialized.final_entities.len() != before {
            for children in materialized.expandable_folders.values_mut() {
                children.retain(|child| !is_excluded(child, &EntityType::Link));
            }
            materialized
                .expandable_folders
                .retain(|_, children| !children.is_empty());
            materialized.entity_map =
                EntityMap::from_entities(self.db, &materialized.final_entities)?;
        }

        info!(
            count = materialized.final_entities.len(),
            profiles = materialized.entity_profiles.len(),
            expandable = materialized.expandable_folders.len(),
            "selected entities from config"
        );

        materialized.diagnostics.missing = missing;
        materialized.diagnostics.left_out = left_out;

        if let Some(spec) = config
            .reading_spec
            .as_ref()
            .filter(|spec| spec.automatic.is_some())
        {
            // Readings plus automatic selection. Only readable programs can
            // ask for it, and init never writes their selection back.
            selection_steps::add_automatic_selection(
                self.db,
                &mut materialized,
                spec,
                selection_steps::AUTOMATIC_LIMIT,
                exclusions,
                &left_out_names(config),
            )?;
        }

        Ok(SelectionResult {
            entities: materialized.final_entities,
            entity_profiles: materialized.entity_profiles,
            expandable_folders: materialized.expandable_folders,
            source: SelectionSource::Config,
            diagnostics: materialized.diagnostics,
        })
    }

    /// Select entities using coverage-aware catalyst-candidate selection + deterministic profiles.
    fn select_catalyst_candidates(
        &self,
        exclusions: &FolderExclusions,
        left_out: &HashSet<(String, EntityType)>,
        limit: usize,
    ) -> Result<SelectionResult> {
        let materialized = selection_steps::build_catalyst_candidate_selection(
            self.db, limit, exclusions, left_out,
        )?;

        info!(
            count = materialized.final_entities.len(),
            profiles = materialized.entity_profiles.len(),
            expandable = materialized.expandable_folders.len(),
            "selected entities from coverage selection"
        );

        Ok(SelectionResult {
            entities: materialized.final_entities,
            entity_profiles: materialized.entity_profiles,
            expandable_folders: materialized.expandable_folders,
            source: SelectionSource::Coverage,
            diagnostics: materialized.diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::enzyme_config::EnzymeConfig;
    use crate::config::paths::EnzymePaths;
    use crate::db::Database;
    use crate::models::{EntityTrends, EntityType, ROOT_FOLDER_ENTITY};
    use tempfile::TempDir;

    /// Serialize tests that modify ENZYME_HOME to avoid env var races.
    use crate::ENZYME_HOME_TEST_LOCK as ENV_LOCK;

    /// Set up a temp vault with .enzyme dir and return (vault_dir, enzyme_home_dir).
    /// Sets ENZYME_HOME so config.toml goes to the temp dir.
    /// Caller MUST hold ENV_LOCK.
    fn setup_test_env() -> (TempDir, TempDir) {
        let vault = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(vault.path().join(".enzyme")).unwrap();
        // SAFETY: test-only, serialized by ENV_LOCK
        unsafe { std::env::set_var("ENZYME_HOME", home.path()) };
        (vault, home)
    }

    fn restore_env(old: Option<String>) {
        // SAFETY: test-only, serialized by ENV_LOCK
        unsafe {
            match old {
                Some(v) => std::env::set_var("ENZYME_HOME", v),
                None => std::env::remove_var("ENZYME_HOME"),
            }
        }
    }

    fn open_db(vault_path: &std::path::Path) -> Database {
        let paths = EnzymePaths::new(vault_path).unwrap();
        Database::open(&paths.vault_db).unwrap()
    }

    fn insert_entity(db: &Database, name: &str, entity_type: EntityType, frequency: i64) {
        let now = chrono::Utc::now().timestamp_millis();
        let trends = serde_json::to_string(&EntityTrends {
            frequency_recent: frequency,
            recency_score: frequency as f64,
            timeline_buckets: HashMap::new(),
            timeline_weights: HashMap::new(),
        })
        .unwrap();
        db.execute_write(
            &format!(
                "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends, metadata)
                 VALUES ('{name}', '{}', {frequency}, {now}, {now}, '{}', '{{}}')",
                entity_type.as_str(),
                trends.replace('\'', "''"),
            ),
            vec![],
        )
        .unwrap();
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            &format!(
                "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
                 VALUES ('{source_ref}', '{source_ref}', '', 'hash-{source_ref}', {now}, 'test', {now}, 'test', {now})"
            ),
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn selection_uses_shortlist_without_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "ethics", EntityType::Tag, 80);
        insert_entity(&db, "inbox", EntityType::Folder, 60);

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Coverage));
        assert_eq!(result.entities.len(), 3);

        restore_env(old);
    }

    #[test]
    fn selection_uses_config_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "ethics", EntityType::Tag, 80);
        insert_entity(&db, "inbox", EntityType::Folder, 60);
        insert_entity(&db, "pkm", EntityType::Tag, 40);

        // Write TOML with only 2 entities
        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(
                vault.path(),
                vec!["#enzyme".to_string(), "#ethics".to_string()],
            )
            .unwrap();

        // Verify TOML was written and loads back
        let loaded = EnzymeConfig::load(vault.path()).unwrap();
        assert!(loaded.is_some(), "config should load from TOML");
        assert!(
            loaded.as_ref().unwrap().has_curated_entities(),
            "config should have curated entities"
        );

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(
            matches!(result.source, SelectionSource::Config),
            "expected Config source, got {:?}",
            result.source
        );
        assert_eq!(result.entities.len(), 2);
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"enzyme"));
        assert!(names.contains(&"ethics"));
        assert!(!names.contains(&"pkm"));

        restore_env(old);
    }

    #[test]
    fn selection_auto_expands_folder_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        // Create folder with page entities above density threshold
        insert_entity(&db, "people", EntityType::Folder, 20);
        insert_entity(&db, "alice", EntityType::Link, 30);
        insert_entity(&db, "bob", EntityType::Link, 25);
        insert_doc(&db, "people/Alice.md");
        insert_doc(&db, "people/Bob.md");

        // Write TOML with just the folder — no children listed
        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(vault.path(), vec!["folder:people".to_string()])
            .unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Config));
        // Folder + expanded children
        assert!(result.entities.len() > 1);
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"people"));
        assert!(
            names.contains(&"alice") || names.contains(&"bob"),
            "expected at least one child to be expanded, got: {:?}",
            names
        );
        // Expandable folders map should contain people, but the persisted base
        // config remains parent-only; child links are runtime materialization.
        assert!(result.expandable_folders.contains_key("people"));
        let reloaded = EnzymeConfig::load(vault.path()).unwrap().unwrap();
        let prefs = reloaded.parse_preferences();
        assert_eq!(prefs.entities.len(), 1);
        assert_eq!(prefs.entities[0].name, "people");
        assert_eq!(prefs.entities[0].entity_type, EntityType::Folder);

        restore_env(old);
    }

    /// A structural folder the user accidentally curated must be dropped even on
    /// the curated path, via the unified `FolderExclusions` applied to the
    /// candidate set (not only the user's own `excluded_names`).
    #[test]
    fn curated_selection_drops_structural_folder() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "templates", EntityType::Folder, 50);
        insert_entity(&db, "people", EntityType::Folder, 40);

        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(
                vault.path(),
                vec!["folder:templates".to_string(), "folder:people".to_string()],
            )
            .unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Config));
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"people"), "content folder should survive");
        assert!(
            !names.contains(&"templates"),
            "structural folder must be dropped even when curated, got: {names:?}"
        );

        restore_env(old);
    }

    /// A user's `config.excluded_folders` should now also filter a folder on the
    /// curated path's candidate set.
    #[test]
    fn curated_selection_honors_config_excluded_folders() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "archive", EntityType::Folder, 50);
        insert_entity(&db, "people", EntityType::Folder, 40);

        let mut config = EnzymeConfig::default();
        config.excluded_folders = vec!["archive".to_string()];
        config
            .set_auto_selected_entities(
                vault.path(),
                vec!["folder:archive".to_string(), "folder:people".to_string()],
            )
            .unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        // Load the persisted config so excluded_folders round-trips as production would.
        let loaded = EnzymeConfig::load(vault.path()).unwrap();
        let result = pipeline.run_with_config(loaded.as_ref()).unwrap();

        assert!(matches!(result.source, SelectionSource::Config));
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"people"));
        assert!(
            !names.contains(&"archive"),
            "user excluded_folders must be honored on the curated path, got: {names:?}"
        );

        restore_env(old);
    }

    #[test]
    fn selection_uses_root_folder_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, ROOT_FOLDER_ENTITY, EntityType::Folder, 2);

        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(vault.path(), vec!["folder:.".to_string()])
            .unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Config));
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, ROOT_FOLDER_ENTITY);
        assert_eq!(result.entities[0].entity_type, EntityType::Folder);

        restore_env(old);
    }

    #[test]
    fn shortlist_expands_folder_children_for_catalyst_generation() {
        // End-to-end: no config → shortlist path → folder children appear in
        // sel_result.entities (which is what CatalystPipeline receives).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        // A tag and a folder with page entities above density threshold
        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "people", EntityType::Folder, 10);
        insert_entity(&db, "alice", EntityType::Link, 15);
        insert_entity(&db, "bob", EntityType::Link, 12);
        insert_doc(&db, "people/Alice.md");
        insert_doc(&db, "people/Bob.md");

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Coverage));

        // The entity list that would be passed to CatalystPipeline
        // should contain the folder AND its children
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"people"), "folder should be in entities");
        assert!(
            names.contains(&"alice") || names.contains(&"bob"),
            "at least one folder child should be expanded into entities for catalyst generation, got: {:?}",
            names
        );

        restore_env(old);
    }

    #[test]
    fn toml_excluded_entities_are_filtered() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "todo", EntityType::Tag, 80);
        insert_entity(&db, "inbox", EntityType::Folder, 60);

        let config_path = EnzymeConfig::config_path().unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let vault_key = vault.path().canonicalize().unwrap();
        std::fs::write(&config_path, format!(
            "[defaults]\nminimum_tags = 8\nminimum_links = 6\nminimum_folders = 6\ntotal_limit = 30\n\n\
             [vaults.\"{}\"]\nentities = [\"#enzyme\", \"#todo\", \"folder:inbox\"]\n\
             excluded_tags = [\"todo\"]\n",
            vault_key.display()
        )).unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"enzyme"));
        assert!(!names.contains(&"todo"), "excluded tag should be filtered");
        assert!(names.contains(&"inbox"));

        restore_env(old);
    }

    /// `leave out links` names a link, not every entity with that name: a
    /// same-named folder stays, and folder expansion cannot re-add the link.
    #[test]
    fn left_out_link_spares_same_named_folder_and_blocks_expansion() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "writing", EntityType::Folder, 20);
        insert_entity(&db, "writing", EntityType::Link, 30);
        insert_entity(&db, "alice", EntityType::Link, 25);
        insert_doc(&db, "writing/Writing.md");
        insert_doc(&db, "writing/Alice.md");

        let config_path = EnzymeConfig::config_path().unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let vault_key = vault.path().canonicalize().unwrap();
        std::fs::write(
            &config_path,
            format!(
                "[vaults.\"{}\"]\nentities = [\"folder:writing\"]\nexcluded_links = [\"writing\"]\n",
                vault_key.display()
            ),
        )
        .unwrap();

        let result = SelectionPipeline::new(&db, vault.path()).run().unwrap();
        let keys: Vec<(&str, &EntityType)> = result
            .entities
            .iter()
            .map(|e| (e.name.as_str(), &e.entity_type))
            .collect();
        assert!(keys.contains(&("writing", &EntityType::Folder)), "{keys:?}");
        assert!(!keys.contains(&("writing", &EntityType::Link)), "{keys:?}");
        assert!(keys.contains(&("alice", &EntityType::Link)), "{keys:?}");

        restore_env(old);
    }

    #[test]
    fn toml_entities_missing_from_db_are_skipped_gracefully() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        // Only "enzyme" exists in DB — "ethics" is in TOML but not in DB
        insert_entity(&db, "enzyme", EntityType::Tag, 100);

        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(
                vault.path(),
                vec!["#enzyme".to_string(), "#ethics".to_string()],
            )
            .unwrap();

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Config));
        // Only enzyme should be selected — ethics is silently skipped
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "enzyme");

        restore_env(old);
    }

    #[test]
    fn config_path_preserves_user_entity_list() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "ethics", EntityType::Tag, 80);
        insert_entity(&db, "pkm", EntityType::Tag, 40);

        // User wrote TOML with specific entities
        let mut config = EnzymeConfig::default();
        config
            .set_auto_selected_entities(
                vault.path(),
                vec!["#enzyme".to_string(), "#ethics".to_string()],
            )
            .unwrap();

        // Selection uses the config path
        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();
        assert!(matches!(result.source, SelectionSource::Config));

        // Simulate what init does: write back the selected entities
        let entity_refs: Vec<String> = result
            .entities
            .iter()
            .map(|e| format!("#{}", e.name))
            .collect();
        let mut config2 = EnzymeConfig::load(vault.path()).unwrap().unwrap();
        config2
            .set_auto_selected_entities(vault.path(), entity_refs)
            .unwrap();

        // Reload — should still be the same two, not shortlist's three
        let reloaded = EnzymeConfig::load(vault.path()).unwrap().unwrap();
        let prefs = reloaded.parse_preferences();
        let names: Vec<&str> = prefs.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"enzyme"));
        assert!(names.contains(&"ethics"));
        assert!(!names.contains(&"pkm"));

        restore_env(old);
    }
}
