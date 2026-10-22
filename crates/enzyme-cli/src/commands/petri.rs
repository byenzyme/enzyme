use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tracing::debug;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;

use enzyme_core::models::{Catalyst, Entity, EntityType, parse_catalyst_text};
use enzyme_core::pipeline::entity_map::{
    EntityDisposition, EntityKey, EntityMap, FolderGroup, FolderOrigin,
};
use enzyme_core::pipeline::selection_steps::{
    CATALYZED_FOLDER_CHILD_LIMIT, select_folder_children,
};

/// Build the petri JSON value from the vault database.
/// Assumes the DB exists — callers must guard this.
pub fn build_petri_json(
    vault_path: &Path,
    top: usize,
    query: Option<&str>,
    catalyst_budget: Option<usize>,
) -> Result<serde_json::Value> {
    let paths = EnzymePaths::new(vault_path)?;
    let db = Database::open(&paths.vault_db)?;

    // Select entities: query-aware or recency-based
    let mut entity_relevances: HashMap<String, f64> = HashMap::new();
    let mut catalyst_relevances: HashMap<String, f64> = HashMap::new();

    let mut entities = if let Some(query_text) = query {
        match build_query_aware(
            &paths,
            &db,
            top,
            query_text,
            catalyst_budget.unwrap_or(3),
            &mut entity_relevances,
            &mut catalyst_relevances,
        ) {
            Ok(ents) if !ents.is_empty() => ents,
            Ok(_) => {
                debug!("query-aware petri returned no results, falling back to recency");
                entity_relevances.clear();
                catalyst_relevances.clear();
                build_recency_based(&db, top)?
            }
            Err(e) => {
                debug!("query-aware petri failed: {}, falling back to recency", e);
                build_recency_based(&db, top)?
            }
        }
    } else {
        build_recency_based(&db, top)?
    };

    // Cap catalysts per entity. The recency path previously emitted every
    // catalyst an entity had, so a large vault returned hundreds of KB that no
    // caller could use; `--catalyst-budget 0` still asks for all of them.
    let per_entity = catalyst_budget.unwrap_or(if query.is_some() { 3 } else { 7 });
    if per_entity > 0 {
        for (_, catalysts) in entities.iter_mut() {
            catalysts.truncate(per_entity);
        }
    }

    // Canonical structural facts for the selected entity set.
    let entity_list_only: Vec<Entity> = entities.iter().map(|(e, _)| e.clone()).collect();
    let entity_map = EntityMap::from_entities(&db, &entity_list_only)?;

    let mut tag_children: HashMap<String, Vec<&(Entity, Vec<Catalyst>)>> = HashMap::new();
    let mut top_level: Vec<&(Entity, Vec<Catalyst>)> = Vec::new();
    let mut folder_children: HashMap<String, Vec<&(Entity, Vec<Catalyst>)>> = HashMap::new();

    for entry in &entities {
        match entity_map.disposition_for(&entry.0) {
            EntityDisposition::TopLevel => top_level.push(entry),
            EntityDisposition::FoldedIntoTag { parent_name } => {
                tag_children.entry(parent_name).or_default().push(entry);
            }
            EntityDisposition::FoldedIntoFolder { folder_name } => {
                folder_children.entry(folder_name).or_default().push(entry);
            }
            EntityDisposition::ShadowedByTag { .. } => {
                continue;
            }
        }
    }

    let top_level_keys: HashMap<EntityKey, &(Entity, Vec<Catalyst>)> = top_level
        .iter()
        .map(|entry| (EntityKey::from(&entry.0), *entry))
        .collect();

    // Build JSON
    let mut entity_list = Vec::new();

    for (entity, catalysts) in &top_level {
        let mut entry =
            build_entity_json(entity, catalysts, &entity_relevances, &catalyst_relevances);

        if entity.entity_type == EntityType::Tag {
            if let Some(children) = tag_children.get(&entity.name) {
                let nested: Vec<serde_json::Value> = children
                    .iter()
                    .map(|(child_entity, child_catalysts)| {
                        build_entity_json(
                            child_entity,
                            child_catalysts,
                            &entity_relevances,
                            &catalyst_relevances,
                        )
                    })
                    .collect();
                if !nested.is_empty() {
                    entry
                        .as_object_mut()
                        .unwrap()
                        .insert("significant_children".to_string(), json!(nested));
                }
            }
        }

        if entity.entity_type == EntityType::Folder {
            if let Some(folder_group) = entity_map.folder_group(&entity.name) {
                enrich_folder_entry(
                    &db,
                    folder_group,
                    folder_children.get(&entity.name),
                    &mut entry,
                    &entity_relevances,
                    &catalyst_relevances,
                    per_entity,
                )?;
            }
        }

        entity_list.push(entry);
    }

    for folder_group in &entity_map.folders {
        let key = EntityKey::from(&folder_group.entity);
        if top_level_keys.contains_key(&key) || folder_group.origin != FolderOrigin::Synthetic {
            continue;
        }

        let mut entry = build_entity_json(
            &folder_group.entity,
            &[],
            &entity_relevances,
            &catalyst_relevances,
        );
        enrich_folder_entry(
            &db,
            folder_group,
            folder_children.get(&folder_group.entity.name),
            &mut entry,
            &entity_relevances,
            &catalyst_relevances,
            per_entity,
        )?;
        entry
            .as_object_mut()
            .unwrap()
            .insert("synthetic".to_string(), json!(true));
        entity_list.push(entry);
    }

    let prepared_targets = db.get_all_applied_targets()?;
    let targets_json: Vec<serde_json::Value> = prepared_targets
        .iter()
        .filter_map(|t| {
            let path = super::target::display_target_path(vault_path, &t.target_path)?;
            Some(json!({
                "path": path,
                "doc_count": t.doc_count,
                "catalyst_count": t.catalyst_count,
                "prepared_at": t.applied_at.to_rfc3339(),
            }))
        })
        .collect();

    let mut output = json!({
        "entities": entity_list,
        "total_entities": entity_list.len(),
        "targets": targets_json,
    });

    if let Some(q) = query {
        output
            .as_object_mut()
            .unwrap()
            .insert("query".to_string(), json!(q));
    }

    Ok(output)
}

fn enrich_folder_entry(
    db: &Database,
    folder_group: &FolderGroup,
    page_entity_children: Option<&Vec<&(Entity, Vec<Catalyst>)>>,
    entry: &mut serde_json::Value,
    entity_relevances: &HashMap<String, f64>,
    catalyst_relevances: &HashMap<String, f64>,
    per_entity: usize,
) -> Result<()> {
    if !folder_group.page_entities.is_empty() {
        let (children, total_count) =
            db.get_folder_children_by_references(&folder_group.entity.name, 10)?;

        let sampled: Vec<serde_json::Value> = children
            .iter()
            .map(|c| {
                json!({
                    "entity": format!("[[{}]]", c.name),
                    "reference_count": c.reference_count,
                    "last_seen": c.last_seen,
                })
            })
            .collect();

        let catalyzed = select_folder_children(
            db,
            &folder_group.entity.name,
            &folder_group.page_entities,
            CATALYZED_FOLDER_CHILD_LIMIT,
        );

        let sampled_count = sampled.len() as i64;
        let obj = entry.as_object_mut().unwrap();
        obj.insert("expandable".to_string(), json!(true));
        obj.insert("total_children".to_string(), json!(total_count));
        obj.insert("sampled_children".to_string(), json!(sampled));
        obj.insert("catalyzed_children".to_string(), json!(catalyzed));
        obj.insert(
            "additional_children_available".to_string(),
            json!(total_count - sampled_count),
        );
    }

    // Show all catalyzed page entity children from the DB, not just
    // query-matched ones. This ensures expandable folders always surface
    // their full child set.
    {
        let all_page_links =
            db.get_folder_page_entity_links(&std::collections::HashSet::from([folder_group
                .entity
                .name
                .as_str()]))?;
        let mut nested: Vec<serde_json::Value> = Vec::new();
        for link_entity in &all_page_links {
            let mut catalysts = db.get_catalysts_for_entity(&link_entity.name)?;
            if catalysts.is_empty() {
                continue;
            }
            // Nested children read straight from the DB, so they need the same
            // cap as the top level or they dominate the payload.
            if per_entity > 0 {
                catalysts.truncate(per_entity);
            }
            let mut child_json = build_entity_json(
                link_entity,
                &catalysts,
                entity_relevances,
                catalyst_relevances,
            );
            // Prefer query-matched version if available (has relevance scores)
            if let Some(children) = page_entity_children {
                for (ce, cc) in children.iter() {
                    if ce.name == link_entity.name {
                        child_json =
                            build_entity_json(ce, cc, entity_relevances, catalyst_relevances);
                    }
                }
            }
            nested.push(child_json);
        }
        // Sort by recency
        nested.sort_by(|a, b| {
            let ra = a
                .get("recency_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let rb = b
                .get("recency_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
        });
        if !nested.is_empty() {
            entry
                .as_object_mut()
                .unwrap()
                .insert("page_entity_children".to_string(), json!(nested));
        }
    }

    Ok(())
}

/// Build JSON for a single entity with its catalysts.
fn build_entity_json(
    entity: &Entity,
    catalysts: &[Catalyst],
    entity_relevances: &HashMap<String, f64>,
    catalyst_relevances: &HashMap<String, f64>,
) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp_millis();
    let last_seen = entity.last_seen.unwrap_or(0);
    let days_since_last = ((now - last_seen) as f64) / (1000.0 * 60.0 * 60.0 * 24.0);

    let activity_trend = if days_since_last <= 7.0 {
        "active"
    } else if days_since_last <= 30.0 {
        "moderate"
    } else {
        "dormant"
    };

    let catalyst_list: Vec<serde_json::Value> = catalysts
        .iter()
        .map(|c| {
            // Stored text carries a fenced list of source notes for the engine;
            // readers get the question alone.
            let text = parse_catalyst_text(&c.text)
                .map(|parsed| parsed.hypothesis)
                .unwrap_or_else(|_| c.text.trim().to_string());
            let mut obj = json!({
                "text": text,
                "context": c.context,
            });
            if let Some(era) = c.metadata.get("era").and_then(|v| v.as_str()) {
                obj.as_object_mut()
                    .unwrap()
                    .insert("era".to_string(), json!(era));
            }
            if let Some(&rel) = catalyst_relevances.get(&c.id) {
                obj.as_object_mut().unwrap().insert(
                    "relevance".to_string(),
                    json!((rel * 1000.0).round() / 1000.0),
                );
            }
            obj
        })
        .collect();

    let mut entry = json!({
        "name": entity.name,
        "type": entity.entity_type.as_str(),
        "frequency": entity.frequency,
        "frequency_12m": entity.trends.frequency_recent,
        "recency_score": entity.trends.recency_score,
        "last_seen": last_seen,
        "days_since_last_seen": days_since_last.round() as i64,
        "activity_trend": activity_trend,
        "catalysts": catalyst_list,
    });

    if let Some(&rel) = entity_relevances.get(&entity.name) {
        entry.as_object_mut().unwrap().insert(
            "relevance".to_string(),
            json!((rel * 1000.0).round() / 1000.0),
        );
    }

    entry
}

/// Select entities by recency score (default behavior when no query is provided).
fn build_recency_based(
    db: &Database,
    top: usize,
) -> Result<
    Vec<(
        enzyme_core::models::Entity,
        Vec<enzyme_core::models::Catalyst>,
    )>,
> {
    let catalyzed_names = db.get_catalyzed_entity_names()?;
    let candidates = db.get_top_entities(top.saturating_mul(3), "recency_score")?;
    let selected: Vec<_> = candidates
        .into_iter()
        .filter(|e| catalyzed_names.contains(&e.name))
        .take(top)
        .collect();
    let mut entities = Vec::new();
    for entity in selected {
        let catalysts = db.get_catalysts_for_entity(&entity.name)?;
        entities.push((entity, catalysts));
    }
    Ok(entities)
}

/// Select entities by query relevance: keyword plus meaning over each
/// entity's catalyst questions, with entities the query names first and no
/// recency term (see `enzyme_core::search::petri`). Returns the top N
/// entities, each with its K best-matching catalysts.
fn build_query_aware(
    paths: &EnzymePaths,
    db: &Database,
    top: usize,
    query: &str,
    catalyst_budget: usize,
    entity_rel: &mut HashMap<String, f64>,
    catalyst_rel: &mut HashMap<String, f64>,
) -> Result<
    Vec<(
        enzyme_core::models::Entity,
        Vec<enzyme_core::models::Catalyst>,
    )>,
> {
    let service = get_embedding_service(&paths.home)?;
    let query_vec = service.embed_query(query)?;

    let catalysts = db.get_all_catalysts()?;
    if catalysts.is_empty() {
        return Ok(vec![]);
    }
    let cached = db.get_all_catalyst_embeddings()?;
    if cached.is_empty() {
        anyhow::bail!("no cached catalyst embeddings");
    }
    let cache_map: HashMap<&str, &Vec<f32>> =
        cached.iter().map(|(id, vec)| (id.as_str(), vec)).collect();

    let entities = db.get_all_entities()?;
    let entity_by_name: HashMap<&str, &Entity> =
        entities.iter().map(|e| (e.name.as_str(), e)).collect();
    let ranked = enzyme_core::search::petri::rank_entities_for_query(
        query, &query_vec, &catalysts, &cache_map, &entities,
    );
    let mut result = Vec::new();
    for ranked_entity in ranked {
        if result.len() >= top {
            break;
        }
        let Some(entity) = entity_by_name.get(ranked_entity.name.as_str()).copied() else {
            continue;
        };
        entity_rel.insert(ranked_entity.name.clone(), ranked_entity.relevance);
        let cats: Vec<Catalyst> = ranked_entity
            .catalysts
            .iter()
            .take(catalyst_budget)
            .map(|&(idx, relevance)| {
                catalyst_rel.insert(catalysts[idx].id.clone(), relevance);
                catalysts[idx].clone()
            })
            .collect();
        result.push((entity.clone(), cats));
    }

    Ok(result)
}

pub async fn run_petri(
    vault_path: &Path,
    top: usize,
    query: Option<&str>,
    catalyst_budget: Option<usize>,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        println!("Vault not initialized. Run 'enzyme init' first.");
        return Ok(());
    }

    let output = build_petri_json(vault_path, top, query, catalyst_budget)?;
    if atty::is(atty::Stream::Stdout) {
        let display_catalyst_budget =
            catalyst_budget.unwrap_or(if query.is_some() { 3 } else { 7 });
        print_petri_tree(&output, display_catalyst_budget);
    } else {
        println!("{}", serde_json::to_string_pretty(&output)?);
    }

    Ok(())
}

fn print_petri_tree(output: &serde_json::Value, catalyst_display_budget: usize) {
    if let Some(query) = output.get("query").and_then(|v| v.as_str()) {
        println!("Petri for \"{query}\"");
    } else {
        println!("Petri");
    }

    let entities = output
        .get("entities")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let targets = output
        .get("targets")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    if entities.is_empty() && targets.is_empty() {
        println!("└─ no catalyzed entities yet");
        return;
    }

    for (idx, entity) in entities.iter().enumerate() {
        let has_targets = !targets.is_empty();
        let is_last = idx + 1 == entities.len() && !has_targets;
        render_entity_tree(entity, "", is_last, catalyst_display_budget);
    }

    if !targets.is_empty() {
        println!("└─ external targets");
        for (idx, target) in targets.iter().enumerate() {
            let is_last = idx + 1 == targets.len();
            let connector = if is_last { "└─" } else { "├─" };
            let path = target
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let docs = target
                .get("doc_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let catalysts = target
                .get("catalyst_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            println!("   {connector} {path} · {docs} docs · {catalysts} catalysts");
        }
    }
}

fn render_entity_tree(
    entity: &serde_json::Value,
    prefix: &str,
    is_last: bool,
    catalyst_display_budget: usize,
) {
    let connector = if is_last { "└─" } else { "├─" };
    let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
    let name = entity
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    let entity_type = entity
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("entity");
    let label = entity_label(entity_type, name);
    let trend = entity
        .get("activity_trend")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let frequency = entity
        .get("frequency")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let relevance = entity
        .get("relevance")
        .and_then(|v| v.as_f64())
        .map(|r| format!(" · relevance {r:.3}"))
        .unwrap_or_default();

    println!(
        "{prefix}{connector} {label} · {entity_type} · {trend} · {frequency} mentions{relevance}"
    );

    let mut sections: Vec<(&str, &str, Option<&Vec<serde_json::Value>>)> = Vec::new();
    sections.push((
        "catalysts",
        "catalysts",
        entity.get("catalysts").and_then(|v| v.as_array()),
    ));
    sections.push((
        "significant children",
        "significant_children",
        entity
            .get("significant_children")
            .and_then(|v| v.as_array()),
    ));
    sections.push((
        "page children",
        "page_entity_children",
        entity
            .get("page_entity_children")
            .and_then(|v| v.as_array()),
    ));
    sections.push((
        "sampled children",
        "sampled_children",
        entity.get("sampled_children").and_then(|v| v.as_array()),
    ));

    let nonempty_sections: Vec<_> = sections
        .into_iter()
        .filter(|(_, _, items)| items.map_or(false, |items| !items.is_empty()))
        .collect();

    for (section_idx, (label, key, items)) in nonempty_sections.iter().enumerate() {
        let section_last = section_idx + 1 == nonempty_sections.len();
        let section_connector = if section_last { "└─" } else { "├─" };
        let section_child_prefix = format!(
            "{}{}",
            child_prefix,
            if section_last { "   " } else { "│  " }
        );
        println!("{child_prefix}{section_connector} {label}");
        let items = items.unwrap();
        if *key == "catalysts" {
            let render_count = items.len().min(catalyst_display_budget.max(1));
            let omitted = items.len().saturating_sub(render_count);
            for (item_idx, item) in items.iter().take(render_count).enumerate() {
                let item_last = item_idx + 1 == render_count && omitted == 0;
                render_catalyst_item(item, &section_child_prefix, item_last);
            }
            if omitted > 0 {
                render_more_item(omitted, &section_child_prefix);
            }
        } else {
            for (item_idx, item) in items.iter().enumerate() {
                let item_last = item_idx + 1 == items.len();
                match *key {
                    "significant_children" | "page_entity_children" => render_entity_tree(
                        item,
                        &section_child_prefix,
                        item_last,
                        catalyst_display_budget,
                    ),
                    "sampled_children" => {
                        render_sampled_child(item, &section_child_prefix, item_last)
                    }
                    _ => {}
                }
            }
        }
    }
}

fn render_more_item(count: usize, prefix: &str) {
    println!("{prefix}└─ … {count} more catalysts");
}

fn render_catalyst_item(catalyst: &serde_json::Value, prefix: &str, is_last: bool) {
    let connector = if is_last { "└─" } else { "├─" };
    let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
    let text = catalyst.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let relevance = catalyst
        .get("relevance")
        .and_then(|v| v.as_f64())
        .map(|r| format!(" · {r:.3}"))
        .unwrap_or_default();
    let mut wrapped = wrap_text(
        text,
        petri_wrap_width(),
        &format!("{prefix}{connector} "),
        &child_prefix,
    );
    if let Some(first) = wrapped.first_mut() {
        first.push_str(&relevance);
    }
    for line in wrapped {
        println!("{line}");
    }
}

fn render_sampled_child(child: &serde_json::Value, prefix: &str, is_last: bool) {
    let connector = if is_last { "└─" } else { "├─" };
    let entity = child
        .get("entity")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    let refs = child
        .get("reference_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    println!("{prefix}{connector} {entity} · {refs} refs");
}

fn entity_label(entity_type: &str, name: &str) -> String {
    match entity_type {
        "tag" => format!("#{name}"),
        "link" => format!("[[{name}]]"),
        "folder" => format!("▣ {name}"),
        _ => format!("• {name}"),
    }
}

fn petri_wrap_width() -> usize {
    crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(120)
        .saturating_sub(12)
        .clamp(80, 140)
}

fn wrap_text(text: &str, width: usize, first_prefix: &str, rest_prefix: &str) -> Vec<String> {
    let available = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= available {
            current.push(' ');
            current.push_str(word);
        } else {
            let prefix = if out.is_empty() {
                first_prefix
            } else {
                rest_prefix
            };
            out.push(format!("{prefix}{current}"));
            current.clear();
            current.push_str(word);
        }
    }

    if !current.is_empty() {
        let prefix = if out.is_empty() {
            first_prefix
        } else {
            rest_prefix
        };
        out.push(format!("{prefix}{current}"));
    }

    if out.is_empty() {
        out.push(first_prefix.trim_end().to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use enzyme_core::db::Database;
    use enzyme_core::models::{Catalyst, EntityTrends};
    use tempfile::TempDir;

    fn create_test_vault() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".enzyme")).unwrap();
        dir
    }

    fn open_test_db(vault_path: &Path) -> Database {
        let paths = EnzymePaths::new(vault_path).unwrap();
        Database::open(&paths.vault_db).unwrap()
    }

    fn insert_doc(db: &Database, source_ref: &str) {
        let now = Utc::now().timestamp_millis();
        db.execute_write(
            &format!(
                "INSERT INTO docs (source_ref, title, content, content_hash, created_at, created_source, modified_at, modified_source, indexed_at)
                 VALUES ('{source_ref}', '{source_ref}', '', 'hash', {now}, 'test', {now}, 'test', {now})"
            ),
            vec![],
        )
        .unwrap();
    }

    fn insert_entity(
        db: &Database,
        name: &str,
        entity_type: EntityType,
        frequency: i64,
        recency_score: f64,
    ) {
        let now = Utc::now().timestamp_millis();
        let trends = serde_json::to_string(&EntityTrends {
            frequency_recent: frequency,
            recency_score,
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

    fn insert_catalyst(db: &Database, entity: &str, id: &str, text: &str) {
        db.insert_catalysts(&[Catalyst {
            id: id.to_string(),
            text: text.to_string(),
            entity: entity.to_string(),
            context: "test".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        }])
        .unwrap();
    }

    #[test]
    fn petri_shows_catalyst_questions_without_their_source_block() {
        let entity = Entity {
            id: None,
            name: "#focus".to_string(),
            entity_type: EntityType::Tag,
            frequency: 1,
            ranking_weight: 1.0,
            first_seen: None,
            last_seen: None,
            trends: EntityTrends::default(),
            metadata: serde_json::json!({}),
        };
        let catalyst = Catalyst {
            id: "c1".to_string(),
            text: "Why did I keep the monolith?\n\n```enzyme-context\n[\"adrs/005.md\"]\n```"
                .to_string(),
            entity: "#focus".to_string(),
            context: "test".to_string(),
            generated_at: Utc::now(),
            metadata: serde_json::json!({}),
        };
        let json = build_entity_json(&entity, &[catalyst], &HashMap::new(), &HashMap::new());
        assert_eq!(json["catalysts"][0]["text"], "Why did I keep the monolith?");
    }

    #[test]
    fn petri_nests_significant_tag_children() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_entity(&db, "enzyme", EntityType::Tag, 100, 100.0);
        insert_entity(&db, "enzyme/pmf", EntityType::Tag, 40, 80.0);
        insert_catalyst(&db, "enzyme", "cat-parent", "parent catalyst");
        insert_catalyst(&db, "enzyme/pmf", "cat-child", "child catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["name"], "enzyme");
        let children = entities[0]["significant_children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "enzyme/pmf");
        assert_eq!(children[0]["type"], "tag");
    }

    #[test]
    fn petri_surfaces_synthetic_folder_page_entity_children() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_doc(&db, "people/Alice Smith.md");
        insert_entity(&db, "alice smith", EntityType::Link, 30, 90.0);
        insert_catalyst(&db, "alice smith", "cat-alice", "alice catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["name"], "people");
        assert_eq!(entities[0]["type"], "folder");
        assert_eq!(entities[0]["synthetic"], true);

        let children = entities[0]["page_entity_children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "alice smith");
        assert_eq!(children[0]["type"], "link");
    }

    #[test]
    fn petri_does_not_duplicate_folded_or_shadowed_entities_at_top_level() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_doc(&db, "people/Alice.md");
        insert_entity(&db, "enzyme", EntityType::Tag, 100, 100.0);
        insert_entity(&db, "enzyme/pmf", EntityType::Tag, 40, 80.0);
        insert_entity(&db, "alice", EntityType::Link, 30, 70.0);
        insert_entity(&db, "shadow", EntityType::Tag, 25, 60.0);
        insert_entity(&db, "shadow", EntityType::Link, 20, 50.0);

        insert_catalyst(&db, "enzyme", "cat-enzyme", "parent catalyst");
        insert_catalyst(&db, "enzyme/pmf", "cat-pmf", "child catalyst");
        insert_catalyst(&db, "alice", "cat-alice", "alice catalyst");
        insert_catalyst(&db, "shadow", "cat-shadow", "shadow catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();

        let top_level_names: Vec<&str> = entities
            .iter()
            .filter_map(|entity| entity["name"].as_str())
            .collect();

        assert!(top_level_names.contains(&"enzyme"));
        assert!(top_level_names.contains(&"people"));
        assert!(top_level_names.contains(&"shadow"));
        assert!(!top_level_names.contains(&"enzyme/pmf"));
        assert!(!top_level_names.contains(&"alice"));

        let enzyme = entities
            .iter()
            .find(|entity| entity["name"] == "enzyme")
            .unwrap();
        let tag_children = enzyme["significant_children"].as_array().unwrap();
        assert_eq!(tag_children.len(), 1);
        assert_eq!(tag_children[0]["name"], "enzyme/pmf");

        let people = entities
            .iter()
            .find(|entity| entity["name"] == "people")
            .unwrap();
        let page_children = people["page_entity_children"].as_array().unwrap();
        assert_eq!(page_children.len(), 1);
        assert_eq!(page_children[0]["name"], "alice");
    }

    #[test]
    fn petri_derives_catalyzed_children_for_expandable_folder_from_runtime_structure() {
        let vault = create_test_vault();
        let db = open_test_db(vault.path());

        insert_doc(&db, "people/Alice.md");
        insert_doc(&db, "people/Bob.md");
        insert_doc(&db, "people/Carol.md");
        insert_doc(&db, "people/Dan.md");

        insert_entity(&db, "alice", EntityType::Link, 53, 72.0);
        insert_entity(&db, "bob", EntityType::Link, 61, 39.0);
        insert_entity(&db, "carol", EntityType::Link, 52, 20.0);
        insert_entity(&db, "dan", EntityType::Link, 8, 20.0);

        let now = Utc::now().timestamp_millis();
        for (target, count) in [("bob", 61), ("alice", 53), ("carol", 52), ("dan", 8)] {
            for i in 0..count {
                let source = format!("notes/{}-{i}.md", target.replace(' ', "-"));
                db.execute_write(
                    &format!(
                        "INSERT INTO docs (source_ref, content_hash, created_at, modified_at, indexed_at)
                         VALUES ('{source}', 'h-{source}', {now}, {now}, {now})"
                    ),
                    vec![],
                )
                .unwrap();

                let doc_id: i64 = db
                    .query_read(
                        &format!("SELECT id FROM docs WHERE source_ref = '{source}'"),
                        vec![],
                        |r| r.get::<i64>(0),
                    )
                    .unwrap()[0];

                db.execute_write(
                    &format!("INSERT INTO doc_links (doc_id, link) VALUES ({doc_id}, '{target}')"),
                    vec![],
                )
                .unwrap();
            }
        }

        insert_catalyst(&db, "alice", "cat-alice", "alice catalyst");
        insert_catalyst(&db, "bob", "cat-bob", "bob catalyst");
        insert_catalyst(&db, "carol", "cat-carol", "carol catalyst");
        insert_catalyst(&db, "dan", "cat-dan", "dan catalyst");

        let json = build_petri_json(vault.path(), 10, None, None).unwrap();
        let entities = json["entities"].as_array().unwrap();
        let people = entities
            .iter()
            .find(|entity| entity["name"] == "people")
            .unwrap();

        let catalyzed_children = people["catalyzed_children"].as_array().unwrap();
        let child_names: Vec<&str> = catalyzed_children
            .iter()
            .filter_map(|child| child.as_str())
            .collect();

        assert_eq!(child_names, vec!["[[bob]]", "[[alice]]", "[[carol]]"]);
    }
}
