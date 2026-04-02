/// Benchmark: libsql (local file) read performance on the catalyze hot path.
///
/// Companion to `catalyze_rusqlite`. This binary intentionally never calls
/// into rusqlite / `Database::open` — libsql must initialize SQLite first or
/// both drivers assert-fail on the threading config. The DB is seeded via
/// libsql directly using the same schema SQL.
///
/// Run with:
///   cargo bench -p enzyme-core --bench catalyze_libsql
///
/// Compare against `catalyze_rusqlite` output to evaluate regression.
/// Accept ≤2ms regression on the catalyst_embeddings/500 group.
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use serde_json;

const N_CATALYSTS: usize = 500;
const EMBEDDING_DIM: usize = 384;
const N_SIMILARITIES: usize = 50;

// ---------------------------------------------------------------------------
// Minimal schema — only the three tables exercised by the hot path.
// Kept in sync with enzyme-core/src/db/schema.rs manually.
// ---------------------------------------------------------------------------

const BENCH_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS catalysts (
    id           TEXT PRIMARY KEY,
    text         TEXT NOT NULL,
    entity       TEXT NOT NULL,
    context      TEXT DEFAULT 'default',
    generated_at TEXT DEFAULT (datetime('now')),
    metadata     TEXT DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS catalyst_embeddings (
    catalyst_id TEXT NOT NULL PRIMARY KEY,
    vector      BLOB NOT NULL
);

-- Packed variant: all embeddings in a single row, one large BLOB.
-- Eliminates per-row async overhead at the cost of whole-table rewrite on update.
CREATE TABLE IF NOT EXISTS catalyst_embeddings_packed (
    id      INTEGER PRIMARY KEY DEFAULT 1,
    count   INTEGER NOT NULL,
    ids     TEXT    NOT NULL,  -- JSON array of catalyst_id strings (ordering matches vectors)
    vectors BLOB    NOT NULL   -- count × EMBEDDING_DIM f32 values, little-endian, concatenated
);

CREATE TABLE IF NOT EXISTS catalyst_similarities (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    catalyst_id     TEXT    NOT NULL,
    catalyst_text   TEXT    NOT NULL,
    file_path       TEXT    NOT NULL,
    chunk_index     INTEGER DEFAULT 0,
    similarity      REAL    NOT NULL,
    chunk_metadata  TEXT    DEFAULT '{}',
    timeline_data   TEXT    DEFAULT '{}',
    created_at      TEXT    DEFAULT (datetime('now'))
);
";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// DB setup via libsql (rusqlite must never be called in this binary)
// ---------------------------------------------------------------------------

async fn setup_libsql_db(
    db_path: &std::path::Path,
) -> (libsql::Database, libsql::Connection) {
    let path = db_path.to_str().expect("non-UTF8 path");
    let ldb = libsql::Builder::new_local(path)
        .build()
        .await
        .expect("libsql build");
    let conn = ldb.connect().expect("libsql connect");

    // Apply schema
    conn.execute_batch(BENCH_SCHEMA).await.expect("schema");

    // Match rusqlite read_conn pragma tuning.
    // These PRAGMAs return a result row in libsql even as setters, so use query() + drain.
    let pragmas = [
        "PRAGMA cache_size = -20000".to_string(),
        format!("PRAGMA mmap_size = {}", 256 * 1024 * 1024),
    ];
    for sql in &pragmas {
        let mut rows = conn.query(sql, ()).await.unwrap();
        while rows.next().await.unwrap().is_some() {}
    }

    // Seed catalysts
    for i in 0..N_CATALYSTS {
        conn.execute(
            "INSERT INTO catalysts (id, text, entity, context, generated_at, metadata)
             VALUES (?1, ?2, ?3, 'default', datetime('now'), '{}')",
            (
                format!("cat_{}", i),
                format!("catalyst text {} about something interesting", i),
                format!("entity_{}", i % 20),
            ),
        )
        .await
        .unwrap();
    }

    // Seed catalyst_embeddings
    for i in 0..N_CATALYSTS {
        let blob = f32_vec_to_blob(&make_embedding(i));
        conn.execute(
            "INSERT INTO catalyst_embeddings (catalyst_id, vector) VALUES (?1, ?2)",
            (format!("cat_{}", i), blob),
        )
        .await
        .unwrap();
    }

    // Seed catalyst_embeddings_packed (all in one row)
    {
        let ids: Vec<String> = (0..N_CATALYSTS).map(|i| format!("cat_{}", i)).collect();
        let ids_json = serde_json::to_string(&ids).unwrap();
        let mut all_vecs: Vec<u8> = Vec::with_capacity(N_CATALYSTS * EMBEDDING_DIM * 4);
        for i in 0..N_CATALYSTS {
            all_vecs.extend_from_slice(&f32_vec_to_blob(&make_embedding(i)));
        }
        conn.execute(
            "INSERT INTO catalyst_embeddings_packed (count, ids, vectors) VALUES (?1, ?2, ?3)",
            (N_CATALYSTS as i64, ids_json, all_vecs),
        )
        .await
        .unwrap();
    }

    // Seed catalyst_similarities for cat_0
    for j in 0..N_SIMILARITIES {
        conn.execute(
            "INSERT INTO catalyst_similarities
             (catalyst_id, catalyst_text, file_path, chunk_index, similarity,
              chunk_metadata, timeline_data)
             VALUES ('cat_0', 'text', ?1, ?2, ?3, '{}', '{}')",
            (
                format!("notes/file_{}.md", j),
                j as i64,
                1.0_f64 - j as f64 * 0.01,
            ),
        )
        .await
        .unwrap();
    }

    (ldb, conn)
}

// ---------------------------------------------------------------------------
// Benchmarks — group names match catalyze_rusqlite for easy comparison
// ---------------------------------------------------------------------------

fn bench_catalyst_embeddings(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let (_ldb, conn) = rt.block_on(setup_libsql_db(&db_path));

    let mut group = c.benchmark_group(format!("get_all_catalyst_embeddings/{N_CATALYSTS}"));
    group.bench_function("libsql/block_on", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut rows = conn
                    .query("SELECT catalyst_id, vector FROM catalyst_embeddings", ())
                    .await
                    .unwrap();
                let mut results = Vec::with_capacity(N_CATALYSTS);
                while let Some(row) = rows.next().await.unwrap() {
                    let id: String = row.get(0).unwrap();
                    let blob: Vec<u8> = row.get(1).unwrap();
                    results.push((id, blob_to_f32_vec(&blob)));
                }
                black_box(results)
            })
        })
    });
    group.finish();
    drop(dir);
}

fn bench_catalyst_embeddings_packed(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let (_ldb, conn) = rt.block_on(setup_libsql_db(&db_path));

    let mut group = c.benchmark_group(format!("get_all_catalyst_embeddings/{N_CATALYSTS}"));
    group.bench_function("libsql/packed", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut rows = conn
                    .query(
                        "SELECT count, ids, vectors FROM catalyst_embeddings_packed WHERE id = 1",
                        (),
                    )
                    .await
                    .unwrap();
                let row = rows.next().await.unwrap().expect("packed row missing");
                let _count: i64 = row.get(0).unwrap();
                let ids_json: String = row.get(1).unwrap();
                let vectors_blob: Vec<u8> = row.get(2).unwrap();
                let ids: Vec<String> = serde_json::from_str(&ids_json).unwrap();
                let results: Vec<(String, Vec<f32>)> = ids
                    .into_iter()
                    .zip(
                        vectors_blob
                            .chunks(EMBEDDING_DIM * 4)
                            .map(blob_to_f32_vec),
                    )
                    .collect();
                black_box(results)
            })
        })
    });
    group.finish();
    drop(dir);
}

fn bench_all_catalysts(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let (_ldb, conn) = rt.block_on(setup_libsql_db(&db_path));

    let mut group = c.benchmark_group(format!("get_all_catalysts/{N_CATALYSTS}"));
    group.bench_function("libsql/block_on", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut rows = conn
                    .query(
                        "SELECT id, text, entity, context, generated_at, metadata FROM catalysts",
                        (),
                    )
                    .await
                    .unwrap();
                let mut results: Vec<(String, String, String)> = Vec::with_capacity(N_CATALYSTS);
                while let Some(row) = rows.next().await.unwrap() {
                    let id: String = row.get(0).unwrap();
                    let text: String = row.get(1).unwrap();
                    let entity: String = row.get(2).unwrap();
                    results.push((id, text, entity));
                }
                black_box(results)
            })
        })
    });
    group.finish();
    drop(dir);
}

fn bench_similarities_for_catalyst(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("bench.db");
    let (_ldb, conn) = rt.block_on(setup_libsql_db(&db_path));

    let mut group =
        c.benchmark_group(format!("get_similarities_for_catalyst/{N_SIMILARITIES}_rows"));
    group.bench_function("libsql/block_on", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut rows = conn
                    .query(
                        "SELECT id, catalyst_id, catalyst_text, file_path, chunk_index,
                                similarity, chunk_metadata, timeline_data, created_at
                         FROM catalyst_similarities
                         WHERE catalyst_id = ?1
                         ORDER BY similarity DESC
                         LIMIT ?2",
                        (String::from("cat_0"), N_SIMILARITIES as i64),
                    )
                    .await
                    .unwrap();
                let mut results: Vec<(String, f64)> = Vec::with_capacity(N_SIMILARITIES);
                while let Some(row) = rows.next().await.unwrap() {
                    let file_path: String = row.get(3).unwrap();
                    let similarity: f64 = row.get(5).unwrap();
                    results.push((file_path, similarity));
                }
                black_box(results)
            })
        })
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
