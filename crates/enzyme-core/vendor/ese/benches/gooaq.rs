use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::fs;
use std::path::PathBuf;

const PARQUET_URLS: &[&str] = &[
    "https://huggingface.co/api/datasets/sentence-transformers/gooaq/parquet/pair/train/0.parquet",
    "https://huggingface.co/api/datasets/sentence-transformers/gooaq/parquet/pair/train/1.parquet",
];
const CACHE_DIR: &str = "target/gooaq";

fn download_parquet(url: &str, path: &PathBuf) {
    let resp = minreq::get(url)
        .send()
        .unwrap_or_else(|e| panic!("failed to download {url}: {e}"));
    let bytes = resp.as_bytes();
    fs::write(path, bytes).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

fn extract_questions(path: &PathBuf) -> Vec<String> {
    let file = fs::File::open(path).expect("failed to open parquet file");
    let reader = SerializedFileReader::new(file).expect("failed to create parquet reader");
    let mut sentences = Vec::new();
    let iter = reader
        .get_row_iter(None)
        .expect("failed to get row iterator");
    for row in iter {
        let row = row.expect("failed to read row");
        for (name, field) in row.get_column_iter() {
            if name == "question" || name == "sentence1" {
                if let Field::Str(s) = field {
                    sentences.push(s.clone());
                }
            }
        }
    }
    sentences
}

fn load_sentences() -> Vec<String> {
    let cache_dir = PathBuf::from(CACHE_DIR);
    fs::create_dir_all(&cache_dir).ok();

    let mut all_sentences = Vec::new();
    for (i, url) in PARQUET_URLS.iter().enumerate() {
        let cached = cache_dir.join(format!("gooaq_{i}.parquet"));
        if !cached.exists() {
            eprintln!("Downloading {url} ...");
            download_parquet(url, &cached);
        }
        all_sentences.extend(extract_questions(&cached));
    }
    eprintln!(
        "Loaded {} sentences from gooaq dataset",
        all_sentences.len()
    );
    all_sentences
}

fn bench_encode_single(c: &mut Criterion) {
    let sentences = load_sentences();
    assert!(!sentences.is_empty(), "dataset is empty");

    let mut group = c.benchmark_group("encode_single");
    group.throughput(Throughput::Elements(1));

    group.bench_function("short", |b| {
        let s = &sentences[0];
        b.iter(|| ese::encode_single(std::hint::black_box(s)));
    });

    let longest = sentences.iter().max_by_key(|s| s.len()).unwrap();
    group.bench_with_input(
        BenchmarkId::new("longest", format!("{}chars", longest.len())),
        longest,
        |b, s| {
            b.iter(|| ese::encode_single(std::hint::black_box(s)));
        },
    );

    group.finish();
}

fn bench_encode_batch(c: &mut Criterion) {
    let sentences = load_sentences();
    let n = sentences.len();

    let mut group = c.benchmark_group("encode_batch");

    for size in [
        1, 16, 64, 256, 1000, 10_000, 65_536, 100_000,
        // 256_000, 512_000, 768000, 1_000_000,
    ] {
        if size > n {
            continue;
        }
        let batch: Vec<_> = sentences[..size].to_vec();
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &batch, |b, batch| {
            b.iter(|| ese::encode(std::hint::black_box(batch.clone())));
        });
    }

    group.finish();
}

fn bench_encode_by_length(c: &mut Criterion) {
    let sentences = load_sentences();
    let mut group = c.benchmark_group("encode_by_length");

    // Build inputs of increasing character length
    for &char_len in &[10, 50, 100, 250, 500, 1000, 2000, 5000] {
        // Find a sentence at least this long, or skip
        let Some(s) = sentences.iter().find(|s| s.len() >= char_len) else {
            continue;
        };
        let input: String = s.chars().take(char_len).collect();
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(BenchmarkId::new("chars", char_len), &input, |b, input| {
            b.iter(|| ese::encode_single(std::hint::black_box(input)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_encode_single,
    bench_encode_batch,
    bench_encode_by_length
);

criterion_main!(benches);
