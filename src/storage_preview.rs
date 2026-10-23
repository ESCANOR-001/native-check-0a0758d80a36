//! Advisory archive selection. Does not delete files or update the native state.
//! Cleanup must revalidate the digest, pins and restore markers under its locks.
use crate::profile_snapshot::{self, regular, Snapshot};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, &'static str>;
const FAILED: &str = "Storage preview unavailable; no files were changed";
const LIMITED: &str = "Storage preview exceeds its bounded scan; no files were changed";
struct Budget {
    start: Instant,
    entries: usize,
}
impl Budget {
    fn tick(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > 100_000 || self.start.elapsed() > Duration::from_secs(3) {
            return Err(LIMITED);
        }
        Ok(())
    }
    fn directory(&mut self, path: &Path) -> Result<Vec<PathBuf>> {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Ok(m) if m.is_dir() && regular(&m) => {}
            _ => return Err(FAILED),
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(path).map_err(|_| FAILED)? {
            self.tick()?;
            files.push(entry.map_err(|_| FAILED)?.path());
        }
        Ok(files)
    }
}
pub(crate) fn archived(path: &str) -> Option<String> {
    let name = path.strip_prefix("archived_sessions/")?;
    if name.is_empty()
        || name.len() > 1024
        || name.contains(['/', '\\', ':'])
        || name.contains("..")
        || name.chars().any(char::is_control)
    {
        return None;
    }
    let logical = path.strip_suffix(".zst").unwrap_or(path);
    logical.ends_with(".jsonl").then(|| logical.to_owned())
}
fn epoch(name: &str) -> bool {
    let parts: Vec<_> = name.split('-').collect();
    parts.len() <= 2
        && parts
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}
pub(crate) fn logical_pin(home: &Path, row: &str) -> Option<String> {
    let row = row.replace('\\', "/");
    let path = Path::new(&row);
    if !path.is_absolute() {
        return archived(&row);
    }
    // The logical .jsonl may have been compressed, so only its .zst sibling
    // exists. Resolve its parent as well as Windows drive/UNC and macOS aliases.
    let resolved = path
        .canonicalize()
        .ok()
        .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))?;
    let relative = resolved
        .strip_prefix(home)
        .ok()?
        .to_str()?
        .replace('\\', "/");
    archived(&relative)
}
fn restoring(home: &Path, budget: &mut Budget) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for path in budget.directory(&home.join(".trash"))? {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !epoch(name) {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|_| FAILED)?;
        if !regular(&meta) || !meta.is_dir() {
            return Err(FAILED);
        }
        match profile_snapshot::bytes(&path.join("native-stage.json"), 8 * 1024 * 1024) {
            Ok(raw) => {
                let value: Value = serde_json::from_slice(&raw).map_err(|_| FAILED)?;
                if value["version"] != 1
                    || !matches!(
                        value["phase"].as_str(),
                        Some("prepared" | "files_staged" | "reconciled" | "restoring" | "restored")
                    )
                {
                    return Err(FAILED);
                }
                let files = value["files"].as_array().ok_or(FAILED)?;
                if files.is_empty() || files.len() > 10_000 {
                    return Err(FAILED);
                }
                for file in files {
                    budget.tick()?;
                    out.insert(archived(file["relative"].as_str().ok_or(FAILED)?).ok_or(FAILED)?);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(FAILED),
        }
        let bytes = match profile_snapshot::bytes(&path.join("restore-pending.json"), 1024 * 1024) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(FAILED),
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| FAILED)?;
        if value["version"] != 1
            || value["filesRestored"] != true
            || !["state", "logs", "memories", "goals"]
                .iter()
                .all(|k| value["pending"][k].is_boolean())
        {
            return Err(FAILED);
        }
        for row in value["acceptedDestRels"].as_array().ok_or(FAILED)? {
            budget.tick()?;
            let path = row.as_str().ok_or(FAILED)?;
            out.insert(archived(path).ok_or(FAILED)?);
        }
    }
    Ok(out)
}
fn pinned(home: &Path, budget: &mut Budget) -> Result<BTreeSet<String>> {
    let mut newest = None;
    for path in budget.directory(home)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let version = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(version) = version {
            if newest.as_ref().is_none_or(|(n, _)| version > *n) {
                newest = Some((version, path));
            }
        }
    }
    let Some((_, path)) = newest else {
        return Ok(BTreeSet::new());
    };
    let snapshot = Snapshot::new(&path).map_err(|_| FAILED)?;
    let db = Connection::open_with_flags(snapshot.path(), OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|_| FAILED)?;
    db.busy_timeout(Duration::from_millis(100))
        .map_err(|_| FAILED)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA query_only=ON;")
        .map_err(|_| FAILED)?;
    let schema: Option<String> = db
        .query_row(
            "SELECT substr(sql,1,256) FROM sqlite_schema WHERE name='threads' AND type='table'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| FAILED)?;
    let Some(schema) = schema else {
        return Ok(BTreeSet::new());
    };
    if schema.to_ascii_uppercase().contains("VIRTUAL") {
        return Err(FAILED);
    }
    let mut cols = db
        .prepare("PRAGMA table_info(threads)")
        .map_err(|_| FAILED)?;
    let columns: Vec<String> = cols
        .query_map([], |r| r.get(1))
        .map_err(|_| FAILED)?
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| FAILED)?;
    if !columns.iter().any(|s| s == "is_pinned") {
        return Ok(BTreeSet::new());
    }
    let mut query = db
        .prepare("SELECT rollout_path FROM threads WHERE is_pinned=1 LIMIT 50001")
        .map_err(|_| FAILED)?;
    let mut out = BTreeSet::new();
    let rows = query
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| FAILED)?;
    for (i, row) in rows.enumerate() {
        budget.tick()?;
        if i >= 50_000 {
            return Err(LIMITED);
        }
        if let Some(logical) = logical_pin(home, &row.map_err(|_| FAILED)?) {
            out.insert(logical);
        }
    }
    Ok(out)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Candidate {
    pub rel_path: String,
    pub bytes: u64,
    pub mtime_ms: f64,
    pub physical_rel_paths: Vec<String>,
    #[serde(skip)]
    physical: Vec<(String, u64, f64)>,
}
pub(crate) struct Selection {
    pub percent: u64,
    pub bytes: u64,
    pub digest: String,
    pub candidates: Vec<Candidate>,
}
impl Selection {
    fn response(&self) -> Value {
        json!({"percent":self.percent,"count":self.candidates.len(),"bytes":self.bytes,"digest":self.digest,"candidates":self.candidates.iter().take(50).collect::<Vec<_>>()})
    }
}
pub(crate) fn preview(home: &Path, percent: f64) -> Result<Value> {
    Ok(select(home, percent)?.response())
}
pub(crate) fn select(home: &Path, percent: f64) -> Result<Selection> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err("invalid_percent");
    }
    let mut budget = Budget {
        start: Instant::now(),
        entries: 0,
    };
    // Resolve only system ancestors; the supplied profile itself must not be a link.
    if !fs::symlink_metadata(home).is_ok_and(|m| regular(&m) && m.is_dir()) {
        return Err(FAILED);
    }
    let home = home.canonicalize().map_err(|_| FAILED)?;
    let pins = pinned(&home, &mut budget)?;
    let pending = restoring(&home, &mut budget)?;
    let mut groups: BTreeMap<String, Candidate> = BTreeMap::new();
    for path in budget.directory(&home.join("archived_sessions"))? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = format!("archived_sessions/{name}");
        let Some(logical) = archived(&rel) else {
            continue;
        };
        let meta = fs::symlink_metadata(&path).map_err(|_| FAILED)?;
        if !regular(&meta) || !meta.is_file() {
            continue;
        }
        let mtime = meta
            .modified()
            .map_err(|_| FAILED)?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| FAILED)?
            .as_secs_f64()
            * 1000.0;
        let c = groups.entry(logical.clone()).or_insert(Candidate {
            rel_path: logical,
            bytes: 0,
            mtime_ms: mtime,
            physical_rel_paths: vec![],
            physical: vec![],
        });
        c.bytes = c
            .bytes
            .checked_add(meta.len())
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or(LIMITED)?;
        c.mtime_ms = c.mtime_ms.min(mtime);
        c.physical_rel_paths.push(rel.clone());
        c.physical.push((rel, meta.len(), mtime));
    }
    let mut all: Vec<_> = groups
        .into_values()
        .filter(|c| !pins.contains(&c.rel_path))
        .collect();
    for c in &mut all {
        c.physical_rel_paths.sort();
        c.physical.sort_by(|a, b| a.0.cmp(&b.0));
    }
    all.sort_by(|a, b| {
        a.mtime_ms
            .total_cmp(&b.mtime_ms)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    let percent = percent.floor() as u64;
    let count = if percent == 0 || all.is_empty() {
        0
    } else {
        (all.len() * percent as usize / 100).max(1)
    };
    let selected: Vec<_> = all
        .into_iter()
        .filter(|c| !pending.contains(&c.rel_path))
        .take(count)
        .collect();
    let mut lines: Vec<_> = selected
        .iter()
        .map(|c| {
            let parts: Vec<_> = c
                .physical
                .iter()
                .map(|(p, b, m)| format!("{p}|{b}|{}", *m as u64))
                .collect();
            format!(
                "{}|{}|{}|{}",
                c.rel_path,
                c.bytes,
                c.mtime_ms as u64,
                parts.join(",")
            )
        })
        .collect();
    lines.sort();
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{percent}\n{}", lines.join("\n")))
    );
    let bytes = selected
        .iter()
        .try_fold(0u64, |sum, c| sum.checked_add(c.bytes))
        .ok_or(LIMITED)?;
    Ok(Selection {
        percent,
        bytes,
        digest,
        candidates: selected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    fn archive(root: &Path, name: &str, n: usize, seconds: u64) {
        let dir = root.join("archived_sessions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, vec![b'x'; n]).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
    }
    fn pending(root: &Path, path: &str) {
        fs::create_dir_all(root.join(".trash/100-1")).unwrap();
        fs::write(
            root.join(".trash/100-1/restore-pending.json"),
            json!({
                "version":1,"filesRestored":true,"acceptedDestRels":[path],
                "pending":{"state":true,"logs":false,"memories":false,"goals":false}
            })
            .to_string(),
        )
        .unwrap();
    }
    #[test]
    fn preview_groups_compressed_files_and_excludes_active_history_without_writes() {
        let root = tempfile::tempdir().unwrap();
        archive(root.path(), "old.jsonl", 10, 1);
        archive(root.path(), "old.jsonl.zst", 4, 2);
        archive(root.path(), "new.jsonl", 8, 3);
        fs::create_dir(root.path().join("sessions")).unwrap();
        fs::write(root.path().join("sessions/active.jsonl"), "ACTIVE_PRIVATE").unwrap();
        let p = preview(root.path(), 50.9).unwrap();
        assert_eq!(p["percent"], 50);
        assert_eq!(p["count"], 1);
        assert_eq!(p["bytes"], 14);
        assert_eq!(p["candidates"][0]["mtimeMs"], 1000.0);
        assert_eq!(
            p["candidates"][0]["physicalRelPaths"],
            json!([
                "archived_sessions/old.jsonl",
                "archived_sessions/old.jsonl.zst"
            ])
        );
        assert!(!p.to_string().contains(root.path().to_str().unwrap()));
        assert!(!p.to_string().contains("ACTIVE_PRIVATE"));
        assert_eq!(
            fs::read(root.path().join("archived_sessions/old.jsonl")).unwrap(),
            vec![b'x'; 10]
        );
        assert_eq!(preview(root.path(), 0.0).unwrap()["count"], 0);
        assert_eq!(preview(root.path(), 100.0).unwrap()["count"], 2);
        let digest = p["digest"].clone();
        archive(root.path(), "old.jsonl", 11, 1);
        assert_ne!(preview(root.path(), 50.0).unwrap()["digest"], digest);
        for n in [-1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert!(preview(root.path(), n).is_err());
        }
    }
    #[test]
    fn preview_reads_pins_from_live_wal_and_backfills_pending_restore() {
        let root = tempfile::tempdir().unwrap();
        for (i, n) in ["pin.jsonl", "restoring.jsonl", "safe.jsonl", "last.jsonl"]
            .iter()
            .enumerate()
        {
            archive(root.path(), n, i + 1, i as u64 + 1);
        }
        let state = root.path().join("state_9.sqlite");
        let db = Connection::open(&state).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE threads(rollout_path TEXT,is_pinned INTEGER);").unwrap();
        db.execute(
            "INSERT INTO threads VALUES(?1,1)",
            [root
                .path()
                .join("archived_sessions/pin.jsonl")
                .to_str()
                .unwrap()],
        )
        .unwrap();
        pending(root.path(), "archived_sessions/restoring.jsonl");
        let paths = [
            &state,
            &state.with_file_name("state_9.sqlite-wal"),
            &state.with_file_name("state_9.sqlite-shm"),
        ];
        let before: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
        let p = preview(root.path(), 50.0).unwrap();
        let after: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
        assert_eq!(
            before, after,
            "Read-only preview must not alter the source DB/WAL/SHM"
        );
        assert_eq!(p["count"], 1);
        assert_eq!(
            p["candidates"][0]["relPath"],
            "archived_sessions/safe.jsonl"
        );
        assert_eq!(preview(root.path(), 100.0).unwrap()["count"], 2);
        db.execute_batch("UPDATE threads SET is_pinned=0").unwrap();
        assert_eq!(preview(root.path(), 50.0).unwrap()["count"], 2);
    }
    #[test]
    fn pinned_paths_normalize_native_separators_and_missing_compressed_logical_file() {
        let root = tempfile::tempdir().unwrap();
        archive(root.path(), "pin.jsonl.zst", 2, 1);
        archive(root.path(), "safe.jsonl", 3, 2);
        let db = Connection::open(root.path().join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(rollout_path TEXT,is_pinned INTEGER)")
            .unwrap();
        let logical = root.path().join("archived_sessions/pin.jsonl");
        for path in [
            logical.to_str().unwrap().to_owned(),
            logical
                .with_extension("jsonl.zst")
                .to_str()
                .unwrap()
                .to_owned(),
            "archived_sessions/pin.jsonl".to_owned(),
            "archived_sessions\\pin.jsonl.zst".to_owned(),
        ] {
            db.execute("DELETE FROM threads", []).unwrap();
            db.execute("INSERT INTO threads VALUES(?1,1)", [path])
                .unwrap();
            let result = preview(root.path(), 100.0).unwrap();
            assert_eq!(result["count"], 1);
            assert_eq!(
                result["candidates"][0]["relPath"],
                "archived_sessions/safe.jsonl"
            );
        }
    }
    #[test]
    fn preview_fails_closed_on_unknown_pin_or_restore_state_and_limits_wire_rows() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..60 {
            archive(root.path(), &format!("{i:03}.jsonl"), 1, i + 1);
        }
        let p = preview(root.path(), 100.0).unwrap();
        assert_eq!(p["count"], 60);
        assert_eq!(p["bytes"], 60);
        assert_eq!(p["candidates"].as_array().unwrap().len(), 50);
        fs::write(root.path().join("state_1.sqlite"), "CORRUPT").unwrap();
        assert!(preview(root.path(), 100.0).is_err());
        assert!(!root.path().join("state_1.sqlite-wal").exists());
        assert!(!root.path().join("state_1.sqlite-shm").exists());
        fs::remove_file(root.path().join("state_1.sqlite")).unwrap();
        pending(root.path(), "archived_sessions/001.jsonl");
        fs::write(root.path().join(".trash/100-1/restore-pending.json"), "{}").unwrap();
        assert!(preview(root.path(), 100.0).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn preview_never_follows_archive_or_state_links_but_accepts_system_ancestor_alias() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        fs::create_dir(&home).unwrap();
        archive(&home, "safe.jsonl", 1, 1);
        fs::write(outside.path().join("secret"), "PRIVATE").unwrap();
        symlink(
            outside.path().join("secret"),
            home.join("archived_sessions/linked.jsonl"),
        )
        .unwrap();
        assert_eq!(preview(&home, 100.0).unwrap()["count"], 1);
        symlink(root.path(), outside.path().join("alias")).unwrap();
        assert_eq!(
            preview(&outside.path().join("alias/profile"), 100.0).unwrap()["count"],
            1
        );
        symlink(outside.path().join("secret"), home.join("state_1.sqlite")).unwrap();
        assert!(preview(&home, 100.0).is_err());
        fs::remove_file(home.join("state_1.sqlite")).unwrap();
        fs::remove_dir_all(home.join("archived_sessions")).unwrap();
        symlink(outside.path(), home.join("archived_sessions")).unwrap();
        assert!(preview(&home, 100.0).is_err());
    }
    #[test]
    fn internal_selection_retains_all_rows_while_wire_response_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..80 {
            archive(root.path(), &format!("{i:03}.jsonl"), 1, i + 1);
        }
        let selected = select(root.path(), 100.0).unwrap();
        assert_eq!(selected.candidates.len(), 80);
        assert_eq!(
            selected.response()["candidates"].as_array().unwrap().len(),
            50
        );
        assert_eq!(selected.response()["count"], 80);
    }
    #[test]
    fn native_journals_exclude_partial_cleanup_and_restore_files_until_finished() {
        let root = tempfile::tempdir().unwrap();
        archive(root.path(), "a.jsonl", 1, 1);
        archive(root.path(), "b.jsonl", 2, 2);
        let lease = crate::storage_stage::Lease::acquire(root.path()).unwrap();
        let mut stage = lease
            .prepare(&["archived_sessions/a.jsonl".into()])
            .unwrap();
        assert_eq!(preview(root.path(), 100.0).unwrap()["count"], 1);
        stage.stage_files().unwrap();
        stage.restore_files().unwrap();
        assert_eq!(preview(root.path(), 100.0).unwrap()["count"], 1);
        stage.finish_restore().unwrap();
        assert_eq!(preview(root.path(), 100.0).unwrap()["count"], 2);
        let stage = lease
            .prepare(&["archived_sessions/a.jsonl".into()])
            .unwrap();
        fs::write(stage.directory().join("native-stage.json"), "{}").unwrap();
        assert!(preview(root.path(), 100.0).is_err());
    }
}
