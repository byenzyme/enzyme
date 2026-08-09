//! In-process engine smoke test.
//!
//! Requires the `local-llm` feature and an installed GGUF; skipped otherwise so
//! CI never needs a 100 MB fixture. Run with:
//!
//! ```text
//! ENZYME_HOME=~/.enzyme ENZYME_LOCAL_MODEL_TEST=1 \
//!   cargo test -p enzyme-core --features local-llm --test local_engine_smoke -- --nocapture
//! ```

#![cfg(feature = "local-llm")]

use enzyme_core::llm::local_engine::LocalEngine;
use enzyme_core::llm::{Message, catalyst_json_schema, gbnf, model_registry};

#[test]
fn generates_grammar_valid_catalyst_json() {
    if std::env::var("ENZYME_LOCAL_MODEL_TEST").is_err() {
        eprintln!("skipping: set ENZYME_LOCAL_MODEL_TEST=1 with a model installed");
        return;
    }
    let (name, path) = model_registry::active().expect("no local model installed");
    let engine =
        LocalEngine::load(&path, &name, model_registry::context_window(&name)).expect("load");

    let grammar = gbnf::json_schema_to_gbnf(&catalyst_json_schema(false)).expect("gbnf");
    let messages = vec![
        Message::text(
            "system",
            "You generate catalyst questions for a knowledge base. A catalyst is a \
             single-sentence probing question grounded in specific excerpts from a user's notes.",
        ),
        Message::text(
            "user",
            "Entity: side projects\n\nExcerpts:\n\
             - I keep starting side projects and abandoning them when the novelty fades.\n\
             - Shipped the parser rewrite after three false starts; the deadline is what did it.\n\
             - Read about spaced repetition and immediately wanted to build an app for it.\n\n\
             Generate 3 catalysts.",
        ),
    ];

    let started = std::time::Instant::now();
    let out = engine
        .complete(&messages, Some(&grammar), 512, 0.4)
        .expect("generation failed");
    let elapsed = started.elapsed();
    // Rough rate: the engine's exact token count is a `debug!` field; chars/3.7
    // is close enough for a smoke signal.
    let approx_tokens = out.len() as f64 / 3.7;
    eprintln!(
        "--- {chars} chars (~{approx_tokens:.0} tok) in {elapsed:?} → ~{rate:.1} tok/s\n{out}",
        chars = out.len(),
        rate = approx_tokens / elapsed.as_secs_f64()
    );

    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).expect("grammar-constrained output must be valid JSON");
    assert!(
        parsed["catalysts"].as_array().is_some_and(|a| !a.is_empty()),
        "expected a non-empty catalysts array, got {parsed}"
    );
    assert!(parsed["topicNames"].is_array(), "expected topicNames array");
}
