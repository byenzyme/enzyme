use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::document::{DocumentProcessor, FileDiscovery};

use super::vault_summary;

pub fn run_scan(vault_path: &Path, guide: bool) -> Result<()> {
    // Use DB if it exists (richer trending data), otherwise filesystem scan
    let paths = EnzymePaths::new(vault_path)?;
    if paths.vault_db.exists() {
        run_scan_db(&paths.vault_db, vault_path, guide)
    } else {
        run_scan_filesystem(vault_path, guide)
    }
}

fn run_scan_db(db_path: &Path, vault_path: &Path, guide: bool) -> Result<()> {
    let db = Database::open(db_path)?;
    let summary = vault_summary::build_summary(&db, vault_path)?;

    if guide {
        println!("{}", vault_summary::format_summary_guide(&summary));
    } else {
        println!("{}", serde_json::to_string(&summary)?);
    }

    Ok(())
}

/// Filesystem-only scan for pre-init use (desktop app onboarding).
fn run_scan_filesystem(vault_path: &Path, guide: bool) -> Result<()> {
    let discovery = FileDiscovery::new(vault_path.to_path_buf());
    let paths = discovery.discover()?;
    let file_count = paths.len();

    let processor = DocumentProcessor::new(vault_path.to_path_buf());
    let docs = processor.process_all(&paths);

    let mut tag_counts: HashMap<String, usize> = HashMap::new();
    let mut link_counts: HashMap<String, usize> = HashMap::new();
    let mut folder_file_counts: HashMap<String, usize> = HashMap::new();

    for result in docs.into_iter().flatten() {
        for tag in &result.tags {
            *tag_counts.entry(tag.clone()).or_default() += 1;
        }
        for link in &result.links {
            *link_counts.entry(link.clone()).or_default() += 1;
        }
        if let Some(folder) = result.folders.first() {
            *folder_file_counts.entry(folder.clone()).or_default() += 1;
        }
    }

    let sorted = |map: HashMap<String, usize>| -> Vec<(String, usize)> {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };

    let folders = sorted(folder_file_counts);
    let tags = sorted(tag_counts);
    let links = sorted(link_counts);

    if guide {
        let mut parts = Vec::new();
        if !folders.is_empty() {
            let names: Vec<_> = folders
                .iter()
                .take(8)
                .map(|(n, _)| format!("{n}/"))
                .collect();
            parts.push(format!(
                "{file_count} notes across folders like {}",
                names.join(", ")
            ));
        } else {
            parts.push(format!("{file_count} notes"));
        }
        if !tags.is_empty() {
            let names: Vec<_> = tags.iter().take(10).map(|(n, _)| format!("#{n}")).collect();
            parts.push(format!("frequent tags {}", names.join(", ")));
        }
        if !links.is_empty() {
            let names: Vec<_> = links
                .iter()
                .take(8)
                .map(|(n, _)| format!("[[{n}]]"))
                .collect();
            parts.push(format!("hub pages {}", names.join(", ")));
        }
        println!("{}", parts.join(". "));
    } else {
        let to_json = |entries: &[(String, usize)]| -> Vec<serde_json::Value> {
            entries
                .iter()
                .map(|(name, count)| json!({"name": name, "count": count}))
                .collect()
        };
        let output = json!({
            "files": file_count,
            "folders": to_json(&folders),
            "tags": to_json(&tags),
            "links": to_json(&links),
        });
        println!("{}", serde_json::to_string(&output)?);
    }

    Ok(())
}
