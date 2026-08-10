//! Context-pool correctness gate for the in-process engine.
//!
//! Four simultaneous grammar-constrained generations on distinct prompts must
//! all return valid JSON with no crash and no cross-talk. Requires the
//! `local-llm` feature and an installed GGUF; skipped otherwise so CI never
//! needs a 100 MB fixture. Run with:
//!
//! ```text
//! ENZYME_HOME=~/.enzyme ENZYME_LOCAL_MODEL_TEST=1 ENZYME_LLM_CONCURRENCY=2 \
//!   cargo test -p enzyme-core --features local-llm \
//!   --test local_engine_concurrency -- --nocapture
//! ```

#![cfg(feature = "local-llm")]

use std::sync::Arc;
use std::time::Instant;

use enzyme_core::llm::local_engine::LocalEngine;
use enzyme_core::llm::{Message, catalyst_json_schema, gbnf, model_registry};

/// Generous cap: grammars only guarantee prefix validity, so a truncated
/// generation is a hard error and would make the test flaky at 512.
const MAX_TOKENS: u32 = 1024;

const SYSTEM: &str = "You generate catalyst questions for a knowledge base. A catalyst is a \
     single-sentence probing question grounded in specific excerpts from a user's notes.";

/// Four deliberately unrelated entities: if the pool leaked KV state or grammar
/// state between contexts, the outputs would bleed into each other.
const ENTITIES: [(&str, &str); 4] = [
    (
        "side projects",
        "- I keep starting side projects and abandoning them when the novelty fades.\n\
         - Shipped the parser rewrite after three false starts; the deadline is what did it.\n\
         - Read about spaced repetition and immediately wanted to build an app for it.",
    ),
    (
        "sourdough",
        "- The starter died again after I skipped two feedings in a row.\n\
         - Higher hydration finally gave me an open crumb, but the loaf spread flat.\n\
         - I bake on the days I cannot concentrate on anything else.",
    ),
    (
        "commuting",
        "- Forty minutes on the train is the only reading I reliably do all week.\n\
         - Driving in leaves me irritable before the day has even started.\n\
         - I moved closer to work and immediately missed the enforced idleness.",
    ),
    (
        "my father",
        "- He never explained his reasoning, he just showed me the finished thing.\n\
         - I hear his phrasing come out of my mouth when I am tired.\n\
         - We only ever talk easily when there is a task between us.",
    ),
];

fn prompt(entity: &str, excerpts: &str) -> Vec<Message> {
    vec![
        Message::text("system", SYSTEM),
        Message::text(
            "user",
            format!("Entity: {entity}\n\nExcerpts:\n{excerpts}\n\nGenerate 3 catalysts."),
        ),
    ]
}

fn check(entity: &str, out: &str) {
    let parsed: serde_json::Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("[{entity}] output is not valid JSON: {e}\n{out}"));
    assert!(
        parsed["catalysts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "[{entity}] expected a non-empty catalysts array, got {parsed}"
    );
    assert!(
        parsed["topicNames"].is_array(),
        "[{entity}] expected topicNames array"
    );
}

#[test]
fn parallel_generations_are_isolated() {
    if std::env::var("ENZYME_LOCAL_MODEL_TEST").is_err() {
        eprintln!("skipping: set ENZYME_LOCAL_MODEL_TEST=1 with a model installed");
        return;
    }
    let (name, path) = model_registry::active().expect("no local model installed");
    let engine = Arc::new(
        LocalEngine::load(&path, &name, model_registry::context_window(&name)).expect("load"),
    );
    let grammar = Arc::new(gbnf::json_schema_to_gbnf(&catalyst_json_schema(false)).expect("gbnf"));

    // Sequential baseline, then the same four prompts all at once.
    let started = Instant::now();
    let mut sequential = Vec::new();
    for (entity, excerpts) in ENTITIES {
        let out = engine
            .complete(&prompt(entity, excerpts), Some(&grammar), MAX_TOKENS, 0.4)
            .unwrap_or_else(|e| panic!("[{entity}] sequential generation failed: {e:#}"));
        check(entity, &out);
        sequential.push(out);
    }
    let sequential_wall = started.elapsed();

    let started = Instant::now();
    let handles: Vec<_> = ENTITIES
        .iter()
        .map(|(entity, excerpts)| {
            let engine = Arc::clone(&engine);
            let grammar = Arc::clone(&grammar);
            let messages = prompt(entity, excerpts);
            std::thread::spawn(move || engine.complete(&messages, Some(&grammar), MAX_TOKENS, 0.4))
        })
        .collect();
    let concurrent: Vec<String> = handles
        .into_iter()
        .zip(ENTITIES)
        .map(|(h, (entity, _))| {
            h.join()
                .unwrap_or_else(|_| panic!("[{entity}] generation thread panicked"))
                .unwrap_or_else(|e| panic!("[{entity}] concurrent generation failed: {e:#}"))
        })
        .collect();
    let concurrent_wall = started.elapsed();

    for ((entity, _), out) in ENTITIES.iter().zip(&concurrent) {
        check(entity, out);
    }
    // Cross-talk check: four unrelated prompts must not produce identical
    // bodies. (Sampling is stochastic, so this is about pool plumbing handing a
    // reply to the wrong caller, not about output quality.)
    for i in 0..concurrent.len() {
        for j in (i + 1)..concurrent.len() {
            assert_ne!(
                concurrent[i], concurrent[j],
                "identical output for {} and {} — replies crossed",
                ENTITIES[i].0, ENTITIES[j].0
            );
        }
    }

    eprintln!(
        "--- 4 generations: sequential {sequential_wall:?}, concurrent {concurrent_wall:?} \
         (concurrency knob: {})",
        std::env::var("ENZYME_LLM_CONCURRENCY").unwrap_or_else(|_| "default".into())
    );
}
