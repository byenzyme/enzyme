use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::search::context::{CatalyzeResponse, ContextSearch};

pub async fn run_catalyze(
    vault_path: &Path,
    query: &str,
    limit: usize,
    target: Option<&Path>,
    _register_str: &str,
) -> Result<()> {
    let paths = EnzymePaths::new(vault_path)?;

    if !paths.vault_db.exists() {
        let output = json!({
            "error": "Vault not initialized. Run 'enzyme init' first.",
            "query": query,
            "results": []
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let db = Database::open(&paths.vault_db)?;

    let service = get_embedding_service(&paths.home)?;

    // When a target is provided, open its DB from source's targets/ directory
    let target_db;
    let search = if let Some(target_path) = target {
        let target_db_path = paths.target_db(target_path);
        if !target_db_path.exists() {
            let output = json!({
                "error": format!("Target not initialized. Run 'enzyme apply {}' first.", target_path.display()),
                "query": query,
                "results": []
            });
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
        target_db = Database::open(&target_db_path)?;
        ContextSearch::with_target(&db, &target_db, service)
    } else {
        ContextSearch::new(&db, service)
    };

    let start = Instant::now();
    let response = search.catalyze_search(query, limit)?;
    let processing_time = start.elapsed().as_secs_f64();

    // Merge response fields with metadata into a single JSON object
    let mut output = serde_json::to_value(&response)?;
    if let Some(obj) = output.as_object_mut() {
        obj.insert("query".into(), json!(query));
        obj.insert("search_strategy".into(), json!("catalyze"));
        obj.insert(
            "processing_time".into(),
            json!((processing_time * 1000.0).round() / 1000.0),
        );
        obj.insert("total_results".into(), json!(response.results.len()));
    }

    if atty::is(atty::Stream::Stdout) {
        print_catalyze_tree(query, &response, processing_time);
    } else {
        println!("{}", serde_json::to_string(&output)?);
    }

    Ok(())
}

fn print_catalyze_tree(query: &str, response: &CatalyzeResponse, processing_time: f64) {
    println!("Catalyze \"{}\"", query);

    if response.top_contributing_catalysts.is_empty() {
        println!("└─ no activated bridges");
        return;
    }

    println!("├─ activated bridges");
    for (idx, catalyst) in response.top_contributing_catalysts.iter().enumerate() {
        let bridge_no = idx + 1;
        println!(
            "│  [{}] {} · {:.3} · {} {}",
            bridge_no,
            catalyst.entity,
            catalyst.relevance_score,
            catalyst.contribution_count,
            pluralize(catalyst.contribution_count, "hit", "hits")
        );
        for line in wrap_text(&catalyst.text, bridge_wrap_width(), "│      ") {
            println!("{line}");
        }
        if idx + 1 != response.top_contributing_catalysts.len() {
            println!("│");
        }
    }

    println!("└─ surfaced notes");
    if response.results.is_empty() {
        println!("   └─ no results");
        return;
    }

    for (idx, result) in response.results.iter().enumerate() {
        let is_last = idx + 1 == response.results.len();
        let connector = if is_last { "└─" } else { "├─" };
        let stem = if is_last { "   " } else { "│  " };
        let bridge_label = result
            .via_catalyst_id
            .as_deref()
            .and_then(|id| bridge_index(response, id))
            .map(|n| format!("[{n}] "))
            .unwrap_or_default();

        println!(
            "   {connector} {bridge_label}{} · {:.3}",
            result.file_path, result.similarity
        );
        println!("   {stem}");
        for line in wrap_preserving_blank_lines(&result.content, content_wrap_width(), "   ", stem)
        {
            println!("{line}");
        }
        if !is_last {
            println!("   │");
        }
    }

    println!(
        "\n{} results · {:.3} ms",
        response.results.len(),
        (processing_time * 1000.0).round() / 1000.0
    );
}

fn bridge_index(response: &CatalyzeResponse, catalyst_id: &str) -> Option<usize> {
    response
        .top_contributing_catalysts
        .iter()
        .position(|c| c.id == catalyst_id)
        .map(|idx| idx + 1)
}

fn bridge_wrap_width() -> usize {
    terminal_width().saturating_sub(8).clamp(80, 140)
}

fn content_wrap_width() -> usize {
    terminal_width().saturating_sub(9).clamp(96, 140)
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(120)
}

fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn wrap_preserving_blank_lines(
    text: &str,
    width: usize,
    result_prefix: &str,
    tree_stem: &str,
) -> Vec<String> {
    let prefix = format!("{result_prefix}{tree_stem}   ");
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        if raw_line.trim().is_empty() {
            lines.push(prefix.trim_end().to_string());
            continue;
        }
        lines.extend(wrap_text(raw_line, width, &prefix));
    }
    if text.ends_with('\n') {
        lines.push(prefix.trim_end().to_string());
    }
    lines
}

fn wrap_text(text: &str, width: usize, prefix: &str) -> Vec<String> {
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
            out.push(format!("{prefix}{current}"));
            current.clear();
            current.push_str(word);
        }
    }

    if !current.is_empty() {
        out.push(format!("{prefix}{current}"));
    }

    if out.is_empty() {
        out.push(prefix.trim_end().to_string());
    }

    out
}
