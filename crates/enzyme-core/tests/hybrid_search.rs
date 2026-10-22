use enzyme_core::db::Database;
use enzyme_core::embedding::get_embedding_service;
use enzyme_core::pipeline::embedding::EmbeddingPipeline;
use enzyme_core::pipeline::indexing::IndexingPipeline;
use enzyme_core::search::HybridSearch;

#[test]
fn hybrid_search_finds_a_named_note_without_catalysts() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("garden.md"),
        "Planted tomatoes and basil along the south fence this spring.",
    )
    .unwrap();
    std::fs::write(
        vault.join("kestrel migration.md"),
        "Moved the Kestrel service off the old queue. The rollback plan kept the audit log intact.",
    )
    .unwrap();
    std::fs::write(
        vault.join("reading.md"),
        "Finished a novel about a lighthouse keeper and the long winter.",
    )
    .unwrap();

    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let service = get_embedding_service(&vault).unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();

    let response = HybridSearch::new(&db, service)
        .search("what happened with kestrel", 2)
        .unwrap();
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.results[0].file_path, "kestrel migration.md");
    assert!(response.results[0].content.contains("rollback plan"));
    assert!(response.results[0].via_catalyst_id.is_none());
    assert!(response.top_contributing_catalysts.is_empty());

    let none = HybridSearch::new(&db, service)
        .search("kestrel", 0)
        .unwrap();
    assert!(none.results.is_empty());
}
