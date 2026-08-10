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
//! # Context pool
//!
//! One `LlamaModel` is loaded once (mmap, shared read-only) and up to
//! [`hardware::local_concurrency`] contexts generate from it in parallel. Each
//! context owns its own KV cache and compute buffers — measured at ~224 MB of
//! RSS per extra context for the 135M checkpoint at its 8k window — and gets a
//! fresh grammar sampler per request, so concurrent calls cannot corrupt each
//! other's automaton state.
//!
//! `LlamaContext` holds a raw `NonNull` and is therefore `!Send`: a context can
//! never be moved between threads. The pool is consequently built from
//! *dedicated OS threads that own their context for life* and take work over a
//! channel — the standard shape for llama.cpp bindings — rather than a
//! `Vec<Mutex<Context>>`. Threads are spawned lazily: the first request spawns
//! one, a request that arrives while every existing worker is busy spawns
//! another, up to the cap. Nothing here uses `unsafe`.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use tracing::debug;

use super::client::{Message, MessageContent};
use super::hardware;

/// Model + backend, shared read-only by every pool worker.
struct Shared {
    backend: LlamaBackend,
    model: LlamaModel,
    n_ctx: u32,
    threads: i32,
}

/// One generation request handed to a pool worker.
struct Job {
    tokens: Vec<LlamaToken>,
    grammar: Option<String>,
    max_tokens: u32,
    temperature: f32,
    reply: Sender<Result<String>>,
}

/// Lazy-growth bookkeeping for the worker pool.
#[derive(Default)]
struct PoolState {
    /// Workers spawned so far (never shrinks; capped at `cap`).
    spawned: usize,
    /// Jobs submitted but not yet answered.
    outstanding: usize,
}

/// A loaded GGUF ready for grammar-constrained completion.
pub struct LocalEngine {
    shared: Arc<Shared>,
    /// Maximum number of concurrent contexts.
    cap: usize,
    jobs: Sender<Job>,
    /// Shared work queue. A worker holds this lock only while parked in
    /// `recv()`; the next worker takes it as soon as a job is handed off.
    queue: Arc<Mutex<Receiver<Job>>>,
    state: Arc<Mutex<PoolState>>,
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
        let size_mb =
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
        eprintln!("local model {name} ({size_mb:.0} MB)");
        debug!(?path, elapsed = ?started.elapsed(), "loaded local model");

        let cap = hardware::local_concurrency().max(1);
        // Thread budget is fixed at load time from the pool cap so that a full
        // pool never oversubscribes the CPU.
        let threads = threads_per_context(cap) as i32;
        debug!(cap, threads, "local context pool configured");

        let (jobs, queue) = channel();
        Ok(Self {
            shared: Arc::new(Shared {
                backend,
                model,
                n_ctx: context_window,
                threads,
            }),
            cap,
            jobs,
            queue: Arc::new(Mutex::new(queue)),
            state: Arc::new(Mutex::new(PoolState::default())),
        })
    }

    /// Render `messages` with the model's chat template and generate a
    /// completion, constrained by `grammar` (GBNF) when supplied.
    ///
    /// Blocking. Callers on an async runtime must wrap it in `spawn_blocking`
    /// (see `client::local_chat_completion`).
    pub fn complete(
        &self,
        messages: &[Message],
        grammar: Option<&str>,
        max_tokens: u32,
        temperature: f32,
    ) -> Result<String> {
        // Prompt rendering and tokenization only read the model, which
        // llama-cpp-2 declares `Sync`, so they run on the caller's thread and
        // fail fast without occupying a context.
        let prompt = self.render_prompt(messages)?;
        let tokens = self
            .shared
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("failed to tokenize prompt")?;
        // Landmine 2: prefill must fit in one batch and leave room for output.
        let budget = self.shared.n_ctx.saturating_sub(max_tokens) as usize;
        if tokens.len() > budget {
            bail!(
                "prompt is {} tokens but only {budget} fit in the {}-token context alongside {max_tokens} output tokens",
                tokens.len(),
                self.shared.n_ctx
            );
        }

        let (reply, answer) = channel();
        let job = Job {
            tokens,
            grammar: grammar.map(str::to_owned),
            max_tokens,
            temperature,
            reply,
        };

        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.outstanding += 1;
            if state.outstanding > state.spawned && state.spawned < self.cap {
                state.spawned += 1;
                debug!(
                    worker = state.spawned,
                    cap = self.cap,
                    threads = self.shared.threads,
                    "spawning local context-pool worker"
                );
                let shared = Arc::clone(&self.shared);
                let queue = Arc::clone(&self.queue);
                let pool_state = Arc::clone(&self.state);
                let spawned = std::thread::Builder::new()
                    .name(format!("enzyme-llm-{}", state.spawned))
                    .spawn(move || worker(&shared, &queue, &pool_state));
                if let Err(e) = spawned {
                    state.spawned -= 1;
                    // Only fatal if this would have been the first worker;
                    // otherwise an already-running one picks the job up.
                    if state.spawned == 0 {
                        state.outstanding -= 1;
                        return Err(e).context("failed to spawn local generation worker");
                    }
                }
            }
        }

        self.jobs
            .send(job)
            .map_err(|_| anyhow!("local generation pool is gone"))?;
        answer
            .recv()
            .map_err(|_| anyhow!("local generation worker died without answering"))?
    }

    /// Apply the GGUF's embedded chat template, falling back to ChatML (which
    /// is what the SmolLM2 catalyst checkpoints were trained with).
    fn render_prompt(&self, messages: &[Message]) -> Result<String> {
        let flat: Vec<(String, String)> = messages
            .iter()
            .map(|m| (m.role.clone(), flatten(&m.content)))
            .collect();

        if let Ok(template) = self.shared.model.chat_template(None) {
            let chat: Result<Vec<LlamaChatMessage>, _> = flat
                .iter()
                .map(|(role, content)| LlamaChatMessage::new(role.clone(), content.clone()))
                .collect();
            if let Ok(chat) = chat {
                if let Ok(rendered) = self
                    .shared
                    .model
                    .apply_chat_template(&template, &chat, true)
                {
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

/// Pool worker: owns one `LlamaContext` for its whole life and serves jobs off
/// the shared queue. The context is created on the first job (so an idle pool
/// costs no KV cache) and reused afterwards, its KV cache cleared per job.
fn worker(shared: &Arc<Shared>, queue: &Mutex<Receiver<Job>>, state: &Mutex<PoolState>) {
    let mut ctx: Option<LlamaContext<'_>> = None;
    loop {
        let job = {
            let guard = queue.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv()
        };
        let Ok(job) = job else { break }; // engine dropped
        let answer = match context(shared, &mut ctx) {
            Ok(ctx) => generate(shared, ctx, &job),
            Err(e) => Err(e),
        };
        let _ = job.reply.send(answer);
        state.lock().unwrap_or_else(|e| e.into_inner()).outstanding -= 1;
    }
}

/// Borrow this worker's context, creating it on first use. A creation failure
/// (realistically: out of memory for the KV cache) fails just this job — the
/// worker stays alive and retries on the next one.
fn context<'a, 'm>(
    shared: &'m Arc<Shared>,
    slot: &'a mut Option<LlamaContext<'m>>,
) -> Result<&'a mut LlamaContext<'m>> {
    if slot.is_none() {
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(shared.n_ctx))
            .with_n_batch(shared.n_ctx)
            .with_n_threads(shared.threads)
            .with_n_threads_batch(shared.threads);
        let started = Instant::now();
        let created = shared
            .model
            .new_context(&shared.backend, params)
            .context("failed to create llama context")?;
        debug!(
            n_ctx = shared.n_ctx,
            threads = shared.threads,
            elapsed = ?started.elapsed(),
            "created local context"
        );
        *slot = Some(created);
    }
    Ok(slot.as_mut().expect("just created"))
}

/// Prefill + grammar-constrained decode of one job on one context.
fn generate(shared: &Shared, ctx: &mut LlamaContext<'_>, job: &Job) -> Result<String> {
    // Contexts are reused across jobs, so start from an empty KV cache.
    ctx.clear_kv_cache();

    let mut batch = LlamaBatch::new(shared.n_ctx as usize, 1);
    let last = job.tokens.len() as i32 - 1;
    for (i, token) in (0i32..).zip(job.tokens.iter()) {
        batch.add(*token, i, &[0], i == last)?;
    }
    let prefill_started = Instant::now();
    ctx.decode(&mut batch).context("prefill failed")?;
    let prefill = prefill_started.elapsed();

    let mut samplers = Vec::new();
    if let Some(grammar) = &job.grammar {
        // A fresh grammar sampler per job: automaton state is per-request and
        // never shared between contexts.
        samplers.push(
            LlamaSampler::grammar(&shared.model, grammar, "root")
                .context("failed to compile GBNF grammar")?,
        );
    }
    samplers.push(LlamaSampler::temp(job.temperature));
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
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        if shared.model.is_eog_token(token) {
            finished = true;
            break;
        }
        if generated >= job.max_tokens {
            break;
        }
        out.push_str(
            &shared
                .model
                .token_to_piece(token, &mut decoder, true, None)?,
        );
        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        ctx.decode(&mut batch).context("decode failed")?;
        n_cur += 1;
        generated += 1;
    }
    let decode = decode_started.elapsed();

    debug!(
        prompt_tokens = job.tokens.len(),
        generated,
        prefill_ms = prefill.as_millis(),
        tok_per_s = generated as f64 / decode.as_secs_f64().max(f64::EPSILON),
        "local generation complete"
    );

    // Landmine 3: grammars guarantee prefix validity only.
    if !finished {
        bail!(
            "local generation hit the {}-token cap before finishing — output is truncated and unparseable",
            job.max_tokens
        );
    }
    Ok(out)
}

/// Decode threads per context, overridable with `ENZYME_LOCAL_THREADS` (used to
/// benchmark the split-vs-oversubscribe choice).
fn threads_per_context(pool_size: usize) -> usize {
    std::env::var("ENZYME_LOCAL_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| hardware::threads_per_context(pool_size))
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
