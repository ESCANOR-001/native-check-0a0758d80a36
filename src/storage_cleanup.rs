//! Recoverable quarantine transactions. The HTTP service must reserve an idle
//! profile before invoking these blocking operations; no startup cleanup runs here.
use crate::{
    storage_preview,
    storage_rows::{Binding, ConsolidationChange, Database, Kind, Rows, Table},
    storage_stage::{Lease, Stage},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path};
const MAX_BACKUP: usize = 16 * 1024 * 1024;
type Result<T> = std::result::Result<T, &'static str>;
#[derive(Debug, Serialize)]
pub struct Failure {
    pub error: &'static str,
    #[serde(rename = "trashDir", skip_serializing_if = "Option::is_none")]
    pub trash_dir: Option<String>,
}
impl From<&'static str> for Failure {
    fn from(error: &'static str) -> Self {
        Self {
            error,
            trash_dir: None,
        }
    }
}
fn fs_error(_: std::io::Error) -> &'static str {
    "fs_failed"
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    binding: Binding,
    rows: Vec<Rows>,
    consolidation: Option<ConsolidationChange>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    version: u32,
    id: String,
    files: Vec<String>,
    sections: Vec<Section>,
}
struct Locked<'a> {
    db: Database<'a>,
    section: Section,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Point {
    FilesStaged,
    DatabaseCommitted(usize),
    RowsRestored,
}
fn acquire(home: &Path) -> Result<Lease> {
    Lease::acquire(home).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            "storage_mutation_busy"
        } else {
            "fs_failed"
        }
    })
}
fn lock(lease: &Lease) -> Result<Vec<Database<'_>>> {
    let mut out = Vec::new();
    for kind in [Kind::State, Kind::Logs, Kind::Memories, Kind::Goals] {
        if let Some(db) = Database::open(lease, kind)? {
            out.push(db);
        }
    }
    Ok(out)
}
fn snapshot<'a>(mut databases: Vec<Database<'a>>, paths: &[String]) -> Result<Vec<Locked<'a>>> {
    let ids = match databases
        .iter()
        .find(|db| db.binding().is_ok_and(|b| b.kind == Kind::State))
    {
        Some(db) => db.cleanup_ids(paths)?,
        None => vec![],
    };
    let mut out = Vec::new();
    let mut budget = 0;
    for mut db in databases.drain(..) {
        let binding = db.binding()?;
        let tables: &[Table] = match binding.kind {
            Kind::State => &[Table::Threads, Table::DynamicTools, Table::SpawnEdges],
            Kind::Logs => &[Table::Logs],
            Kind::Memories => &[Table::Stage1, Table::Jobs],
            Kind::Goals => &[Table::Goals, Table::Deferrals],
        };
        let mut rows = Vec::new();
        let mut consolidation = None;
        for table in tables {
            if let Some(saved) = db.capture(*table, &ids)? {
                if *table == Table::Stage1 {
                    consolidation = db.refresh_consolidation(&saved)?;
                }
                budget += serde_json::to_vec(&saved)
                    .map_err(|_| "db_reconcile_failed")?
                    .len();
                if budget > MAX_BACKUP {
                    return Err("storage_backup_too_large");
                }
                rows.push(saved);
            } else if !ids.is_empty()
                && matches!(
                    table,
                    Table::Threads | Table::Logs | Table::Stage1 | Table::Goals
                )
            {
                return Err("storage_schema_not_supported");
            }
        }
        // Delete dependents before their parents. All changes are still inside
        // transactions; dropping any handle rolls them back.
        for saved in rows.iter().rev() {
            db.delete(saved)?;
        }
        out.push(Locked {
            db,
            section: Section {
                binding,
                rows,
                consolidation,
            },
        });
    }
    Ok(out)
}
/// An explicit caller-selected quarantine; never invoked automatically.
pub fn quarantine(home: &Path, percent: f64, digest: &str) -> std::result::Result<Value, Failure> {
    quarantine_inner(home, percent, digest, &|_| Ok(()))
}
fn quarantine_inner(
    home: &Path,
    percent: f64,
    digest: &str,
    hook: &dyn Fn(Point) -> Result<()>,
) -> std::result::Result<Value, Failure> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid_digest".into());
    }
    let lease = acquire(home)?;
    let databases = lock(&lease)?;
    let selected = storage_preview::select(lease.home(), percent)?;
    if !selected.digest.eq_ignore_ascii_case(digest) {
        return Err("stale_preview".into());
    }
    let logical: Vec<_> = selected
        .candidates
        .iter()
        .map(|c| c.rel_path.clone())
        .collect();
    if logical.is_empty() {
        return Ok(
            json!({"ok":true,"mode":"quarantine","percent":selected.percent,"count":0,"bytes":0,"removedPaths":[]}),
        );
    }
    let files: Vec<_> = selected
        .candidates
        .iter()
        .flat_map(|c| c.physical_rel_paths.clone())
        .collect();
    let locked = snapshot(databases, &logical)?;
    let mut stage = lease.prepare(&files).map_err(fs_error)?;
    let id = stage.id().to_owned();
    let mut databases = Vec::new();
    let mut sections = Vec::new();
    for entry in locked {
        databases.push(entry.db);
        sections.push(entry.section);
    }
    let backup = Backup {
        version: 1,
        id: id.clone(),
        files,
        sections,
    };
    let raw = serde_json::to_vec(&backup).map_err(|_| Failure::from("db_reconcile_failed"))?;
    let manifest=serde_json::to_vec(&json!({"nativeVersion":1,"quarantinedAt":crate::auth::now(),"mode":"quarantine","percent":selected.percent,"digest":selected.digest,"entries":selected.candidates})).map_err(|_|Failure::from("db_reconcile_failed"))?;
    let records = (|| {
        if raw.len() > MAX_BACKUP {
            return Err("storage_backup_too_large");
        }
        stage
            .write_record("native-rows.json", &raw)
            .map_err(fs_error)?;
        stage
            .write_record("manifest.json", &manifest)
            .map_err(fs_error)
    })();
    if let Err(error) = records {
        drop(databases);
        let restored = (|| {
            stage.restore_files()?;
            stage.rows_restored()?;
            stage.finish_restore()
        })();
        return Err(Failure {
            error,
            trash_dir: restored.err().map(|_| id),
        });
    }
    let operation = (|| {
        stage.stage_files().map_err(fs_error)?;
        hook(Point::FilesStaged)?;
        // State last: a committed satellite deletion always retains the thread
        // row until its recovery backup is durable and all satellites are done.
        if !databases.is_empty() && backup.sections[0].binding.kind == Kind::State {
            databases.rotate_left(1);
        }
        for (i, db) in databases.drain(..).enumerate() {
            db.commit()?;
            hook(Point::DatabaseCommitted(i))?;
        }
        stage.reconciled().map_err(fs_error)
    })();
    drop(databases);
    if let Err(error) = operation {
        drop(stage);
        let restored = restore_locked(&lease, &id, &|_| Ok(()));
        return Err(Failure {
            error,
            trash_dir: restored.err().map(|_| id),
        });
    }
    Ok(
        json!({"ok":true,"mode":"quarantine","percent":selected.percent,"count":logical.len(),"bytes":selected.bytes,"removedPaths":logical,"trashDir":id}),
    )
}
/// Restore files and metadata without replacing newer files, rows or memory jobs.
pub fn restore(home: &Path, id: &str) -> std::result::Result<Value, Failure> {
    let lease = acquire(home)?;
    restore_locked(&lease, id, &|_| Ok(())).map_err(|error| Failure {
        error,
        trash_dir: Some(id.to_owned()),
    })
}
fn restore_locked(lease: &Lease, id: &str, hook: &dyn Fn(Point) -> Result<()>) -> Result<Value> {
    let mut stage = lease.resume(id).map_err(fs_error)?;
    let files = stage.relatives();
    let bytes = stage.bytes();
    let restored: Vec<_> = files
        .iter()
        .map(|p| p.strip_suffix(".zst").unwrap_or(p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let response = json!({"ok":true,"trashDir":id,"count":restored.len(),"bytes":bytes,"restoredPaths":restored});
    if stage.restoration_complete() {
        stage.finish_restore().map_err(fs_error)?;
        return Ok(response);
    }
    let raw = match stage.read_record("native-rows.json") {
        Ok(raw) => raw,
        Err(_) if stage.uncommitted() => {
            // Prepared is never persisted after any database commit. A crash
            // during the initial record write can therefore roll back files
            // without trusting a partial row backup.
            stage.restore_files().map_err(fs_error)?;
            stage.rows_restored().map_err(fs_error)?;
            stage.finish_restore().map_err(fs_error)?;
            return Ok(response);
        }
        Err(_) => return Err("db_reconcile_failed"),
    };
    let backup: Backup = serde_json::from_slice(&raw).map_err(|_| "db_reconcile_failed")?;
    validate_backup(&stage, &backup)?;
    let mut databases = Vec::new();
    for section in &backup.sections {
        let db = Database::open(lease, section.binding.kind)?.ok_or("db_reconcile_failed")?;
        if db.binding()? != section.binding {
            return Err("storage_database_changed");
        }
        for rows in &section.rows {
            db.check_restore(rows)?;
        }
        databases.push(db);
    }
    stage.restore_files().map_err(fs_error)?;
    for (mut db, section) in databases.into_iter().zip(&backup.sections) {
        for rows in &section.rows {
            db.restore(rows)?;
        }
        if let Some(change) = &section.consolidation {
            db.restore_consolidation(change)?;
        }
        db.commit()?;
    }
    stage.rows_restored().map_err(fs_error)?;
    hook(Point::RowsRestored)?;
    stage.finish_restore().map_err(fs_error)?;
    Ok(response)
}
fn validate_backup(stage: &Stage<'_>, backup: &Backup) -> Result<()> {
    if backup.version != 1
        || backup.id != stage.id()
        || backup.files != stage.relatives()
        || backup.sections.len() > 4
    {
        return Err("db_reconcile_failed");
    }
    let mut kinds = BTreeSet::new();
    for section in &backup.sections {
        if !kinds.insert(section.binding.kind as u8) {
            return Err("db_reconcile_failed");
        }
        let expected: &[Table] = match section.binding.kind {
            Kind::State => &[Table::Threads, Table::DynamicTools, Table::SpawnEdges],
            Kind::Logs => &[Table::Logs],
            Kind::Memories => &[Table::Stage1, Table::Jobs],
            Kind::Goals => &[Table::Goals, Table::Deferrals],
        };
        let mut last = None;
        for rows in &section.rows {
            let index = expected
                .iter()
                .position(|t| *t == rows.table)
                .ok_or("db_reconcile_failed")?;
            if last.is_some_and(|n| index <= n) {
                return Err("db_reconcile_failed");
            }
            last = Some(index);
        }
        if section.consolidation.is_some() && section.binding.kind != Kind::Memories {
            return Err("db_reconcile_failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        fs::create_dir(home.join("archived_sessions")).unwrap();
        fs::write(home.join("archived_sessions/a.jsonl"), "A\n").unwrap();
        fs::write(home.join("archived_sessions/a.jsonl.zst"), [0, 255, 2]).unwrap();
        fs::write(home.join("archived_sessions/b.jsonl"), "B\n").unwrap();
        let state = Connection::open(home.join("state_1.sqlite")).unwrap();
        state.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,archived INTEGER,is_pinned INTEGER,body TEXT);INSERT INTO threads VALUES('a','archived_sessions/a.jsonl',1,0,'A'),('b','archived_sessions/b.jsonl',1,0,'B'),('live','sessions/live.jsonl',0,0,'LIVE');CREATE TABLE thread_dynamic_tools(thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE,position INTEGER,body TEXT,PRIMARY KEY(thread_id,position));INSERT INTO thread_dynamic_tools VALUES('a',0,'TOOL');CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,PRIMARY KEY(parent_thread_id,child_thread_id));INSERT INTO thread_spawn_edges VALUES('a','b');").unwrap();
        let logs = Connection::open(home.join("logs_1.sqlite")).unwrap();
        logs.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,thread_id TEXT,message TEXT);INSERT INTO logs VALUES(1,'a','LOG'),(2,'live','KEEP');").unwrap();
        let memories = Connection::open(home.join("memories_1.sqlite")).unwrap();
        memories.execute_batch("CREATE TABLE stage1_outputs(thread_id TEXT PRIMARY KEY,selected_for_phase2 INTEGER,body TEXT);INSERT INTO stage1_outputs VALUES('a',1,'MEMORY');CREATE TABLE jobs(kind TEXT,job_key TEXT,status TEXT NOT NULL,worker_id TEXT,ownership_token TEXT,started_at INTEGER,finished_at INTEGER,lease_until INTEGER,retry_at INTEGER,retry_remaining INTEGER NOT NULL,last_error TEXT,input_watermark INTEGER,last_success_watermark INTEGER,PRIMARY KEY(kind,job_key));INSERT INTO jobs VALUES('memory_stage1','a','done',NULL,NULL,NULL,NULL,NULL,NULL,3,NULL,1,1);").unwrap();
        let goals = Connection::open(home.join("goals_1.sqlite")).unwrap();
        goals.execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,body TEXT);INSERT INTO thread_goals VALUES('a','GOAL');CREATE TABLE thread_goal_continuation_deferrals(thread_id TEXT REFERENCES thread_goals(thread_id) ON DELETE CASCADE,seq INTEGER,PRIMARY KEY(thread_id,seq));INSERT INTO thread_goal_continuation_deferrals VALUES('a',1);").unwrap();
        root
    }
    fn digest(home: &Path) -> String {
        storage_preview::select(home, 100.0).unwrap().digest
    }
    fn count(home: &Path, db: &str, table: &str) -> i64 {
        Connection::open(home.join(db))
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn restored(home: &Path) {
        assert_eq!(
            fs::read(home.join("archived_sessions/a.jsonl")).unwrap(),
            b"A\n"
        );
        assert_eq!(
            fs::read(home.join("archived_sessions/a.jsonl.zst")).unwrap(),
            [0, 255, 2]
        );
        assert_eq!(
            fs::read(home.join("archived_sessions/b.jsonl")).unwrap(),
            b"B\n"
        );
        for (db, table, n) in [
            ("state_1.sqlite", "threads", 3),
            ("state_1.sqlite", "thread_dynamic_tools", 1),
            ("state_1.sqlite", "thread_spawn_edges", 1),
            ("logs_1.sqlite", "logs", 2),
            ("memories_1.sqlite", "stage1_outputs", 1),
            ("memories_1.sqlite", "jobs", 1),
            ("goals_1.sqlite", "thread_goals", 1),
            ("goals_1.sqlite", "thread_goal_continuation_deferrals", 1),
        ] {
            assert_eq!(count(home, db, table), n, "{db}:{table}");
        }
    }
    #[test]
    fn quarantine_and_restore_keep_all_files_and_database_rows() {
        let h = fixture();
        let before = fs::metadata(h.path().join("archived_sessions/a.jsonl"))
            .unwrap()
            .modified()
            .unwrap();
        let result = quarantine(h.path(), 100.0, &digest(h.path())).unwrap();
        let id = result["trashDir"].as_str().unwrap();
        assert_eq!(result["count"], 2);
        assert_eq!(result["bytes"], 7);
        assert_eq!(count(h.path(), "state_1.sqlite", "threads"), 1);
        assert_eq!(count(h.path(), "logs_1.sqlite", "logs"), 1);
        assert_eq!(count(h.path(), "memories_1.sqlite", "stage1_outputs"), 0);
        assert_eq!(count(h.path(), "memories_1.sqlite", "jobs"), 1);
        assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        let trash = crate::storage_trash::list(h.path()).unwrap();
        assert_eq!(trash["entries"][0]["id"], id);
        assert_eq!(trash["entries"][0]["fileCount"], 3);
        assert_eq!(restore(h.path(), id).unwrap()["count"], 2);
        restored(h.path());
        assert_eq!(
            fs::metadata(h.path().join("archived_sessions/a.jsonl"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert!(!h.path().join(id).exists());
    }
    #[test]
    fn every_commit_failure_rolls_back_files_and_all_already_committed_databases() {
        for stop in [
            Point::FilesStaged,
            Point::DatabaseCommitted(0),
            Point::DatabaseCommitted(1),
            Point::DatabaseCommitted(2),
            Point::DatabaseCommitted(3),
        ] {
            let h = fixture();
            let error = quarantine_inner(h.path(), 100.0, &digest(h.path()), &|p| {
                if p == stop {
                    Err("injected_failure")
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.error, "injected_failure");
            assert!(error.trash_dir.is_none());
            restored(h.path());
            assert_eq!(
                crate::storage_trash::list(h.path()).unwrap()["entries"],
                json!([])
            );
        }
    }
    #[test]
    fn stale_preview_pins_external_forks_and_busy_database_do_not_move_files() {
        let h = fixture();
        let stale = digest(h.path());
        fs::write(h.path().join("archived_sessions/b.jsonl"), "CHANGED").unwrap();
        assert_eq!(
            quarantine(h.path(), 100.0, &stale).unwrap_err().error,
            "stale_preview"
        );
        fs::write(h.path().join("archived_sessions/b.jsonl"), "B\n").unwrap();
        let state = Connection::open(h.path().join("state_1.sqlite")).unwrap();
        let stale = digest(h.path());
        state
            .execute("UPDATE threads SET is_pinned=1 WHERE id='a'", [])
            .unwrap();
        assert_eq!(
            quarantine(h.path(), 100.0, &stale).unwrap_err().error,
            "stale_preview"
        );
        state.execute("UPDATE threads SET is_pinned=0", []).unwrap();
        state
            .execute("INSERT INTO thread_spawn_edges VALUES('live','a')", [])
            .unwrap();
        assert_eq!(
            quarantine(h.path(), 100.0, &digest(h.path()))
                .unwrap_err()
                .error,
            "referenced_history"
        );
        state
            .execute(
                "DELETE FROM thread_spawn_edges WHERE parent_thread_id='live'",
                [],
            )
            .unwrap();
        let current = digest(h.path());
        state.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            quarantine(h.path(), 100.0, &current).unwrap_err().error,
            "codex_busy"
        );
        state.execute_batch("ROLLBACK").unwrap();
        restored(h.path());
        assert!(!h.path().join(".trash").exists());
    }
    #[test]
    fn restore_keeps_conflicting_files_rows_and_upgraded_database_untouched() {
        let h = fixture();
        let result = quarantine(h.path(), 100.0, &digest(h.path())).unwrap();
        let id = result["trashDir"].as_str().unwrap();
        fs::write(h.path().join("archived_sessions/a.jsonl"), "USER FILE").unwrap();
        assert!(restore(h.path(), id).is_err());
        assert_eq!(
            fs::read(h.path().join("archived_sessions/a.jsonl")).unwrap(),
            b"USER FILE"
        );
        fs::remove_file(h.path().join("archived_sessions/a.jsonl")).unwrap();
        let state = Connection::open(h.path().join("state_1.sqlite")).unwrap();
        state
            .execute(
                "INSERT INTO threads VALUES('a','archived_sessions/a.jsonl',1,0,'USER ROW')",
                [],
            )
            .unwrap();
        assert_eq!(
            restore(h.path(), id).unwrap_err().error,
            "storage_restore_conflict"
        );
        assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        state
            .execute("DELETE FROM threads WHERE id='a'", [])
            .unwrap();
        drop(state);
        fs::copy(
            h.path().join("state_1.sqlite"),
            h.path().join("state_2.sqlite"),
        )
        .unwrap();
        let upgraded = fs::read(h.path().join("state_2.sqlite")).unwrap();
        assert_eq!(
            restore(h.path(), id).unwrap_err().error,
            "storage_database_changed"
        );
        assert_eq!(fs::read(h.path().join("state_2.sqlite")).unwrap(), upgraded);
        fs::remove_file(h.path().join("state_2.sqlite")).unwrap();
        restore(h.path(), id).unwrap();
        restored(h.path());
    }
    #[test]
    fn completed_restore_survives_metadata_cleanup_interruption() {
        let h = fixture();
        let result = quarantine(h.path(), 100.0, &digest(h.path())).unwrap();
        let id = result["trashDir"].as_str().unwrap();
        let lease = acquire(h.path()).unwrap();
        assert_eq!(
            restore_locked(&lease, id, &|p| if p == Point::RowsRestored {
                Err("injected_failure")
            } else {
                Ok(())
            }),
            Err("injected_failure")
        );
        drop(lease);
        restored(h.path());
        fs::remove_file(h.path().join(id).join("native-rows.json")).unwrap();
        restore(h.path(), id).unwrap();
        assert!(!h.path().join(id).exists());
        restored(h.path());
    }
    #[test]
    fn malformed_backup_refuses_restore_without_discarding_quarantine() {
        let h = fixture();
        let result = quarantine(h.path(), 100.0, &digest(h.path())).unwrap();
        let id = result["trashDir"].as_str().unwrap();
        let file = h.path().join(id).join("native-rows.json");
        let bytes = fs::read(&file).unwrap();
        fs::write(&file, "{}").unwrap();
        assert_eq!(
            restore(h.path(), id).unwrap_err().error,
            "db_reconcile_failed"
        );
        assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        assert!(h.path().join(id).join("a.jsonl").exists());
        fs::write(&file, bytes).unwrap();
        restore(h.path(), id).unwrap();
        restored(h.path());
    }
    #[test]
    fn storage_crash_child() {
        let Ok(path) = std::env::var("REMODEX_STORAGE_CRASH_HOME") else {
            return;
        };
        let home = Path::new(&path);
        let point: usize = std::env::var("REMODEX_STORAGE_CRASH_POINT")
            .unwrap()
            .parse()
            .unwrap();
        let _ = quarantine_inner(home, 100.0, &digest(home), &|p| {
            if p == Point::DatabaseCommitted(point) {
                std::process::exit(91)
            }
            Ok(())
        });
        panic!("child did not stop at the requested boundary");
    }
    #[test]
    fn abrupt_child_exit_recovers_each_database_commit_boundary() {
        for point in 0..4 {
            let h = fixture();
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "storage_cleanup::tests::storage_crash_child",
                "--test-threads=1",
            ])
            .env("REMODEX_STORAGE_CRASH_HOME", h.path())
            .env("REMODEX_STORAGE_CRASH_POINT", point.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000);
            }
            assert_eq!(cmd.status().unwrap().code(), Some(91));
            let entries = crate::storage_trash::list(h.path()).unwrap();
            let id = entries["entries"][0]["id"].as_str().unwrap();
            restore(h.path(), id).unwrap();
            restored(h.path());
        }
    }
    #[test]
    fn interrupted_backup_creation_remains_visible_and_can_be_rolled_back() {
        let h = fixture();
        let lease = acquire(h.path()).unwrap();
        let stage = lease
            .prepare(&["archived_sessions/a.jsonl".into()])
            .unwrap();
        let id = stage.id().to_owned();
        fs::write(
            stage
                .directory()
                .join(format!("record-{}.tmp", uuid::Uuid::new_v4())),
            b"PARTIAL PRIVATE BACKUP",
        )
        .unwrap();
        drop(stage);
        drop(lease);
        let listed = crate::storage_trash::list(h.path()).unwrap();
        assert_eq!(listed["entries"][0]["id"], id);
        assert_eq!(restore(h.path(), &id).unwrap()["bytes"], 2);
        restored(h.path());
        assert!(!h.path().join(id).exists());
    }
    #[test]
    fn valid_but_incomplete_backup_is_rejected_by_the_durable_digest() {
        let h = fixture();
        let result = quarantine(h.path(), 100.0, &digest(h.path())).unwrap();
        let id = result["trashDir"].as_str().unwrap();
        let path = h.path().join(id).join("native-rows.json");
        let raw = fs::read(&path).unwrap();
        let mut value: Value = serde_json::from_slice(&raw).unwrap();
        value["sections"] = json!([]);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            restore(h.path(), id).unwrap_err().error,
            "db_reconcile_failed"
        );
        assert_eq!(count(h.path(), "state_1.sqlite", "threads"), 1);
        assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        fs::write(path, raw).unwrap();
        restore(h.path(), id).unwrap();
        restored(h.path());
    }
}
