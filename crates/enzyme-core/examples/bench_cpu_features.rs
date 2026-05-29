use std::hint::black_box;
use std::time::Instant;

fn main() {
    print_cpu_info();

    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4096);
    let reps: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);

    let texts: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "doc {i}: semantic memory search ritual craft attention context catalyst \
                 embedding throughput cpu feature benchmark vector arithmetic repeated phrase {}",
                i % 97
            )
        })
        .collect();

    // Warm up allocator/threadpool/cache.
    let warm: Vec<&str> = texts.iter().take(64).map(String::as_str).collect();
    black_box(ese::encode(warm));

    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let start = Instant::now();
    let embeddings = black_box(ese::encode(refs));
    let encode_elapsed = start.elapsed();

    let query = black_box(ese::encode_single(
        "semantic search catalyst memory benchmark",
    ));
    let start = Instant::now();
    let mut checksum = 0.0f32;
    for _ in 0..reps {
        for emb in &embeddings {
            checksum = black_box(checksum + cosine(&query, emb));
        }
    }
    let cosine_elapsed = start.elapsed();

    let encode_ms = encode_elapsed.as_secs_f64() * 1000.0;
    let cosine_ms = cosine_elapsed.as_secs_f64() * 1000.0;
    let sims = n * reps;

    println!("dims: {}", ese::DIMENSIONS);
    println!("docs: {n}");
    println!("cosine_reps: {reps}");
    println!("encode_ms: {encode_ms:.3}");
    println!(
        "encode_docs_per_s: {:.1}",
        n as f64 / encode_elapsed.as_secs_f64()
    );
    println!("cosine_ms: {cosine_ms:.3}");
    println!(
        "cosine_sims_per_s: {:.1}",
        sims as f64 / cosine_elapsed.as_secs_f64()
    );
    println!("checksum: {checksum:.6}");
}

#[inline(never)]
fn cosine(a: &[f32; ese::DIMENSIONS], b: &[f32; ese::DIMENSIONS]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn print_cpu_info() {
    println!("target_arch: {}", std::env::consts::ARCH);

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        println!("runtime_avx: {}", std::is_x86_feature_detected!("avx"));
        println!("runtime_avx2: {}", std::is_x86_feature_detected!("avx2"));
        println!("runtime_fma: {}", std::is_x86_feature_detected!("fma"));
        println!(
            "runtime_avx512f: {}",
            std::is_x86_feature_detected!("avx512f")
        );
    }

    #[cfg(target_arch = "aarch64")]
    {
        println!("runtime_neon_asimd: true");
        println!("note: AVX/AVX2/FMA are x86 names; Apple Silicon uses ARM ASIMD/NEON/FMLA/FMADD.");
    }
}
