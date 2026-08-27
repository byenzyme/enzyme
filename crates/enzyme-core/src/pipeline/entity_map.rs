//! Materializes a list of entities into a structured entity map.
//!
//! Single source of truth for:
//! - Deduplicating link entities that shadow a tag with the same name
//! - Folding page-entity links into their parent folders
//! - Surfacing significant children (>10% of parent frequency)
//! - Deduplicating child tags that also appear as standalone entries
//!
//! `from_entities` accepts any `&[Entity]` — trending, catalyzed, query-ranked,
//! or config-selected. `select_catalyst_candidates` uses coverage-aware greedy selection;
//! `from_entities` accepts any pre-selected list.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::db::Database;
use crate::models::{Entity, EntityType, ROOT_FOLDER_ENTITY};
use crate::pipeline::exclusions::FolderExclusions;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityKey {
    pub name: String,
    pub entity_type: EntityType,
}

impl EntityKey {
    pub fn new(name: impl Into<String>, entity_type: EntityType) -> Self {
        Self {
            name: name.into(),
            entity_type,
        }
    }
}

impl From<&Entity> for EntityKey {
    fn from(entity: &Entity) -> Self {
        Self::new(entity.name.clone(), entity.entity_type.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityDisposition {
    TopLevel,
    FoldedIntoTag { parent_name: String },
    FoldedIntoFolder { folder_name: String },
    ShadowedByTag { tag_name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderOrigin {
    Input,
    Synthetic,
}

/// A structured view of the vault's entity landscape.
#[derive(Debug, Clone)]
pub struct EntityMap {
    /// Tag groups: parent tag with significant children surfaced.
    pub tags: Vec<TagGroup>,
    /// Folder groups: folder with page entities (links that live inside it).
    pub folders: Vec<FolderGroup>,
    /// Standalone links not subsumed by any folder and not shadowed by a tag.
    pub links: Vec<Entity>,
    /// Config-declared single-file append-only logs (`log:` entries).
    pub append_logs: Vec<Entity>,
    /// Total file count in the vault.
    pub file_count: usize,
    /// Structural disposition for each input entity.
    dispositions: HashMap<EntityKey, EntityDisposition>,
}

#[derive(Debug, Clone)]
pub struct TagGroup {
    /// The parent entity.
    pub entity: Entity,
    /// Children with >10% of parent frequency, or all-recent with meaningful count.
    pub significant_children: Vec<Entity>,
}

#[derive(Debug, Clone)]
pub struct FolderGroup {
    /// The folder entity.
    pub entity: Entity,
    /// Links that have source files inside this folder.
    pub page_entities: Vec<String>,
    /// Whether this folder was present in the input or synthesized structurally.
    pub origin: FolderOrigin,
}

/// Threshold: a child is "significant" if its frequency is >10% of the parent's.
const CHILD_SIGNIFICANCE_RATIO: f64 = 0.10;
/// Minimum frequency for an all-recent child to be surfaced even below the ratio.
const ALL_RECENT_MIN_FREQUENCY: i64 = 10;

/// The **soft tier** of the surface gate for folders: "know it, but don't feature
/// it." These are real content the user writes into, but too generic to be a
/// catalyst topic on their own. Unlike [`FolderExclusions`] (the hard tier, which
/// drops everywhere), these are demoted only from *automatic* topic-picking here
/// in `select_catalyst_candidates`; the curated path does not apply them, so a user who
/// explicitly names `daily`/`notes` still gets it. That path-dependence *is* the
/// "curatable middle state" — see `docs/design/attention-and-exclusions.md`.
const GENERIC_FOLDERS: &[&str] = &["daily", "notes", "convos", "journal", "journals"];

/// The soft tier of the surface gate for *tags*: tooling/system tag noise demoted
/// from automatic catalyst-topic selection (same curatable-if-explicit semantic as
/// [`GENERIC_FOLDERS`]). These names mirror the non-dotfile structural folders
/// but apply to the tag entity space, which the folder-side [`FolderExclusions`]
/// does not cover; there is no unified `TagExclusions` yet.
const EXCLUDED_TAGS: &[&str] = &[
    "excalidraw",
    "templates",
    "config",
    "templater",
    "migrations",
];

impl EntityMap {
    /// Build an EntityMap from an arbitrary list of entities.
    ///
    /// This is the core folding logic. It accepts any entity list —
    /// trending, catalyzed, query-ranked, or manually curated.
    /// The folding decisions (page entities, children, dedup) are
    /// applied uniformly regardless of how the entities were selected.
    pub fn from_entities(db: &Database, entities: &[Entity]) -> Result<Self> {
        let file_count: usize = db
            .query_read("SELECT COUNT(*) FROM docs", vec![], |row| row.get::<i64>(0))?
            .first()
            .copied()
            .unwrap_or(0) as usize;

        let mut dispositions: HashMap<EntityKey, EntityDisposition> = HashMap::new();

        // ── Link-tag dedup ──────────────────────────────────────────────
        // If both [[enzyme]] (link) and #enzyme (tag) exist, drop the link.
        // The tag is the canonical entity.
        let tag_names: HashSet<&str> = entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Tag)
            .map(|e| e.name.as_str())
            .collect();

        let entities: Vec<&Entity> = entities
            .iter()
            .filter(|e| {
                if e.entity_type == EntityType::Link && tag_names.contains(e.name.as_str()) {
                    dispositions.insert(
                        EntityKey::from(*e),
                        EntityDisposition::ShadowedByTag {
                            tag_name: e.name.clone(),
                        },
                    );
                    return false;
                }
                dispositions
                    .entry(EntityKey::from(*e))
                    .or_insert(EntityDisposition::TopLevel);
                true
            })
            .collect();

        // ── Page entity detection ───────────────────────────────────────
        // Check all link entities against all folders to find page entities.
        let link_names: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Link)
            .map(|e| e.name.as_str())
            .collect();

        let folder_structure = db.get_folder_structure()?;
        let mut folder_page_map: HashMap<String, Vec<String>> = HashMap::new();
        if !link_names.is_empty() {
            for folder in &folder_structure {
                let pages = db
                    .get_folder_page_entities(&folder.name, &link_names)
                    .unwrap_or_default();
                if !pages.is_empty() {
                    folder_page_map.insert(folder.name.clone(), pages);
                }
            }
        }

        // Set of all link names subsumed by folders
        let subsumed_links: HashSet<String> = folder_page_map
            .values()
            .flat_map(|pages| pages.iter().cloned())
            .collect();

        // ── Build tag groups ────────────────────────────────────────────
        let mut folded_children: HashSet<String> = HashSet::new();
        let mut tags = Vec::new();

        let parent_tags: Vec<&Entity> = entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Tag && !e.name.contains('/'))
            .copied()
            .collect();

        for parent in &parent_tags {
            let all_children = db
                .get_child_entities(&parent.name, parent.entity_type.as_str(), 20)
                .unwrap_or_default();

            let significant: Vec<Entity> = all_children
                .into_iter()
                .filter(|c| {
                    let ratio = c.frequency as f64 / parent.frequency.max(1) as f64;
                    let all_recent = c.trends.frequency_recent == c.frequency;
                    ratio > CHILD_SIGNIFICANCE_RATIO
                        || (all_recent && c.frequency >= ALL_RECENT_MIN_FREQUENCY)
                })
                .collect();

            for child in &significant {
                folded_children.insert(child.name.clone());
            }

            tags.push(TagGroup {
                entity: (*parent).clone(),
                significant_children: significant,
            });
        }

        // Add child tags that are in the input but NOT folded into a parent
        for entity in &entities {
            if entity.entity_type == EntityType::Tag
                && entity.name.contains('/')
                && !folded_children.contains(&entity.name)
            {
                tags.push(TagGroup {
                    entity: (*entity).clone(),
                    significant_children: Vec::new(),
                });
            }
        }

        for entity in &entities {
            if entity.entity_type != EntityType::Tag || !entity.name.contains('/') {
                continue;
            }
            if let Some((parent_name, _)) = entity.name.split_once('/') {
                if folded_children.contains(&entity.name) {
                    dispositions.insert(
                        EntityKey::from(*entity),
                        EntityDisposition::FoldedIntoTag {
                            parent_name: parent_name.to_string(),
                        },
                    );
                }
            }
        }

        // ── Build folder groups ─────────────────────────────────────────
        let mut folders = Vec::new();
        let mut seen_folders: HashSet<String> = HashSet::new();

        // Folders from the input entity list
        for entity in &entities {
            if entity.entity_type == EntityType::Folder && !entity.name.contains('/') {
                let page_entities = folder_page_map.remove(&entity.name).unwrap_or_default();
                seen_folders.insert(entity.name.clone());
                folders.push(FolderGroup {
                    entity: (*entity).clone(),
                    page_entities,
                    origin: FolderOrigin::Input,
                });
            }
        }

        // Folders not in the input but that have page entities (e.g. people/)
        for (folder_name, pages) in folder_page_map {
            if seen_folders.contains(&folder_name) {
                continue;
            }
            let folder_stats = folder_structure.iter().find(|f| f.name == folder_name);
            let entity = Entity {
                id: None,
                name: folder_name,
                entity_type: EntityType::Folder,
                frequency: folder_stats.map(|f| f.file_count).unwrap_or(0),
                ranking_weight: folder_stats.map(|f| f.file_count as f64).unwrap_or(0.0),
                first_seen: None,
                last_seen: None,
                trends: Default::default(),
                metadata: serde_json::Value::Null,
            };
            folders.push(FolderGroup {
                entity,
                page_entities: pages,
                origin: FolderOrigin::Synthetic,
            });
        }

        let folder_name_by_page_entity: HashMap<String, String> = folders
            .iter()
            .flat_map(|folder| {
                folder
                    .page_entities
                    .iter()
                    .cloned()
                    .map(move |page_entity| (page_entity, folder.entity.name.clone()))
            })
            .collect();

        for entity in &entities {
            if entity.entity_type == EntityType::Link {
                if let Some(folder_name) = folder_name_by_page_entity.get(&entity.name) {
                    dispositions.insert(
                        EntityKey::from(*entity),
                        EntityDisposition::FoldedIntoFolder {
                            folder_name: folder_name.clone(),
                        },
                    );
                }
            }
        }

        // ── Standalone links ────────────────────────────────────────────
        let links: Vec<Entity> = entities
            .iter()
            .filter(|e| {
                e.entity_type == EntityType::Link
                    && !e.name.contains('/')
                    && !subsumed_links.contains(&e.name)
            })
            .copied()
            .cloned()
            .collect();

        for entity in &links {
            dispositions.insert(EntityKey::from(entity), EntityDisposition::TopLevel);
        }

        // ── Append-only logs ────────────────────────────────────────────
        let append_logs: Vec<Entity> = entities
            .iter()
            .filter(|e| e.entity_type == EntityType::AppendLog)
            .copied()
            .cloned()
            .collect();

        for entity in &append_logs {
            dispositions.insert(EntityKey::from(entity), EntityDisposition::TopLevel);
        }

        Ok(EntityMap {
            tags,
            folders,
            links,
            append_logs,
            file_count,
            dispositions,
        })
    }

    /// Names of all links that were subsumed by folders (page entities).
    pub fn subsumed_link_names(&self) -> HashSet<String> {
        self.folders
            .iter()
            .flat_map(|f| f.page_entities.iter().cloned())
            .collect()
    }

    /// Coverage-aware greedy selection of catalyst-candidate entities.
    ///
    /// This is the **surface gate** in the attention model
    /// (`docs/design/attention-and-exclusions.md`): given every indexed entity,
    /// pick the set enzyme will generate catalysts about. (Historically named
    /// "shortlist"; it is not a preview — its output is the generation input. It
    /// superseded the recency-only "trending" path.)
    ///
    /// Scores entities by a geometric mean of recency and frequency, then picks
    /// greedily with a marginal coverage signal: each pick's score is adjusted
    /// by what fraction of its files are not yet covered by earlier picks.
    ///
    /// Filtering (child tags, subfolders, system folders) happens here.
    /// Structural folding (children into parents, links into folders) is left
    /// to `from_entities`.
    ///
    /// Two tiers of "don't feature this" apply to candidates:
    /// * `folder_exclusions` — the **hard tier** ([`FolderExclusions`]: built-ins
    ///   + user `config.excluded_folders`); never a topic, anywhere.
    /// * [`GENERIC_FOLDERS`] / [`EXCLUDED_TAGS`] — the **soft tier**; demoted from
    ///   *automatic* picking here, but honored if the user explicitly curates
    ///   them (the curated path does not apply them).
    pub fn select_catalyst_candidates(
        db: &Database,
        limit: usize,
        folder_exclusions: &FolderExclusions,
    ) -> Result<Vec<Entity>> {
        let all = db.get_all_entities()?;
        let entity_file_sets = db.get_entity_file_sets()?;
        let folder_file_sets = db.get_folder_file_sets()?;

        if folder_file_sets.is_empty() {
            if let Some(root) = all.iter().find(|entity| {
                entity.entity_type == EntityType::Folder && entity.name == ROOT_FOLDER_ENTITY
            }) {
                return Ok(vec![root.clone()]);
            }
        }

        // Pre-compute folder link strength (how many active links live in each folder)
        let folder_active_links: HashMap<String, usize> = {
            let link_entities: HashMap<&str, &Entity> = all
                .iter()
                .filter(|e| e.entity_type == EntityType::Link)
                .map(|e| (e.name.as_str(), e))
                .collect();
            let mut strength: HashMap<String, usize> = HashMap::new();
            // Use docs table: for each folder, count how many link entities
            // have a matching filename in that folder
            for (folder, files) in &folder_file_sets {
                let mut active = 0usize;
                for file_path in files {
                    if let Some(slash) = file_path.rfind('/') {
                        let stem = &file_path[slash + 1..];
                        let stem = stem.strip_suffix(".md").unwrap_or(stem).to_lowercase();
                        if let Some(link_entity) = link_entities.get(stem.as_str()) {
                            if link_entity.trends.frequency_recent > 0 {
                                active += 1;
                            }
                        }
                    }
                }
                strength.insert(folder.clone(), active);
            }
            strength
        };

        let generic_folders: HashSet<&str> = GENERIC_FOLDERS.iter().copied().collect();
        let excluded_tags: HashSet<&str> = EXCLUDED_TAGS.iter().copied().collect();

        // Filter to candidates: root-level, non-system, non-generic
        let candidates: Vec<&Entity> = all
            .iter()
            .filter(|e| {
                match e.entity_type {
                    EntityType::Folder => {
                        !e.name.contains('/')
                            && !folder_exclusions.is_excluded(&e.name)
                            && !generic_folders.contains(e.name.to_lowercase().as_str())
                            && !e.name.starts_with('.')
                    }
                    EntityType::Tag => {
                        // Only root tags — EntityMap folds children into parents
                        !e.name.contains('/')
                            && !excluded_tags.contains(e.name.to_lowercase().as_str())
                    }
                    EntityType::Link => !e.name.contains('/') && e.ranking_weight >= 3.0,
                    EntityType::AppendLog => true,
                }
            })
            .collect();

        let mut picked: Vec<Entity> = Vec::new();
        let mut covered_files: HashSet<String> = HashSet::new();

        for _ in 0..limit {
            let picked_keys: HashSet<(&str, &EntityType)> = picked
                .iter()
                .map(|e| (e.name.as_str(), &e.entity_type))
                .collect();

            let mut best: Option<&Entity> = None;
            let mut best_score: f64 = f64::NEG_INFINITY;

            for entity in &candidates {
                if picked_keys.contains(&(entity.name.as_str(), &entity.entity_type)) {
                    continue;
                }

                let recency_sig = entity.trends.recency_score / 100.0;
                let freq_sig = (entity.ranking_weight.max(0.0) + 1.0).ln();
                let freq_recent_sig = (entity.trends.frequency_recent as f64 + 1.0).ln();

                let mut score = 0.7 * (recency_sig.max(0.001) * freq_sig).sqrt()
                    + 0.5 * freq_recent_sig
                    + 0.4 * freq_sig;

                // Type baseline
                match entity.entity_type {
                    EntityType::Folder => {
                        score += 1.0;
                        let active = *folder_active_links
                            .get(&entity.name.to_lowercase())
                            .unwrap_or(&0);
                        score += 0.3 * (active as f64 + 1.0).ln();
                    }
                    EntityType::Tag => {
                        score += 0.8;
                    }
                    // A deliberately declared log is high-signal: above a tag,
                    // just above a folder.
                    EntityType::AppendLog => {
                        score += 1.1;
                    }
                    EntityType::Link => {}
                }

                // Marginal file coverage
                let entity_files: HashSet<&String> = match entity.entity_type {
                    EntityType::Tag | EntityType::Link | EntityType::AppendLog => entity
                        .id
                        .and_then(|id| entity_file_sets.get(&id))
                        .map(|s| s.iter().collect())
                        .unwrap_or_default(),
                    EntityType::Folder => folder_file_sets
                        .get(&entity.name.to_lowercase())
                        .map(|s| s.iter().collect())
                        .unwrap_or_default(),
                };

                if !entity_files.is_empty() {
                    let unique = entity_files
                        .iter()
                        .filter(|f| !covered_files.contains(**f))
                        .count();
                    let marginal = unique as f64 / entity_files.len() as f64;
                    score += 1.2 * marginal + 0.3 * (unique as f64 + 1.0).ln();
                    score -= (0.6 * (1.0 - marginal)).min(0.5);
                }

                if score > best_score {
                    best = Some(entity);
                    best_score = score;
                }
            }

            let Some(winner) = best else { break };
            picked.push(winner.clone());

            // Update coverage
            match winner.entity_type {
                EntityType::Tag | EntityType::Link | EntityType::AppendLog => {
                    if let Some(id) = winner.id {
                        if let Some(files) = entity_file_sets.get(&id) {
                            covered_files.extend(files.iter().cloned());
                        }
                    }
                }
                EntityType::Folder => {
                    if let Some(files) = folder_file_sets.get(&winner.name.to_lowercase()) {
                        covered_files.extend(files.iter().cloned());
                    }
                }
            }
        }

        Ok(picked)
    }

    /// Get the page entity map (folder name → link names).
    pub fn folder_page_entity_map(&self) -> HashMap<String, Vec<String>> {
        self.folders
            .iter()
            .filter(|f| !f.page_entities.is_empty())
            .map(|f| (f.entity.name.clone(), f.page_entities.clone()))
            .collect()
    }

    pub fn disposition_for(&self, entity: &Entity) -> EntityDisposition {
        self.dispositions
            .get(&EntityKey::from(entity))
            .cloned()
            .unwrap_or(EntityDisposition::TopLevel)
    }

    pub fn disposition_for_key(&self, key: &EntityKey) -> Option<&EntityDisposition> {
        self.dispositions.get(key)
    }

    pub fn folder_group(&self, folder_name: &str) -> Option<&FolderGroup> {
        self.folders
            .iter()
            .find(|folder| folder.entity.name == folder_name)
    }

    pub fn tag_group(&self, tag_name: &str) -> Option<&TagGroup> {
        self.tags.iter().find(|tag| tag.entity.name == tag_name)
    }

    /// All entities as a flat list.
    /// Tags come first (parents, then children), then folders, then links.
    pub fn all_entities(&self) -> Vec<&Entity> {
        let mut out = Vec::new();
        for tg in &self.tags {
            out.push(&tg.entity);
            for child in &tg.significant_children {
                out.push(child);
            }
        }
        for fg in &self.folders {
            out.push(&fg.entity);
        }
        for link in &self.links {
            out.push(link);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::EntityTrends;

    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
    }

    fn make_entity(name: &str, etype: EntityType, frequency: i64) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type: etype,
            frequency,
            ranking_weight: frequency as f64,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: frequency,
                recency_score: frequency as f64,
                timeline_buckets: HashMap::new(),
                timeline_weights: HashMap::new(),
            },
            metadata: serde_json::Value::Null,
        }
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        use libsql::Value as V;
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
             VALUES (?1, ?2, '', 'hash', ?3, 'test', ?3, 'test', ?3)",
            vec![
                V::Text(source_ref.to_string()),
                V::Text(source_ref.to_string()),
                V::Integer(now),
            ],
        ).unwrap();
    }

    fn insert_entity(db: &Database, name: &str, etype: &str, frequency: i64) {
        use libsql::Value as V;
        let now = chrono::Utc::now().timestamp_millis();
        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends)
             VALUES (?1, ?2, ?3, ?4, ?4, '{}')",
            vec![
                V::Text(name.to_string()),
                V::Text(etype.to_string()),
                V::Integer(frequency),
                V::Integer(now),
            ],
        )
        .unwrap();
    }

    // ── Link-tag dedup ──────────────────────────────────────────────────

    #[test]
    fn link_shadowed_by_tag_is_dropped() {
        let db = test_db();
        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 100),
            make_entity("enzyme", EntityType::Link, 80),
            make_entity("alice", EntityType::Link, 50),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.tags.len(), 1);
        assert_eq!(map.tags[0].entity.name, "enzyme");
        // "enzyme" link should be gone, "alice" should remain
        assert_eq!(map.links.len(), 1);
        assert_eq!(map.links[0].name, "alice");
        assert_eq!(
            map.disposition_for(&entities[1]),
            EntityDisposition::ShadowedByTag {
                tag_name: "enzyme".to_string(),
            }
        );
    }

    #[test]
    fn link_without_tag_counterpart_survives() {
        let db = test_db();
        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 100),
            make_entity("alice", EntityType::Link, 50),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.links.len(), 1);
        assert_eq!(map.links[0].name, "alice");
    }

    #[test]
    fn shortlist_uses_root_as_sole_entity_for_flat_vault() {
        let db = test_db();
        insert_doc(&db, "note-a.md");
        insert_doc(&db, "note-b.md");
        insert_entity(&db, ROOT_FOLDER_ENTITY, "folder", 2);
        insert_entity(&db, "topic", "tag", 10);

        let selected =
            EntityMap::select_catalyst_candidates(&db, 20, &FolderExclusions::defaults()).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].entity_type, EntityType::Folder);
        assert_eq!(selected[0].name, ROOT_FOLDER_ENTITY);
    }

    /// Regression for the historical drift: `excalidraw` was in the folding
    /// path's `EXCLUDED_FOLDERS` but not the trending path's list. The unified
    /// `FolderExclusions::defaults()` must keep it out of the shortlist while a
    /// normal content folder still surfaces.
    #[test]
    fn shortlist_filters_excalidraw_via_unified_defaults() {
        let db = test_db();
        insert_doc(&db, "projects/a.md");
        insert_doc(&db, "projects/b.md");
        insert_doc(&db, "excalidraw/x.md");
        insert_doc(&db, "excalidraw/y.md");
        insert_entity(&db, "projects", "folder", 2);
        insert_entity(&db, "excalidraw", "folder", 2);

        let selected =
            EntityMap::select_catalyst_candidates(&db, 20, &FolderExclusions::defaults()).unwrap();

        let names: HashSet<&str> = selected.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains("projects"), "content folder should surface");
        assert!(
            !names.contains("excalidraw"),
            "excalidraw is structural and must be excluded (drift regression)"
        );
    }

    /// The user's `config.excluded_folders` must now bite on the shortlist path,
    /// not only the curated path. Threading `with_extra` here is the whole point
    /// of the migration.
    #[test]
    fn shortlist_honors_config_excluded_folders() {
        let db = test_db();
        insert_doc(&db, "projects/a.md");
        insert_doc(&db, "projects/b.md");
        insert_doc(&db, "archive/x.md");
        insert_doc(&db, "archive/y.md");
        insert_entity(&db, "projects", "folder", 2);
        insert_entity(&db, "archive", "folder", 2);

        // Without config: both content folders are eligible.
        let defaults =
            EntityMap::select_catalyst_candidates(&db, 20, &FolderExclusions::defaults()).unwrap();
        let default_names: HashSet<&str> = defaults.iter().map(|e| e.name.as_str()).collect();
        assert!(default_names.contains("archive"));

        // With `excluded_folders: [archive]`, the user config now filters it here.
        let exclusions = FolderExclusions::with_extra(["archive"]);
        let selected = EntityMap::select_catalyst_candidates(&db, 20, &exclusions).unwrap();
        let names: HashSet<&str> = selected.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains("projects"));
        assert!(
            !names.contains("archive"),
            "user excluded_folders must be honored on the shortlist path"
        );
    }

    // ── Page entity folding ─────────────────────────────────────────────

    #[test]
    fn link_with_source_file_in_folder_is_subsumed() {
        let db = test_db();
        // Create a doc in people/ matching the link name
        insert_doc(&db, "people/Alice Smith.md");

        let entities = vec![
            make_entity("people", EntityType::Folder, 50),
            make_entity("alice smith", EntityType::Link, 30),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.folders.len(), 1);
        assert_eq!(map.folders[0].entity.name, "people");
        assert_eq!(map.folders[0].page_entities, vec!["alice smith"]);
        assert!(map.links.is_empty(), "alice smith should be subsumed");
        assert_eq!(
            map.disposition_for(&entities[1]),
            EntityDisposition::FoldedIntoFolder {
                folder_name: "people".to_string(),
            }
        );
    }

    #[test]
    fn folder_without_page_entities_has_empty_list() {
        let db = test_db();
        let entities = vec![
            make_entity("inbox", EntityType::Folder, 500),
            make_entity("random concept", EntityType::Link, 30),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.folders.len(), 1);
        assert!(map.folders[0].page_entities.is_empty());
        assert_eq!(map.links.len(), 1);
    }

    #[test]
    fn non_trending_folder_added_when_it_has_page_entities() {
        let db = test_db();
        insert_doc(&db, "people/Alice.md");

        // people/ is NOT in the entity list, but alice is
        let entities = vec![
            make_entity("inbox", EntityType::Folder, 500),
            make_entity("alice", EntityType::Link, 30),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        // people/ should be added synthetically
        assert_eq!(map.folders.len(), 2);
        let people = map
            .folders
            .iter()
            .find(|f| f.entity.name == "people")
            .unwrap();
        assert_eq!(people.page_entities, vec!["alice"]);
        assert_eq!(people.origin, FolderOrigin::Synthetic);
        assert!(map.links.is_empty());
    }

    // ── Significant children ────────────────────────────────────────────

    #[test]
    fn significant_child_above_ratio_is_surfaced() {
        let db = test_db();
        // Insert child entity in DB for get_child_entities to find
        insert_entity(&db, "enzyme", "tag", 100);
        insert_entity(&db, "enzyme/pmf", "tag", 50); // 50% of parent

        let entities = vec![make_entity("enzyme", EntityType::Tag, 100)];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.tags.len(), 1);
        assert_eq!(map.tags[0].significant_children.len(), 1);
        assert_eq!(map.tags[0].significant_children[0].name, "enzyme/pmf");
    }

    #[test]
    fn insignificant_child_below_ratio_is_not_surfaced() {
        let db = test_db();
        insert_entity(&db, "enzyme", "tag", 100);
        insert_entity(&db, "enzyme/tiny", "tag", 5); // 5% of parent

        let entities = vec![make_entity("enzyme", EntityType::Tag, 100)];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        assert_eq!(map.tags.len(), 1);
        assert!(map.tags[0].significant_children.is_empty());
    }

    #[test]
    fn child_tag_in_input_is_folded_into_parent() {
        let db = test_db();
        insert_entity(&db, "enzyme", "tag", 100);
        insert_entity(&db, "enzyme/pmf", "tag", 50);

        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 100),
            make_entity("enzyme/pmf", EntityType::Tag, 50),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        // enzyme/pmf should appear as a child of enzyme, not as a standalone tag
        assert_eq!(map.tags.len(), 1);
        assert_eq!(map.tags[0].entity.name, "enzyme");
        assert_eq!(map.tags[0].significant_children.len(), 1);
        assert_eq!(
            map.disposition_for(&entities[1]),
            EntityDisposition::FoldedIntoTag {
                parent_name: "enzyme".to_string(),
            }
        );
    }

    #[test]
    fn orphan_child_tag_without_parent_stays_standalone() {
        let db = test_db();
        // enzyme/pmf is in the input but enzyme is NOT
        let entities = vec![
            make_entity("enzyme/pmf", EntityType::Tag, 50),
            make_entity("ethics", EntityType::Tag, 100),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        // enzyme/pmf should appear as its own tag group
        let names: Vec<&str> = map.tags.iter().map(|t| t.entity.name.as_str()).collect();
        assert!(names.contains(&"enzyme/pmf"));
        assert!(names.contains(&"ethics"));
    }

    // ── Combined scenarios ──────────────────────────────────────────────

    #[test]
    fn full_scenario_dedup_fold_and_children() {
        let db = test_db();
        insert_doc(&db, "people/Alice Jones.md");
        insert_doc(&db, "people/Bob Chen.md");
        insert_entity(&db, "enzyme", "tag", 200);
        insert_entity(&db, "enzyme/gtm", "tag", 70);

        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 200),
            make_entity("enzyme/gtm", EntityType::Tag, 70),
            make_entity("storytelling", EntityType::Tag, 100),
            make_entity("storytelling", EntityType::Link, 80), // should be deduped
            make_entity("alice jones", EntityType::Link, 50),  // should fold into people/
            make_entity("bob chen", EntityType::Link, 40),     // should fold into people/
            make_entity("inbox", EntityType::Folder, 500),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        // Tags: enzyme (with child gtm), storytelling
        assert_eq!(map.tags.len(), 2);
        let enzyme = map.tags.iter().find(|t| t.entity.name == "enzyme").unwrap();
        assert_eq!(enzyme.significant_children.len(), 1);
        assert_eq!(enzyme.significant_children[0].name, "enzyme/gtm");

        // Folders: inbox (no page entities) + people/ (synthetic, with alice jones + bob chen)
        assert_eq!(map.folders.len(), 2);
        let people = map
            .folders
            .iter()
            .find(|f| f.entity.name == "people")
            .unwrap();
        assert!(people.page_entities.contains(&"alice jones".to_string()));
        assert!(people.page_entities.contains(&"bob chen".to_string()));

        // Links: storytelling link deduped, alice jones + bob chen subsumed
        assert!(map.links.is_empty());
    }
}
