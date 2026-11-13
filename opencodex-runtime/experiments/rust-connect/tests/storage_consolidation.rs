use remodex_connect_native::{
    storage_rows::{ConsolidationChange, Database, Kind, Table},
    storage_stage::Lease,
};
use rusqlite::Connection;
fn fixture() -> (tempfile::TempDir, Connection) {
    let h = tempfile::tempdir().unwrap();
    let c = Connection::open(h.path().join("memories_1.sqlite")).unwrap();
    c.execute_batch("CREATE TABLE stage1_outputs(thread_id TEXT PRIMARY KEY,selected_for_phase2 INTEGER,body TEXT); INSERT INTO stage1_outputs VALUES('a',1,'A'),('b',0,'B'); CREATE TABLE jobs(kind TEXT,job_key TEXT,status TEXT NOT NULL,worker_id TEXT,ownership_token TEXT,started_at INTEGER,finished_at INTEGER,lease_until INTEGER,retry_at INTEGER,retry_remaining INTEGER NOT NULL,last_error TEXT,input_watermark INTEGER,last_success_watermark INTEGER,PRIMARY KEY(kind,job_key));INSERT INTO jobs VALUES('memory_stage1','a','done',NULL,NULL,NULL,NULL,NULL,NULL,3,NULL,1,1);").unwrap();
    (h, c)
}
type JobStatus = (String, Option<String>, Option<i64>, i64, i64);
fn status(c: &Connection) -> Option<JobStatus> {
    use rusqlite::OptionalExtension;
    c.query_row("SELECT status,worker_id,retry_at,retry_remaining,input_watermark FROM jobs WHERE kind='memory_consolidate_global'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().unwrap()
}
#[test]
fn new_consolidation_job_is_durable_and_removed_only_when_owned_image_matches() {
    let (h, c) = fixture();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
    let change = d.refresh_consolidation(&inputs).unwrap().unwrap();
    let change: ConsolidationChange =
        serde_json::from_slice(&serde_json::to_vec(&change).unwrap()).unwrap();
    d.delete(&inputs).unwrap();
    d.commit().unwrap();
    let live = status(&c).unwrap();
    assert_eq!(live.0, "pending");
    assert_eq!(live.3, 3);
    assert!((remodex_connect_native::auth::now() / 1000 - live.4).abs() < 10);
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    d.restore(&inputs).unwrap();
    assert!(d.restore_consolidation(&change).unwrap());
    assert!(d.restore_consolidation(&change).unwrap());
    d.commit().unwrap();
    assert!(status(&c).is_none());
    assert_eq!(
        c.query_row("SELECT count(*) FROM stage1_outputs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn running_worker_and_lease_are_preserved_and_old_image_is_restored_exactly() {
    let (h, c) = fixture();
    let future = remodex_connect_native::auth::now() / 1000 + 100;
    c.execute("INSERT INTO jobs VALUES('memory_consolidate_global','global','running','worker','owner',11,NULL,22,33,5,NULL,?1,44)",[future]).unwrap();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
    let change = d.refresh_consolidation(&inputs).unwrap().unwrap();
    d.commit().unwrap();
    assert_eq!(
        status(&c).unwrap(),
        (
            "running".into(),
            Some("worker".into()),
            Some(33),
            5,
            future + 1
        )
    );
    assert_eq!(
        c.query_row(
            "SELECT ownership_token,lease_until FROM jobs WHERE job_key='global'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        )
        .unwrap(),
        ("owner".into(), 22)
    );
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    assert!(d.restore_consolidation(&change).unwrap());
    d.commit().unwrap();
    assert_eq!(
        status(&c).unwrap(),
        ("running".into(), Some("worker".into()), Some(33), 5, future)
    );
}
#[test]
fn newer_job_and_deleted_job_are_not_overwritten_on_cleanup_restore() {
    for remove in [false, true] {
        let (h, c) = fixture();
        let lease = Lease::acquire(h.path()).unwrap();
        let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
        let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
        let change = d.refresh_consolidation(&inputs).unwrap().unwrap();
        d.delete(&inputs).unwrap();
        d.commit().unwrap();
        if remove {
            c.execute("DELETE FROM jobs WHERE job_key='global'", [])
                .unwrap();
        } else {
            c.execute("UPDATE jobs SET status='running',worker_id='new-worker',input_watermark=input_watermark+1 WHERE job_key='global'",[]).unwrap();
        }
        let before = status(&c);
        let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
        d.restore(&inputs).unwrap();
        let result = d.restore_consolidation(&change).unwrap();
        assert_eq!(result, remove);
        d.commit().unwrap();
        assert_eq!(status(&c), before);
        assert_eq!(
            c.query_row("SELECT count(*) FROM stage1_outputs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
}
#[test]
fn unselected_inputs_and_schemas_without_jobs_do_not_create_global_work() {
    let (h, c) = fixture();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["b".into()]).unwrap().unwrap();
    assert!(d.refresh_consolidation(&inputs).unwrap().is_none());
    d.commit().unwrap();
    assert!(status(&c).is_none());
    c.execute_batch("DROP TABLE jobs").unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
    assert!(d.refresh_consolidation(&inputs).unwrap().is_none());
    d.commit().unwrap();
}
#[test]
fn refresh_failure_rolls_back_prior_deletes_and_rejects_watermark_overflow() {
    let (h, c) = fixture();
    let lease = Lease::acquire(h.path()).unwrap();
    c.execute("INSERT INTO jobs VALUES('memory_consolidate_global','global','done',NULL,NULL,NULL,NULL,NULL,NULL,3,NULL,?1,0)",[i64::MAX]).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
    d.delete(&inputs).unwrap();
    assert!(d.refresh_consolidation(&inputs).is_err());
    assert!(d.commit().is_err());
    assert_eq!(status(&c).unwrap().4, i64::MAX);
    assert_eq!(
        c.query_row("SELECT count(*) FROM stage1_outputs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn forged_change_cannot_delete_an_unrelated_job_and_aborts_earlier_restore() {
    let (h, c) = fixture();
    let lease = Lease::acquire(h.path()).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    let inputs = d.capture(Table::Stage1, &["a".into()]).unwrap().unwrap();
    let change = d.refresh_consolidation(&inputs).unwrap().unwrap();
    d.delete(&inputs).unwrap();
    d.commit().unwrap();
    let raw = serde_json::to_string(&change)
        .unwrap()
        .replace("memory_consolidate_global", "memory_stage1");
    let bad: ConsolidationChange = serde_json::from_str(&raw).unwrap();
    let mut d = Database::open(&lease, Kind::Memories).unwrap().unwrap();
    d.restore(&inputs).unwrap();
    assert!(d.restore_consolidation(&bad).is_err());
    assert!(d.commit().is_err());
    assert_eq!(
        c.query_row("SELECT count(*) FROM stage1_outputs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(status(&c).is_some());
}
