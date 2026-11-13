use crate::{
    storage_cleanup,
    storage_rows::{Database, Kind, Table},
    storage_stage::Lease,
};
use rusqlite::{types::Value, Connection};
use std::fs;

const SCHEMA: &str = include_str!("../tests/fixtures/storage-state.sql");
fn image(db: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut query = db
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
        .unwrap();
    let count = query.column_count();
    query
        .query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn insert(db: &Connection, id: &str, path: &str) {
    db.execute("INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived) VALUES(?1,?2,10,20,'cli','openai','workspace',?1,'read-only','on-request',1)", [id,path]).unwrap();
}
#[test]
fn native_timestamp_triggers_and_related_chat_metadata_restore_exactly() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("archived_sessions")).unwrap();
    let path = home.path().join("archived_sessions/target.jsonl");
    fs::write(&path, "ARCHIVE").unwrap();
    let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    insert(&db, "target", path.to_str().unwrap());
    insert(&db, "untouched", "sessions/keep.jsonl");
    // A backfilled older row may contain null/zero values. Restore must retain
    // them rather than silently replaying timestamp initialization triggers.
    db.execute_batch("UPDATE threads SET created_at_ms=NULL,updated_at_ms=NULL,recency_at=0,recency_at_ms=0 WHERE id='target';
        INSERT INTO thread_dynamic_tools(thread_id,position,name,description,input_schema) VALUES('target',0,'read','tool','{}');
        INSERT INTO thread_attachments VALUES('attachment','target','file','identity','{}',10);
        INSERT INTO guardian_review_feedback VALUES('feedback','target',X'00FF');
        INSERT INTO thread_read_receipts VALUES('target',NULL,'revision','turn');").unwrap();
    let tables = [
        "threads",
        "thread_dynamic_tools",
        "thread_attachments",
        "guardian_review_feedback",
        "thread_read_receipts",
    ];
    let before: Vec<_> = tables.iter().map(|t| image(&db, t)).collect();
    let reader = crate::storage_preview::preview(home.path(), 100.0).unwrap();
    let cleaned =
        storage_cleanup::quarantine(home.path(), 100.0, reader["digest"].as_str().unwrap())
            .unwrap();
    assert!(!path.exists());
    assert_eq!(image(&db, "threads").len(), 1);
    for table in &tables[1..] {
        assert!(image(&db, table).is_empty());
    }
    storage_cleanup::restore(home.path(), cleaned["trashDir"].as_str().unwrap()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"ARCHIVE");
    assert_eq!(
        tables.iter().map(|t| image(&db, t)).collect::<Vec<_>>(),
        before
    );
    // The agent's independent connection still executes its own triggers.
    insert(&db, "later", "sessions/later.jsonl");
    assert_eq!(
        db.query_row(
            "SELECT created_at_ms FROM threads WHERE id='later'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        10_000
    );
}
#[test]
fn a_known_trigger_name_with_changed_behavior_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.execute_batch("DROP TRIGGER threads_recency_at_after_insert; CREATE TRIGGER threads_recency_at_after_insert AFTER INSERT ON threads BEGIN DELETE FROM threads; END;").unwrap();
    let lease = Lease::acquire(home.path()).unwrap();
    let reader = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert!(matches!(
        reader.capture(Table::Threads, &["target".into()]),
        Err("storage_schema_not_supported")
    ));
}
