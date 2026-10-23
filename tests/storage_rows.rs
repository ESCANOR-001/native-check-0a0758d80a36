use remodex_connect_native::{
    storage_rows::{Database, Kind, Rows, Table},
    storage_stage::Lease,
};
use rusqlite::{params, Connection};
use std::fs;
fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
fn db(home: &tempfile::TempDir, name: &str, sql: &str) -> Connection {
    let c = Connection::open(home.path().join(name)).unwrap();
    c.execute_batch(sql).unwrap();
    c
}
const THREADS: &str = "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, body TEXT);";
#[test]
fn missing_database_is_not_created_and_only_latest_version_is_selected() {
    let h = home();
    let lease = Lease::acquire(h.path()).unwrap();
    assert!(Database::open(&lease, Kind::State).unwrap().is_none());
    assert!(!h.path().join("state_1.sqlite").exists());
    let old = db(&h, "state_1.sqlite", THREADS);
    old.execute("INSERT INTO threads VALUES('old','old','OLD')", [])
        .unwrap();
    let new = db(&h, "state_2.sqlite", THREADS);
    new.execute("INSERT INTO threads VALUES('new','new','NEW')", [])
        .unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d
        .capture(Table::Threads, &["new".into(), "old".into()])
        .unwrap()
        .unwrap();
    assert_eq!(r.len(), 1);
    d.delete(&r).unwrap();
    d.commit().unwrap();
    assert_eq!(
        old.query_row("SELECT body FROM threads", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "OLD"
    );
    assert_eq!(
        new.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn row_round_trip_is_lossless_for_integer_blob_null_and_quoted_columns() {
    let h = home();
    let c=db(&h,"logs_1.sqlite","CREATE TABLE logs(id INTEGER PRIMARY KEY,thread_id TEXT, n INTEGER, b BLOB, r REAL, \"quote\"\"col\" TEXT, nullable TEXT);");
    c.execute(
        "INSERT INTO logs VALUES(1,'a',?1,?2,0.12345678901234567,'quoted',NULL)",
        params![9_007_199_254_740_993_i64, vec![0u8, 255, 128]],
    )
    .unwrap();
    c.execute(
        "INSERT INTO logs VALUES(2,'other',0,NULL,0,'KEEP',NULL)",
        [],
    )
    .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::Logs).unwrap().unwrap();
    let backup = d.capture(Table::Logs, &["a".into()]).unwrap().unwrap();
    let bytes = serde_json::to_vec(&backup).unwrap();
    let backup: Rows = serde_json::from_slice(&bytes).unwrap();
    d.delete(&backup).unwrap();
    d.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM logs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let mut d = Database::open(&lease, Kind::Logs).unwrap().unwrap();
    d.restore(&backup).unwrap();
    d.restore(&backup).unwrap();
    d.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT n,b,nullable FROM logs WHERE id=1", [], |r| Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, Option<String>>(2)?
        )))
        .unwrap(),
        (9_007_199_254_740_993, vec![0, 255, 128], None)
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM logs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn drop_rolls_back_and_busy_database_is_rejected_without_mutating_rows() {
    let h = home();
    let c = db(&h, "state_1.sqlite", THREADS);
    c.execute("INSERT INTO threads VALUES('a','x','KEEP')", [])
        .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        Database::open(&lease, Kind::State),
        Err("codex_busy")
    ));
    c.execute_batch("ROLLBACK").unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d.capture(Table::Threads, &["a".into()]).unwrap().unwrap();
    d.delete(&r).unwrap();
    drop(d);
    assert_eq!(
        c.query_row("SELECT body FROM threads", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "KEEP"
    );
}
#[test]
fn restore_conflicts_roll_back_earlier_inserts_and_preserve_new_user_rows() {
    let h = home();
    let c = db(&h, "state_1.sqlite", THREADS);
    c.execute_batch("INSERT INTO threads VALUES('a','a','A'),('b','b','B')")
        .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d
        .capture(Table::Threads, &["a".into(), "b".into()])
        .unwrap()
        .unwrap();
    d.delete(&r).unwrap();
    d.commit().unwrap();
    c.execute("INSERT INTO threads VALUES('b','b','USER_EDIT')", [])
        .unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.restore(&r), Err("storage_restore_conflict"));
    assert!(d.commit().is_err());
    assert_eq!(
        c.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        c.query_row("SELECT body FROM threads", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "USER_EDIT"
    );
}
#[test]
fn changed_delete_rows_abort_the_whole_transaction() {
    let h = home();
    let c = db(&h, "state_1.sqlite", THREADS);
    c.execute_batch("INSERT INTO threads VALUES('a','a','A'),('b','b','B')")
        .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d
        .capture(Table::Threads, &["a".into(), "b".into()])
        .unwrap()
        .unwrap();
    drop(d);
    c.execute("UPDATE threads SET body='NEW' WHERE id='b'", [])
        .unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.delete(&r), Err("storage_rows_changed"));
    assert!(d.commit().is_err());
    assert_eq!(
        c.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn foreign_key_cascades_cannot_delete_unrecorded_dependents() {
    let h = home();
    let c=db(&h,"state_1.sqlite","CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,body TEXT); CREATE TABLE thread_dynamic_tools(thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE,position INTEGER,body TEXT, PRIMARY KEY(thread_id,position));INSERT INTO threads VALUES('a','a','A');INSERT INTO thread_dynamic_tools VALUES('a',0,'TOOL');");
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d.capture(Table::Threads, &["a".into()]).unwrap().unwrap();
    assert_eq!(d.delete(&r), Err("storage_dependents_remaining"));
    drop(d);
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let tools = d
        .capture(Table::DynamicTools, &["a".into()])
        .unwrap()
        .unwrap();
    d.delete(&tools).unwrap();
    d.delete(&r).unwrap();
    d.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM thread_dynamic_tools", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    d.restore(&r).unwrap();
    d.restore(&tools).unwrap();
    d.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT body FROM thread_dynamic_tools", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "TOOL"
    );
}
#[test]
fn triggers_schema_changes_and_wrong_database_kind_are_refused() {
    let h = home();
    let c = db(&h, "state_1.sqlite", THREADS);
    c.execute("INSERT INTO threads VALUES('a','a','A')", [])
        .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert!(d.capture(Table::Logs, &["a".into()]).is_err());
    let r = d.capture(Table::Threads, &["a".into()]).unwrap().unwrap();
    drop(d);
    c.execute_batch("ALTER TABLE threads ADD COLUMN added TEXT;")
        .unwrap();
    let mut d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.delete(&r), Err("storage_schema_changed"));
    drop(d);
    c.execute_batch(
        "CREATE TRIGGER no_delete AFTER DELETE ON threads BEGIN DELETE FROM threads; END;",
    )
    .unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert!(matches!(
        d.capture(Table::Threads, &["a".into()]),
        Err("storage_schema_not_supported")
    ));
    assert_eq!(
        c.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn chunked_edge_snapshot_deduplicates_and_memory_jobs_exclude_consolidation() {
    let h = home();
    let c=db(&h,"state_1.sqlite","CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,PRIMARY KEY(parent_thread_id,child_thread_id));INSERT INTO thread_spawn_edges VALUES('000','200');");
    let lease = Lease::acquire(h.path()).unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    let r = d
        .capture(
            Table::SpawnEdges,
            &(0..201).map(|i| format!("{i:03}")).collect::<Vec<_>>(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.len(), 1);
    drop(d);
    drop(c);
    let c=db(&h,"memories_1.sqlite","CREATE TABLE jobs(kind TEXT,job_key TEXT,body TEXT,PRIMARY KEY(kind,job_key));INSERT INTO jobs VALUES('memory_stage1','a','STAGE1'),('memory_consolidate_global','global','KEEP');");
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let r = d
        .capture(Table::Jobs, &["a".into(), "global".into()])
        .unwrap()
        .unwrap();
    assert_eq!(r.len(), 1);
    d.delete(&r).unwrap();
    d.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT body FROM jobs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "KEEP"
    );
}
#[test]
fn oversized_cells_fail_before_backup_allocation_or_deletion() {
    let h = home();
    let c=db(&h,"logs_1.sqlite","CREATE TABLE logs(id INTEGER PRIMARY KEY,thread_id TEXT,body BLOB);INSERT INTO logs VALUES(1,'a',zeroblob(9000000));");
    let lease = Lease::acquire(h.path()).unwrap();
    let d = Database::open(&lease, Kind::Logs).unwrap().unwrap();
    assert!(d.capture(Table::Logs, &["a".into()]).is_err());
    drop(d);
    assert_eq!(
        c.query_row("SELECT length(body) FROM logs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        9_000_000
    );
}
#[cfg(any(unix, windows))]
#[test]
fn linked_databases_and_sidecars_are_rejected_without_writing_the_target() {
    let h = home();
    let outside = home();
    let c = db(&outside, "state_1.sqlite", THREADS);
    drop(c);
    let original = fs::read(outside.path().join("state_1.sqlite")).unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("state_1.sqlite"),
        h.path().join("state_1.sqlite"),
    )
    .unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(
        outside.path().join("state_1.sqlite"),
        h.path().join("state_1.sqlite"),
    )
    .unwrap();
    assert!(Database::open(&lease, Kind::State).is_err());
    fs::remove_file(h.path().join("state_1.sqlite")).unwrap();
    fs::hard_link(
        outside.path().join("state_1.sqlite"),
        h.path().join("state_1.sqlite"),
    )
    .unwrap();
    assert!(Database::open(&lease, Kind::State).is_err());
    assert_eq!(
        fs::read(outside.path().join("state_1.sqlite")).unwrap(),
        original
    );
}
