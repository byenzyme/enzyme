//! Diagnostic: show adaptive era clustering on a real vault.
//!
//! Usage: cargo run -p enzyme-core --example era_diagnostic -- ~/obsidian

use std::env;
use enzyme_core::db::Database;
use enzyme_core::pipeline::eras::{cluster_eras_periodic, allocate_catalysts_to_eras};

fn main() {
    let vault_path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: era_diagnostic <vault_path>");
        std::process::exit(1);
    });

    let db_path = format!("{}/.enzyme/enzyme.db", vault_path);
    let db = Database::open(&db_path).expect("failed to open database");

    let entities = db.get_top_entities(20, "recency_score").expect("failed to get entities");

    let budget = 7usize;

    for entity in &entities {
        let name = &entity.name;
        let buckets = &entity.trends.timeline_buckets;
        let freq = entity.frequency;

        if buckets.is_empty() {
            println!("[{}] {}: no buckets", entity.entity_type, name);
            continue;
        }

        let eras = cluster_eras_periodic(buckets, 3, budget);
        let allocs = allocate_catalysts_to_eras(&eras, budget, 1);

        // Compute span
        let mut keys: Vec<&String> = buckets.keys().collect();
        keys.sort();
        let span = format!("{} to {}", keys.first().unwrap(), keys.last().unwrap());
        let span_months = buckets.len();

        let mode = if span_months <= 6 { "monthly" } else if span_months <= 36 { "quarterly" } else { "hybrid" };

        println!("\n[{}] {} (freq={}, span={}, mode={})", entity.entity_type, name, freq, span, mode);

        if eras.len() <= 1 {
            println!("  1 era (flat)");
            continue;
        }

        for (i, era) in eras.iter().enumerate() {
            let alloc = allocs.get(i).copied().unwrap_or(0);
            let pct = (era.count as f64 / freq as f64 * 100.0).round() as i64;
            println!(
                "  {:>3}% | {:<25} {:>4} notes  alloc={}",
                pct, era.label, era.count, alloc
            );
        }
    }
}
