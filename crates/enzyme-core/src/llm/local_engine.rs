//! In-process llama.cpp engine for offline catalyst generation.
//!
//! Feature-gated behind `local-llm` — without it nothing here is compiled and
//! the C++ toolchain (cmake + libclang) is never touched. Ported from the
//! 2026-08-08 spike (`docs/2026-08-08-local-llm-bundling-recommendation.md`),
//! including its landmine guards:
//!
//! 1. `LlamaSampler::sample()` **already accepts** the token into the chain.
//!    A manual `accept()` afterwards advances the grammar automaton twice and
//!    aborts the process inside llama.cpp (`GGML_ASSERT(!stacks.empty())`).
//!    Never call `accept()` here.
//! 2. `n_batch` must cover the whole prefill; exceeding it is an abort, not an
//!    error. We size it to the context window and refuse over-long prompts.
//! 3. GBNF guarantees *prefix* validity only, so hitting the token cap yields
//!    unparseable JSON. Truncation is a hard error, not a silent partial.
//!
//! Generation is serialized behind one mutex: the models we ship are small and
//! prefill-bound, so parallel contexts would only oversubscribe the same cores
//! (see `llm::hardware`). No slot scheduler.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use tracing::debug;

use super::client::{Message, MessageContent};
use super::hardware;

/// A loaded GGUF ready for grammar-constrained completion.
pub struct LocalEngine {
    backend: LlamaBackend,
    model: LlamaModel,
    n_ctx: u32,
    threads: i32,
    /// Serializes generation — see the module note.
    lock: Mutex<()>,
}

impl LocalEngine {
    /// Load a GGUF. Prints one line naming the model and its size; nothing else
    /// is ever printed by the engine.
    pub fn load(path: &Path, name: &str, context_window: u32) -> Result<Self> {
        // llama.cpp is extremely chatty on stderr by default. Route its logs
        // through `tracing` so they obey the CLI's existing verbosity flags and
        // stay silent at the default `warn` level.
        static LOGS: std::sync::Once = std::sync::Once::new();
        LOGS.call_once(|| {
            llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());
        });

        let backend = LlamaBackend::init().context("failed to initialize llama.cpp backend")?;
        let started = Instant::now();
        let model = LlamaModel::load_from_file(&backend, path, &LlamaModelParams::default())
            .with_context(|| format!("failed to load model {}", path.display()))?;
        let size_mb = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
        eprintln!("local model {name} ({size_mb:.0} MB)");
        debug!(?path, elapsed = ?started.elapsed(), "loaded local model");

        Ok(Self {
            backend,
            model,
            n_ctx: context_window,
            threads: hardware::default_threads() as i32,
            lock: Mutex::new(()),
        })
    }

    /// Render `messages` with the model's chat template and generate a
    /// completion, constrained by `grammar` (GBNF) when supplied.
    pub fn complete(
        &self,
        messages: &[Message],
        grammar: Option<&str>,
        max_tokens: u32,
        temperature: f32,
    ) -> Result<String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let prompt = self.render_prompt(messages)?;

        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("failed to tokenize prompt")?;
        // Landmine 2: prefill must fit in one batch and leave room for output.
        let budget = self.n_ctx.saturating_sub(max_tokens) as usize;
        if tokens.len() > budget {
            bail!(
                "prompt is {} tokens but only {budget} fit in the {}-token context alongside {max_tokens} output tokens",
                tokens.len(),
                self.n_ctx
            );
        }

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(self.n_ctx))
            .with_n_batch(self.n_ctx)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads);
        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .context("failed to create llama context")?;

        let mut batch = LlamaBatch::new(self.n_ctx as usize, 1);
        let last = tokens.len() as i32 - 1;
        for (i, token) in (0i32..).zip(tokens.iter()) {
            batch.add(*token, i, &[0], i == last)?;
        }
        let prefill_started = Instant::now();
        ctx.decode(&mut batch).context("prefill failed")?;
        let prefill = prefill_started.elapsed();

        let mut samplers = Vec::new();
        if let Some(grammar) = grammar {
            samplers.push(
                LlamaSampler::grammar(&self.model, grammar, "root")
                    .context("failed to compile GBNF grammar")?,
            );
        }
        samplers.push(LlamaSampler::temp(temperature));
        samplers.push(LlamaSampler::dist(seed()));
        let mut sampler = LlamaSampler::chain_simple(samplers);

        let mut out = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut n_cur = batch.n_tokens();
        let mut generated = 0u32;
        let decode_started = Instant::now();
        let mut finished = false;
        loop {
            // Landmine 1: sample() accepts the token; do NOT call accept().
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                finished = true;
                break;
            }
            if generated >= max_tokens {
                break;
            }
            out.push_str(&self.model.token_to_piece(token, &mut decoder, true, None)?);
            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            ctx.decode(&mut batch).context("decode failed")?;
            n_cur += 1;
            generated += 1;
        }
        let decode = decode_started.elapsed();

        debug!(
            prompt_tokens = tokens.len(),
            generated,
            prefill_ms = prefill.as_millis(),
            tok_per_s = generated as f64 / decode.as_secs_f64().max(f64::EPSILON),
            "local generation complete"
        );

        // Landmine 3: grammars guarantee prefix validity only.
        if !finished {
            bail!("local generation hit the {max_tokens}-token cap before finishing — output is truncated and unparseable");
        }
        Ok(out)
    }

    /// Apply the GGUF's embedded chat template, falling back to ChatML (which
    /// is what the SmolLM2 catalyst checkpoints were trained with).
    fn render_prompt(&self, messages: &[Message]) -> Result<String> {
        let flat: Vec<(String, String)> = messages
            .iter()
            .map(|m| (m.role.clone(), flatten(&m.content)))
            .collect();

        if let Ok(template) = self.model.chat_template(None) {
            let chat: Result<Vec<LlamaChatMessage>, _> = flat
                .iter()
                .map(|(role, content)| LlamaChatMessage::new(role.clone(), content.clone()))
                .collect();
            if let Ok(chat) = chat {
                if let Ok(rendered) = self.model.apply_chat_template(&template, &chat, true) {
                    return Ok(rendered);
                }
            }
        }

        let mut prompt = String::new();
        for (role, content) in &flat {
            prompt.push_str(&format!("<|im_start|>{role}\n{content}<|im_end|>\n"));
        }
        prompt.push_str("<|im_start|>assistant\n");
        Ok(prompt)
    }
}

/// Flatten a message body (possibly cache-split into parts) into plain text.
fn flatten(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Sampling seed; fixed via `ENZYME_LOCAL_SEED` for reproducible runs.
fn seed() -> u32 {
    std::env::var("ENZYME_LOCAL_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        })
}
