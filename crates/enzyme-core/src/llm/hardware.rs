//! Hardware-adaptive defaults for local (on-device) catalyst generation.
//!
//! Constants come from the 2026-08 local-inference benchmark
//! (`docs/2026-08-08-local-llm-bundling-recommendation.md`, bench results in
//! `benchmarks/`): on a 4-weak-core box the throughput knee is at concurrency 1
//! because generation is prefill-bound at roughly 25:1 prefill:decode, so extra
//! request slots only steal threads from the prefill that dominates wall time.
//! Apple Silicon has enough memory bandwidth and core headroom (plus Metal) to
//! absorb 4 concurrent requests, and an 8+ physical-core desktop absorbs 2.

/// Char cap for the per-entity prompt context on local models.
///
/// The shipping checkpoint has an 8,192-token window; at ~3.5 chars/token that
/// is ~28k chars, and we keep prompt + 512 output tokens comfortably inside it
/// with a large safety margin for tokenizer variance.
pub const LOCAL_MAX_CONTEXT_CHARS: usize = 12_000;

/// Physical core count, falling back to logical parallelism.
pub fn physical_cores() -> usize {
    #[cfg(target_os = "linux")]
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut cores: Vec<(String, String)> = Vec::new();
        let (mut physical_id, mut core_id) = (String::new(), String::new());
        for line in cpuinfo.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim() {
                    "physical id" => physical_id = value.trim().to_string(),
                    "core id" => core_id = value.trim().to_string(),
                    _ => {}
                }
            } else if !core_id.is_empty() {
                cores.push((
                    std::mem::take(&mut physical_id),
                    std::mem::take(&mut core_id),
                ));
            }
        }
        if !core_id.is_empty() {
            cores.push((physical_id, core_id));
        }
        cores.sort();
        cores.dedup();
        if !cores.is_empty() {
            return cores.len();
        }
    }

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Threads to hand llama.cpp: one per physical core.
pub fn default_threads() -> usize {
    physical_cores().max(1)
}

/// Default number of in-flight LLM requests for a local model.
///
/// `ENZYME_LLM_CONCURRENCY` overrides this at the call site (the CLI already
/// reads it into `CatalystConfig::concurrency`).
///
/// The in-process engine keeps a pool of one llama context per in-flight
/// request (see `llm::local_engine`), so this also caps that pool.
pub fn default_local_concurrency() -> usize {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        4
    } else if physical_cores() >= 8 {
        2
    } else {
        1
    }
}

/// Resolved local concurrency: `ENZYME_LLM_CONCURRENCY` if set and sane,
/// otherwise [`default_local_concurrency`].
///
/// Both the catalyst pipeline's worker count and the in-process engine's
/// context-pool cap read this, so one knob moves both together.
pub fn local_concurrency() -> usize {
    std::env::var("ENZYME_LLM_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(default_local_concurrency)
}

/// Decode threads to give each context when `pool_size` contexts may run at
/// once: split the physical cores between them, never below 2.
///
/// Splitting is not a nicety: ggml's CPU threads spin-wait, so oversubscribing
/// them collapses throughput. Measured on a 4-core box, four concurrent
/// generations at pool size 2 (135M Q4_K_M):
///
/// | threads/ctx | wall time    |
/// |-------------|--------------|
/// | 2 (split)   | 11.0s, 12.3s |
/// | 4 (all)     | 218s, 262s   |
pub fn threads_per_context(pool_size: usize) -> usize {
    (physical_cores() / pool_size.max(1)).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_cores_is_at_least_one() {
        assert!(physical_cores() >= 1);
        assert!(default_threads() >= 1);
    }

    #[test]
    fn threads_per_context_splits_the_cpu_without_starving() {
        let cores = physical_cores();
        assert_eq!(threads_per_context(1), cores.max(2));
        for pool in 1..=8 {
            let t = threads_per_context(pool);
            assert!(t >= 2, "pool {pool} got {t} threads");
            assert!(t <= cores.max(2), "pool {pool} oversubscribes: {t}");
        }
    }

    #[test]
    fn concurrency_is_bounded() {
        let c = default_local_concurrency();
        assert!((1..=4).contains(&c), "unexpected concurrency {c}");
    }
}
