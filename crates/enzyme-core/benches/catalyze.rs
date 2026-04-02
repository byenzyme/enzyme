/// Benchmark: catalyze hot path performance via the production Database layer.
///
/// Uses `Database::open` (libsql local) with the packed catalyst embeddings
/// format.  Run with:
///   cargo bench -p enzyme-core --bench catalyze
///
/// This replaces the earlier two-binary comparison (catalyze_rusqlite /
/// catalyze_libsql).  Since rusqlite has been dropped, a single bench against
/// the production layer is sufficient for ongoing regression tracking.
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use enzyme_core::db::Database;
use tempfile::TempDir;

const N_CATALYSTS: usize = 500;
const EMBEDDING_DIM: usize = 384;
const N_SIMILARITIES: usize = 50;

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
    let embeddings: Vec<(&str, Vec<f32>)> = (0..N_CATALYSTS)
        .map(|i| ("", make_embedding(i)))
        .collect();
    // We can't easily pass &str refs here, so build the owned version
    let ids: Vec<String> = (0..N_CATALYSTS).map(|i| format!("cat_{i}")).collect();
    let id_refs: Vec<(&str, &[f32])> = ids
        .iter()
        .zip(embeddings.iter().map(|(_, v)| v.as_slice()))
        .map(|(id, v)| (id.as_str(), v))
        .collect();
    db.upsert_catalyst_embeddings(&id_refs).unwrap();

    // Similarities for cat_0
    for j in 0..N_SIMILARITIES {
        db.execute_write(
            "INSERT INTO catalyst_similarities
             (catalyst_id, catalyst_text, file_path, chunk_index, similarity,
              chunk_metadata, timeline_data)
             VALUES ('cat_0', 'text', ?1, ?2, ?3, '{}', '{}')",
            vec![
                Value::Text(format!("notes/file_{j}.md")),
                Value::Integer(j as i64),
                Value::Real(1.0_f64 - j as f64 * 0.01),
            ],
        )
        .unwrap();
    }
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

fn bench_similarities_for_catalyst(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group =
        c.benchmark_group(format!("get_similarities_for_catalyst/{N_SIMILARITIES}_rows"));
    group.bench_function("libsql", |b| {
        b.iter(|| {
            black_box(db.get_similarities_for_catalyst("cat_0", N_SIMILARITIES).unwrap())
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_catalyst_embeddings,
    bench_all_catalysts,
    bench_similarities_for_catalyst,
);
criterion_main!(benches);
