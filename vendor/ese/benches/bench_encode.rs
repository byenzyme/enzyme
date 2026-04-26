use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use std::hint::black_box;

use ese::{encode, encode_single};

use model2vec_rs::model::StaticModel;
use std::sync::LazyLock;

static MODEL2VEC: LazyLock<StaticModel> = LazyLock::new(|| {
    StaticModel::from_pretrained("minishlab/potion-base-8M", None, None, None).unwrap()
});

fn sentences(n: usize) -> Vec<String> {
    let corpus = [
        "The quick brown fox jumps over the lazy dog",
        "Rust is a systems programming language focused on safety and performance",
        "Static embeddings offer a compelling trade-off between speed and quality",
        "Machine learning models are getting smaller and faster every year",
        "Tokenization is the first step in most NLP pipelines",
        "Semantic search lets you find documents by meaning rather than keywords",
        "Vector databases have become a critical piece of modern AI infrastructure",
        "Quantization reduces model size with minimal impact on accuracy",
    ];
    corpus
        .iter()
        .cycle()
        .take(n)
        .map(|s| s.to_string())
        .collect()
}

fn bench_single(c: &mut Criterion) {
    let input = "The quick brown fox jumps over the lazy dog";

    let mut group = c.benchmark_group("encode_single");

    group.bench_function("custom", |b| {
        b.iter(|| encode_single(black_box(input)));
    });

    group.bench_function("model2vec-rs", |b| {
        let model = &*MODEL2VEC;
        b.iter(|| model.encode(black_box(&[input.into()])));
    });

    group.finish();
}

fn bench_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_batch");

    for n in [1, 8, 32, 128, 512] {
        let data = sentences(n);

        group.bench_with_input(BenchmarkId::new("custom", n), &data, |b, data| {
            b.iter(|| encode(black_box(data.clone())));
        });

        group.bench_with_input(BenchmarkId::new("model2vec-rs", n), &data, |b, data| {
            let model = &*MODEL2VEC;
            b.iter(|| model.encode(black_box(data)));
        });
    }

    group.finish();
}

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.sample_size(50);

    for n in [100, 1000] {
        let data = sentences(n);

        group.throughput(criterion::Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("custom", n), &data, |b, data| {
            b.iter(|| encode(black_box(data.clone())));
        });

        group.bench_with_input(BenchmarkId::new("model2vec-rs", n), &data, |b, data| {
            let model = &*MODEL2VEC;
            b.iter(|| model.encode(black_box(data)));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_single, bench_batch, bench_throughput);
criterion_main!(benches);
