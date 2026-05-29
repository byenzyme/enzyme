use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use chrono::{TimeZone, Utc};
use serde::Serialize;

use enzyme_core::config::enzyme_config::{CatalyzedEntity, EnzymeConfig};
use enzyme_core::document::{
    DocumentProcessor, FileDiscovery, ProcessedDocument, strip_frontmatter,
};
use enzyme_core::llm::CATALYST_PROFILES;
use enzyme_core::models::ROOT_FOLDER_ENTITY;
use enzyme_core::pipeline::selection_steps::{EXPANDABLE_DENSITY_THRESHOLD, MAX_FOLDER_CHILDREN};

/// Filesystem-only vault scan used before init.
///
/// This command intentionally does not read the enzyme DB. Its job is to
/// propose the config that init should later consume.
pub fn run_scan(vault_path: &Path, write_config: bool) -> Result<()> {
    let docs = process_docs(vault_path)?;
    let mut suggestion = build_scan_suggestion(vault_path, &docs)?;

    if write_config {
        let mut config = EnzymeConfig::load(vault_path)?.unwrap_or_default();
        if config.has_curated_entities() {
            suggestion.status = ScanStatus::AlreadyConfigured;
            suggestion.message =
                Some("config already has entities for this vault; not overwriting".to_string());
        } else {
            config.excluded_folders = suggestion.excluded_folders.clone();
            config.set_auto_selected_entities(vault_path, suggestion.entities.clone())?;
            suggestion.status = ScanStatus::Written;
            suggestion.config_path =
                Some(EnzymeConfig::config_path()?.to_string_lossy().to_string());
            suggestion.current_config = current_config_summary(vault_path)?;
        }
    }

    println!("{}", serde_json::to_string(&suggestion)?);
    Ok(())
}

#[derive(Debug, Serialize)]
struct ScanSuggestion {
    /// Existing flat fields are intentionally kept for older setup clients.
    files: usize,
    entities: Vec<String>,
    excluded_folders: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_path: Option<String>,
    status: ScanStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,

    /// Rich bounded evidence for newer setup agents.
    schema_version: &'static str,
    scan_id: String,
    vault_path: String,
    generated_at: String,
    instructions: Vec<String>,
    summary: ScanSummary,
    top_entities: Vec<EntityEvidence>,
    top_tags: Vec<EntityEvidence>,
    top_links: Vec<EntityEvidence>,
    top_folders: Vec<EntityEvidence>,
    folder_stats: Vec<FolderStat>,
    folder_page_entities: Vec<FolderPageEntityGroup>,
    folder_children: Vec<ChildGroup>,
    tag_children: Vec<ChildGroup>,
    frontmatter_samples: Vec<FrontmatterSample>,
    entity_samples: Vec<EntitySample>,
    sample_files: Vec<SampleFile>,
    current_config: CurrentConfigSummary,
    available_profiles: Vec<ProfileSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScanStatus {
    Suggested,
    Written,
    AlreadyConfigured,
}

#[derive(Debug, Serialize)]
struct ScanSummary {
    file_count: usize,
    total_words: i64,
    total_chars: i64,
    folder_count: usize,
    tag_count: usize,
    link_count: usize,
    frontmatter_file_count: usize,
    newest_modified: Option<String>,
    oldest_modified: Option<String>,
    flat_vault: bool,
}

#[derive(Debug, Serialize, Clone)]
struct EntityEvidence {
    spec: String,
    entity_type: String,
    name: String,
    frequency: usize,
    file_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_modified: Option<String>,
}

#[derive(Debug, Serialize)]
struct FolderStat {
    folder: String,
    spec: String,
    file_count: usize,
    word_count: i64,
    latest_modified: Option<String>,
    page_entities: Vec<String>,
    expandable_candidate: bool,
}

#[derive(Debug, Serialize, Clone)]
struct FolderPageEntityGroup {
    folder: String,
    spec: String,
    file_count: usize,
    page_entities: Vec<FolderPageEntity>,
    expandable_candidate: bool,
}

#[derive(Debug, Serialize, Clone)]
struct FolderPageEntity {
    name: String,
    spec: String,
    source_ref: String,
    link_frequency: usize,
    latest_modified: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChildGroup {
    parent: String,
    children: Vec<NamedCount>,
}

#[derive(Debug, Serialize)]
struct NamedCount {
    name: String,
    count: usize,
}

#[derive(Debug, Serialize)]
struct FrontmatterSample {
    source_ref: String,
    title: Option<String>,
    tags: Vec<String>,
    aliases: Vec<String>,
    links: Vec<String>,
    extra_keys: Vec<String>,
    created: Option<String>,
    modified: Option<String>,
}

#[derive(Debug, Serialize)]
struct EntitySample {
    spec: String,
    entity_type: String,
    name: String,
    samples: Vec<FileExcerpt>,
}

#[derive(Debug, Serialize)]
struct FileExcerpt {
    source_ref: String,
    title: Option<String>,
    modified_at: Option<String>,
    excerpt: String,
}

#[derive(Debug, Serialize)]
struct SampleFile {
    source_ref: String,
    title: Option<String>,
    folders: Vec<String>,
    tags: Vec<String>,
    links: Vec<String>,
    word_count: i64,
    modified_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct CurrentConfigSummary {
    status: String,
    config_path: Option<String>,
    has_curated_entities: bool,
    entities: Vec<String>,
    excluded_folders: Vec<String>,
    excluded_tags: Vec<String>,
    excluded_links: Vec<String>,
    catalyst_format: Option<String>,
    max_embedding_files: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ProfileSummary {
    key: String,
    display_name: String,
    description: String,
}

#[derive(Debug, Default)]
struct Metric {
    frequency: usize,
    files: HashSet<String>,
    latest_modified: Option<i64>,
}

fn process_docs(vault_path: &Path) -> Result<Vec<ProcessedDocument>> {
    let discovery = FileDiscovery::new(vault_path.to_path_buf());
    let paths = discovery.discover()?;
    let processor = DocumentProcessor::new(vault_path.to_path_buf());
    Ok(processor
        .process_all(&paths)
        .into_iter()
        .flatten()
        .collect())
}

fn build_scan_suggestion(vault_path: &Path, docs: &[ProcessedDocument]) -> Result<ScanSuggestion> {
    let mut folder_counts: HashMap<String, usize> = HashMap::new();
    let mut tag_counts: HashMap<String, usize> = HashMap::new();
    let mut link_counts: HashMap<String, usize> = HashMap::new();

    for doc in docs {
        // Preserve the historic suggestion behavior: root files do not make
        // `folder:.` a candidate unless the whole vault is flat.
        for folder in &doc.folders {
            *folder_counts.entry(folder.clone()).or_default() += 1;
        }
        for tag in &doc.tags {
            *tag_counts.entry(tag.clone()).or_default() += 1;
        }
        for link in &doc.links {
            *link_counts.entry(link.clone()).or_default() += 1;
        }
    }

    let folder_page_entities =
        folder_page_entities(docs, &folder_counts, &link_counts, 25, MAX_FOLDER_CHILDREN);
    let entities = suggested_entities(
        docs,
        &folder_counts,
        &tag_counts,
        &link_counts,
        &folder_page_entities,
    );
    let metrics = build_metrics(docs);
    let current_config = current_config_summary(vault_path)?;
    let existing_configured = current_config.has_curated_entities;

    Ok(ScanSuggestion {
        files: docs.len(),
        entities: entities.clone(),
        excluded_folders: default_excluded_folders(),
        config_path: None,
        status: ScanStatus::Suggested,
        message: existing_configured.then(|| {
            "config already has entities for this vault; scan is a preview only".to_string()
        }),
        schema_version: "scan.v2",
        scan_id: format!("scan-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ")),
        vault_path: vault_path.to_string_lossy().to_string(),
        generated_at: Utc::now().to_rfc3339(),
        instructions: vec![
            "Use entities/top_entities as bounded base setup candidates; keep existing flat fields for compatibility.".to_string(),
            "Prefer base folders over descendant folders; inspect folder_children/folder_stats for covered descendants.".to_string(),
            "Use folder_page_entities to identify expandable folders whose page-link children should be materialized at runtime, not persisted as config children.".to_string(),
            "Prefer top_entities with supporting entity_samples; avoid structural folders listed in excluded_folders.".to_string(),
            "Use folder/tag/link child maps and frontmatter_samples to infer profile and exclusion choices.".to_string(),
        ],
        summary: build_summary(docs, &metrics),
        top_entities: evidence_for_specs(&entities, &metrics),
        top_tags: top_evidence_by_type(&metrics, "tag", 25),
        top_links: top_evidence_by_type(&metrics, "link", 25),
        top_folders: top_evidence_by_type(&metrics, "folder", 25),
        folder_stats: folder_stats(docs, &metrics, &folder_page_entities, 50),
        folder_page_entities,
        folder_children: folder_children(&metrics, 25, 25),
        tag_children: tag_children(&metrics, 25, 25),
        frontmatter_samples: frontmatter_samples(docs, 12),
        entity_samples: entity_samples(docs, &entities, 12, 3),
        sample_files: sample_files(docs, 30),
        current_config,
        available_profiles: available_profiles(),
    })
}

fn build_metrics(docs: &[ProcessedDocument]) -> HashMap<String, Metric> {
    let mut metrics = HashMap::new();

    for doc in docs {
        for folder in folder_names_for_doc(doc) {
            if !is_structural_folder(&folder) {
                add_metric(&mut metrics, entity_spec("folder", &folder), doc);
            }
        }
        for tag in &doc.tags {
            if !is_structural_tag(tag) {
                add_metric(&mut metrics, entity_spec("tag", tag), doc);
            }
        }
        for link in &doc.links {
            add_metric(&mut metrics, entity_spec("link", link), doc);
        }
    }

    metrics
}

fn add_metric(metrics: &mut HashMap<String, Metric>, spec: String, doc: &ProcessedDocument) {
    let metric = metrics.entry(spec).or_default();
    metric.frequency += 1;
    metric.files.insert(doc.source_ref.clone());
    metric.latest_modified = Some(
        metric
            .latest_modified
            .unwrap_or(i64::MIN)
            .max(doc.modified_at),
    );
}

fn build_summary(docs: &[ProcessedDocument], metrics: &HashMap<String, Metric>) -> ScanSummary {
    let newest_modified = docs.iter().map(|doc| doc.modified_at).max();
    let oldest_modified = docs.iter().map(|doc| doc.modified_at).min();

    ScanSummary {
        file_count: docs.len(),
        total_words: docs.iter().map(|doc| doc.word_count).sum(),
        total_chars: docs.iter().map(|doc| doc.char_count).sum(),
        folder_count: metrics
            .keys()
            .filter(|spec| spec.starts_with("folder:"))
            .count(),
        tag_count: metrics.keys().filter(|spec| spec.starts_with('#')).count(),
        link_count: metrics.keys().filter(|spec| spec.starts_with("[[")).count(),
        frontmatter_file_count: docs.iter().filter(|doc| doc.frontmatter.is_some()).count(),
        newest_modified: newest_modified.and_then(millis_to_rfc3339),
        oldest_modified: oldest_modified.and_then(millis_to_rfc3339),
        flat_vault: is_flat_vault(docs),
    }
}

fn current_config_summary(vault_path: &Path) -> Result<CurrentConfigSummary> {
    let config_path = EnzymeConfig::config_path().ok();
    let config_path_string = config_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let config = EnzymeConfig::load(vault_path)?;

    Ok(match config {
        Some(config) => CurrentConfigSummary {
            status: if config.has_curated_entities() {
                "configured".to_string()
            } else {
                "config_found".to_string()
            },
            config_path: config_path_string,
            has_curated_entities: config.has_curated_entities(),
            entities: config_entity_refs(&config.catalyzed_entities),
            excluded_folders: config.excluded_folders,
            excluded_tags: config.excluded_tags,
            excluded_links: config.excluded_links,
            catalyst_format: config.catalyst_format,
            max_embedding_files: config.max_embedding_files,
        },
        None => CurrentConfigSummary {
            status: "not_found".to_string(),
            config_path: config_path_string,
            has_curated_entities: false,
            entities: Vec::new(),
            excluded_folders: Vec::new(),
            excluded_tags: Vec::new(),
            excluded_links: Vec::new(),
            catalyst_format: None,
            max_embedding_files: None,
        },
    })
}

fn config_entity_refs(entities: &[CatalyzedEntity]) -> Vec<String> {
    entities
        .iter()
        .flat_map(|entry| match entry {
            CatalyzedEntity::Simple(raw) => vec![raw.clone()],
            CatalyzedEntity::WithOptions(map) => map.keys().cloned().collect(),
        })
        .collect()
}

fn available_profiles() -> Vec<ProfileSummary> {
    CATALYST_PROFILES
        .iter()
        .map(|profile| ProfileSummary {
            key: profile.key.to_string(),
            display_name: profile.display_name.to_string(),
            description: profile.description.to_string(),
        })
        .collect()
}

fn evidence_for_specs(specs: &[String], metrics: &HashMap<String, Metric>) -> Vec<EntityEvidence> {
    specs
        .iter()
        .filter_map(|spec| entity_ref_from_spec(spec).map(|entity_ref| (spec, entity_ref)))
        .map(|(spec, entity_ref)| evidence_from_metric(spec, entity_ref, metrics.get(spec)))
        .collect()
}

fn top_evidence_by_type(
    metrics: &HashMap<String, Metric>,
    entity_type: &str,
    limit: usize,
) -> Vec<EntityEvidence> {
    let mut entries: Vec<_> = metrics
        .iter()
        .filter_map(|(spec, metric)| {
            let entity_ref = entity_ref_from_spec(spec)?;
            (entity_ref.entity_type == entity_type)
                .then(|| evidence_from_metric(spec, entity_ref, Some(metric)))
        })
        .collect();

    sort_evidence(&mut entries);
    entries.truncate(limit);
    entries
}

fn evidence_from_metric(
    spec: &str,
    entity_ref: EntityRef,
    metric: Option<&Metric>,
) -> EntityEvidence {
    EntityEvidence {
        spec: spec.to_string(),
        entity_type: entity_ref.entity_type.to_string(),
        name: entity_ref.name.to_string(),
        frequency: metric.map(|m| m.frequency).unwrap_or(0),
        file_count: metric.map(|m| m.files.len()).unwrap_or(0),
        latest_modified: metric
            .and_then(|m| m.latest_modified)
            .and_then(millis_to_rfc3339),
    }
}

fn sort_evidence(entries: &mut [EntityEvidence]) {
    entries.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| b.frequency.cmp(&a.frequency))
            .then_with(|| a.spec.cmp(&b.spec))
    });
}

fn folder_page_entities(
    docs: &[ProcessedDocument],
    folder_counts: &HashMap<String, usize>,
    link_counts: &HashMap<String, usize>,
    parent_limit: usize,
    child_limit: usize,
) -> Vec<FolderPageEntityGroup> {
    let mut direct_pages: HashMap<String, Vec<FolderPageEntity>> = HashMap::new();

    for doc in docs {
        let Some((folder, stem)) = direct_child_stem(&doc.source_ref) else {
            continue;
        };
        let folder = folder.to_lowercase();
        if is_structural_folder(&folder) || !folder_counts.contains_key(&folder) {
            continue;
        }

        let link_name = stem.to_lowercase();
        let Some(link_frequency) = link_counts.get(&link_name).copied() else {
            continue;
        };

        direct_pages
            .entry(folder)
            .or_default()
            .push(FolderPageEntity {
                name: link_name.clone(),
                spec: entity_spec("link", &link_name),
                source_ref: doc.source_ref.clone(),
                link_frequency,
                latest_modified: millis_to_rfc3339(doc.modified_at),
            });
    }

    let mut groups: Vec<_> = direct_pages
        .into_iter()
        .filter_map(|(folder, mut page_entities)| {
            page_entities.sort_by(|a, b| {
                b.link_frequency
                    .cmp(&a.link_frequency)
                    .then_with(|| b.latest_modified.cmp(&a.latest_modified))
                    .then_with(|| a.name.cmp(&b.name))
            });
            page_entities.dedup_by(|a, b| a.name == b.name);

            let file_count = folder_counts.get(&folder).copied().unwrap_or_default();
            if file_count == 0 {
                return None;
            }

            let active_page_entity_count = page_entities
                .iter()
                .filter(|entity| entity.link_frequency >= 3)
                .count();
            let expandable_candidate =
                active_page_entity_count as f64 / file_count as f64 >= EXPANDABLE_DENSITY_THRESHOLD;

            page_entities.truncate(child_limit);
            Some(FolderPageEntityGroup {
                spec: entity_spec("folder", &folder),
                folder,
                file_count,
                page_entities,
                expandable_candidate,
            })
        })
        .collect();

    groups.sort_by(|a, b| {
        b.expandable_candidate
            .cmp(&a.expandable_candidate)
            .then_with(|| b.page_entities.len().cmp(&a.page_entities.len()))
            .then_with(|| b.file_count.cmp(&a.file_count))
            .then_with(|| a.folder.cmp(&b.folder))
    });
    groups.truncate(parent_limit);
    groups
}

fn direct_child_stem(source_ref: &str) -> Option<(&str, &str)> {
    let (folder, file_name) = source_ref.split_once('/')?;
    if folder.is_empty() || file_name.contains('/') {
        return None;
    }
    let stem = file_name.strip_suffix(".md").unwrap_or(file_name).trim();
    (!stem.is_empty()).then_some((folder, stem))
}

fn folder_stats(
    docs: &[ProcessedDocument],
    metrics: &HashMap<String, Metric>,
    folder_page_entities: &[FolderPageEntityGroup],
    limit: usize,
) -> Vec<FolderStat> {
    let mut word_counts: HashMap<String, i64> = HashMap::new();
    for doc in docs {
        for folder in folder_names_for_doc(doc) {
            if !is_structural_folder(&folder) {
                *word_counts.entry(folder).or_default() += doc.word_count;
            }
        }
    }

    let page_entity_by_folder: HashMap<&str, &FolderPageEntityGroup> = folder_page_entities
        .iter()
        .map(|group| (group.folder.as_str(), group))
        .collect();

    let mut stats: Vec<_> = metrics
        .iter()
        .filter_map(|(spec, metric)| {
            let entity_ref = entity_ref_from_spec(spec)?;
            (entity_ref.entity_type == "folder").then(|| {
                let page_group = page_entity_by_folder.get(entity_ref.name).copied();
                FolderStat {
                    folder: entity_ref.name.to_string(),
                    spec: spec.clone(),
                    file_count: metric.files.len(),
                    word_count: *word_counts.get(entity_ref.name).unwrap_or(&0),
                    latest_modified: metric.latest_modified.and_then(millis_to_rfc3339),
                    page_entities: page_group
                        .map(|group| {
                            group
                                .page_entities
                                .iter()
                                .map(|entity| entity.name.clone())
                                .collect()
                        })
                        .unwrap_or_default(),
                    expandable_candidate: page_group
                        .map(|group| group.expandable_candidate)
                        .unwrap_or(false),
                }
            })
        })
        .collect();
    stats.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.folder.cmp(&b.folder))
    });
    stats.truncate(limit);
    stats
}

fn folder_children(
    metrics: &HashMap<String, Metric>,
    parent_limit: usize,
    child_limit: usize,
) -> Vec<ChildGroup> {
    let mut groups: HashMap<String, Vec<NamedCount>> = HashMap::new();

    for (spec, metric) in metrics {
        let Some(entity_ref) = entity_ref_from_spec(spec) else {
            continue;
        };
        if entity_ref.entity_type != "folder" || entity_ref.name == ROOT_FOLDER_ENTITY {
            continue;
        }
        let parent = entity_ref
            .name
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_else(|| ROOT_FOLDER_ENTITY.to_string());
        groups.entry(parent).or_default().push(NamedCount {
            name: entity_ref.name.to_string(),
            count: metric.files.len(),
        });
    }

    sorted_child_groups(groups, parent_limit, child_limit)
}

fn tag_children(
    metrics: &HashMap<String, Metric>,
    parent_limit: usize,
    child_limit: usize,
) -> Vec<ChildGroup> {
    let mut groups: HashMap<String, Vec<NamedCount>> = HashMap::new();

    for (spec, metric) in metrics {
        let Some(entity_ref) = entity_ref_from_spec(spec) else {
            continue;
        };
        if entity_ref.entity_type != "tag" || !entity_ref.name.contains('/') {
            continue;
        }
        let parent = entity_ref
            .name
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        groups.entry(parent).or_default().push(NamedCount {
            name: entity_ref.name.to_string(),
            count: metric.files.len(),
        });
    }

    sorted_child_groups(groups, parent_limit, child_limit)
}

fn sorted_child_groups(
    groups: HashMap<String, Vec<NamedCount>>,
    parent_limit: usize,
    child_limit: usize,
) -> Vec<ChildGroup> {
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(parent, mut children)| {
            children.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
            children.truncate(child_limit);
            ChildGroup { parent, children }
        })
        .collect();

    groups.sort_by(|a, b| {
        let a_total: usize = a.children.iter().map(|child| child.count).sum();
        let b_total: usize = b.children.iter().map(|child| child.count).sum();
        b_total.cmp(&a_total).then_with(|| a.parent.cmp(&b.parent))
    });
    groups.truncate(parent_limit);
    groups
}

fn frontmatter_samples(docs: &[ProcessedDocument], limit: usize) -> Vec<FrontmatterSample> {
    let mut docs: Vec<_> = docs
        .iter()
        .filter(|doc| doc.frontmatter.is_some() && !is_structural_doc(doc))
        .collect();
    docs.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.source_ref.cmp(&b.source_ref))
    });

    docs.into_iter()
        .take(limit)
        .filter_map(|doc| {
            let fm = doc.frontmatter.as_ref()?;
            let mut extra_keys: Vec<_> = fm.extra.keys().cloned().collect();
            extra_keys.sort();
            Some(FrontmatterSample {
                source_ref: doc.source_ref.clone(),
                title: fm.title.clone().or_else(|| doc.title.clone()),
                tags: fm.tags.clone(),
                aliases: fm.aliases.clone(),
                links: fm.links.clone(),
                extra_keys,
                created: fm.created.map(|dt| dt.to_rfc3339()),
                modified: fm.modified.map(|dt| dt.to_rfc3339()),
            })
        })
        .collect()
}

fn entity_samples(
    docs: &[ProcessedDocument],
    entity_specs: &[String],
    entity_limit: usize,
    samples_per_entity: usize,
) -> Vec<EntitySample> {
    entity_specs
        .iter()
        .take(entity_limit)
        .filter_map(|spec| {
            let entity_ref = entity_ref_from_spec(spec)?;
            let mut matching_docs: Vec<_> = docs
                .iter()
                .filter(|doc| doc_matches_entity(doc, &entity_ref))
                .collect();
            matching_docs.sort_by(|a, b| {
                b.modified_at
                    .cmp(&a.modified_at)
                    .then_with(|| a.source_ref.cmp(&b.source_ref))
            });
            let samples = matching_docs
                .into_iter()
                .take(samples_per_entity)
                .map(|doc| FileExcerpt {
                    source_ref: doc.source_ref.clone(),
                    title: doc.title.clone(),
                    modified_at: millis_to_rfc3339(doc.modified_at),
                    excerpt: excerpt_for_entity(doc, &entity_ref),
                })
                .collect::<Vec<_>>();

            (!samples.is_empty()).then(|| EntitySample {
                spec: spec.clone(),
                entity_type: entity_ref.entity_type.to_string(),
                name: entity_ref.name.to_string(),
                samples,
            })
        })
        .collect()
}

fn sample_files(docs: &[ProcessedDocument], limit: usize) -> Vec<SampleFile> {
    let mut docs: Vec<_> = docs.iter().filter(|doc| !is_structural_doc(doc)).collect();
    docs.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.source_ref.cmp(&b.source_ref))
    });

    docs.into_iter()
        .take(limit)
        .map(|doc| SampleFile {
            source_ref: doc.source_ref.clone(),
            title: doc.title.clone(),
            folders: folder_names_for_doc(doc),
            tags: doc
                .tags
                .iter()
                .filter(|tag| !is_structural_tag(tag))
                .cloned()
                .collect(),
            links: doc.links.clone(),
            word_count: doc.word_count,
            modified_at: millis_to_rfc3339(doc.modified_at),
        })
        .collect()
}

fn doc_matches_entity(doc: &ProcessedDocument, entity_ref: &EntityRef<'_>) -> bool {
    match entity_ref.entity_type {
        "folder" => folder_names_for_doc(doc)
            .iter()
            .any(|folder| folder == entity_ref.name),
        "tag" => doc.tags.iter().any(|tag| tag == entity_ref.name),
        "link" => doc.links.iter().any(|link| link == entity_ref.name),
        _ => false,
    }
}

fn excerpt_for_entity(doc: &ProcessedDocument, entity_ref: &EntityRef<'_>) -> String {
    let body = strip_frontmatter(&doc.content);
    let needles = match entity_ref.entity_type {
        "tag" => vec![format!("#{}", entity_ref.name), entity_ref.name.to_string()],
        "link" => vec![
            format!("[[{}", entity_ref.name),
            entity_ref.name.to_string(),
        ],
        _ => vec![entity_ref.name.to_string()],
    };

    for needle in needles {
        if let Some(excerpt) = excerpt_around(body, &needle, 320) {
            return excerpt;
        }
    }

    first_excerpt(body, 320)
}

fn excerpt_around(content: &str, needle: &str, max_chars: usize) -> Option<String> {
    let lower_content = content.to_lowercase();
    let lower_needle = needle.to_lowercase();
    let index = lower_content.find(&lower_needle)?;
    let start = content[..index]
        .char_indices()
        .rev()
        .nth(max_chars / 4)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end = content[index..]
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| index + idx)
        .unwrap_or(content.len());
    Some(clean_excerpt(&content[start..end]))
}

fn first_excerpt(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    let end = trimmed
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(trimmed.len());
    clean_excerpt(&trimmed[..end])
}

fn clean_excerpt(excerpt: &str) -> String {
    excerpt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn folder_names_for_doc(doc: &ProcessedDocument) -> Vec<String> {
    if doc.folders.is_empty() {
        vec![ROOT_FOLDER_ENTITY.to_string()]
    } else {
        doc.folders.clone()
    }
}

fn is_structural_doc(doc: &ProcessedDocument) -> bool {
    doc.folders
        .iter()
        .any(|folder| is_structural_folder(folder))
}

#[derive(Clone, Copy)]
struct EntityRef<'a> {
    entity_type: &'a str,
    name: &'a str,
}

fn entity_ref_from_spec(spec: &str) -> Option<EntityRef<'_>> {
    if let Some(name) = spec.strip_prefix("folder:") {
        Some(EntityRef {
            entity_type: "folder",
            name,
        })
    } else if let Some(name) = spec.strip_prefix('#') {
        Some(EntityRef {
            entity_type: "tag",
            name,
        })
    } else if spec.starts_with("[[") && spec.ends_with("]]") {
        Some(EntityRef {
            entity_type: "link",
            name: &spec[2..spec.len() - 2],
        })
    } else {
        None
    }
}

fn entity_spec(entity_type: &str, name: &str) -> String {
    match entity_type {
        "folder" => format!("folder:{name}"),
        "tag" => format!("#{name}"),
        "link" => format!("[[{name}]]"),
        _ => name.to_string(),
    }
}

fn millis_to_rfc3339(millis: i64) -> Option<String> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|dt| dt.to_rfc3339())
}

fn suggested_folders(folder_counts: &HashMap<String, usize>) -> Vec<String> {
    sorted_counts(folder_counts)
        .into_iter()
        .filter(|(folder, _)| is_base_folder_candidate(folder))
        .map(|(folder, _)| folder)
        .collect()
}

fn suggested_entities(
    docs: &[ProcessedDocument],
    folder_counts: &HashMap<String, usize>,
    tag_counts: &HashMap<String, usize>,
    link_counts: &HashMap<String, usize>,
    folder_page_entities: &[FolderPageEntityGroup],
) -> Vec<String> {
    let mut entities = Vec::new();
    let mut seen = HashSet::new();
    let mut selected_folders = HashSet::new();

    if is_flat_vault(docs) {
        push_entity(&mut entities, &mut seen, "folder:.".to_string());
    } else {
        for folder in suggested_folders(folder_counts) {
            selected_folders.insert(folder.clone());
            push_entity(&mut entities, &mut seen, format!("folder:{folder}"));
        }
    }

    let covered_page_links: HashSet<&str> = folder_page_entities
        .iter()
        .filter(|group| selected_folders.contains(&group.folder))
        .flat_map(|group| {
            group
                .page_entities
                .iter()
                .map(|entity| entity.name.as_str())
        })
        .collect();

    for (tag, _) in sorted_counts(tag_counts)
        .into_iter()
        .filter(|(tag, _)| !is_structural_tag(tag))
        .take(20)
    {
        push_entity(&mut entities, &mut seen, format!("#{tag}"));
    }
    for (link, _) in sorted_counts(link_counts)
        .into_iter()
        .filter(|(link, count)| {
            *count >= 3 && !link.contains('/') && !covered_page_links.contains(link.as_str())
        })
        .take(8)
    {
        push_entity(&mut entities, &mut seen, format!("[[{link}]]"));
    }

    entities
}

fn is_flat_vault(docs: &[ProcessedDocument]) -> bool {
    !docs.is_empty()
        && docs.iter().all(|doc| {
            doc.folders
                .iter()
                .all(|folder| folder == ROOT_FOLDER_ENTITY)
        })
}

fn is_base_folder_candidate(folder: &str) -> bool {
    !folder.contains('/') && !is_structural_folder(folder) && folder != ROOT_FOLDER_ENTITY
}

fn sorted_counts(map: &HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut entries: Vec<_> = map
        .iter()
        .map(|(name, count)| (name.clone(), *count))
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

fn push_entity(entities: &mut Vec<String>, seen: &mut HashSet<String>, entity: String) {
    if seen.insert(entity.clone()) {
        entities.push(entity);
    }
}

fn default_excluded_folders() -> Vec<String> {
    vec![
        ".agents".to_string(),
        ".aside".to_string(),
        ".claude".to_string(),
        ".codex".to_string(),
        ".codex-work".to_string(),
        ".conversations".to_string(),
        ".enzyme".to_string(),
        ".git".to_string(),
        ".hermes".to_string(),
        ".local".to_string(),
        ".obsidian".to_string(),
        ".pi".to_string(),
        ".trash".to_string(),
        "__pycache__".to_string(),
        "build".to_string(),
        "dist".to_string(),
        "node_modules".to_string(),
        "target".to_string(),
        "templates".to_string(),
    ]
}

fn is_structural_folder(folder: &str) -> bool {
    folder.split('/').any(is_structural_folder_component)
}

fn is_structural_folder_component(name: &str) -> bool {
    if name.starts_with('.') && name != ROOT_FOLDER_ENTITY {
        return true;
    }

    matches!(
        name,
        ".agents"
            | ".aside"
            | ".claude"
            | ".codex"
            | ".codex-work"
            | ".conversations"
            | ".enzyme"
            | ".git"
            | ".hermes"
            | ".local"
            | ".obsidian"
            | ".pi"
            | ".trash"
            | "__pycache__"
            | "build"
            | "demo"
            | "demo-bible-study-vault"
            | "dist"
            | "node_modules"
            | "obsidian-demo-vault"
            | "target"
            | "templates"
            | "templater"
    )
}

fn is_structural_tag(tag: &str) -> bool {
    matches!(tag, "todo" | "template" | "archived" | "archive")
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_core::config::enzyme_config::EnzymeConfig;
    use enzyme_core::document::Frontmatter;
    use enzyme_core::models::DateSource;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn doc(folders: Vec<&str>) -> ProcessedDocument {
        doc_with(folders, Vec::new(), Vec::new(), None, "note.md", 0, "")
    }

    fn doc_with(
        folders: Vec<&str>,
        tags: Vec<&str>,
        links: Vec<&str>,
        frontmatter: Option<Frontmatter>,
        source_ref: &str,
        modified_at: i64,
        content: &str,
    ) -> ProcessedDocument {
        ProcessedDocument {
            source_ref: source_ref.to_string(),
            title: Some(source_ref.trim_end_matches(".md").to_string()),
            content: content.to_string(),
            content_hash: "hash".to_string(),
            word_count: content.split_whitespace().count() as i64,
            char_count: content.chars().count() as i64,
            created_at: modified_at,
            created_source: DateSource::Filesystem,
            modified_at,
            modified_source: DateSource::Filesystem,
            tags: tags.into_iter().map(str::to_string).collect(),
            links: links.into_iter().map(str::to_string).collect(),
            folders: folders.into_iter().map(str::to_string).collect(),
            chunks: Vec::new(),
            frontmatter,
        }
    }

    fn restore_env(old: Option<String>) {
        // SAFETY: tests that mutate ENZYME_HOME hold ENV_LOCK.
        unsafe {
            match old {
                Some(value) => std::env::set_var("ENZYME_HOME", value),
                None => std::env::remove_var("ENZYME_HOME"),
            }
        }
    }

    #[test]
    fn structural_folder_excludes_descendants() {
        assert!(is_structural_folder(".claude"));
        assert!(is_structural_folder(".claude/commands"));
        assert!(is_structural_folder("projects/.obsidian/cache"));
        assert!(is_structural_folder("work/templates/client"));
        assert!(is_structural_folder(".agents/runs"));
        assert!(is_structural_folder("notes/.pi/agents"));
        assert!(is_structural_folder("projects/target/debug"));
        assert!(is_structural_folder("build/generated"));
        assert!(is_structural_folder("inbox/.aside"));
        assert!(is_structural_folder("demo/synthetic-rosebud-vault"));
        assert!(is_structural_folder("obsidian-demo-vault"));
        assert!(!is_structural_folder("people"));
        assert!(!is_structural_folder("projects/client-a"));
    }

    #[test]
    fn suggested_folders_filters_structural_descendants() {
        let mut counts = HashMap::new();
        counts.insert("emails".to_string(), 5);
        counts.insert(".claude/commands".to_string(), 10);
        counts.insert(".agents/runs".to_string(), 9);
        counts.insert("people".to_string(), 3);
        counts.insert("target/debug".to_string(), 8);
        counts.insert("inbox/.aside".to_string(), 7);
        counts.insert("demo".to_string(), 6);

        let folders = suggested_folders(&counts);

        assert_eq!(folders, vec!["emails".to_string(), "people".to_string()]);
    }

    #[test]
    fn suggested_folders_fold_descendant_prefixes_into_base_folders() {
        let mut counts = HashMap::new();
        counts.insert("readwise".to_string(), 10);
        counts.insert("readwise/articles".to_string(), 10);
        counts.insert("projects".to_string(), 7);
        counts.insert("projects/done".to_string(), 7);
        counts.insert("interview_prep".to_string(), 5);
        counts.insert("interview_prep/system design".to_string(), 5);

        let folders = suggested_folders(&counts);

        assert_eq!(
            folders,
            vec![
                "readwise".to_string(),
                "projects".to_string(),
                "interview_prep".to_string(),
            ]
        );
    }

    #[test]
    fn flat_vault_suggests_root_folder_entity() {
        let docs = vec![doc(Vec::new()), doc(Vec::new())];

        let entities = suggested_entities(
            &docs,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
        );

        assert_eq!(entities.first().map(String::as_str), Some("folder:."));
    }

    #[test]
    fn scan_json_includes_rich_evidence_and_legacy_fields() {
        let vault = TempDir::new().unwrap();
        let mut fm = Frontmatter {
            title: Some("Alice Project".to_string()),
            tags: vec!["project/x".to_string()],
            ..Frontmatter::default()
        };
        fm.aliases.push("AP".to_string());

        let docs = vec![
            doc_with(
                vec!["projects", "projects/x"],
                vec!["project", "project/x"],
                vec!["alice"],
                Some(fm),
                "projects/x/plan.md",
                1_700_000_000_000,
                "---\ntitle: Alice Project\n---\n#project/x working with [[alice]] on launch notes",
            ),
            doc_with(
                vec!["projects", "projects/x"],
                vec!["project"],
                vec!["alice"],
                None,
                "projects/x/update.md",
                1_700_000_100_000,
                "Follow-up with [[alice]] about #project details",
            ),
        ];

        let suggestion = build_scan_suggestion(vault.path(), &docs).unwrap();
        let value = serde_json::to_value(&suggestion).unwrap();

        assert_eq!(value["files"], 2);
        assert!(value["entities"].is_array());
        assert!(
            value["excluded_folders"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(".agents"))
        );
        assert_eq!(value["schema_version"], "scan.v2");
        assert_eq!(value["summary"]["file_count"], 2);
        assert!(
            value["top_entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["spec"] == "folder:projects")
        );
        assert!(
            value["top_tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["spec"] == "#project")
        );
        assert!(
            value["top_links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["spec"] == "[[alice]]")
        );
        assert!(!value["frontmatter_samples"].as_array().unwrap().is_empty());
        assert!(!value["entity_samples"].as_array().unwrap().is_empty());
        assert!(!value["sample_files"].as_array().unwrap().is_empty());
        assert!(!value["available_profiles"].as_array().unwrap().is_empty());
    }

    #[test]
    fn scan_json_includes_folder_page_entity_evidence() {
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("ENZYME_HOME").ok();
        let home = TempDir::new().unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("ENZYME_HOME", home.path()) };

        let vault = TempDir::new().unwrap();
        let docs = vec![
            doc_with(
                vec!["people"],
                Vec::new(),
                Vec::new(),
                None,
                "people/Alice.md",
                1_700_000_000_000,
                "Alice profile",
            ),
            doc_with(
                vec!["people"],
                Vec::new(),
                Vec::new(),
                None,
                "people/Bob.md",
                1_700_000_010_000,
                "Bob profile",
            ),
            doc_with(
                vec!["projects"],
                Vec::new(),
                vec!["alice", "bob"],
                None,
                "projects/launch.md",
                1_700_000_020_000,
                "Working with [[alice]] and [[bob]]",
            ),
            doc_with(
                vec!["meetings"],
                Vec::new(),
                vec!["alice", "bob"],
                None,
                "meetings/one.md",
                1_700_000_030_000,
                "Talked to [[alice]] and [[bob]]",
            ),
            doc_with(
                vec!["meetings"],
                Vec::new(),
                vec!["alice", "bob"],
                None,
                "meetings/two.md",
                1_700_000_040_000,
                "Follow-up with [[alice]] and [[bob]]",
            ),
        ];

        let suggestion = build_scan_suggestion(vault.path(), &docs).unwrap();
        let value = serde_json::to_value(&suggestion).unwrap();
        let groups = value["folder_page_entities"].as_array().unwrap();
        let people = groups
            .iter()
            .find(|group| group["folder"] == "people")
            .expect("people page-entity group");

        assert_eq!(people["expandable_candidate"], true);
        assert!(
            people["page_entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["name"] == "alice" && entry["spec"] == "[[alice]]" })
        );
        assert!(
            value["folder_stats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["folder"] == "people"
                        && entry["expandable_candidate"] == true
                        && entry["page_entities"]
                            .as_array()
                            .unwrap()
                            .contains(&serde_json::json!("alice"))
                })
        );
        assert!(
            !suggestion
                .entities
                .iter()
                .any(|entity| entity == "[[alice]]"),
            "page link covered by selected folder should not be persisted: {:?}",
            suggestion.entities
        );

        restore_env(old);
    }

    #[test]
    fn scan_write_config_persists_base_folders_not_descendants_or_page_children() {
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("ENZYME_HOME").ok();
        let home = TempDir::new().unwrap();
        let vault = TempDir::new().unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("ENZYME_HOME", home.path()) };

        let files = [
            ("readwise/articles/a.md", "Article about [[alice]]"),
            ("projects/done/b.md", "Project notes for [[alice]]"),
            (
                "interview_prep/system design/c.md",
                "System design with [[alice]]",
            ),
            ("people/Alice.md", "Alice profile"),
        ];
        for (relative, content) in files {
            let path = vault.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        run_scan(vault.path(), true).unwrap();
        let config = EnzymeConfig::load(vault.path()).unwrap().unwrap();
        let refs = config_entity_refs(&config.catalyzed_entities);

        assert!(refs.contains(&"folder:readwise".to_string()), "{refs:?}");
        assert!(refs.contains(&"folder:projects".to_string()), "{refs:?}");
        assert!(
            refs.contains(&"folder:interview_prep".to_string()),
            "{refs:?}"
        );
        assert!(
            !refs.contains(&"folder:readwise/articles".to_string()),
            "{refs:?}"
        );
        assert!(
            !refs.contains(&"folder:projects/done".to_string()),
            "{refs:?}"
        );
        assert!(
            !refs.contains(&"folder:interview_prep/system design".to_string()),
            "{refs:?}"
        );
        assert!(!refs.contains(&"[[alice]]".to_string()), "{refs:?}");

        restore_env(old);
    }
}
