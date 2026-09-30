use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;
use tracing::{info, warn};

use crate::db::Database;
use crate::db::repository::{EntitySelectionStats, PAGE_LINK_MIN_FREQUENCY, is_active_page_link};
use crate::models::{Entity, EntityType};
use crate::pipeline::entity_map::EntityMap;
use crate::pipeline::exclusions::FolderExclusions;
use crate::pipeline::profile_assignment;

/// Maximum page-entity children to materialize per expandable folder.
pub const MAX_FOLDER_CHILDREN: usize = 10;
/// Number of page-entity children to surface as catalyzed/expandable hints.
pub const CATALYZED_FOLDER_CHILD_LIMIT: usize = 3;

/// Minimum fraction of a folder's files that must map to active page-entity
/// links before the folder is treated as expandable by auto-detection.
pub const EXPANDABLE_DENSITY_THRESHOLD: f64 = 0.05;

/// Shared deterministic selection result after running the step pipeline.
pub struct MaterializedSelection {
    /// Base entities before folder-child expansion.
    pub base_entities: Vec<Entity>,
    /// Structural view over the final entity set.
    pub entity_map: EntityMap,
    /// Expandable folders and their capped child link names.
    pub expandable_folders: HashMap<String, Vec<String>>,
    /// Final entity list after folder-child expansion.
    pub final_entities: Vec<Entity>,
    /// Final per-entity profile assignment map.
    pub entity_profiles: HashMap<String, String>,
    /// Why readable folder expansions and name patterns resolved as they did.
    /// Only the readable path fills this; other paths leave it empty.
    pub diagnostics: SelectionDiagnostics,
}

/// Explanations collected while a readable selection runs, so previews can
/// report what selection did without re-deriving it or parsing logs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SelectionDiagnostics {
    /// The resolved `as of` instant; `None` means the wall clock.
    pub reference_ms: Option<i64>,
    /// Per folder in scope (lowercase entity name).
    pub folders: BTreeMap<String, FolderExpansion>,
    /// One entry per name-pattern reading, in declaration order.
    pub patterns: Vec<PatternDiagnostics>,
    /// One source-scoped `including who links` expansion per reading.
    pub who_links: Vec<PatternDiagnostics>,
    /// Explicit readings whose entity is not in the index.
    pub missing: Vec<String>,
    /// Explicit readings dropped by `leave out` or structural exclusions.
    pub left_out: Vec<String>,
}

/// How one folder's linked pages were considered for expansion.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FolderExpansion {
    /// Direct child pages whose name is an indexed link.
    pub linked_pages: usize,
    /// Of those, linked fewer than [`PAGE_LINK_MIN_FREQUENCY`] times.
    pub below_frequency: usize,
    /// Frequent enough, but without cached activity in the year before the
    /// index-time reference instant.
    pub inactive: usize,
    /// Passed both gates.
    pub eligible: usize,
    /// Eligible pages per folder document.
    pub density: f64,
    /// `including linked pages` bypasses the density gate.
    pub density_exempt: bool,
    /// Eligible pages exist, but the folder was below the density gate.
    pub below_density: bool,
    /// Selected children in rank order.
    pub selected: Vec<ScoredEntity>,
}

/// How one `tags|links matching` reading resolved.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PatternDiagnostics {
    /// The reading's entity selector, e.g. `#auto-cluster-*`.
    pub entity: String,
    pub matched: usize,
    /// Matches removed by `leave out tags|links`.
    pub left_out: usize,
    /// Matches that have their own explicit reading.
    pub explicit: usize,
    /// Matches with no occurrence at or before the reference instant.
    pub undated: usize,
    pub eligible: usize,
    pub selected: Vec<ScoredEntity>,
}

/// A selected entity with the live selection scores used to rank it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ScoredEntity {
    pub name: String,
    pub entity_type: String,
    /// Distinct linked source documents (the `by frequency` score).
    pub documents: usize,
    /// 30-day half-life activity (the `by recency` score).
    pub recency: f64,
}

fn scored(entities: &[Entity], stats: &HashMap<i64, EntitySelectionStats>) -> Vec<ScoredEntity> {
    entities
        .iter()
        .map(|e| {
            let live = e.id.and_then(|id| stats.get(&id));
            ScoredEntity {
                name: e.name.clone(),
                entity_type: e.entity_type.as_str().to_string(),
                documents: live.map_or(0, |s| s.documents),
                recency: live.map_or(0.0, |s| s.recency),
            }
        })
        .collect()
}

/// Build the default deterministic selection pipeline:
/// catalyst-candidate selection -> detect expandable folders -> expand child links -> assign profiles.
///
/// `folder_exclusions` is the unified structural-exclusion set threaded from the
/// config boundary (see `docs/design/entity-exclusions-refactor.md`); it is what
/// makes the user's `config.excluded_folders` bite on the catalyst-candidate path.
pub fn build_catalyst_candidate_selection(
    db: &Database,
    limit: usize,
    folder_exclusions: &FolderExclusions,
) -> Result<MaterializedSelection> {
    let base_entities = EntityMap::select_catalyst_candidates(db, limit, folder_exclusions)?;
    let expandable_folders = auto_expandable_folders(db, &base_entities)?;
    let final_entities = expand_with_folder_children(db, &base_entities, &expandable_folders)?;
    let entity_map = EntityMap::from_entities(db, &final_entities)?;
    let entity_profiles =
        profile_assignment::assign_profiles(&final_entities, &HashMap::new(), None);

    Ok(MaterializedSelection {
        base_entities,
        entity_map,
        expandable_folders,
        final_entities,
        entity_profiles,
        diagnostics: SelectionDiagnostics::default(),
    })
}

/// Build the curated selection pipeline:
/// curated entities -> detect expandable folders -> expand child links -> assign profiles.
pub fn build_curated_selection(
    db: &Database,
    base_entities: Vec<Entity>,
    profile_overrides: &HashMap<String, String>,
) -> Result<MaterializedSelection> {
    let expandable_folders = auto_expandable_folders(db, &base_entities)?;
    let final_entities = expand_with_folder_children(db, &base_entities, &expandable_folders)?;
    let entity_map = EntityMap::from_entities(db, &final_entities)?;
    let mut entity_profiles =
        profile_assignment::assign_profiles(&final_entities, &HashMap::new(), None);
    for entity in &final_entities {
        if let Some(profile) = profile_overrides.get(&entity.name) {
            entity_profiles.insert(entity.name.clone(), profile.clone());
        }
    }

    Ok(MaterializedSelection {
        base_entities,
        entity_map,
        expandable_folders,
        final_entities,
        entity_profiles,
        diagnostics: SelectionDiagnostics::default(),
    })
}

/// `as_of_ms` is the vault's resolved `as of` instant (see [`crate::pipeline::as_of`]).
pub(crate) fn readable_folder_children(
    db: &Database,
    base: &[Entity],
    spec: &enzyme_spec::Vault,
    as_of_ms: Option<i64>,
) -> Result<HashMap<String, Vec<String>>> {
    Ok(readable_folder_children_explained(db, base, spec, as_of_ms)?.0)
}

/// [`readable_folder_children`] plus a per-folder account of the gates.
fn readable_folder_children_explained(
    db: &Database,
    base: &[Entity],
    spec: &enzyme_spec::Vault,
    as_of_ms: Option<i64>,
) -> Result<(
    HashMap<String, Vec<String>>,
    BTreeMap<String, FolderExpansion>,
)> {
    let policies: HashMap<String, enzyme_spec::Selection> = base
        .iter()
        .filter(|e| e.entity_type == EntityType::Folder)
        .filter_map(|e| {
            spec.reading(&e.name, "folder")
                .and_then(|r| r.learning.selection.clone())
                .map(|p| (e.name.clone(), p))
        })
        .collect();
    let explicit: HashMap<String, i64> = base
        .iter()
        .filter(|e| {
            spec.reading(&e.name, e.entity_type.as_str())
                .is_some_and(|r| r.include_linked_pages)
        })
        .map(|e| (e.name.clone(), e.frequency))
        .collect();
    let density_exempt = explicit.keys().map(String::as_str).collect();
    load_expandable_folder_children(
        db,
        &folder_scope(base),
        &density_exempt,
        &policies,
        as_of_ms,
    )
}

/// One `tags|links matching "…"` reading resolved against the index.
pub(crate) struct PatternExpansion<'a> {
    pub reading: &'a enzyme_spec::Reading,
    /// Indexed names matching the glob, before exclusions and eligibility.
    pub matched: usize,
    /// Matches removed by `leave out tags|links`.
    pub left_out: usize,
    /// Matches that have their own explicit reading.
    pub explicit: usize,
    /// Remaining matches without an occurrence at or before the reference.
    pub undated: usize,
    /// Matches that are not left out, not read explicitly, and occur at or
    /// before the reference instant.
    pub eligible: usize,
    pub selected: Vec<Entity>,
    /// Live selection scores of `selected`, in rank order.
    pub scores: Vec<ScoredEntity>,
}

impl PatternExpansion<'_> {
    pub fn diagnostics(&self) -> PatternDiagnostics {
        PatternDiagnostics {
            entity: self.reading.entity.clone(),
            matched: self.matched,
            left_out: self.left_out,
            explicit: self.explicit,
            undated: self.undated,
            eligible: self.eligible,
            selected: self.scores.clone(),
        }
    }
}

pub(crate) struct WhoLinkExpansion<'a> {
    pub reading: &'a enzyme_spec::Reading,
    pub source_prefix: String,
    pub selected: Vec<Entity>,
    pub diagnostics: PatternDiagnostics,
}

/// Expand the `who` links emitted by each SQLite source, scoring only evidence
/// from that source. A source key is already escaped and resolved in the spec.
pub(crate) fn readable_who_link_children<'a>(
    db: &Database,
    spec: &'a enzyme_spec::Vault,
    as_of_ms: Option<i64>,
) -> Result<Vec<WhoLinkExpansion<'a>>> {
    let mut expansions = Vec::new();
    for reading in spec.readings.iter().filter(|r| r.include_who_links) {
        let (kind, name) = enzyme_spec::split_entity(&reading.entity);
        anyhow::ensure!(
            kind == "collection" && name.starts_with("sqlite:"),
            "including who links needs a resolved SQLite source reading"
        );
        let source_prefix = format!("{name}/");
        let candidates = db.get_sqlite_who_link_candidates(&source_prefix)?;
        let matched = candidates.len();
        let kept: Vec<_> = candidates
            .into_iter()
            .filter(|e| {
                !spec.excluded_links.iter().any(|x| {
                    x.trim_start_matches("[[")
                        .trim_end_matches("]]")
                        .eq_ignore_ascii_case(&e.name)
                })
            })
            .collect();
        let left_out = matched - kept.len();
        let candidates: Vec<_> = kept
            .into_iter()
            .filter(|e| spec.reading(&e.name, "link").is_none())
            .collect();
        let explicit = matched - left_out - candidates.len();
        let ids: Vec<_> = candidates.iter().filter_map(|e| e.id).collect();
        let stats =
            db.get_entity_selection_stats_in_prefix(&ids, as_of_ms, Some(&source_prefix))?;
        let unfiltered = candidates.len();
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|e| e.id.is_some_and(|id| stats.contains_key(&id)))
            .collect();
        let eligible = candidates.len();
        let default_selection = enzyme_spec::Selection {
            amount: enzyme_spec::SelectionAmount::Count {
                count: MAX_FOLDER_CHILDREN,
            },
            by: enzyme_spec::SelectionMode::Frequency,
        };
        let selected = rank_and_limit(
            candidates,
            &stats,
            Some(
                reading
                    .learning
                    .selection
                    .as_ref()
                    .unwrap_or(&default_selection),
            ),
        );
        let diagnostics = PatternDiagnostics {
            entity: reading.entity.clone(),
            matched,
            left_out,
            explicit,
            undated: unfiltered - eligible,
            eligible,
            selected: scored(&selected, &stats),
        };
        expansions.push(WhoLinkExpansion {
            reading,
            source_prefix,
            selected,
            diagnostics,
        });
    }
    Ok(expansions)
}

/// Expand every name-pattern reading, in declaration order. Explicit readings
/// are not candidates: they keep their own policy and consume no pattern slot.
/// There is no density, frequency or recent-year gate; naming the pattern is the
/// opt-in. Without `select`, the folder default applies (legacy ranking, cap of
/// [`MAX_FOLDER_CHILDREN`]).
pub(crate) fn readable_pattern_children<'a>(
    db: &Database,
    spec: &'a enzyme_spec::Vault,
    as_of_ms: Option<i64>,
) -> Result<Vec<PatternExpansion<'a>>> {
    let mut by_type = HashMap::<&str, Vec<Entity>>::new();
    let mut expansions = Vec::new();
    for reading in spec.readings.iter().filter(|r| r.pattern) {
        let (kind, glob) = enzyme_spec::split_entity(&reading.entity);
        let (entity_type, excluded) = match kind {
            "tag" => (EntityType::Tag, &spec.excluded_tags),
            _ => (EntityType::Link, &spec.excluded_links),
        };
        if !by_type.contains_key(kind) {
            by_type.insert(kind, db.get_entities_by_type(&entity_type)?);
        }
        let matches: Vec<&Entity> = by_type[kind]
            .iter()
            .filter(|e| enzyme_spec::glob_matches(glob, &e.name))
            .collect();
        let matched = matches.len();
        let kept: Vec<&Entity> = matches
            .into_iter()
            .filter(|e| {
                !excluded.iter().any(|x| {
                    x.trim_start_matches('#')
                        .trim_start_matches("[[")
                        .trim_end_matches("]]")
                        .eq_ignore_ascii_case(&e.name)
                })
            })
            .collect();
        let left_out = matched - kept.len();
        let candidates: Vec<Entity> = kept
            .into_iter()
            .filter(|e| spec.reading(&e.name, kind).is_none())
            .cloned()
            .collect();
        let explicit = matched - left_out - candidates.len();
        let ids: Vec<_> = candidates.iter().filter_map(|e| e.id).collect();
        // Stats only exist for entities with an occurrence at or before `as of`.
        let stats = db.get_entity_selection_stats(&ids, as_of_ms)?;
        let unfiltered = candidates.len();
        let candidates: Vec<Entity> = candidates
            .into_iter()
            .filter(|e| e.id.is_some_and(|id| stats.contains_key(&id)))
            .collect();
        let eligible = candidates.len();
        let selected = rank_and_limit(candidates, &stats, reading.learning.selection.as_ref());
        let scores = scored(&selected, &stats);
        expansions.push(PatternExpansion {
            reading,
            matched,
            left_out,
            explicit,
            undated: unfiltered - eligible,
            eligible,
            selected,
            scores,
        });
    }
    Ok(expansions)
}

/// Explicit readable expansion keeps the folder and promotes its linked pages.
/// Child readings inherit the parent's profile unless explicitly configured themselves.
/// Name-pattern readings add their selected entities under the same rules.
pub fn build_readable_selection(
    db: &Database,
    base: Vec<Entity>,
    overrides: &HashMap<String, String>,
    spec: &enzyme_spec::Vault,
) -> Result<MaterializedSelection> {
    let explicit: HashMap<String, i64> = base
        .iter()
        .filter(|e| {
            spec.reading(&e.name, e.entity_type.as_str())
                .is_some_and(|r| r.include_linked_pages)
        })
        .map(|e| (e.name.clone(), e.frequency))
        .collect();
    let as_of_ms = crate::pipeline::as_of::resolve(db, spec.learning.as_of.as_ref())?;
    let (folders, folder_diagnostics) =
        readable_folder_children_explained(db, &base, spec, as_of_ms)?;
    let mut entities = expand_with_folder_children(db, &base, &folders)?;
    let patterns = readable_pattern_children(db, spec, as_of_ms)?;
    let who_links = readable_who_link_children(db, spec, as_of_ms)?;
    let mut existing: HashSet<(String, EntityType)> = entities
        .iter()
        .map(|e| (e.name.clone(), e.entity_type.clone()))
        .collect();
    for expansion in &patterns {
        let (kind, glob) = enzyme_spec::split_entity(&expansion.reading.entity);
        if expansion.matched == 0 {
            warn!(kind, pattern = glob, "reading pattern matched no {kind}s");
        } else if expansion.selected.is_empty() {
            warn!(
                kind,
                pattern = glob,
                matched = expansion.matched,
                "reading pattern matched {kind}s, but none are eligible"
            );
        }
        info!(
            kind,
            pattern = glob,
            matched = expansion.matched,
            eligible = expansion.eligible,
            selected = expansion.selected.len(),
            "expanded reading pattern"
        );
        for entity in &expansion.selected {
            if existing.insert((entity.name.clone(), entity.entity_type.clone())) {
                entities.push(entity.clone());
            }
        }
    }
    for expansion in &who_links {
        for entity in &expansion.selected {
            if existing.insert((entity.name.clone(), entity.entity_type.clone())) {
                entities.push(entity.clone());
            }
        }
    }
    let mut profiles = profile_assignment::assign_profiles(&entities, &HashMap::new(), None);
    let mut inherited = HashMap::<String, String>::new();
    for expansion in &patterns {
        let profile = &expansion.reading.profile;
        if profile == "auto" {
            continue;
        }
        for child in &expansion.selected {
            if let Some(previous) = inherited.insert(child.name.clone(), profile.clone()) {
                anyhow::ensure!(
                    previous == *profile || overrides.contains_key(&child.name),
                    "{} {} inherits conflicting profiles from reading patterns; declare an explicit reading for it",
                    child.entity_type.as_str(),
                    child.name
                );
            }
        }
    }
    for expansion in &who_links {
        if expansion.reading.profile == "auto" {
            continue;
        }
        for child in &expansion.selected {
            if let Some(previous) =
                inherited.insert(child.name.clone(), expansion.reading.profile.clone())
            {
                anyhow::ensure!(
                    previous == expansion.reading.profile || overrides.contains_key(&child.name),
                    "who link {} inherits conflicting profiles; declare an explicit reading for it",
                    child.name
                );
            }
        }
    }
    for parent in explicit.keys() {
        if let Some(profile) = overrides.get(parent) {
            for child in folders.get(parent).into_iter().flatten() {
                if let Some(previous) = inherited.insert(child.clone(), profile.clone()) {
                    anyhow::ensure!(
                        previous == *profile || overrides.contains_key(child),
                        "linked page {child} inherits conflicting profiles; declare an explicit reading for it"
                    );
                }
            }
        }
    }
    profiles.extend(inherited);
    profiles.extend(overrides.clone());
    let entity_map = EntityMap::from_entities(db, &entities)?;
    let diagnostics = SelectionDiagnostics {
        reference_ms: as_of_ms,
        folders: folder_diagnostics,
        patterns: patterns.iter().map(PatternExpansion::diagnostics).collect(),
        who_links: who_links.iter().map(|e| e.diagnostics.clone()).collect(),
        missing: Vec::new(),
        left_out: Vec::new(),
    };
    Ok(MaterializedSelection {
        base_entities: base,
        entity_map,
        expandable_folders: folders,
        final_entities: entities,
        entity_profiles: profiles,
        diagnostics,
    })
}

/// Deterministically rank a folder's page entities for catalyst/display use.
///
/// Ranking prefers children that are highly referenced from within the folder,
/// then falls back to overall frequency and recency. Results are returned as
/// wikilinks for direct reuse in config/petri surfaces.
pub fn select_folder_children(
    db: &Database,
    folder_name: &str,
    page_entities: &[String],
    limit: usize,
) -> Vec<String> {
    if page_entities.is_empty() || limit == 0 {
        return Vec::new();
    }

    let page_entity_set: HashSet<&str> = page_entities.iter().map(|name| name.as_str()).collect();

    let lookup_pairs: Vec<(&str, EntityType)> = page_entities
        .iter()
        .map(|name| (name.as_str(), EntityType::Link))
        .collect();
    let entities = db.get_entities_by_names(&lookup_pairs).unwrap_or_default();
    let entity_by_name: HashMap<String, Entity> = entities
        .into_iter()
        .map(|entity| (entity.name.clone(), entity))
        .collect();

    let reference_lookup_limit = page_entities.len().max(limit);
    let referenced_children = db
        .get_folder_children_by_references(folder_name, reference_lookup_limit)
        .map(|(children, _)| children)
        .unwrap_or_default();

    let mut ranked: Vec<(String, i64, i64, i64)> = referenced_children
        .into_iter()
        .filter_map(|child| {
            let normalized = child.name.to_lowercase();
            if !page_entity_set.contains(normalized.as_str()) {
                return None;
            }
            let entity = entity_by_name.get(&normalized)?;
            Some((
                normalized,
                child.reference_count,
                entity.frequency,
                entity.last_seen.unwrap_or(0),
            ))
        })
        .collect();

    let mut ranked_names: HashSet<String> =
        ranked.iter().map(|(name, _, _, _)| name.clone()).collect();

    for page_entity in page_entities {
        if ranked_names.contains(page_entity) {
            continue;
        }
        if let Some(entity) = entity_by_name.get(page_entity) {
            ranked.push((
                page_entity.clone(),
                0,
                entity.frequency,
                entity.last_seen.unwrap_or(0),
            ));
            ranked_names.insert(page_entity.clone());
        }
    }

    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(limit);

    ranked
        .into_iter()
        .map(|(name, _, _, _)| format!("[[{}]]", name))
        .collect()
}

fn auto_expandable_folders(
    db: &Database,
    entities: &[Entity],
) -> Result<HashMap<String, Vec<String>>> {
    let folders = folder_scope(entities);
    Ok(load_expandable_folder_children(db, &folders, &HashSet::new(), &HashMap::new(), None)?.0)
}

fn folder_scope(entities: &[Entity]) -> HashMap<String, i64> {
    entities
        .iter()
        .filter(|entity| entity.entity_type == EntityType::Folder)
        .map(|entity| (entity.name.clone(), entity.frequency.max(1)))
        .collect()
}

fn load_expandable_folder_children(
    db: &Database,
    folders: &HashMap<String, i64>,
    density_exempt: &HashSet<&str>,
    policies: &HashMap<String, enzyme_spec::Selection>,
    as_of_ms: Option<i64>,
) -> Result<(
    HashMap<String, Vec<String>>,
    BTreeMap<String, FolderExpansion>,
)> {
    if folders.is_empty() {
        return Ok(Default::default());
    }

    let folder_names: HashSet<&str> = folders.keys().map(|name| name.as_str()).collect();
    let candidates_by_folder = db.get_folder_page_entity_candidates_grouped(&folder_names)?;

    let ids: Vec<_> = candidates_by_folder
        .values()
        .flatten()
        .filter(|e| is_active_page_link(e))
        .filter_map(|e| e.id)
        .collect();
    let stats = db.get_entity_selection_stats(&ids, as_of_ms)?;
    let mut result = HashMap::new();
    let mut explained = BTreeMap::new();
    for (folder, file_count) in folders {
        let candidates = candidates_by_folder
            .get(folder)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let links: Vec<Entity> = candidates
            .iter()
            .filter(|e| is_active_page_link(e))
            .cloned()
            .collect();
        let below_frequency = candidates
            .iter()
            .filter(|e| e.frequency < PAGE_LINK_MIN_FREQUENCY)
            .count();
        let mut explanation = FolderExpansion {
            linked_pages: candidates.len(),
            below_frequency,
            inactive: candidates.len() - below_frequency - links.len(),
            eligible: links.len(),
            density: links.len() as f64 / *file_count as f64,
            density_exempt: density_exempt.contains(folder.as_str()),
            ..Default::default()
        };
        explanation.below_density = !links.is_empty()
            && !explanation.density_exempt
            && explanation.density < EXPANDABLE_DENSITY_THRESHOLD;
        if !links.is_empty() && !explanation.below_density {
            let selected = rank_and_limit(links, &stats, policies.get(folder));
            explanation.selected = scored(&selected, &stats);
            if !selected.is_empty() {
                result.insert(
                    folder.clone(),
                    selected.into_iter().map(|entity| entity.name).collect(),
                );
            }
        }
        explained.insert(folder.clone(), explanation);
    }

    Ok((result, explained))
}

/// Order linked entities by an explicit selection policy (distinct documents or
/// recency, then name) or, without one, by the legacy cached activity score,
/// frequency and name; then keep the policy's amount or the legacy cap.
fn rank_and_limit(
    mut entities: Vec<Entity>,
    stats: &HashMap<i64, EntitySelectionStats>,
    policy: Option<&enzyme_spec::Selection>,
) -> Vec<Entity> {
    let score = |entity: &Entity| {
        let live = entity.id.and_then(|id| stats.get(&id));
        match policy.map(|p| p.by) {
            Some(enzyme_spec::SelectionMode::Frequency) => live.map_or(0.0, |s| s.documents as f64),
            Some(enzyme_spec::SelectionMode::Recency) => live.map_or(0.0, |s| s.recency),
            None => live
                .and_then(|s| s.corrected_legacy_recency)
                .unwrap_or(entity.trends.recency_score),
        }
    };
    entities.sort_by(|a, b| {
        score(b)
            .total_cmp(&score(a))
            .then_with(|| {
                if policy.is_none() {
                    b.frequency.cmp(&a.frequency)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    entities.truncate(policy.map_or(MAX_FOLDER_CHILDREN, |p| p.limit(entities.len())));
    entities
}

fn expand_with_folder_children(
    db: &Database,
    base_entities: &[Entity],
    expandable_folders: &HashMap<String, Vec<String>>,
) -> Result<Vec<Entity>> {
    let mut final_entities = base_entities.to_vec();
    let mut existing: HashSet<(String, EntityType)> = final_entities
        .iter()
        .map(|entity| (entity.name.clone(), entity.entity_type.clone()))
        .collect();

    let mut ordered_missing = Vec::new();
    let mut folder_names: Vec<&String> = expandable_folders.keys().collect();
    folder_names.sort();

    for folder_name in folder_names {
        if let Some(children) = expandable_folders.get(folder_name) {
            for child_name in children {
                let key = (child_name.clone(), EntityType::Link);
                if existing.insert(key) {
                    ordered_missing.push(child_name.clone());
                }
            }
        }
    }

    if ordered_missing.is_empty() {
        return Ok(final_entities);
    }

    let lookup: Vec<(&str, EntityType)> = ordered_missing
        .iter()
        .map(|name| (name.as_str(), EntityType::Link))
        .collect();
    let found = db.get_entities_by_names(&lookup)?;
    let found_by_name: HashMap<String, Entity> = found
        .into_iter()
        .map(|entity| (entity.name.clone(), entity))
        .collect();

    for child_name in ordered_missing {
        if let Some(entity) = found_by_name.get(&child_name) {
            final_entities.push(entity.clone());
        }
    }

    Ok(final_entities)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::EntityTrends;

    fn test_db() -> Database {
        Database::open_in_memory().expect("failed to open in-memory database")
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
        )
        .unwrap();
    }

    fn insert_entity(db: &Database, name: &str, etype: &str, frequency: i64, recent: i64) {
        use libsql::Value as V;
        let now = chrono::Utc::now().timestamp_millis();
        let trends = serde_json::json!({
            "frequency_recent": recent,
            "recency_score": frequency as f64,
            "timeline_buckets": {},
        });
        db.execute_write(
            "INSERT INTO entities (name, type, frequency, first_seen, last_seen, trends)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            vec![
                V::Text(name.to_string()),
                V::Text(etype.to_string()),
                V::Integer(frequency),
                V::Integer(now),
                V::Text(trends.to_string()),
            ],
        )
        .unwrap();
    }

    fn make_entity(name: &str, entity_type: EntityType, frequency: i64) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type,
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

    fn insert_occurrence(db: &Database, name: &str, file: &str, date: i64) {
        use libsql::Value as V;
        db.execute_write(
            "INSERT INTO entity_occurrences (entity_id, file_path, created_at, effective_date)
             SELECT id, ?2, ?3, ?3 FROM entities WHERE name = ?1 AND type = 'link'",
            vec![V::Text(name.into()), V::Text(file.into()), V::Integer(date)],
        )
        .unwrap();
    }

    #[test]
    fn who_links_expand_from_only_the_named_sqlite_source() {
        let db = test_db();
        let date = chrono::Utc::now().timestamp_millis() - 86_400_000;
        insert_entity(&db, "sqlite:messages", "collection", 3, 3);
        for name in ["alice", "bob", "carol"] {
            insert_entity(&db, name, "link", 10, 10);
        }
        for (name, file) in [
            ("alice", "sqlite:messages/custom-a"),
            ("alice", "sqlite:messages/custom-b"),
            ("bob", "sqlite:messages/custom-c"),
            ("bob", "sqlite:other/custom-a"),
            ("bob", "sqlite:other/custom-b"),
            ("carol", "sqlite:other/custom-c"),
        ] {
            insert_doc(&db, file);
            insert_occurrence(&db, name, file, date);
        }
        let program = enzyme_spec::resolve(
            vec![
                enzyme_spec::parse(
                    r#"
workspace "conversations" {
  source sqlite "messages" {
    database "/tmp/messages.sqlite"
    query "SELECT id, person, sent_at, body FROM messages"
    id "id"
    who "person"
    when "sent_at" unit ms
    what "body"
  }
  learn questions from source "messages" including who links select 1 by frequency
}"#,
                )
                .unwrap(),
            ],
            std::path::Path::new("/home/test"),
        )
        .unwrap();
        let spec = &program.vaults[0];
        let base = db
            .get_entities_by_names(&[("sqlite:messages", EntityType::Collection)])
            .unwrap();
        let result = build_readable_selection(&db, base, &HashMap::new(), spec).unwrap();
        assert_eq!(
            result
                .final_entities
                .iter()
                .filter(|e| e.entity_type == EntityType::Link)
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["alice"]
        );
        let detail = &result.diagnostics.who_links[0];
        assert_eq!(
            (
                detail.matched,
                detail.eligible,
                detail.selected[0].documents
            ),
            (2, 2, 2)
        );
    }

    #[test]
    fn explicit_selection_flows_from_config_to_runtime_with_distinct_documents() {
        use crate::config::EnzymeConfig;
        use crate::pipeline::selection::SelectionPipeline;
        let db = test_db();
        let home = tempfile::tempdir().unwrap();
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("configs")).unwrap();
        insert_entity(&db, "efforts", "folder", 1000, 10); // below auto density
        let now = chrono::Utc::now().timestamp_millis();
        for (name, docs, repeats, age) in [
            ("frequent", 6, 1, 300),
            ("recent", 3, 30, 0),
            ("tie-a", 3, 1, 10),
            ("tie-b", 3, 10, 10),
        ] {
            insert_entity(&db, name, "link", docs * repeats, 1);
            insert_doc(&db, &format!("efforts/{name}.md"));
            for d in 0..docs {
                for _ in 0..repeats {
                    insert_occurrence(&db, name, &format!("source-{d}.md"), now - age * 86_400_000);
                }
            }
        }
        let load = |clause: &str| {
            std::fs::write(
                home.path().join("configs/vault.enzyme"),
                format!(
                    r#"
profile custom {{ seek "continuity" notice ["evidence"] }}
vault {:?} {{ learn questions from folder "efforts" including linked pages about custom {clause} }}
"#,
                    vault.path().to_string_lossy()
                ),
            )
            .unwrap();
            EnzymeConfig::load_at(vault.path(), &home.path().join("config.toml"))
                .unwrap()
                .unwrap()
        };
        for (clause, expected) in [
            ("select 1 by frequency", vec!["frequent"]),
            ("select 1 by recency", vec!["recent"]),
            ("select 50% by frequency", vec!["frequent", "recent"]),
            ("select 26% by frequency up to 1", vec!["frequent"]),
            (
                "select 100 by frequency",
                vec!["frequent", "recent", "tie-a", "tie-b"],
            ),
        ] {
            let config = load(clause);
            let json = serde_json::to_string(&config).unwrap();
            let restored: EnzymeConfig = serde_json::from_str(&json).unwrap();
            let result = SelectionPipeline::new(&db, vault.path())
                .run_with_config(Some(&restored))
                .unwrap();
            assert_eq!(result.expandable_folders["efforts"], expected, "{clause}");
            assert_eq!(result.entities.len(), expected.len() + 1);
            for child in expected {
                assert_eq!(result.entity_profiles[child], "custom");
            }
        }
        let before = serde_json::to_string(&load("select 1 by frequency")).unwrap();
        let after = serde_json::to_string(&load("select 1 by recency")).unwrap();
        assert_ne!(
            before, after,
            "selection must affect the serialized generation contract"
        );
    }

    #[test]
    fn readable_selection_explains_folder_gates_and_pattern_filters() {
        use libsql::Value as V;
        let db = test_db();
        insert_entity(&db, "people", "folder", 4, 4);
        for (name, frequency, recent) in [("a", 5, 1), ("b", 2, 1), ("c", 5, 0)] {
            insert_entity(&db, name, "link", frequency, recent);
            insert_doc(&db, &format!("people/{name}.md"));
            insert_occurrence(&db, name, "source.md", 1);
        }
        insert_doc(&db, "people/unlinked.md");
        for tag in ["p-1", "p-2", "p-3", "p-4", "q"] {
            insert_entity(&db, tag, "tag", 3, 1);
        }
        for tag in ["p-1", "p-2", "p-3"] {
            db.execute_write(
                "INSERT INTO entity_occurrences (entity_id, file_path, created_at, effective_date)
                 SELECT id, 'tagged.md', 1, 1 FROM entities WHERE name = ?1 AND type = 'tag'",
                vec![V::Text(tag.into())],
            )
            .unwrap();
        }
        let source = r#"vault "/notes" {
  leave out tags ["p-2"]
  learn questions from folder "people" including linked pages
  learn questions from tag "p-3"
  learn questions from tags matching "p-*"
}"#;
        let program = enzyme_spec::resolve(
            vec![enzyme_spec::parse(source).unwrap()],
            std::path::Path::new("/home/test"),
        )
        .unwrap();
        let base = db
            .get_entities_by_names(&[("people", EntityType::Folder), ("p-3", EntityType::Tag)])
            .unwrap();
        let result =
            build_readable_selection(&db, base, &HashMap::new(), &program.vaults[0]).unwrap();
        let d = &result.diagnostics;
        assert_eq!(d.reference_ms, None);
        let people = &d.folders["people"];
        assert_eq!(
            (
                people.linked_pages,
                people.below_frequency,
                people.inactive,
                people.eligible
            ),
            (3, 1, 1, 1)
        );
        assert!(people.density_exempt && !people.below_density);
        assert_eq!(people.selected.len(), 1);
        assert_eq!(people.selected[0].name, "a");
        assert_eq!(people.selected[0].documents, 1);
        assert_eq!(result.expandable_folders["people"], ["a"]);

        assert_eq!(d.patterns.len(), 1);
        let p = &d.patterns[0];
        assert_eq!(p.entity, "#p-*");
        assert_eq!(
            (p.matched, p.left_out, p.explicit, p.undated, p.eligible),
            (4, 1, 1, 1, 1)
        );
        let names: Vec<_> = p.selected.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["p-1"]);
    }

    #[test]
    fn selection_count_percent_cap_empty_and_legacy_default() {
        let db = test_db();
        insert_entity(&db, "people", "folder", 15, 15);
        insert_entity(&db, "empty", "folder", 1, 1);
        for i in (0..15).rev() {
            let name = format!("person-{i:02}");
            insert_entity(&db, &name, "link", 3, 1);
            insert_doc(&db, &format!("people/{name}.md"));
            insert_occurrence(&db, &name, "source.md", 1);
        }
        for (clause, count) in [
            ("", 10),
            ("select 12 by frequency", 12),
            ("select 50% by frequency", 8),
            ("select 100% by frequency", 15),
            ("select 10% by frequency up to 1", 1),
            ("select 100 by recency", 15),
        ] {
            let source = format!(
                r#"vault "/notes" {{ learn questions from folders ["people", "empty"] including linked pages {clause} }}"#
            );
            let p = enzyme_spec::resolve(
                vec![enzyme_spec::parse(&source).unwrap()],
                std::path::Path::new("/home/test"),
            )
            .unwrap();
            let base = db
                .get_entities_by_names(&[
                    ("people", EntityType::Folder),
                    ("empty", EntityType::Folder),
                ])
                .unwrap();
            let result =
                build_readable_selection(&db, base, &HashMap::new(), &p.vaults[0]).unwrap();
            assert_eq!(result.expandable_folders["people"].len(), count, "{clause}");
            assert!(!result.expandable_folders.contains_key("empty"));
            assert_eq!(
                result.expandable_folders["people"][0], "person-00",
                "ties use names"
            );
        }
    }

    #[test]
    fn future_timestamps_cannot_dominate_live_or_cached_child_rankings() {
        let db = test_db();
        let now = chrono::Utc::now().timestamp_millis();
        insert_entity(&db, "people", "folder", 2, 2);
        for (name, date) in [("future", i64::MAX), ("today", now)] {
            insert_entity(&db, name, "link", 3, 3);
            insert_doc(&db, &format!("people/{name}.md"));
            for d in 0..3 {
                insert_occurrence(&db, name, &format!("source-{d}.md"), date);
            }
        }
        db.execute_write("UPDATE entities SET trends = '{\"frequency_recent\":3,\"recency_score\":2.7e271,\"timeline_buckets\":{}}' WHERE name='future'", vec![]).unwrap();
        let future = db
            .get_entities_by_names(&[("future", EntityType::Link)])
            .unwrap()
            .remove(0);
        let stats = db
            .get_entity_selection_stats(&[future.id.unwrap()], None)
            .unwrap();
        let score = &stats[&future.id.unwrap()];
        assert_eq!(score.documents, 3);
        assert_eq!(score.recency, 3.0);
        assert!(score.corrected_legacy_recency.unwrap() < 10.0);
        // A cached score of 20 beats a repaired future score, proving the legacy path uses the repair.
        db.execute_write("UPDATE entities SET trends = '{\"frequency_recent\":3,\"recency_score\":20,\"timeline_buckets\":{}}' WHERE name='today'", vec![]).unwrap();
        let result = build_curated_selection(
            &db,
            vec![make_entity("people", EntityType::Folder, 2)],
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(result.expandable_folders["people"][0], "today");
        assert_eq!(crate::models::activity_decay(now, i64::MAX), 1.0);
        assert_eq!(crate::models::activity_decay(now, i64::MIN), 0.0);
        assert!(crate::models::activity_decay(now, now - 30 * 86_400_000) < 0.501);
    }

    #[test]
    fn readable_expansion_inherits_profile_below_auto_density() {
        let db = test_db();
        insert_entity(&db, "people", "folder", 100, 10);
        insert_entity(&db, "alice", "link", 4, 1);
        insert_doc(&db, "people/alice.md");
        for i in 0..99 {
            insert_doc(&db, &format!("people/other-{i}.md"));
        }
        let program=enzyme_spec::resolve(vec![enzyme_spec::parse(r#"vault "/notes" { learn questions from folder "people" including linked pages about relationships }"#).unwrap()],std::path::Path::new("/home/test")).unwrap();
        let spec = &program.vaults[0];
        let base = vec![make_entity("people", EntityType::Folder, 100)];
        let overrides = HashMap::from([("people".into(), "relational".into())]);
        assert!(
            build_curated_selection(&db, base.clone(), &overrides)
                .unwrap()
                .expandable_folders
                .is_empty()
        );
        let result = build_readable_selection(&db, base, &overrides, spec).unwrap();
        assert!(result.final_entities.iter().any(|e| e.name == "alice"));
        assert_eq!(result.entity_profiles["alice"], "relational");
    }

    #[test]
    fn auto_expandable_folder_loads_page_entities_from_db() {
        let db = test_db();
        insert_doc(&db, "people/Alice Smith.md");
        insert_entity(&db, "alice smith", "link", 12, 4);

        let selected = vec![make_entity("people", EntityType::Folder, 20)];

        let result = build_curated_selection(&db, selected, &HashMap::new()).unwrap();

        assert_eq!(
            result.expandable_folders.get("people"),
            Some(&vec!["alice smith".to_string()])
        );
        assert!(
            result.final_entities.iter().any(
                |entity| entity.name == "alice smith" && entity.entity_type == EntityType::Link
            )
        );
    }

    #[test]
    fn curated_selection_preserves_explicit_profile_overrides() {
        let db = test_db();
        let selected = vec![make_entity("user-recipe-comment", EntityType::Folder, 20)];
        let overrides = HashMap::from([(
            "user-recipe-comment".to_string(),
            "preference_evidence".to_string(),
        )]);

        let result = build_curated_selection(&db, selected, &overrides).unwrap();

        assert_eq!(
            result.entity_profiles.get("user-recipe-comment").unwrap(),
            "preference_evidence"
        );
    }

    #[test]
    fn expandable_folder_promotes_children_not_in_shortlist() {
        // Scenario: people/ is a shortlisted folder. Its page entity children
        // (alice, bob, carol) are low-frequency links that would never make
        // the top-level shortlist on their own. But because people/ is expandable,
        // they should appear in final_entities and get catalyzed.
        let db = test_db();

        // Create a high-frequency tag so it dominates the shortlist
        insert_entity(&db, "enzyme", "tag", 500, 300);

        // Create the people folder with moderate frequency
        insert_entity(&db, "people", "folder", 50, 10);

        // Create low-frequency person links — these would never rank as top entities
        insert_entity(&db, "alice", "link", 8, 5);
        insert_entity(&db, "bob", "link", 6, 3);
        insert_entity(&db, "carol", "link", 4, 2);

        // Create docs so the links are detected as page entities of people/
        insert_doc(&db, "people/Alice.md");
        insert_doc(&db, "people/Bob.md");
        insert_doc(&db, "people/Carol.md");

        // Build curated selection with just enzyme + people
        let base = vec![
            make_entity("enzyme", EntityType::Tag, 500),
            make_entity("people", EntityType::Folder, 50),
        ];
        let result = build_curated_selection(&db, base, &HashMap::new()).unwrap();

        // people/ should be expandable
        assert!(
            result.expandable_folders.contains_key("people"),
            "people/ should be expandable"
        );

        // All three person links should be in the expandable children
        let children = result.expandable_folders.get("people").unwrap();
        assert!(children.contains(&"alice".to_string()));
        assert!(children.contains(&"bob".to_string()));
        assert!(children.contains(&"carol".to_string()));

        // All three should appear in final_entities even though they're low-frequency
        let final_names: Vec<&str> = result
            .final_entities
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            final_names.contains(&"alice"),
            "alice should be in final entities"
        );
        assert!(
            final_names.contains(&"bob"),
            "bob should be in final entities"
        );
        assert!(
            final_names.contains(&"carol"),
            "carol should be in final entities"
        );

        // The base entities should still be there
        assert!(final_names.contains(&"enzyme"));
        assert!(final_names.contains(&"people"));
    }

    #[test]
    fn expandable_folder_caps_children_at_limit() {
        // Scenario: people/ has 15 page entity links but the cap is 10.
        // Only the top 10 by recency should be expanded.
        let db = test_db();
        insert_entity(&db, "people", "folder", 50, 10);

        // Create 15 person links with varying recency
        for i in 0..15 {
            let name = format!("person-{:02}", i);
            let recency = 15 - i; // person-00 is most recent
            insert_entity(&db, &name, "link", 10, recency);
            insert_doc(&db, &format!("people/{}.md", name));
        }

        let base = vec![make_entity("people", EntityType::Folder, 50)];
        let result = build_curated_selection(&db, base, &HashMap::new()).unwrap();

        let children = result.expandable_folders.get("people").unwrap();
        assert_eq!(
            children.len(),
            MAX_FOLDER_CHILDREN,
            "should cap at MAX_FOLDER_CHILDREN"
        );

        // The most recent should be included, the least recent excluded
        assert!(
            children.contains(&"person-00".to_string()),
            "most recent should be included"
        );
        assert!(
            !children.contains(&"person-14".to_string()),
            "least recent should be excluded"
        );
    }

    #[test]
    fn non_expandable_folder_skipped_by_density() {
        // Scenario: inbox/ has 100 files but only 2 page entity links (2% density).
        // Below the 5% threshold, so it should NOT be expandable.
        let db = test_db();
        insert_entity(&db, "inbox", "folder", 100, 80);
        insert_entity(&db, "some-note", "link", 5, 3);
        insert_entity(&db, "other-note", "link", 4, 2);
        insert_doc(&db, "inbox/Some Note.md");
        insert_doc(&db, "inbox/Other Note.md");
        // Add many docs without matching links to dilute density
        for i in 0..98 {
            insert_doc(&db, &format!("inbox/note-{}.md", i));
        }

        let base = vec![make_entity("inbox", EntityType::Folder, 100)];
        let result = build_curated_selection(&db, base, &HashMap::new()).unwrap();

        assert!(
            !result.expandable_folders.contains_key("inbox"),
            "inbox/ should NOT be expandable at 2% density"
        );
        // The link entities should NOT be in final entities
        let final_names: Vec<&str> = result
            .final_entities
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(!final_names.contains(&"some-note"));
        assert!(!final_names.contains(&"other-note"));
    }

    #[test]
    fn folder_child_expansion_deduplicates_shared_links() {
        let db = test_db();
        insert_entity(&db, "alice smith", "link", 12, 4);

        let base_entities = vec![
            make_entity("people", EntityType::Folder, 20),
            make_entity("team", EntityType::Folder, 20),
        ];
        let expandable_folders = HashMap::from([
            ("people".to_string(), vec!["alice smith".to_string()]),
            ("team".to_string(), vec!["alice smith".to_string()]),
        ]);

        let final_entities =
            expand_with_folder_children(&db, &base_entities, &expandable_folders).unwrap();
        let alice_count = final_entities
            .iter()
            .filter(|entity| entity.name == "alice smith" && entity.entity_type == EntityType::Link)
            .count();

        assert_eq!(alice_count, 1);
    }

    #[test]
    fn as_of_makes_historical_pages_eligible_and_hides_later_evidence() {
        use crate::pipeline::indexing::IndexingPipeline;
        let vault = tempfile::tempdir().unwrap();
        let recent = (chrono::Utc::now() - chrono::Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        std::fs::create_dir_all(vault.path().join("people")).unwrap();
        std::fs::create_dir_all(vault.path().join("journal")).unwrap();
        for (page, date) in [
            ("alice", "2024-03-01"),
            ("bob", "2024-09-01"),
            ("carol", &recent),
        ] {
            std::fs::write(vault.path().join(format!("people/{page}.md")), "# Page\n").unwrap();
            for i in 0..3 {
                std::fs::write(
                    vault.path().join(format!("journal/{page}-{i}.md")),
                    format!("---\ndate: {date}\n---\nMet [[{page}]] today.\n"),
                )
                .unwrap();
            }
        }
        let children = |as_of: &str, select: &str| {
            let db = test_db();
            let source = format!(
                r#"vault "/notes" {{ {as_of} learn questions from folder "people" including linked pages {select} }}"#
            );
            let spec = enzyme_spec::resolve(
                vec![enzyme_spec::parse(&source).unwrap()],
                std::path::Path::new("/home/test"),
            )
            .unwrap()
            .vaults
            .remove(0);
            IndexingPipeline::new(&db, vault.path().to_path_buf())
                .with_as_of(spec.learning.as_of.clone())
                .run(true, |_| {})
                .unwrap();
            let base = db
                .get_entities_by_names(&[("people", EntityType::Folder)])
                .unwrap();
            let mut names = build_readable_selection(&db, base, &HashMap::new(), &spec)
                .unwrap()
                .expandable_folders
                .remove("people")
                .unwrap_or_default();
            names.sort();
            names
        };
        // The wall clock sees only the recent page; the 2024 corpus has no recent-year activity.
        assert_eq!(children("", ""), ["carol"]);
        // As of 2024, later evidence does not exist yet and 2024 pages are active.
        assert_eq!(children(r#"as of "2024-10-01""#, ""), ["alice", "bob"]);
        assert_eq!(
            children(r#"as of "2024-10-01""#, "select 1 by recency"),
            ["bob"]
        );
        assert_eq!(children("as of latest evidence", ""), ["carol"]);
    }

    /// Journal notes carrying `#tag` dated `date`, `docs` notes per tag.
    pub(crate) fn tagged_vault(tags: &[(&str, usize, &str)]) -> tempfile::TempDir {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("journal")).unwrap();
        for (tag, docs, date) in tags {
            for i in 0..*docs {
                std::fs::write(
                    vault.path().join(format!("journal/{tag}-{i}.md")),
                    format!("---\ndate: {date}\n---\nNotes on #{tag} and what followed.\n"),
                )
                .unwrap();
            }
        }
        vault
    }

    #[test]
    fn name_patterns_select_eligible_matches_and_inherit_like_folder_children() {
        use crate::config::EnzymeConfig;
        use crate::pipeline::indexing::IndexingPipeline;
        use crate::pipeline::selection::SelectionPipeline;
        let vault = tagged_vault(&[
            ("auto-cluster-a", 4, "2024-03-01"),
            ("auto-cluster-b", 3, "2024-03-01"),
            ("auto-cluster-c", 2, "2024-03-01"),
            ("auto-cluster-late", 1, "2025-06-01"),
            ("other", 6, "2024-03-01"),
        ]);
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("configs")).unwrap();
        let run = |body: &str| {
            std::fs::write(
                home.path().join("configs/vault.enzyme"),
                format!("vault {:?} {{ {body} }}", vault.path().to_string_lossy()),
            )
            .unwrap();
            let config =
                EnzymeConfig::load_at(vault.path(), &home.path().join("config.toml"))?.unwrap();
            let db = test_db();
            IndexingPipeline::new(&db, vault.path().to_path_buf())
                .with_as_of(config.reading_spec.as_ref().unwrap().learning.as_of.clone())
                .run(true, |_| {})?;
            let result =
                SelectionPipeline::new(&db, vault.path()).run_with_config(Some(&config))?;
            let mut tags: Vec<(String, String)> = result
                .entities
                .iter()
                .filter(|e| e.entity_type == EntityType::Tag)
                .map(|e| {
                    let profile = result.entity_profiles.get(&e.name).cloned();
                    (e.name.clone(), profile.unwrap_or_default())
                })
                .collect();
            tags.sort();
            anyhow::Ok(tags)
        };
        let names = |body: &str| -> Vec<String> {
            run(body)
                .unwrap()
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        let pattern = r#"learn questions from tags matching "AUTO-cluster-*" about decisions"#;
        // The two clusters with the most distinct documents; `other` never matches.
        let selected = run(&format!("{pattern} select 2 by frequency")).unwrap();
        assert_eq!(
            selected,
            [
                ("auto-cluster-a".into(), "decision_trace".into()),
                ("auto-cluster-b".into(), "decision_trace".into())
            ]
        );
        // Without select, the folder default cap (10) keeps every eligible match.
        assert_eq!(
            names(pattern),
            [
                "auto-cluster-a",
                "auto-cluster-b",
                "auto-cluster-c",
                "auto-cluster-late"
            ]
        );
        // As of 2024, a tag whose only occurrences are later is not a match.
        assert_eq!(
            names(&format!(r#"as of "2024-12-31" {pattern}"#)),
            ["auto-cluster-a", "auto-cluster-b", "auto-cluster-c"]
        );
        // An explicit reading keeps its own profile and consumes no pattern slot.
        let overridden = run(&format!(
            r#"{pattern} select 2 by frequency learn questions from tag "auto-cluster-a" about relationships"#
        ))
        .unwrap();
        assert_eq!(
            overridden,
            [
                ("auto-cluster-a".into(), "relational".into()),
                ("auto-cluster-b".into(), "decision_trace".into()),
                ("auto-cluster-c".into(), "decision_trace".into())
            ]
        );
        assert_eq!(
            names(&format!(
                r#"{pattern} select 2 by frequency leave out tags ["auto-cluster-b"]"#
            )),
            ["auto-cluster-a", "auto-cluster-c"]
        );
        // A pattern with no match selects nothing (and warns) rather than failing.
        assert!(names(r#"learn questions from tags matching "nothing-*""#).is_empty());
        // Overlapping patterns follow the folder rule: agreeing policies are fine,
        // conflicting inherited profiles need an explicit reading.
        assert_eq!(
            names(&format!(
                r#"{pattern} learn questions from tags matching "*-a" about decisions"#
            ))
            .len(),
            4
        );
        let conflict = run(&format!(
            r#"{pattern} learn questions from tags matching "*-a" about relationships"#
        ))
        .unwrap_err();
        assert!(
            format!("{conflict:#}").contains("auto-cluster-a"),
            "{conflict:#}"
        );
        assert!(
            run(&format!(
                r#"{pattern} learn questions from tags matching "*-a" about relationships learn questions from tag "auto-cluster-a""#
            ))
            .is_ok()
        );
    }
}
