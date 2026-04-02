/// Benchmark: rusqlite read performance on the catalyze hot path.
///
/// Companion to `catalyze_libsql` — run both and compare numbers for the
/// go/no-go decision on the libsql migration. See docs/2026-03-31-libsql-migration-path.md.
///
/// Run with:
///   cargo bench -p enzyme-core --bench catalyze_rusqlite
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use enzyme_core::db::Database;
use tempfile::TempDir;
use serde_json;

const N_CATALYSTS: usize = 500;
const EMBEDDING_DIM: usize = 384;
const N_SIMILARITIES: usize = 50;

fn f32_vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for &x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

fn blob_to_f32_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn make_embedding(seed: usize) -> Vec<f32> {
    (0..EMBEDDING_DIM)
        .map(|i| ((seed * EMBEDDING_DIM + i) as f32 * 0.001).sin())
        .collect()
}

fn seed_packed(db: &Database) {
    let conn = db.write_conn();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalyst_embeddings_packed (
            id      INTEGER PRIMARY KEY DEFAULT 1,
            count   INTEGER NOT NULL,
            ids     TEXT    NOT NULL,
            vectors BLOB    NOT NULL
         )",
    )
    .unwrap();
    let ids: Vec<String> = (0..N_CATALYSTS).map(|i| format!("cat_{}", i)).collect();
    let ids_json = serde_json::to_string(&ids).unwrap();
    let mut all_vecs: Vec<u8> = Vec::with_capacity(N_CATALYSTS * EMBEDDING_DIM * 4);
    for i in 0..N_CATALYSTS {
        all_vecs.extend_from_slice(&f32_vec_to_blob(&make_embedding(i)));
    }
    conn.execute(
        "INSERT INTO catalyst_embeddings_packed (count, ids, vectors) VALUES (?1, ?2, ?3)",
        rusqlite::params![N_CATALYSTS as i64, ids_json, all_vecs],
    )
    .unwrap();
}

fn seed_db(db: &Database) {
    let conn = db.write_conn();

    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for i in 0..N_CATALYSTS {
        conn.execute(
            "INSERT INTO catalysts (id, text, entity, context, generated_at, metadata)
             VALUES (?1, ?2, ?3, 'default', datetime('now'), '{}')",
            rusqlite::params![
                format!("cat_{}", i),
                format!("catalyst text {} about something interesting", i),
                format!("entity_{}", i % 20),
            ],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for i in 0..N_CATALYSTS {
        let blob = f32_vec_to_blob(&make_embedding(i));
        conn.execute(
            "INSERT INTO catalyst_embeddings (catalyst_id, vector) VALUES (?1, ?2)",
            rusqlite::params![format!("cat_{}", i), blob],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for j in 0..N_SIMILARITIES {
        conn.execute(
            "INSERT INTO catalyst_similarities
             (catalyst_id, catalyst_text, file_path, chunk_index, similarity,
              chunk_metadata, timeline_data)
             VALUES ('cat_0', 'text', ?1, ?2, ?3, '{}', '{}')",
            rusqlite::params![
                format!("notes/file_{}.md", j),
                j as i64,
                1.0_f64 - j as f64 * 0.01,
            ],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn bench_catalyst_embeddings(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group = c.benchmark_group(format!("get_all_catalyst_embeddings/{N_CATALYSTS}"));
    group.bench_function("rusqlite", |b| {
        b.iter(|| black_box(db.get_all_catalyst_embeddings().unwrap()))
    });
    group.finish();
    drop(dir);
}

fn bench_catalyst_embeddings_packed(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_packed(&db);

    let mut group = c.benchmark_group(format!("get_all_catalyst_embeddings/{N_CATALYSTS}"));
    group.bench_function("rusqlite/packed", |b| {
        b.iter(|| {
            let rows = db
                .query_read(
                    "SELECT count, ids, vectors FROM catalyst_embeddings_packed WHERE id = 1",
                    &[],
                    |row| {
                        let ids_json: String = row.get(1)?;
                        let vectors_blob: Vec<u8> = row.get(2)?;
                        Ok((ids_json, vectors_blob))
                    },
                )
                .unwrap();
            let (ids_json, vectors_blob) = rows.into_iter().next().expect("packed row missing");
            let ids: Vec<String> = serde_json::from_str(&ids_json).unwrap();
            let results: Vec<(String, Vec<f32>)> = ids
                .into_iter()
                .zip(vectors_blob.chunks(EMBEDDING_DIM * 4).map(blob_to_f32_vec))
                .collect();
            black_box(results)
        })
    });
    group.finish();
    drop(dir);
}

fn bench_all_catalysts(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group = c.benchmark_group(format!("get_all_catalysts/{N_CATALYSTS}"));
    group.bench_function("rusqlite", |b| {
        b.iter(|| black_box(db.get_all_catalysts().unwrap()))
    });
    group.finish();
    drop(dir);
}

fn bench_similarities_for_catalyst(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("bench.db")).unwrap();
    seed_db(&db);

    let mut group =
        c.benchmark_group(format!("get_similarities_for_catalyst/{N_SIMILARITIES}_rows"));
    group.bench_function("rusqlite", |b| {
        b.iter(|| black_box(db.get_similarities_for_catalyst("cat_0", N_SIMILARITIES).unwrap()))
    });
    group.finish();
    drop(dir);
}

criterion_group!(
    benches,
    bench_catalyst_embeddings,
    bench_catalyst_embeddings_packed,
    bench_all_catalysts,
    bench_similarities_for_catalyst,
);
criterion_main!(benches);
