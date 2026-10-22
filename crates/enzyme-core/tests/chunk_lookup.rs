use enzyme_core::db::Database;
use enzyme_core::pipeline::indexing::IndexingPipeline;

#[test]
fn chunk_lookup_handles_more_paths_than_sqlite_binds() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    for name in ["b.md", "a.md", "c.md"] {
        std::fs::write(vault.join(name), format!("Notes about {name}.")).unwrap();
    }
    let db = Database::open(temp.path().join("enzyme.db")).unwrap();
    IndexingPipeline::new(&db, vault.clone())
        .run(true, |_| {})
        .unwrap();

    let mut paths: Vec<String> = (0..40_000).map(|i| format!("missing-{i}.md")).collect();
    paths.extend(["c.md", "a.md", "b.md", "a.md"].map(String::from));
    let chunks = db.get_chunks_for_files(&paths).unwrap();
    let found: Vec<&str> = chunks.iter().map(|c| c.file_path.as_str()).collect();
    assert_eq!(found, vec!["a.md", "b.md", "c.md"]);
    assert!(chunks[0].content.contains("a.md"));
}
