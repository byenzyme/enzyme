//! Materializes raw DB trending data into a structured entity map.
//!
//! Single source of truth for:
//! - Folding page-entity links into their parent folders
//! - Surfacing significant children (>10% of parent frequency)
//! - Deduplicating child tags that also appear as standalone trending entries
//!
//! Consumed by: vault_summary (guide/display), petri (query output),
//! context.rs (LLM XML), llm_selection (post-processing).

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::db::Database;
use crate::models::{Entity, EntityType};

/// A structured view of the vault's entity landscape.
#[derive(Debug, Clone)]
pub struct EntityMap {
    /// Tag groups: parent tag with significant children surfaced.
    pub tags: Vec<TagGroup>,
    /// Folder groups: folder with page entities (trending links that live inside it).
    pub folders: Vec<FolderGroup>,
    /// Standalone links not subsumed by any folder.
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
    /// Trending links that have source files inside this folder.
    pub page_entities: Vec<String>,
}

/// Threshold: a child is "significant" if its frequency is >10% of the parent's.
const CHILD_SIGNIFICANCE_RATIO: f64 = 0.10;
/// Minimum frequency for an all-recent child to be surfaced even below the ratio.
const ALL_RECENT_MIN_FREQUENCY: i64 = 10;

impl EntityMap {
    /// Build an EntityMap from the database's trending entities.
    ///
    /// This is the canonical way to get a structured view of the vault.
    /// All consumers should use this instead of calling `get_trending_entities`
    /// and doing their own folding.
    pub fn build(db: &Database, max_entities: usize) -> Result<Self> {
        let file_count: usize = db.query_read(
            "SELECT COUNT(*) FROM docs",
            vec![],
            |row| row.get::<i64>(0),
        )?.first().copied().unwrap_or(0) as usize;

        let trending = db.get_trending_entities(3, 2, 2, max_entities)?;

        // Collect trending link names for page entity detection
        let trending_link_names: Vec<&str> = trending.iter()
            .filter(|e| e.entity_type == EntityType::Link)
            .map(|e| e.name.as_str())
            .collect();

        // Scan ALL top-level folders for page entities (not just trending ones),
        // so we catch folders like people/ that aren't trending but contain
        // trending links.
        let folder_structure = db.get_folder_structure()?;
        let mut folder_page_map: HashMap<String, Vec<String>> = HashMap::new();
        if !trending_link_names.is_empty() {
            for folder in &folder_structure {
                let pages = db.get_folder_page_entities(&folder.name, &trending_link_names)
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

        // Set of child tag names that get folded into their parent
        let mut folded_children: HashSet<String> = HashSet::new();

        // ── Build tag groups ────────────────────────────────────────────────
        let mut tags = Vec::new();
        let parent_tags: Vec<&Entity> = trending.iter()
            .filter(|e| e.entity_type == EntityType::Tag && !e.name.contains('/'))
            .collect();

        for parent in &parent_tags {
            let all_children = db.get_child_entities(
                &parent.name,
                parent.entity_type.as_str(),
                20, // fetch more, we'll filter
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

        // Add child tags that are trending on their own but NOT folded into a parent
        // (e.g. if the parent didn't make the trending cut)
        for entity in &trending {
            if entity.entity_type == EntityType::Tag
                && entity.name.contains('/')
                && !folded_children.contains(&entity.name)
            {
                tags.push(TagGroup {
                    entity: entity.clone(),
                    significant_children: Vec::new(),
                });
            }
        }

        // ── Build folder groups ─────────────────────────────────────────────
        let mut folders = Vec::new();
        let mut seen_folders: HashSet<String> = HashSet::new();

        // First: trending folders
        for entity in &trending {
            if entity.entity_type == EntityType::Folder && !entity.name.contains('/') {
                let page_entities = folder_page_map.remove(&entity.name).unwrap_or_default();
                seen_folders.insert(entity.name.clone());
                folders.push(FolderGroup {
                    entity: entity.clone(),
                    page_entities,
                });
            }
        }

        // Second: non-trending folders that have page entities (e.g. people/)
        for (folder_name, pages) in folder_page_map {
            if seen_folders.contains(&folder_name) {
                continue;
            }
            // Look up folder stats for frequency
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

        // ── Standalone links ────────────────────────────────────────────────
        let links: Vec<Entity> = trending.into_iter()
            .filter(|e| {
                e.entity_type == EntityType::Link
                    && !e.name.contains('/')
                    && !subsumed_links.contains(&e.name)
            })
            .collect();

        Ok(EntityMap { tags, folders, links, file_count })
    }

    /// Get the page entity map (folder name → link names) for use by
    /// context.rs and llm_selection post-processing.
    pub fn folder_page_entity_map(&self) -> HashMap<String, Vec<String>> {
        self.folders.iter()
            .filter(|f| !f.page_entities.is_empty())
            .map(|f| (f.entity.name.clone(), f.page_entities.clone()))
            .collect()
    }

    /// All entities as a flat list (for consumers that need it).
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
