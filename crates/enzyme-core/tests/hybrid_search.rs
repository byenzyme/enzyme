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

#[test]
fn fts_tracks_note_add_edit_and_delete_and_escapes_queries() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    let service = get_embedding_service(&vault).unwrap();
    let note = vault.join("ticket.md");

    std::fs::write(&note, "ENG-4902 investigates amber orbits.").unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    assert!(db.has_chunk_fts().unwrap());
    let hits = db.search_chunks_fts("\"eng 4902\"", 10, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "ticket.md");
    assert_eq!(
        HybridSearch::new(&db, service)
            .search("ENG-4902: OR *", 10)
            .unwrap()
            .results[0]
            .file_path,
        "ticket.md"
    );

    std::fs::write(&note, "The violet horizon replaced the old ticket.").unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    assert!(
        db.search_chunks_fts("\"amber\"", 10, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.search_chunks_fts("\"violet\"", 10, 10).unwrap().len(), 1);

    std::fs::remove_file(&note).unwrap();
    IndexingPipeline::new(&db, vault).run(true, |_| {}).unwrap();
    assert!(
        db.search_chunks_fts("\"violet\"", 10, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn old_read_only_vault_uses_bm25_then_refresh_builds_fts() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("signal.md"),
        "The cobalt signal reached the lighthouse.",
    )
    .unwrap();
    let path = temp.path().join("enzyme.db");
    let db = Database::open(&path).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let service = get_embedding_service(&vault).unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();
    for trigger in [
        "chunks_fts_insert",
        "chunks_fts_delete",
        "chunks_fts_update",
    ] {
        db.execute_write(&format!("DROP TRIGGER {trigger}"), vec![])
            .unwrap();
    }
    db.execute_write("DROP TABLE chunks_fts", vec![]).unwrap();
    drop(db);

    let old = Database::open_read_only(&path).unwrap();
    assert!(!old.has_chunk_fts().unwrap());
    assert_eq!(
        HybridSearch::new(&old, service)
            .search("cobalt lighthouse", 1)
            .unwrap()
            .results[0]
            .file_path,
        "signal.md"
    );
    drop(old);

    let upgraded = Database::open(&path).unwrap();
    assert!(!upgraded.has_chunk_fts().unwrap());
    IndexingPipeline::new(&upgraded, vault)
        .run(false, |_| {})
        .unwrap();
    assert!(upgraded.has_chunk_fts().unwrap());
    assert_eq!(
        upgraded.search_chunks_fts("\"cobalt\"", 10, 10).unwrap()[0].0,
        "signal.md"
    );
}

#[test]
fn notes_are_found_by_frontmatter_people_and_title() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("renewal-thread.md"),
        "---\ntitle: \"Escalation: rollback guarantees for Northwind\"\npeople:\n  - \"[[Sam Rivera]]\"\n---\nWe agreed to keep the old image for thirty days.",
    )
    .unwrap();
    std::fs::write(
        vault.join("lunch.md"),
        "---\ntitle: \"Lunch plans\"\n---\nTacos on Friday near the office.",
    )
    .unwrap();

    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();
    let service = get_embedding_service(&vault).unwrap();
    EmbeddingPipeline::new(&db, service).run(|_| {}).unwrap();

    let by_person = db
        .search_chunks_fts("\"sam\" OR \"rivera\"", 10, 10)
        .unwrap();
    assert_eq!(by_person.len(), 1);
    assert_eq!(by_person[0].0, "renewal-thread.md");
    let by_title = db.search_chunks_fts("\"northwind\"", 10, 10).unwrap();
    assert_eq!(by_title[0].0, "renewal-thread.md");
}
