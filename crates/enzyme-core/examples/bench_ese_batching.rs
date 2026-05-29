use std::hint::black_box;
use std::time::{Duration, Instant};

use anyhow::Result;
use enzyme_core::embedding::{EmbeddingBackend, EmbeddingService, EseBackend};

fn make_corpus(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            format!(
                "Document chunk {i}: enzyme embedding hot path benchmark text with repeated natural-language tokens, links, dates 2026-05-27, and markdown-ish structure. Section {} discusses catalysts, SQLite writes, and ESE batching.",
                i % 97
            )
        })
        .collect()
}

fn checksum_vecs(vecs: &[Vec<f32>]) -> f32 {
    vecs.iter()
        .map(|v| v.first().copied().unwrap_or_default())
        .sum()
}

fn checksum_arrays(vecs: &[[f32; ese::DIMENSIONS]]) -> f32 {
    vecs.iter().map(|v| v[0]).sum()
}

fn time_arrays<F>(iters: usize, mut f: F) -> (Duration, f32)
where
    F: FnMut() -> Vec<[f32; ese::DIMENSIONS]>,
{
    let mut elapsed = Duration::ZERO;
    let mut checksum = 0.0f32;
    for _ in 0..iters {
        let start = Instant::now();
        let result = f();
        elapsed += start.elapsed();
        checksum += checksum_arrays(black_box(&result));
    }
    (elapsed, checksum)
}

fn time_vecs<F>(iters: usize, mut f: F) -> (Duration, f32)
where
    F: FnMut() -> Vec<Vec<f32>>,
{
    let mut elapsed = Duration::ZERO;
    let mut checksum = 0.0f32;
    for _ in 0..iters {
        let start = Instant::now();
        let result = f();
        elapsed += start.elapsed();
        checksum += checksum_vecs(black_box(&result));
    }
    (elapsed, checksum)
}

fn print_result(label: &str, elapsed: Duration, chunks: usize, checksum: f32) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    let rate = chunks as f64 / elapsed.as_secs_f64();
    println!("{label:32} {ms:9.3} ms  {rate:10.0} chunks/s  checksum={checksum:.6}");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let count = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(17_572);
    let iters = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let batch_size = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(128);

    let corpus = make_corpus(count);
    let refs: Vec<&str> = corpus.iter().map(String::as_str).collect();
    let total_chunks = count * iters;

    println!("ESE batching microbenchmark: count={count}, iters={iters}, batch_size={batch_size}");

    // Warm up Rayon and instruction/data caches before measuring.
    black_box(ese::encode(refs.iter().copied()));

    let (elapsed, checksum) = time_arrays(iters, || ese::encode(refs.iter().copied()));
    print_result("ese::encode(all) raw", elapsed, total_chunks, checksum);

    let (elapsed, checksum) = time_arrays(iters, || {
        let mut out = Vec::with_capacity(refs.len());
        for batch in refs.chunks(batch_size) {
            out.extend(ese::encode(batch.iter().copied()));
        }
        out
    });
    print_result("ese::encode(chunked) raw", elapsed, total_chunks, checksum);

    let (elapsed, checksum) = time_vecs(iters, || {
        ese::encode(refs.iter().copied())
            .into_iter()
            .map(|arr| arr.to_vec())
            .collect()
    });
    print_result("ese all + Vec conversion", elapsed, total_chunks, checksum);

    let (elapsed, checksum) = time_vecs(iters, || {
        let mut out = Vec::with_capacity(refs.len());
        for batch in refs.chunks(batch_size) {
            out.extend(
                ese::encode(batch.iter().copied())
                    .into_iter()
                    .map(|arr| arr.to_vec()),
            );
        }
        out
    });
    print_result(
        "ese chunked + Vec conversion",
        elapsed,
        total_chunks,
        checksum,
    );

    let service = EmbeddingService::new(Box::new(EseBackend::new()));
    let (elapsed, checksum) = time_vecs(iters, || service.embed_documents(&refs).unwrap());
    print_result(
        "EmbeddingService::new(ESE)",
        elapsed,
        total_chunks,
        checksum,
    );

    let backend = EseBackend::new();
    let (elapsed, checksum) = time_vecs(iters, || {
        let mut out = Vec::with_capacity(refs.len());
        for batch in refs.chunks(batch_size) {
            out.extend(backend.embed_batch(batch).unwrap());
        }
        out
    });
    print_result("manual chunked backend", elapsed, total_chunks, checksum);

    Ok(())
}
