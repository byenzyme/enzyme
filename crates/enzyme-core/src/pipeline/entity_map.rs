//! Materializes a list of entities into a structured entity map.
//!
//! Single source of truth for:
//! - Deduplicating link entities that shadow a tag with the same name
//! - Folding page-entity links into their parent folders
//! - Surfacing significant children (>10% of parent frequency)
//! - Deduplicating child tags that also appear as standalone entries
//!
//! `from_entities` accepts any `&[Entity]` — trending, catalyzed, query-ranked,
//! or config-selected. `from_trending` is a convenience that feeds in the
//! trending set.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::db::Database;
use crate::models::{Entity, EntityType};

/// A structured view of the vault's entity landscape.
#[derive(Debug, Clone)]
pub struct EntityMap {
    /// Tag groups: parent tag with significant children surfaced.
    pub tags: Vec<TagGroup>,
    /// Folder groups: folder with page entities (links that live inside it).
    pub folders: Vec<FolderGroup>,
    /// Standalone links not subsumed by any folder and not shadowed by a tag.
    pub links: Vec<Entity>,
    /// Total file count in the vault.
    pub file_count: usize,
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
}

/// Threshold: a child is "significant" if its frequency is >10% of the parent's.
const CHILD_SIGNIFICANCE_RATIO: f64 = 0.10;
/// Minimum frequency for an all-recent child to be surfaced even below the ratio.
const ALL_RECENT_MIN_FREQUENCY: i64 = 10;

impl EntityMap {
    /// Build an EntityMap from the database's trending entities.
    /// Convenience wrapper around `from_entities`.
    pub fn from_trending(db: &Database, max_entities: usize) -> Result<Self> {
        let entities = db.get_trending_entities(3, 2, 2, max_entities)?;
        Self::from_entities(db, &entities)
    }

    /// Build an EntityMap from an arbitrary list of entities.
    ///
    /// This is the core folding logic. It accepts any entity list —
    /// trending, catalyzed, query-ranked, or manually curated.
    /// The folding decisions (page entities, children, dedup) are
    /// applied uniformly regardless of how the entities were selected.
    pub fn from_entities(db: &Database, entities: &[Entity]) -> Result<Self> {
        let file_count: usize = db.query_read(
            "SELECT COUNT(*) FROM docs",
            vec![],
            |row| row.get::<i64>(0),
        )?.first().copied().unwrap_or(0) as usize;

        // ── Link-tag dedup ──────────────────────────────────────────────
        // If both [[enzyme]] (link) and #enzyme (tag) exist, drop the link.
        // The tag is the canonical entity.
        let tag_names: HashSet<&str> = entities.iter()
            .filter(|e| e.entity_type == EntityType::Tag)
            .map(|e| e.name.as_str())
            .collect();

        let entities: Vec<&Entity> = entities.iter()
            .filter(|e| {
                !(e.entity_type == EntityType::Link && tag_names.contains(e.name.as_str()))
            })
            .collect();

        // ── Page entity detection ───────────────────────────────────────
        // Check all link entities against all folders to find page entities.
        let link_names: Vec<&str> = entities.iter()
            .filter(|e| e.entity_type == EntityType::Link)
            .map(|e| e.name.as_str())
            .collect();

        let folder_structure = db.get_folder_structure()?;
        let mut folder_page_map: HashMap<String, Vec<String>> = HashMap::new();
        if !link_names.is_empty() {
            for folder in &folder_structure {
                let pages = db.get_folder_page_entities(&folder.name, &link_names)
                    .unwrap_or_default();
                if !pages.is_empty() {
                    folder_page_map.insert(folder.name.clone(), pages);
                }
            }
        }

        // Set of all link names subsumed by folders
        let subsumed_links: HashSet<String> = folder_page_map.values()
            .flat_map(|pages| pages.iter().cloned())
            .collect();

        // ── Build tag groups ────────────────────────────────────────────
        let mut folded_children: HashSet<String> = HashSet::new();
        let mut tags = Vec::new();

        let parent_tags: Vec<&Entity> = entities.iter()
            .filter(|e| e.entity_type == EntityType::Tag && !e.name.contains('/'))
            .copied()
            .collect();

        for parent in &parent_tags {
            let all_children = db.get_child_entities(
                &parent.name,
                parent.entity_type.as_str(),
                20,
            ).unwrap_or_default();

            let significant: Vec<Entity> = all_children.into_iter()
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
                first_seen: None,
                last_seen: None,
                trends: Default::default(),
                metadata: serde_json::Value::Null,
            };
            folders.push(FolderGroup {
                entity,
                page_entities: pages,
            });
        }

        // ── Standalone links ────────────────────────────────────────────
        let links: Vec<Entity> = entities.into_iter()
            .filter(|e| {
                e.entity_type == EntityType::Link
                    && !e.name.contains('/')
                    && !subsumed_links.contains(&e.name)
            })
            .cloned()
            .collect();

        Ok(EntityMap { tags, folders, links, file_count })
    }

    /// Names of all links that were subsumed by folders (page entities).
    pub fn subsumed_link_names(&self) -> HashSet<String> {
        self.folders.iter()
            .flat_map(|f| f.page_entities.iter().cloned())
            .collect()
    }

    /// Convenience: build from trending entities.
    pub fn build(db: &Database, max_entities: usize) -> Result<Self> {
        Self::from_trending(db, max_entities)
    }

    /// Get the page entity map (folder name → link names).
    pub fn folder_page_entity_map(&self) -> HashMap<String, Vec<String>> {
        self.folders.iter()
            .filter(|f| !f.page_entities.is_empty())
            .map(|f| (f.entity.name.clone(), f.page_entities.clone()))
            .collect()
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
            first_seen: None,
            last_seen: None,
            trends: EntityTrends {
                frequency_recent: frequency,
                recency_score: frequency as f64,
                timeline_buckets: HashMap::new(),
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
        ).unwrap();
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
        let people = map.folders.iter().find(|f| f.entity.name == "people").unwrap();
        assert_eq!(people.page_entities, vec!["alice"]);
        assert!(map.links.is_empty());
    }

    // ── Significant children ────────────────────────────────────────────

    #[test]
    fn significant_child_above_ratio_is_surfaced() {
        let db = test_db();
        // Insert child entity in DB for get_child_entities to find
        insert_entity(&db, "enzyme", "tag", 100);
        insert_entity(&db, "enzyme/pmf", "tag", 50); // 50% of parent

        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 100),
        ];

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

        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 100),
        ];

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
        insert_doc(&db, "people/Maren Holt.md");
        insert_doc(&db, "people/Ari.md");
        insert_entity(&db, "enzyme", "tag", 200);
        insert_entity(&db, "enzyme/gtm", "tag", 70);

        let entities = vec![
            make_entity("enzyme", EntityType::Tag, 200),
            make_entity("enzyme/gtm", EntityType::Tag, 70),
            make_entity("storytelling", EntityType::Tag, 100),
            make_entity("storytelling", EntityType::Link, 80), // should be deduped
            make_entity("maren holt", EntityType::Link, 50),   // should fold into people/
            make_entity("ari", EntityType::Link, 40),            // should fold into people/
            make_entity("inbox", EntityType::Folder, 500),
        ];

        let map = EntityMap::from_entities(&db, &entities).unwrap();

        // Tags: enzyme (with child gtm), storytelling
        assert_eq!(map.tags.len(), 2);
        let enzyme = map.tags.iter().find(|t| t.entity.name == "enzyme").unwrap();
        assert_eq!(enzyme.significant_children.len(), 1);
        assert_eq!(enzyme.significant_children[0].name, "enzyme/gtm");

        // Folders: inbox (no page entities) + people/ (synthetic, with maren holt + ari)
        assert_eq!(map.folders.len(), 2);
        let people = map.folders.iter().find(|f| f.entity.name == "people").unwrap();
        assert!(people.page_entities.contains(&"maren holt".to_string()));
        assert!(people.page_entities.contains(&"ari".to_string()));

        // Links: storytelling link deduped, maren holt + ari subsumed
        assert!(map.links.is_empty());
    }
}
