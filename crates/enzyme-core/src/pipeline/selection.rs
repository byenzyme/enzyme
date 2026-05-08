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

        let materialized =
            selection_steps::build_curated_selection(self.db, found, &prefs.entity_profiles)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::enzyme_config::EnzymeConfig;
    use crate::config::paths::EnzymePaths;
    use crate::db::Database;
    use crate::models::{EntityTrends, EntityType, ROOT_FOLDER_ENTITY};
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialize tests that modify ENZYME_HOME to avoid env var races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("ENZYME_HOME").ok();
        let (vault, _home) = setup_test_env();
        let db = open_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100);
        insert_entity(&db, "ethics", EntityType::Tag, 80);
        insert_entity(&db, "inbox", EntityType::Folder, 60);

        let pipeline = SelectionPipeline::new(&db, vault.path());
        let result = pipeline.run().unwrap();

        assert!(matches!(result.source, SelectionSource::Shortlist));
        assert_eq!(result.entities.len(), 3);

        restore_env(old);
    }

    #[test]
    fn selection_uses_config_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
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
        let _lock = ENV_LOCK.lock().unwrap();
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
        // Expandable folders map should contain people
        assert!(result.expandable_folders.contains_key("people"));

        restore_env(old);
    }

    #[test]
    fn selection_uses_root_folder_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
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
        let _lock = ENV_LOCK.lock().unwrap();
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

        assert!(matches!(result.source, SelectionSource::Shortlist));

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
        let _lock = ENV_LOCK.lock().unwrap();
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

    #[test]
    fn toml_entities_missing_from_db_are_skipped_gracefully() {
        let _lock = ENV_LOCK.lock().unwrap();
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
        let _lock = ENV_LOCK.lock().unwrap();
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
