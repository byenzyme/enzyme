use enzyme_core::{db::Database, initialize_sqlite_runtime};
use libsql::Value;
use rusqlite::Connection;

#[test]
fn initializer_then_source_wrapper_then_recall_wrapper() {
    initialize_sqlite_runtime().unwrap();
    initialize_sqlite_runtime().unwrap();

    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.db");
    let source = Connection::open(source_path).unwrap();
    source
        .execute_batch(
            "CREATE TABLE source_rows (value INTEGER); INSERT INTO source_rows VALUES (41);",
        )
        .unwrap();
    let source_value: i64 = source
        .query_row("SELECT value + 1 FROM source_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source_value, 42);

    let recall = Database::open_in_memory().unwrap();
    recall
        .execute_write("CREATE TABLE runtime_order (value INTEGER)", vec![])
        .unwrap();
    recall
        .execute_write(
            "INSERT INTO runtime_order VALUES (?1)",
            vec![Value::Integer(42)],
        )
        .unwrap();
    let values = recall
        .query_read("SELECT value FROM runtime_order", vec![], |row| {
            row.get::<i64>(0)
        })
        .unwrap();
    assert_eq!(values, vec![42]);
}
