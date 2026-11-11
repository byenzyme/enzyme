/// Benchmark: catalyze hot path performance via the production Database layer.
///
/// Uses `Database::open` (libsql local) with the packed catalyst embeddings
/// format.  Run with:
///   cargo bench -p enzyme-core --bench catalyze
///
/// This replaces the earlier two-binary comparison (catalyze_rusqlite /
/// catalyze_libsql).  Since rusqlite has been dropped, a single bench against
/// the production layer is sufficient for ongoing regression tracking.
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use enzyme_core::db::Database;
use enzyme_core::models::EMBEDDING_DIMENSION as EMBEDDING_DIM;
use enzyme_core::search::catalyst_links::{CATALYST_TOP_K, ChunkVectors};
use tempfile::TempDir;

const N_CATALYSTS: usize = 500;
/// Chunks scored per catalyst when routing; a mid-sized vault.
const N_CHUNKS: usize = 15_000;
/// Catalysts catalyst routing scores per query.
const ROUTED_CATALYSTS: usize = 10;

fn make_embedding(seed: usize) -> Vec<f32> {
    (0..EMBEDDING_DIM)
        .map(|i| ((seed * EMBEDDING_DIM + i) as f32 * 0.001).sin())
        .collect()
}

fn seed_db(db: &Database) {
    use libsql::Value;

    // Catalysts
    for i in 0..N_CATALYSTS {
        db.execute_write(
            "INSERT INTO catalysts (id, text, entity, context, generated_at, metadata)
             VALUES (?1, ?2, ?3, 'default', datetime('now'), '{}')",
            vec![
                Value::Text(format!("cat_{i}")),
                Value::Text(format!("catalyst text {i} about something interesting")),
                Value::Text(format!("entity_{}", i % 20)),
            ],
        )
        .unwrap();
    }

    // Catalyst embeddings (packed)
    let embeddings: Vec<(&str, Vec<f32>)> =
        (0..N_CATALYSTS).map(|i| ("", make_embedding(i))).collect();
    // We can't easily pass &str refs here, so build the owned version
    let ids: Vec<String> = (0..N_CATALYSTS).map(|i| format!("cat_{i}")).collect();
    let id_refs: Vec<(&str, &[f32])> = ids
        .iter()
        .zip(embeddings.iter().map(|(_, v)| v.as_slice()))
        .map(|(id, v)| (id.as_str(), v))
        .collect();
    db.upsert_catalyst_embeddings(&id_refs).unwrap();

    // Chunk vectors for on-demand catalyst links
    let chunks: Vec<(String, u32, Vec<f32>)> = (0..N_CHUNKS)
        .map(|j| {
            (
                format!("notes/file_{}.md", j / 3),
                (j % 3) as u32,
                make_embedding(N_CATALYSTS + j),
            )
        })
        .collect();
    db.replace_chunk_embeddings_for_documents(vec![], chunks, vec![])
        .unwrap();
}

fn bench_catalyst_embeddings(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group = c.benchmark_group(format!("get_all_catalyst_embeddings/{N_CATALYSTS}"));
    group.bench_function("libsql/packed", |b| {
        b.iter(|| black_box(db.get_all_catalyst_embeddings().unwrap()))
    });
    group.finish();
}

fn bench_all_catalysts(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group = c.benchmark_group(format!("get_all_catalysts/{N_CATALYSTS}"));
    group.bench_function("libsql", |b| {
        b.iter(|| black_box(db.get_all_catalysts().unwrap()))
    });
    group.finish();
}

fn bench_top_chunks(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);
    let catalysts: Vec<(String, Vec<f32>)> = (0..ROUTED_CATALYSTS)
        .map(|i| (format!("cat_{i}"), make_embedding(i)))
        .collect();
    let refs: Vec<(&str, &[f32])> = catalysts
        .iter()
        .map(|(id, v)| (id.as_str(), v.as_slice()))
        .collect();

    let mut group = c.benchmark_group(format!(
        "top_chunks/{ROUTED_CATALYSTS}_catalysts_x_{N_CHUNKS}_chunks"
    ));
    group.bench_function("load_and_score", |b| {
        b.iter(|| {
            let vectors = ChunkVectors::load_all(&db).unwrap();
            black_box(vectors.top_chunks(&refs, CATALYST_TOP_K))
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_catalyst_embeddings,
    bench_all_catalysts,
    bench_top_chunks,
);
criterion_main!(benches);
