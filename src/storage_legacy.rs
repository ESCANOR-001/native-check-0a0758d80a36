//! Lossless adoption of complete quarantines made by the former JS bridge.
//! Incomplete restores and untyped/lossy row backups remain untouched.
use crate::{
    profile_snapshot::bytes,
    storage_cleanup::{Backup, Section},
    storage_rows::{Database, Kind, Table},
    storage_stage::{Lease, Stage},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};
type Result<T> = std::result::Result<T, &'static str>;
const INVALID: &str = "legacy_backup_invalid";
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Saved {
    thread_ids: Vec<String>,
    threads: Option<Vec<Value>>,
    dynamic_tools: Option<Vec<Value>>,
    spawn_edges: Option<Vec<Value>>,
    logs: Option<Logs>,
    memories: Option<Memories>,
    goals: Option<Goals>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Logs {
    path: String,
    rows: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Memories {
    path: String,
    stage1: Vec<Value>,
    stage1_jobs: Vec<Value>,
    consolidate_job: Option<Value>,
    consolidate_touched: bool,
    consolidate_post_image: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Goals {
    path: String,
    goals: Vec<Value>,
    deferrals: Vec<Value>,
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or(INVALID)
}
fn logical(path: &str) -> Result<String> {
    let normalized = path.replace('\\', "/");
    let path = normalized
        .strip_prefix("archived_sessions/")
        .or_else(|| {
            normalized
                .rsplit_once("/archived_sessions/")
                .map(|(_, v)| v)
        })
        .ok_or(INVALID)?;
    if path.is_empty()
        || path.len() > 1024
        || path.contains(['/', ':'])
        || path.contains("..")
        || path.chars().any(char::is_control)
    {
        return Err(INVALID);
    }
    let path = path.strip_suffix(".zst").unwrap_or(path);
    if !path.ends_with(".jsonl") {
        return Err(INVALID);
    }
    Ok(format!("archived_sessions/{path}"))
}
fn member(value: &Value, key: &str, ids: &BTreeSet<String>) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|v| ids.contains(v))
}
fn scoped(rows: &[Value], table: Table, ids: &BTreeSet<String>) -> Result<()> {
    if rows.len() > 50_000 {
        return Err("storage_backup_too_large");
    }
    for row in rows {
        let valid = match table {
            Table::Threads => member(row, "id", ids),
            Table::SpawnEdges => {
                member(row, "parent_thread_id", ids) || member(row, "child_thread_id", ids)
            }
            Table::Jobs => {
                member(row, "job_key", ids)
                    && row.get("kind").and_then(Value::as_str) == Some("memory_stage1")
            }
            _ => member(row, "thread_id", ids),
        };
        if !valid {
            return Err(INVALID);
        }
    }
    Ok(())
}
fn section(
    lease: &Lease,
    kind: Kind,
    tables: &[(Table, &[Value])],
    ids: &BTreeSet<String>,
) -> Result<Section> {
    let db = Database::open(lease, kind)?.ok_or("storage_schema_changed")?;
    let mut rows = Vec::new();
    for (table, values) in tables {
        scoped(values, *table, ids)?;
        // An absent optional table with no rows contains nothing to restore.
        if values.is_empty() {
            continue;
        }
        rows.push(db.legacy_rows(*table, values)?);
    }
    Ok(Section {
        binding: db.binding()?,
        rows,
        consolidation: None,
    })
}
fn database_path(path: &str, prefix: &str) -> Result<()> {
    let normalized = path.replace('\\', "/");
    let name = normalized.rsplit('/').next().ok_or(INVALID)?;
    let version = name
        .strip_prefix(prefix)
        .and_then(|v| v.strip_suffix(".sqlite"))
        .ok_or(INVALID)?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Err(INVALID);
    }
    // Deliberately never open the serialized path. Only the newest validated
    // database under this already leased profile is used.
    Ok(())
}
pub(crate) fn adopt<'a>(lease: &'a Lease, id: &str) -> Result<Stage<'a>> {
    let path = lease.home().join(id);
    match fs::symlink_metadata(path.join("restore-pending.json")) {
        Ok(_) => return Err("legacy_restore_incomplete"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(INVALID),
    }
    let raw_manifest = bytes(&path.join("manifest.json"), 1024 * 1024).map_err(|_| INVALID)?;
    let manifest: Value = serde_json::from_slice(&raw_manifest).map_err(|_| INVALID)?;
    if manifest.get("nativeVersion").is_some()
        || manifest.get("purgeIncomplete").is_some()
        || manifest.get("purgedRelPaths").is_some()
    {
        return Err("legacy_restore_incomplete");
    }
    let entries = manifest
        .get("entries")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 10_000)
        .ok_or(INVALID)?;
    let mut files = Vec::new();
    let mut logical_ids = BTreeMap::new();
    let mut physical = BTreeSet::new();
    for entry in entries {
        let relative = text(entry, "relPath")?;
        if logical(relative)? != relative {
            return Err(INVALID);
        }
        let id = text(entry, "threadId")?;
        if id.is_empty()
            || id.len() > 256
            || logical_ids
                .insert(id.to_owned(), relative.to_owned())
                .is_some()
        {
            return Err(INVALID);
        }
        if logical(text(entry, "rolloutPath")?)? != relative
            || entry.get("archived").and_then(Value::as_i64) != Some(1)
        {
            return Err(INVALID);
        }
        if entry.get("bytes").and_then(Value::as_u64).is_none()
            || entry
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_none_or(|v| !v.is_finite())
        {
            return Err(INVALID);
        }
        let paths = entry
            .get("physicalRelPaths")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty() && v.len() <= 2)
            .ok_or(INVALID)?;
        for value in paths {
            let p = value.as_str().ok_or(INVALID)?;
            if !(p == relative || p == format!("{relative}.zst")) || !physical.insert(p.to_owned())
            {
                return Err(INVALID);
            }
            files.push(p.to_owned());
        }
    }
    let raw_saved = bytes(&path.join("satellite-backup.json"), 16 * 1024 * 1024)
        .map_err(|_| "legacy_backup_incomplete")?;
    let saved: Saved = serde_json::from_slice(&raw_saved).map_err(|_| INVALID)?;
    let ids: BTreeSet<_> = saved.thread_ids.iter().cloned().collect();
    if ids.len() != saved.thread_ids.len() || ids != logical_ids.keys().cloned().collect() {
        return Err(INVALID);
    }
    let threads = saved.threads.as_ref().ok_or("legacy_backup_incomplete")?;
    let mut row_ids = BTreeSet::new();
    for row in threads {
        let id = text(row, "id")?;
        if !row_ids.insert(id.to_owned())
            || logical_ids.get(id) != Some(&logical(text(row, "rollout_path")?)?)
            || row.get("archived").and_then(Value::as_i64) != Some(1)
        {
            return Err(INVALID);
        }
    }
    if row_ids != ids {
        return Err("legacy_backup_incomplete");
    }
    let mut sections = vec![section(
        lease,
        Kind::State,
        &[
            (Table::Threads, threads),
            (
                Table::DynamicTools,
                saved.dynamic_tools.as_deref().unwrap_or(&[]),
            ),
            (
                Table::SpawnEdges,
                saved.spawn_edges.as_deref().unwrap_or(&[]),
            ),
        ],
        &ids,
    )?];
    if let Some(logs) = saved.logs {
        database_path(&logs.path, "logs_")?;
        sections.push(section(
            lease,
            Kind::Logs,
            &[(Table::Logs, &logs.rows)],
            &ids,
        )?);
    }
    if let Some(memories) = saved.memories {
        database_path(&memories.path, "memories_")?;
        let mut section = section(
            lease,
            Kind::Memories,
            &[
                (Table::Stage1, &memories.stage1),
                (Table::Jobs, &memories.stage1_jobs),
            ],
            &ids,
        )?;
        if memories.consolidate_touched {
            let db = Database::open(lease, Kind::Memories)?.ok_or("storage_schema_changed")?;
            section.consolidation = Some(
                db.legacy_consolidation(
                    memories.consolidate_job.as_ref(),
                    memories
                        .consolidate_post_image
                        .as_ref()
                        .ok_or("legacy_backup_incomplete")?,
                )?,
            );
        }
        sections.push(section);
    }
    if let Some(goals) = saved.goals {
        database_path(&goals.path, "goals_")?;
        sections.push(section(
            lease,
            Kind::Goals,
            &[
                (Table::Goals, &goals.goals),
                (Table::Deferrals, &goals.deferrals),
            ],
            &ids,
        )?);
    }
    let backup = Backup {
        version: 1,
        id: id.into(),
        files: files.clone(),
        sections,
    };
    let raw = serde_json::to_vec(&backup).map_err(|_| INVALID)?;
    if raw.len() > 16 * 1024 * 1024 {
        return Err("storage_backup_too_large");
    }
    let records = BTreeMap::from([
        ("manifest.json".into(), raw_manifest),
        ("satellite-backup.json".into(), raw_saved),
        ("native-rows.json".into(), raw),
    ]);
    lease.adopt_legacy(id, &files, &records).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            "dest_exists"
        } else {
            INVALID
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_cleanup;
    use rusqlite::Connection;
    use serde_json::json;
    const ID: &str = ".trash/1700000000000";
    fn fixture() -> tempfile::TempDir {
        let h = tempfile::tempdir().unwrap();
        fs::create_dir(h.path().join("archived_sessions")).unwrap();
        fs::create_dir_all(h.path().join(ID)).unwrap();
        fs::write(h.path().join(ID).join("a.jsonl"), b"original history\n").unwrap();
        fs::write(h.path().join(ID).join("a.jsonl.zst"), [1, 0, 255, 4]).unwrap();
        let db = Connection::open(h.path().join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,archived INTEGER,body TEXT); CREATE TABLE thread_dynamic_tools(thread_id TEXT,position INTEGER,body TEXT,PRIMARY KEY(thread_id,position)); CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,PRIMARY KEY(parent_thread_id,child_thread_id)); INSERT INTO threads VALUES('live','sessions/live.jsonl',0,'unchanged');").unwrap();
        let db = Connection::open(h.path().join("logs_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,thread_id TEXT,message TEXT)")
            .unwrap();
        let db = Connection::open(h.path().join("goals_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,body TEXT); CREATE TABLE thread_goal_continuation_deferrals(thread_id TEXT PRIMARY KEY,seq INTEGER)").unwrap();
        let manifest = json!({"quarantinedAt":1700000000000u64,"mode":"quarantine","percent":100,"digest":"legacy-preview","entries":[{"relPath":"archived_sessions/a.jsonl","bytes":21,"mtimeMs":1700000000000u64,"physicalRelPaths":["archived_sessions/a.jsonl","archived_sessions/a.jsonl.zst"],"threadId":"a","rolloutPath":"archived_sessions/a.jsonl","archived":1}]});
        fs::write(
            h.path().join(ID).join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let saved = json!({"threadIds":["a"],"threads":[{"id":"a","rollout_path":"archived_sessions/a.jsonl","archived":1,"body":"original"}],"dynamicTools":[{"thread_id":"a","position":0,"body":"tool"}],"spawnEdges":[{"parent_thread_id":"a","child_thread_id":"live"}],"logs":{"path":"/old/profile/logs_1.sqlite","rows":[{"id":7,"thread_id":"a","message":"LOG"}]},"goals":{"path":"C:\\old\\profile\\goals_1.sqlite","goals":[{"thread_id":"a","body":"GOAL"}],"deferrals":[{"thread_id":"a","seq":3}]}});
        fs::write(
            h.path().join(ID).join("satellite-backup.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        h
    }
    fn edit(h: &std::path::Path, change: impl FnOnce(&mut Value)) {
        let p = h.join(ID).join("satellite-backup.json");
        let mut v = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        change(&mut v);
        fs::write(p, serde_json::to_vec(&v).unwrap()).unwrap();
    }
    fn assert_restored(h: &std::path::Path) {
        assert_eq!(
            fs::read(h.join("archived_sessions/a.jsonl")).unwrap(),
            b"original history\n"
        );
        assert_eq!(
            fs::read(h.join("archived_sessions/a.jsonl.zst")).unwrap(),
            [1, 0, 255, 4]
        );
        for (name, table, count) in [
            ("state", "threads", 2),
            ("state", "thread_dynamic_tools", 1),
            ("state", "thread_spawn_edges", 1),
            ("logs", "logs", 1),
            ("goals", "thread_goals", 1),
            ("goals", "thread_goal_continuation_deferrals", 1),
        ] {
            let db = Connection::open(h.join(format!("{name}_1.sqlite"))).unwrap();
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                count
            );
        }
        let db = Connection::open(h.join("state_1.sqlite")).unwrap();
        assert_eq!(
            db.query_row("SELECT body FROM threads WHERE id='live'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "unchanged"
        );
        assert!(!h.join(ID).exists());
    }
    #[test]
    fn complete_legacy_quarantine_restores_files_rows_and_dependents() {
        let h = fixture();
        let m = fs::metadata(h.path().join(ID).join("a.jsonl"))
            .unwrap()
            .modified()
            .unwrap();
        let result = storage_cleanup::restore(h.path(), ID).unwrap();
        assert_eq!(result["count"], 1);
        assert_restored(h.path());
        assert_eq!(
            fs::metadata(h.path().join("archived_sessions/a.jsonl"))
                .unwrap()
                .modified()
                .unwrap(),
            m
        );
    }
    #[test]
    fn adopted_journal_resumes_after_interruption_without_redecoding_legacy_rows() {
        let h = fixture();
        {
            let lease = Lease::acquire(h.path()).unwrap();
            let mut stage = adopt(&lease, ID).unwrap();
            stage.restore_files().unwrap();
        }
        storage_cleanup::restore(h.path(), ID).unwrap();
        assert_restored(h.path());
    }
    #[test]
    fn newer_destination_and_newer_rows_are_never_overwritten() {
        let h = fixture();
        fs::write(h.path().join("archived_sessions/a.jsonl"), "NEW").unwrap();
        assert_eq!(
            storage_cleanup::restore(h.path(), ID).unwrap_err().error,
            "dest_exists"
        );
        assert_eq!(
            fs::read_to_string(h.path().join("archived_sessions/a.jsonl")).unwrap(),
            "NEW"
        );
        assert!(!h.path().join(ID).join("native-stage.json").exists());
        fs::remove_file(h.path().join("archived_sessions/a.jsonl")).unwrap();
        let db = Connection::open(h.path().join("state_1.sqlite")).unwrap();
        db.execute_batch("INSERT INTO threads VALUES('a','sessions/new.jsonl',0,'NEW')")
            .unwrap();
        assert_eq!(
            storage_cleanup::restore(h.path(), ID).unwrap_err().error,
            "storage_restore_conflict"
        );
        assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        assert!(h.path().join(ID).join("satellite-backup.json").exists());
    }
    #[test]
    fn ambiguous_partial_and_lossy_backups_remain_untouched() {
        for mutation in 0..6 {
            let h = fixture();
            match mutation {
                0 => {
                    fs::write(h.path().join(ID).join("restore-pending.json"), "{}").unwrap();
                }
                1 => edit(h.path(), |v| {
                    v.as_object_mut().unwrap().remove("threads");
                }),
                2 => edit(h.path(), |v| {
                    v["dynamicTools"][0]["position"] = json!(9007199254740992u64)
                }),
                3 => edit(h.path(), |v| v["threads"][0]["body"] = json!({"0":255})),
                4 => edit(h.path(), |v| {
                    v["logs"]["rows"][0]["thread_id"] = json!("unrelated")
                }),
                _ => edit(h.path(), |v| {
                    v["threads"][0]["extra_column"] = json!("unknown")
                }),
            }
            assert!(
                storage_cleanup::restore(h.path(), ID).is_err(),
                "case {mutation}"
            );
            assert!(h.path().join(ID).join("a.jsonl").exists());
            assert!(h.path().join(ID).join("satellite-backup.json").exists());
            assert!(!h.path().join(ID).join("native-stage.json").exists());
            assert!(!h.path().join("archived_sessions/a.jsonl").exists());
        }
    }
    #[test]
    fn permanent_resume_never_adopts_or_deletes_ordinary_legacy_quarantine() {
        let h = fixture();
        assert_eq!(
            storage_cleanup::resume_purge(h.path(), ID)
                .unwrap_err()
                .error,
            "legacy_trash"
        );
        assert!(h.path().join(ID).join("a.jsonl").exists());
        assert!(!h.path().join(ID).join("native-stage.json").exists());
    }
    #[test]
    fn changed_original_metadata_is_kept_even_after_rows_restore() {
        let h = fixture();
        {
            let lease = Lease::acquire(h.path()).unwrap();
            adopt(&lease, ID).unwrap();
        }
        fs::write(
            h.path().join(ID).join("satellite-backup.json"),
            "new recovery metadata",
        )
        .unwrap();
        assert!(storage_cleanup::restore(h.path(), ID).is_err());
        assert_eq!(
            fs::read_to_string(h.path().join(ID).join("satellite-backup.json")).unwrap(),
            "new recovery metadata"
        );
        assert!(h.path().join(ID).join("native-stage.json").exists());
    }
    #[test]
    fn orphaned_import_record_can_be_reused_after_crash_before_journal_publish() {
        let h = fixture();
        {
            let lease = Lease::acquire(h.path()).unwrap();
            adopt(&lease, ID).unwrap();
        }
        fs::remove_file(h.path().join(ID).join("native-stage.json")).unwrap();
        storage_cleanup::restore(h.path(), ID).unwrap();
        assert_restored(h.path());
    }
    #[test]
    fn legacy_global_memory_job_restores_only_its_own_post_image() {
        for newer in [false, true] {
            let h = fixture();
            let db = Connection::open(h.path().join("memories_1.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE stage1_outputs(thread_id TEXT PRIMARY KEY,body TEXT);CREATE TABLE jobs(kind TEXT,job_key TEXT,status TEXT,PRIMARY KEY(kind,job_key));INSERT INTO jobs VALUES('memory_consolidate_global','global','pending')").unwrap();
            if newer {
                db.execute_batch("UPDATE jobs SET status='newer'").unwrap();
            }
            edit(
                h.path(),
                |v| v["memories"] = json!({"path":"/old/memories_1.sqlite","stage1":[{"thread_id":"a","body":"memory"}],"stage1Jobs":[{"kind":"memory_stage1","job_key":"a","status":"done"}],"consolidateJob":{"kind":"memory_consolidate_global","job_key":"global","status":"done"},"consolidateTouched":true,"consolidatePostImage":{"kind":"memory_consolidate_global","job_key":"global","status":"pending"}}),
            );
            storage_cleanup::restore(h.path(), ID).unwrap();
            assert_restored(h.path());
            assert_eq!(
                db.query_row("SELECT status FROM jobs WHERE job_key='global'", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
                if newer { "newer" } else { "done" }
            );
            assert_eq!(
                db.query_row(
                    "SELECT body FROM stage1_outputs WHERE thread_id='a'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "memory"
            );
        }
    }
}
