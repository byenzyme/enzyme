use std::path::PathBuf;

use enzyme_core::config::paths::EnzymePaths;
use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::similarity::SimilarityPipeline;

fn main() -> anyhow::Result<()> {
    let vault_path = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: recompute_sim <vault_path>"),
    );
    let paths = EnzymePaths::new(&vault_path)?;
    let db = Database::open(&paths.vault_db)?;
    let svc = get_embedding_service(&paths.home)?;

    eprintln!("backend dim: {}", svc.dimension());

    // Re-embed all chunks
    let emb_pipeline = EmbeddingPipeline::new(&db, svc);
    let emb_stats = emb_pipeline.run(|progress| {
        eprintln!("embedding: {progress:?}");
    })?;
    eprintln!(
        "embedded: {} docs, {} chunks",
        emb_stats.documents_embedded, emb_stats.chunks_embedded
    );

    // Verify stored dim
    let check: Vec<i64> = db.query_read(
        "SELECT length(vector) FROM chunk_embeddings LIMIT 1",
        vec![],
        |row| row.get::<i64>(0),
    )?;
    eprintln!(
        "stored vector bytes: {:?} (= {} f32s)",
        check,
        check.first().unwrap_or(&0) / 4
    );

    // Recompute similarities
    let sim_pipeline = SimilarityPipeline::new(&db, svc);
    let stats = sim_pipeline.run(|done, total| {
        if done % 50 == 0 || done == total {
            eprintln!("similarity: {done}/{total} catalysts");
        }
    })?;
    eprintln!(
        "done: {} catalysts, {} similarities",
        stats.catalysts_processed, stats.similarities_computed
    );
    Ok(())
}
