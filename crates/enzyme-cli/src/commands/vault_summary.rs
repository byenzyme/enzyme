use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use enzyme_core::db::Database;
use enzyme_core::models::EntityType;

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

/// Build a vault summary from the DB.
pub fn build_summary(db: &Database, _vault_path: &Path) -> Result<VaultSummary> {
    // Total file count
    let files: usize = db.query_read(
        "SELECT COUNT(*) FROM docs",
        vec![],
        |row| row.get::<i64>(0),
    )?.first().copied().unwrap_or(0) as usize;

    // Get trending entities (same defaults as SelectionPipeline)
    let trending = db.get_trending_entities(3, 2, 2, 15)?;

    // Collect all trending link names for page entity detection
    let trending_links: Vec<&str> = trending.iter()
        .filter(|e| e.entity_type == EntityType::Link)
        .map(|e| e.name.as_str())
        .collect();

    let mut entities = Vec::new();

    for entity in &trending {
        // Children (for tags and folders with hierarchical names)
        let children = db.get_child_entities(
            &entity.name,
            entity.entity_type.as_str(),
            5,
        ).unwrap_or_default()
            .into_iter()
            .map(|c| SummaryChild { name: c.name, frequency: c.frequency })
            .collect::<Vec<_>>();

        // Page entities (only for folders)
        let page_entities = if entity.entity_type == EntityType::Folder && !trending_links.is_empty() {
            db.get_folder_page_entities(&entity.name, &trending_links).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Determine trend
        let recent = entity.trends.frequency_recent;
        let total = entity.frequency;
        let trend = if total > 0 && recent == total {
            Trend::New
        } else if total > 0 && (recent as f64 / total as f64) > 0.6 {
            Trend::Growing
        } else {
            Trend::Steady
        };

        entities.push(SummaryEntity {
            name: entity.name.clone(),
            entity_type: entity.entity_type.as_str().to_string(),
            frequency: total,
            recent_12mo: recent,
            trend,
            children,
            page_entities,
        });
    }

    Ok(VaultSummary { files, entities })
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

        // Show children indented
        if !entity.children.is_empty() {
            let child_names: Vec<String> = entity.children.iter()
                .map(|c| {
                    // Strip parent prefix for readability
                    let short = c.name.strip_prefix(&entity.name)
                        .and_then(|s| s.strip_prefix('/'))
                        .unwrap_or(&c.name);
                    short.to_string()
                })
                .collect();
            lines.push(format!("    children: {}", child_names.join(", ")));
        }

        // Show page entities for folders
        if !entity.page_entities.is_empty() {
            let page_names: Vec<String> = entity.page_entities.iter()
                .map(|p| format!("[[{}]]", p))
                .collect();
            lines.push(format!("    → {}", page_names.join(", ")));
        }
    }

    lines.join("\n")
}

/// Format a vault summary as a one-line guide string suitable for `--guide`.
pub fn format_summary_guide(summary: &VaultSummary) -> String {
    let mut parts = Vec::new();

    let folders: Vec<_> = summary.entities.iter()
        .filter(|e| e.entity_type == "folder")
        .take(8)
        .map(|e| format!("{}/", e.name))
        .collect();
    if !folders.is_empty() {
        parts.push(format!("{} notes across folders like {}", summary.files, folders.join(", ")));
    } else {
        parts.push(format!("{} notes", summary.files));
    }

    let tags: Vec<_> = summary.entities.iter()
        .filter(|e| e.entity_type == "tag")
        .take(10)
        .map(|e| format!("#{}", e.name))
        .collect();
    if !tags.is_empty() {
        parts.push(format!("frequent tags {}", tags.join(", ")));
    }

    let links: Vec<_> = summary.entities.iter()
        .filter(|e| e.entity_type == "link")
        .take(8)
        .map(|e| format!("[[{}]]", e.name))
        .collect();
    if !links.is_empty() {
        parts.push(format!("hub pages {}", links.join(", ")));
    }

    parts.join(". ")
}
