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
    fs::remove_file(h.path().join("state_1.sqlite")).unwrap();
    fs::copy(
        outside.path().join("state_1.sqlite"),
        h.path().join("state_1.sqlite"),
    )
    .unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let link = h.path().join(format!("state_1.sqlite{suffix}"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("state_1.sqlite"), &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(outside.path().join("state_1.sqlite"), &link).unwrap();
        assert!(Database::open(&lease, Kind::State).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(outside.path().join("state_1.sqlite"), &link).unwrap();
        assert!(Database::open(&lease, Kind::State).is_err());
        fs::remove_file(link).unwrap();
        assert_eq!(
            fs::read(outside.path().join("state_1.sqlite")).unwrap(),
            original
        );
        assert_eq!(fs::read(h.path().join("state_1.sqlite")).unwrap(), original);
    }
}

const GUARDED_THREADS: &str = "CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL, archived INTEGER,is_pinned INTEGER,history_mode TEXT,forked_from_id TEXT,parent_thread_id TEXT,source_thread_id TEXT);";
fn guarded(h: &tempfile::TempDir) -> Connection {
    fs::create_dir(h.path().join("archived_sessions")).unwrap();
    fs::write(h.path().join("archived_sessions/a.jsonl.zst"), "COMPRESSED").unwrap();
    fs::write(h.path().join("archived_sessions/b.jsonl"), "B").unwrap();
    let c = db(h, "state_1.sqlite", GUARDED_THREADS);
    c.execute_batch("INSERT INTO threads VALUES('a','archived_sessions/a.jsonl',1,0,'linear',NULL,NULL,NULL),('b','archived_sessions/b.jsonl',1,0,'linear',NULL,NULL,NULL),('outside','sessions/out.jsonl',0,0,'linear',NULL,NULL,NULL)").unwrap();
    c
}
fn selected() -> Vec<String> {
    vec![
        "archived_sessions/a.jsonl".into(),
        "archived_sessions/b.jsonl".into(),
    ]
}
#[test]
fn cleanup_selection_normalizes_compressed_absolute_paths_and_preserves_write_lock() {
    let h = home();
    let c = guarded(&h);
    c.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id='a'",
        [h.path().join("archived_sessions/a.jsonl").to_str().unwrap()],
    )
    .unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.cleanup_ids(&selected()).unwrap(), vec!["a", "b"]);
    c.busy_timeout(std::time::Duration::from_millis(1)).unwrap();
    assert!(c
        .execute("UPDATE threads SET is_pinned=1 WHERE id='a'", [])
        .is_err());
    assert!(d.cleanup_ids(&["../outside".into()]).is_err());
    assert!(d
        .cleanup_ids(&[selected()[0].clone(), selected()[0].clone()])
        .is_err());
    assert_eq!(
        d.capture(Table::Threads, &["a".into(), "b".into()])
            .unwrap()
            .unwrap()
            .len(),
        2
    );
    drop(d);
    c.execute("UPDATE threads SET is_pinned=1 WHERE id='a'", [])
        .unwrap();
}
#[test]
fn cleanup_refuses_active_pinned_paginated_and_corrupt_flags_before_any_change() {
    let h = home();
    let c = guarded(&h);
    let lease = Lease::acquire(h.path()).unwrap();
    for (field, value, error) in [
        ("archived", "0", "referenced_history"),
        ("archived", "NULL", "referenced_history"),
        ("is_pinned", "1", "pinned_thread"),
        ("history_mode", "'PAGINATED'", "referenced_history"),
        ("archived", "'unknown'", "db_reconcile_failed"),
    ] {
        c.execute_batch("UPDATE threads SET archived=1,is_pinned=0,history_mode='linear'")
            .unwrap();
        c.execute_batch(&format!("UPDATE threads SET {field}={value} WHERE id='a'"))
            .unwrap();
        let d = Database::open(&lease, Kind::State).unwrap().unwrap();
        assert_eq!(d.cleanup_ids(&selected()), Err(error));
    }
    assert_eq!(
        c.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        fs::read(h.path().join("archived_sessions/a.jsonl.zst")).unwrap(),
        b"COMPRESSED"
    );
    assert!(!h.path().join(".trash").exists());
}
#[test]
fn cleanup_refuses_incoming_and_outgoing_spawn_edges_but_allows_complete_subtree() {
    let h = home();
    let c = guarded(&h);
    let lease = Lease::acquire(h.path()).unwrap();
    c.execute_batch("CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,PRIMARY KEY(parent_thread_id,child_thread_id));INSERT INTO thread_spawn_edges VALUES('a','b');").unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.cleanup_ids(&selected()).unwrap(), vec!["a", "b"]);
    drop(d);
    for edge in [("outside", "a"), ("b", "outside")] {
        c.execute(
            "INSERT INTO thread_spawn_edges VALUES(?1,?2)",
            params![edge.0, edge.1],
        )
        .unwrap();
        let d = Database::open(&lease, Kind::State).unwrap().unwrap();
        assert_eq!(d.cleanup_ids(&selected()), Err("referenced_history"));
        drop(d);
        c.execute(
            "DELETE FROM thread_spawn_edges WHERE parent_thread_id=?1 AND child_thread_id=?2",
            params![edge.0, edge.1],
        )
        .unwrap();
    }
}
#[test]
fn cleanup_refuses_external_fork_reference_for_all_supported_schema_columns() {
    let h = home();
    let c = guarded(&h);
    let lease = Lease::acquire(h.path()).unwrap();
    for column in ["forked_from_id", "parent_thread_id", "source_thread_id"] {
        c.execute_batch(&format!("UPDATE threads SET {column}='a' WHERE id='b'"))
            .unwrap();
        let d = Database::open(&lease, Kind::State).unwrap().unwrap();
        assert_eq!(d.cleanup_ids(&selected()).unwrap(), vec!["a", "b"]);
        drop(d);
        c.execute_batch(&format!(
            "UPDATE threads SET {column}='a' WHERE id='outside'"
        ))
        .unwrap();
        let d = Database::open(&lease, Kind::State).unwrap().unwrap();
        assert_eq!(d.cleanup_ids(&selected()), Err("referenced_history"));
        drop(d);
        c.execute_batch(&format!("UPDATE threads SET {column}=NULL"))
            .unwrap();
    }
}
#[test]
fn cleanup_supports_older_schema_but_refuses_missing_metadata_and_oversized_scan() {
    let h = home();
    let c = db(&h, "state_1.sqlite", THREADS);
    let lease = Lease::acquire(h.path()).unwrap();
    c.execute(
        "INSERT INTO threads VALUES('a','archived_sessions/a.jsonl','KEEP')",
        [],
    )
    .unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.cleanup_ids(&selected()).unwrap(), vec!["a"]);
    drop(d);
    c.execute_batch("DROP TABLE threads;CREATE TABLE threads(id TEXT PRIMARY KEY)")
        .unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert!(d.cleanup_ids(&selected()).is_err());
    drop(d);
    c.execute_batch("DROP TABLE threads;CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT);WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50001) INSERT INTO threads SELECT CAST(i AS TEXT),'sessions/active' FROM n;").unwrap();
    let d = Database::open(&lease, Kind::State).unwrap().unwrap();
    assert_eq!(d.cleanup_ids(&selected()), Err("storage_backup_too_large"));
}
