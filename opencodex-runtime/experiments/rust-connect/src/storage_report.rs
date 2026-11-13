//! Bounded, read-only profile measurement for the existing Storage page.
//! SQLite counts use disposable snapshots, never creating source WAL/SHM sidecars.
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};
const KEYS: [&str; 7] = [
    "sessions",
    "archived_sessions",
    "logs_db",
    "state_db",
    "attachments",
    "deletion_manifests",
    "other",
];
const LABELS: [&str; 7] = [
    "Active sessions",
    "Archived sessions",
    "Logs database",
    "State database",
    "Attachments",
    "Deletion manifests",
    "Other",
];
#[derive(Serialize)]
struct Entry {
    path: String,
    bytes: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    key: &'static str,
    label: &'static str,
    bytes: u64,
    file_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    newest: Option<u64>,
    largest: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<Option<u64>>,
}
fn regular(m: &Metadata) -> bool {
    if m.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
fn db_name(name: &str) -> Option<(usize, u64, bool)> {
    for (prefix, index) in [("state_", 3), ("logs_", 2)] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let (version, suffix) = rest.split_once(".sqlite")?;
            if !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit())
                && ["", "-wal", "-shm"].contains(&suffix)
            {
                return Some((index, version.parse().ok()?, suffix.is_empty()));
            }
        }
    }
    None
}
fn row_count(path: &Path, table: &str) -> Option<u64> {
    let before = fs::symlink_metadata(path).ok()?;
    // Measurement must not turn a large/corrupt log database into an unbounded job.
    if !before.is_file() || !regular(&before) || before.len() > 64 * 1024 * 1024 {
        return None;
    }
    // immutable=1 ignores a live WAL. Recover only our bounded disposable copy
    // so recently committed rows count without changing the source profile.
    let snapshot = crate::profile_snapshot::Snapshot::new(path).ok()?;
    let db =
        Connection::open_with_flags(snapshot.path(), OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    db.busy_timeout(Duration::ZERO).ok()?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA cache_size=-2048; PRAGMA mmap_size=0;").ok()?;
    let sql: String = db
        .query_row(
            "SELECT substr(sql,1,256) FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )
        .ok()?;
    if sql.to_ascii_uppercase().contains("VIRTUAL") {
        return None;
    }
    let n: i64 = match table {
        "threads" => db
            .query_row("SELECT count(*) FROM threads", [], |r| r.get(0))
            .ok()?,
        "logs" => db
            .query_row("SELECT count(*) FROM logs", [], |r| r.get(0))
            .ok()?,
        _ => return None,
    };
    let after = fs::symlink_metadata(path).ok()?;
    if !regular(&after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return None;
        }
    }
    u64::try_from(n).ok()
}
pub(crate) fn scan(home: &Path) -> Value {
    let start = Instant::now();
    let mut buckets: Vec<_> = KEYS
        .iter()
        .zip(LABELS)
        .map(|(key, label)| Bucket {
            key,
            label,
            bytes: 0,
            file_count: 0,
            oldest: None,
            newest: None,
            largest: Vec::new(),
            rows: None,
        })
        .collect();
    let valid = match fs::symlink_metadata(home) {
        Ok(m) => m.is_dir() && regular(&m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({"codexHome":home,"generatedAt":crate::auth::now(),"total":{"bytes":0,"fileCount":0},"buckets":buckets})
        }
        _ => false,
    };
    if !valid {
        return json!({"codexHome":home,"generatedAt":crate::auth::now(),"total":{"bytes":0,"fileCount":0},"buckets":[],"error":"scan_failed"});
    }
    let mut pending: Vec<(PathBuf, Option<usize>, usize)> = vec![(home.to_owned(), None, 0)];
    let mut visited = 0;
    let mut limited = false;
    let mut newest: [Option<(u64, PathBuf)>; 2] = [None, None];
    while let Some((directory, bucket, depth)) = pending.pop() {
        if start.elapsed() > Duration::from_secs(2) || visited >= 100_000 {
            limited = true;
            break;
        }
        if !fs::symlink_metadata(&directory).is_ok_and(|m| m.is_dir() && regular(&m)) {
            limited = true;
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            limited = true;
            continue;
        };
        for entry in entries {
            visited += 1;
            if visited > 100_000 || start.elapsed() > Duration::from_secs(2) {
                limited = true;
                break;
            }
            let Ok(entry) = entry else {
                limited = true;
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                limited = true;
                continue;
            };
            if !regular(&meta) {
                continue;
            }
            if meta.is_dir() {
                if depth == 0 && name == ".trash" {
                    continue;
                }
                if depth >= 64 || pending.len() >= 1024 || path.as_os_str().len() > 4096 {
                    limited = true;
                    continue;
                }
                let index = bucket.unwrap_or_else(|| {
                    KEYS.iter()
                        .position(|k| *k == name && !["logs_db", "state_db"].contains(k))
                        .unwrap_or(6)
                });
                pending.push((path, Some(index), depth + 1));
            } else if meta.is_file() {
                let index = match bucket {
                    Some(index) => index,
                    None => {
                        if let Some((index, version, main)) = db_name(&name) {
                            if main
                                && newest[index - 2]
                                    .as_ref()
                                    .is_none_or(|(old, _)| version > *old)
                            {
                                newest[index - 2] = Some((version, path.clone()));
                            }
                            index
                        } else {
                            6
                        }
                    }
                };
                let b = &mut buckets[index];
                b.bytes = b.bytes.saturating_add(meta.len());
                b.file_count += 1;
                if let Some(ms) = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .and_then(|d| u64::try_from(d.as_millis()).ok())
                {
                    b.oldest = Some(b.oldest.map_or(ms, |old| old.min(ms)));
                    b.newest = Some(b.newest.map_or(ms, |old| old.max(ms)));
                }
                let relative = path
                    .strip_prefix(home)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                b.largest.push(Entry {
                    path: relative,
                    bytes: meta.len(),
                });
                b.largest
                    .sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
                b.largest.truncate(5);
            }
        }
    }
    for (slot, item) in newest.into_iter().enumerate() {
        if buckets[slot + 2].file_count > 0 {
            buckets[slot + 2].rows = Some(item.and_then(|(_, path)| {
                row_count(&path, if slot == 0 { "logs" } else { "threads" })
            }));
        }
    }
    let bytes = buckets
        .iter()
        .map(|b| b.bytes)
        .fold(0u64, u64::saturating_add);
    let files: u64 = buckets.iter().map(|b| b.file_count).sum();
    let mut report = json!({"codexHome":home,"generatedAt":crate::auth::now(),"total":{"bytes":bytes,"fileCount":files},"buckets":buckets});
    if limited {
        report["error"] = json!("scan_incomplete");
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bucket(v: &Value, key: &str) -> Value {
        v["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["key"] == key)
            .unwrap()
            .clone()
    }
    #[test]
    fn storage_report_groups_files_and_counts_newest_immutable_database_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile with spaces # %");
        fs::create_dir(&home).unwrap();
        fs::create_dir_all(home.join("sessions/nested")).unwrap();
        fs::write(home.join("sessions/nested/chat.jsonl"), "PRIVATE_CHAT").unwrap();
        fs::create_dir(home.join("attachments")).unwrap();
        for i in 0..10 {
            fs::write(home.join(format!("attachments/{i}.txt")), vec![b'x'; i + 1]).unwrap();
        }
        fs::create_dir(home.join(".trash")).unwrap();
        fs::write(home.join(".trash/deleted"), vec![0; 10_000]).unwrap();
        for (name, table, rows) in [
            ("state_2.sqlite", "threads", 3),
            ("state_1.sqlite", "threads", 1),
            ("logs_1.sqlite", "logs", 2),
        ] {
            let db = Connection::open(home.join(name)).unwrap();
            db.execute_batch(&format!(
                "PRAGMA journal_mode=WAL; CREATE TABLE {table}(id INTEGER);"
            ))
            .unwrap();
            for i in 0..rows {
                db.execute(&format!("INSERT INTO {table} VALUES(?1)"), [i])
                    .unwrap();
            }
        }
        let snapshot = || {
            let mut entries: Vec<_> = fs::read_dir(&home)
                .unwrap()
                .map(|e| {
                    let p = e.unwrap().path();
                    let bytes = if p.is_file() {
                        fs::read(&p).unwrap()
                    } else {
                        vec![]
                    };
                    (p, bytes)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        };
        let before = snapshot();
        let report = scan(&home);
        assert_eq!(snapshot(), before);
        assert_eq!(report["error"], Value::Null);
        assert_eq!(bucket(&report, "sessions")["fileCount"], 1);
        assert_eq!(bucket(&report, "state_db")["rows"], 3);
        assert_eq!(bucket(&report, "logs_db")["rows"], 2);
        assert_eq!(bucket(&report, "attachments")["bytes"], 55);
        assert_eq!(
            bucket(&report, "attachments")["largest"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            bucket(&report, "attachments")["largest"][0]["path"],
            "attachments/9.txt"
        );
        assert_eq!(report["total"]["fileCount"], 14);
        assert!(!report.to_string().contains("PRIVATE_CHAT"));
    }
    #[test]
    fn storage_report_marks_unknown_databases_and_refuses_broken_home() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("state_999.sqlite"), "not sqlite").unwrap();
        fs::write(root.path().join("state_999.sqlite-wal"), "partial").unwrap();
        let v = scan(root.path());
        assert_eq!(bucket(&v, "state_db")["rows"], Value::Null);
        assert_eq!(bucket(&v, "state_db")["fileCount"], 2);
        assert_eq!(
            scan(&root.path().join("state_999.sqlite"))["error"],
            "scan_failed"
        );
        assert_eq!(scan(&root.path().join("missing"))["total"]["fileCount"], 0);
        assert_eq!(db_name("state_10.sqlite-shm"), Some((3, 10, false)));
        assert!(db_name("state_bad.sqlite").is_none());
    }
    #[test]
    fn storage_report_counts_committed_wal_rows_without_changing_source_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state_1.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE threads(id INTEGER); INSERT INTO threads VALUES(1),(2);").unwrap();
        let names = ["state_1.sqlite", "state_1.sqlite-wal", "state_1.sqlite-shm"];
        let before: Vec<_> = names
            .iter()
            .map(|n| fs::read(root.path().join(n)).unwrap())
            .collect();
        assert_eq!(row_count(&path, "threads"), Some(2));
        assert_eq!(
            names
                .iter()
                .map(|n| fs::read(root.path().join(n)).unwrap())
                .collect::<Vec<_>>(),
            before
        );
        db.execute_batch("INSERT INTO threads VALUES(3)").unwrap();
        assert_eq!(row_count(&path, "threads"), Some(3));
    }
    #[cfg(unix)]
    #[test]
    fn storage_report_excludes_linked_files_directories_and_databases() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private"), vec![0; 512]).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("sessions")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("private"),
            root.path().join("state_1.sqlite"),
        )
        .unwrap();
        assert_eq!(scan(root.path())["total"]["fileCount"], 0);
        assert!(row_count(&root.path().join("state_1.sqlite"), "threads").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn storage_report_reads_database_beneath_a_system_path_alias_without_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let actual = root.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let path = actual.join("state_1.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE threads(id INTEGER); INSERT INTO threads VALUES(1);")
            .unwrap();
        drop(db);
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(row_count(&alias.join("state_1.sqlite"), "threads"), Some(1));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&actual).unwrap().count(), 1);
        std::os::unix::fs::symlink(&path, actual.join("state_2.sqlite")).unwrap();
        assert_eq!(row_count(&alias.join("state_2.sqlite"), "threads"), None);
    }
}
pub(crate) enum Mutation {
    Cleanup {
        percent: f64,
        mode: String,
        digest: String,
    },
    Restore(String),
    ResumePurge(String),
    Policy(crate::storage_policy::Policy),
}
#[derive(Clone)]
pub(crate) struct Reader {
    home: Option<PathBuf>,
    cache: Arc<Mutex<Option<(Instant, Value)>>>,
    gate: Arc<tokio::sync::Semaphore>,
    waiting: Arc<tokio::sync::Semaphore>,
}
impl Reader {
    pub fn new(home: impl Into<Option<PathBuf>>) -> Self {
        Self {
            home: home.into(),
            cache: Arc::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
            waiting: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    pub async fn mutate(
        &self,
        action: Mutation,
        reservation: crate::chat::RestartReservation,
        protected: Vec<String>,
    ) -> Result<Value, crate::storage_cleanup::Failure> {
        self.blocking_mutation(reservation, move |home| match action {
            Mutation::Cleanup {
                percent,
                mode,
                digest,
            } => {
                crate::storage_cleanup::cleanup_protected(home, percent, &mode, &digest, &protected)
            }
            Mutation::Restore(id) => crate::storage_cleanup::restore(home, &id),
            Mutation::ResumePurge(id) => crate::storage_cleanup::resume_purge(home, &id),
            Mutation::Policy(policy) => crate::storage_cleanup::policy_cleanup(
                home,
                policy.selection(),
                policy.trigger.archived_bytes_over,
                policy.mode(),
                &protected,
            ),
        })
        .await
    }
    async fn blocking_mutation<G, F>(
        &self,
        reservation: G,
        action: F,
    ) -> Result<Value, crate::storage_cleanup::Failure>
    where
        G: Send + 'static,
        F: FnOnce(&std::path::Path) -> Result<Value, crate::storage_cleanup::Failure>
            + Send
            + 'static,
    {
        let permit = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| crate::storage_cleanup::Failure::from("storage_mutation_busy"))?;
        let home = self
            .home
            .clone()
            .ok_or_else(|| crate::storage_cleanup::Failure::from("codex_unavailable"))?;
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            // HTTP cancellation cannot release admission or allow a second scan
            // while an irreversible filesystem/SQLite step is still running.
            let _reservation = reservation;
            let _permit = permit;
            *cache
                .lock()
                .map_err(|_| crate::storage_cleanup::Failure::from("storage_unavailable"))? = None;
            action(&home)
        })
        .await
        .map_err(|_| crate::storage_cleanup::Failure::from("storage_worker_failed"))?
    }
    pub async fn read(&self) -> Result<Value, String> {
        let permit = self.read_permit().await?;
        let home = self
            .home
            .clone()
            .ok_or("Codex is not configured on this PC")?;
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut cache = cache.lock().map_err(|_| "Storage scan unavailable")?;
            if let Some((at, v)) = &*cache {
                if at.elapsed() < Duration::from_secs(2) {
                    return Ok(v.clone());
                }
            }
            let report = scan(&home);
            *cache = Some((Instant::now(), report.clone()));
            Ok(report)
        })
        .await
        .map_err(|_| "Storage scanner stopped".to_owned())?
    }
    pub async fn preview(&self, percent: f64) -> Result<Value, String> {
        let permit = self.read_permit().await?;
        let home = self
            .home
            .clone()
            .ok_or("Codex is not configured on this PC")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::storage_preview::preview(&home, percent).map_err(str::to_owned)
        })
        .await
        .map_err(|_| "Storage scanner stopped".to_owned())?
    }
    pub async fn trash(&self) -> Result<Value, String> {
        let permit = self.read_permit().await?;
        let home = self
            .home
            .clone()
            .ok_or("Codex is not configured on this PC")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::storage_trash::list(&home).map_err(str::to_owned)
        })
        .await
        .map_err(|_| "Storage scanner stopped".to_owned())?
    }
    async fn read_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        // The unchanged dashboard refreshes usage and quarantine together.
        // Serialize a small bounded group instead of making one refresh fail.
        let _waiting = self
            .waiting
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Storage scan is busy; retry shortly")?;
        tokio::time::timeout(Duration::from_secs(5), self.gate.clone().acquire_owned())
            .await
            .map_err(|_| "Storage scan is busy; retry shortly")?
            .map_err(|_| "Storage scanner stopped".into())
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Reservation(Arc<AtomicBool>);
    impl Drop for Reservation {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    #[tokio::test]
    async fn cancelled_http_owner_cannot_release_storage_or_chat_admission_early() {
        let root = tempfile::tempdir().unwrap();
        let reader = Reader::new(root.path().to_path_buf());
        reader.read().await.unwrap();
        assert!(reader.cache.lock().unwrap().is_some());
        let reserved = Arc::new(AtomicBool::new(true));
        let guard = Reservation(reserved.clone());
        let (started, start) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let other = reader.clone();
        let task = tokio::spawn(async move {
            other
                .blocking_mutation(guard, move |_| {
                    let _ = started.send(());
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    Err("injected_failure".into())
                })
                .await
        });
        start.await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(reserved.load(Ordering::Acquire));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reader.read())
                .await
                .is_err()
        );
        assert!(reader.cache.lock().unwrap().is_none());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while reserved.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        reader.read().await.unwrap();
    }
    #[tokio::test]
    async fn concurrent_dashboard_reads_queue_behind_one_scan_with_bounded_waiters() {
        let root = tempfile::tempdir().unwrap();
        let reader = Reader::new(root.path().to_path_buf());
        let gate = reader.gate.clone().acquire_owned().await.unwrap();
        let r = reader.clone();
        let task = tokio::spawn(async move { tokio::join!(r.read(), r.trash(), r.preview(100.0)) });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(gate);
        let (report, trash, preview) = task.await.unwrap();
        assert!(report.is_ok() && trash.is_ok() && preview.is_ok());
        assert_eq!(reader.waiting.available_permits(), 4);
    }
}
