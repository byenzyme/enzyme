use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;

use enzyme_core::document::{DocumentProcessor, FileDiscovery};

pub fn run_scan(vault_path: &Path) -> Result<()> {
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
        // Count files per top-level folder only
        if let Some(folder) = result.folders.first() {
            *folder_file_counts.entry(folder.clone()).or_default() += 1;
        }
    }

    let sorted_entries = |map: HashMap<String, usize>| -> Vec<serde_json::Value> {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.into_iter()
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect()
    };

    let output = json!({
        "files": file_count,
        "folders": sorted_entries(folder_file_counts),
        "tags": sorted_entries(tag_counts),
        "links": sorted_entries(link_counts),
    });

    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}
