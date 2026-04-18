use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use enzyme_core::db::Database;
use enzyme_core::pipeline::entity_map::EntityMap;

/// A snapshot of vault structure and trending entities, built from the DB.
#[derive(Debug, Serialize)]
pub struct VaultSummary {
    pub files: usize,
    pub entities: Vec<SummaryEntity>,
}

#[derive(Debug, Serialize)]
pub struct SummaryEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub frequency: i64,
    pub recent_12mo: i64,
    pub trend: Trend,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SummaryChild>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub page_entities: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SummaryChild {
    pub name: String,
    pub frequency: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Growing,
    Steady,
    New,
}

fn entity_trend(frequency: i64, recent: i64) -> Trend {
    if frequency > 0 && recent == frequency {
        Trend::New
    } else if frequency > 0 && (recent as f64 / frequency as f64) > 0.6 {
        Trend::Growing
    } else {
        Trend::Steady
    }
}

/// Build a vault summary from the DB using EntityMap.
pub fn build_summary(db: &Database, _vault_path: &Path) -> Result<VaultSummary> {
    let entity_map = EntityMap::build(db, 15)?;

    let mut entities = Vec::new();

    // Tags
    for tg in &entity_map.tags {
        let children: Vec<SummaryChild> = tg
            .significant_children
            .iter()
            .map(|c| SummaryChild {
                name: c.name.clone(),
                frequency: c.frequency,
            })
            .collect();

        entities.push(SummaryEntity {
            name: tg.entity.name.clone(),
            entity_type: "tag".to_string(),
            frequency: tg.entity.frequency,
            recent_12mo: tg.entity.trends.frequency_recent,
            trend: entity_trend(tg.entity.frequency, tg.entity.trends.frequency_recent),
            children,
            page_entities: Vec::new(),
        });
    }

    // Folders
    for fg in &entity_map.folders {
        entities.push(SummaryEntity {
            name: fg.entity.name.clone(),
            entity_type: "folder".to_string(),
            frequency: fg.entity.frequency,
            recent_12mo: fg.entity.trends.frequency_recent,
            trend: entity_trend(fg.entity.frequency, fg.entity.trends.frequency_recent),
            children: Vec::new(),
            page_entities: fg.page_entities.clone(),
        });
    }

    // Standalone links
    for link in &entity_map.links {
        entities.push(SummaryEntity {
            name: link.name.clone(),
            entity_type: "link".to_string(),
            frequency: link.frequency,
            recent_12mo: link.trends.frequency_recent,
            trend: entity_trend(link.frequency, link.trends.frequency_recent),
            children: Vec::new(),
            page_entities: Vec::new(),
        });
    }

    Ok(VaultSummary {
        files: entity_map.file_count,
        entities,
    })
}

/// Format a vault summary as human-readable text for the terminal.
pub fn format_summary_human(summary: &VaultSummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!("  Indexed {} notes\n", summary.files));

    for entity in &summary.entities {
        let trend_label = match entity.trend {
            Trend::Growing => ", growing",
            Trend::New => ", new",
            Trend::Steady => "",
        };

        let prefix = match entity.entity_type.as_str() {
            "tag" => format!("#{}", entity.name),
            "link" => format!("[[{}]]", entity.name),
            "folder" => format!("{}/", entity.name),
            _ => entity.name.clone(),
        };

        lines.push(format!(
            "  {} ({} notes, {} recent{})",
            prefix, entity.frequency, entity.recent_12mo, trend_label
        ));

        if !entity.children.is_empty() {
            let child_names: Vec<String> = entity
                .children
                .iter()
                .map(|c| {
                    let short = c
                        .name
                        .strip_prefix(&entity.name)
                        .and_then(|s| s.strip_prefix('/'))
                        .unwrap_or(&c.name);
                    short.to_string()
                })
                .collect();
            lines.push(format!("    children: {}", child_names.join(", ")));
        }

        if !entity.page_entities.is_empty() {
            let page_names: Vec<String> = entity
                .page_entities
                .iter()
                .map(|p| format!("[[{}]]", p))
                .collect();
            lines.push(format!("    → {}", page_names.join(", ")));
        }
    }

    lines.join("\n")
}

/// Format a vault summary as a one-line guide string suitable for `--guide`.
///
/// Groups entities by type, folds significant children into parents,
/// folds page-entity links into their parent folders, and only surfaces
/// standalone links that aren't subsumed by a folder.
pub fn format_summary_guide(summary: &VaultSummary) -> String {
    // Track which tags are children of a parent in the summary
    let child_tag_names: HashSet<&str> = summary
        .entities
        .iter()
        .flat_map(|e| e.children.iter().map(|c| c.name.as_str()))
        .collect();

    let mut sections = Vec::new();

    // ── Tags ─────────────────────────────────────────────────────────────
    let mut tag_parts = Vec::new();
    for entity in &summary.entities {
        if entity.entity_type != "tag" {
            continue;
        }
        if entity.name.contains('/') && child_tag_names.contains(entity.name.as_str()) {
            continue;
        }

        let significant_children: Vec<&str> = entity
            .children
            .iter()
            .map(|c| {
                c.name
                    .strip_prefix(&entity.name)
                    .and_then(|s| s.strip_prefix('/'))
                    .unwrap_or(&c.name)
            })
            .collect();

        if significant_children.is_empty() {
            tag_parts.push(format!("#{}", entity.name));
        } else {
            let subs: Vec<String> = significant_children
                .iter()
                .map(|c| format!("#{}/{}", entity.name, c))
                .collect();
            tag_parts.push(format!("#{}; subtags {}", entity.name, subs.join(", ")));
        }
    }
    if !tag_parts.is_empty() {
        sections.push(format!("Main tags: {}.", tag_parts.join(". ")));
    }

    // ── Folders ──────────────────────────────────────────────────────────
    let mut folder_parts = Vec::new();
    for entity in &summary.entities {
        if entity.entity_type != "folder" {
            continue;
        }
        if entity.name.contains('/') {
            continue;
        }

        if entity.page_entities.is_empty() {
            folder_parts.push(format!("{}/", entity.name));
        } else {
            let pages: Vec<String> = entity
                .page_entities
                .iter()
                .take(5)
                .map(|p| format!("[[{}]]", p))
                .collect();
            let suffix = if entity.page_entities.len() > 5 {
                format!(" and {} more", entity.page_entities.len() - 5)
            } else {
                String::new()
            };
            folder_parts.push(format!("{}/: {}{}", entity.name, pages.join(", "), suffix));
        }
    }
    if !folder_parts.is_empty() {
        sections.push(format!("Main folders: {}.", folder_parts.join(". ")));
    }

    // ── Standalone links ────────────────────────────────────────────────
    let standalone_links: Vec<String> = summary
        .entities
        .iter()
        .filter(|e| e.entity_type == "link" && !e.name.contains('/'))
        .take(8)
        .map(|e| format!("[[{}]]", e.name))
        .collect();
    if !standalone_links.is_empty() {
        sections.push(format!("Main links: {}.", standalone_links.join(", ")));
    }

    format!("{} notes. {}", summary.files, sections.join(" "))
}
