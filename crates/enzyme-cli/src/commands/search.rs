//! `enzyme search`: catalyst, direct, and exact-phrase hits from one read-only open.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::search::index::{Bridged, Hit};
use enzyme_core::search::TopCatalyst;
use enzyme_core::search::index::open_read_only;

pub const SEARCH_SCHEMA: &str = "enzyme.search.v1";

#[derive(Debug, Serialize)]
struct SearchHit {
    path: String,
    score: f64,
    content: String,
    via_catalyst_id: Option<String>,
    via_catalyst_text: Option<String>,
}

impl From<Hit> for SearchHit {
    fn from(hit: Hit) -> Self {
        Self {
            path: hit.path,
            score: hit.score,
            content: hit.content,
            via_catalyst_id: None,
            via_catalyst_text: None,
        }
    }
}

fn bridged_label(bridged: Bridged) -> &'static str {
    match bridged {
        Bridged::Ready => "ready",
        Bridged::NoGenerator => "no_generator",
        Bridged::NoEntities => "no_entities",
    }
}

/// Run every search mode against the index and return the versioned envelope.
///
/// The index is opened read-only: a search never creates, migrates, or writes
/// it. Exact-phrase lookup uses `phrase` when given, otherwise the query.
pub fn build_search_json(
    vault_path: &Path,
    query: &str,
    limit: usize,
    phrase: Option<&str>,
) -> Result<serde_json::Value> {
    let paths = EnzymePaths::new(vault_path)?;
    // An index built by an older binary is not migrated here; like a missing
    // index (and `status`'s `schema_outdated`), it needs `init`/`refresh`: exit 3.
    if paths.vault_db.exists() {
        Database::open_read_only(&paths.vault_db)?
            .validate_expected_schema()
            .with_context(|| {
                format!(
                    "index at {} was built by an older enzyme; run `enzyme refresh` or `enzyme init` to rebuild it",
                    paths.vault_db.display()
                )
            })
            .map_err(super::exit::CliFailure::config)?;
    }
    let index = open_read_only(&paths.home, &paths.vault_db)?
        .with_context(|| {
            format!(
                "no index at {}; run `enzyme init` first",
                paths.vault_db.display()
            )
        })
        .map_err(super::exit::CliFailure::config)?;

    let (catalyst_hits, top_catalysts): (Vec<SearchHit>, Vec<TopCatalyst>) =
        if index.bridged == Bridged::Ready {
            let response = index.catalyst_search_response(query, limit)?;
            let hits = response
                .results
                .into_iter()
                .map(|hit| SearchHit {
                    path: hit.file_path,
                    score: hit.similarity,
                    content: hit.content,
                    via_catalyst_id: hit.via_catalyst_id,
                    via_catalyst_text: hit.via_catalyst_text,
                })
                .collect();
            (hits, response.top_contributing_catalysts)
        } else {
            (Vec::new(), Vec::new())
        };
    let direct_hits: Vec<SearchHit> = index
        .direct_search(query, limit)?
        .into_iter()
        .map(SearchHit::from)
        .collect();
    let phrase = phrase.unwrap_or(query);
    let exact_hits: Vec<SearchHit> = index
        .exact_phrase_search(phrase, limit)?
        .into_iter()
        .map(SearchHit::from)
        .collect();

    Ok(json!({
        "schema": SEARCH_SCHEMA,
        "query": query,
        "phrase": phrase,
        "limit": limit,
        "bridged": bridged_label(index.bridged),
        "document_count": index.document_count,
        "catalyst_hits": catalyst_hits,
        "direct_hits": direct_hits,
        "exact_hits": exact_hits,
        "top_catalysts": top_catalysts,
    }))
}

pub fn run_search(
    vault_path: &Path,
    query: &str,
    limit: usize,
    phrase: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let output = build_search_json(vault_path, query, limit, phrase)?;
    if json_output {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!(
        "Search \"{query}\" · {} documents · bridges {}",
        output["document_count"],
        output["bridged"].as_str().unwrap_or("unknown")
    );
    for (label, key) in [
        ("catalyst", "catalyst_hits"),
        ("direct", "direct_hits"),
        ("exact phrase", "exact_hits"),
    ] {
        let hits = output[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
        println!();
        println!("{label} ({})", hits.len());
        for hit in hits {
            println!(
                "  {:.3}  {}",
                hit["score"].as_f64().unwrap_or(0.0),
                hit["path"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}
