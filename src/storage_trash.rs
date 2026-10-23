//! Bounded, read-only listing of quarantine stages for the existing dashboard.
use crate::profile_snapshot::{bytes, regular};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
const ERROR: &str = "trash_list_failed";
fn epoch(name: &str) -> bool {
    let mut parts = name.split('-');
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits) && parts.next().is_none()
}
fn manifest(raw: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(raw).ok()?;
    let entries = value["entries"].as_array()?;
    for e in entries {
        if !e.is_object()
            || e["relPath"].as_str().is_none_or(str::is_empty)
            || e["bytes"].as_u64().is_none()
            || e["mtimeMs"].as_f64().is_none_or(|n| !n.is_finite())
        {
            return None;
        }
        let paths = e["physicalRelPaths"].as_array()?;
        if paths.is_empty() || paths.iter().any(|p| p.as_str().is_none_or(str::is_empty)) {
            return None;
        }
        for k in ["threadId", "rolloutPath"] {
            if e.get(k).is_some_and(|v| !v.is_string()) {
                return None;
            }
        }
        if e.get("archived")
            .is_some_and(|v| !v.is_null() && !v.is_number())
        {
            return None;
        }
    }
    Some(value)
}
pub(crate) fn list(home: &Path) -> Result<Value, &'static str> {
    let started = Instant::now();
    let mut visited = 0usize;
    if !fs::symlink_metadata(home).is_ok_and(|m| regular(&m) && m.is_dir()) {
        return Err(ERROR);
    }
    let root = home.join(".trash");
    match fs::symlink_metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({"entries":[]})),
        Ok(m) if regular(&m) && m.is_dir() => {}
        _ => return Err(ERROR),
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&root).map_err(|_| ERROR)? {
        visited += 1;
        if visited > 50_000 || entries.len() >= 2048 || started.elapsed() > Duration::from_secs(3) {
            return Err(ERROR);
        }
        let entry = entry.map_err(|_| ERROR)?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !epoch(&name) {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|_| ERROR)?;
        if !regular(&meta) {
            return Err(ERROR);
        }
        if !meta.is_dir() {
            continue;
        }
        let raw = match bytes(&path.join("manifest.json"), 1024 * 1024) {
            Ok(raw) => Some(raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(ERROR),
        };
        // A malformed existing manifest remains visible. Never present it as
        // an empty quarantine or use a partially parsed entry list for totals.
        let manifest = raw.as_deref().and_then(manifest);
        let mut count = 0u64;
        let mut size = 0u64;
        for file in fs::read_dir(&path).map_err(|_| ERROR)? {
            visited += 1;
            if visited > 50_000 || started.elapsed() > Duration::from_secs(3) {
                return Err(ERROR);
            }
            let file = file.map_err(|_| ERROR)?;
            let filename = file.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            if !filename.ends_with(".jsonl") && !filename.ends_with(".jsonl.zst") {
                continue;
            }
            let meta = fs::symlink_metadata(file.path()).map_err(|_| ERROR)?;
            if !regular(&meta) {
                return Err(ERROR);
            }
            if !meta.is_file() {
                continue;
            }
            count += 1;
            size = size.checked_add(meta.len()).ok_or(ERROR)?;
        }
        if count == 0 {
            if let Some(list) = manifest.as_ref().and_then(|v| v["entries"].as_array()) {
                for e in list {
                    count = count
                        .checked_add(e["physicalRelPaths"].as_array().unwrap().len() as u64)
                        .ok_or(ERROR)?;
                    size = size
                        .checked_add(e["bytes"].as_u64().unwrap())
                        .ok_or(ERROR)?;
                }
            }
        }
        if count == 0 && raw.is_none() {
            continue;
        }
        if size > 9_007_199_254_740_991 {
            return Err(ERROR);
        }
        let mut row =
            json!({"id":format!(".trash/{name}"),"epoch":name,"fileCount":count,"bytes":size});
        if let Some(m) = manifest {
            if let Some(n) = m["quarantinedAt"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
            {
                row["quarantinedAt"] = json!(n);
            }
            if m["mode"] == "quarantine" || m["mode"] == "permanent" {
                row["mode"] = m["mode"].clone();
            }
        }
        entries.push(row);
    }
    let order = |v: &Value| {
        v["quarantinedAt"].as_f64().unwrap_or_else(|| {
            v["epoch"]
                .as_str()
                .unwrap()
                .split('-')
                .next()
                .unwrap()
                .parse::<f64>()
                .unwrap_or(0.0)
        })
    };
    entries.sort_by(|a, b| {
        order(b)
            .total_cmp(&order(a))
            .then_with(|| b["epoch"].as_str().cmp(&a["epoch"].as_str()))
    });
    Ok(json!({"entries":entries}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stage(home: &Path, id: &str, manifest: Value) {
        fs::create_dir_all(home.join(".trash").join(id)).unwrap();
        fs::write(
            home.join(".trash").join(id).join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();
    }
    #[test]
    fn quarantine_listing_prefers_files_preserves_bad_manifest_and_uses_complete_fallback() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        assert_eq!(list(home).unwrap(), json!({"entries":[]}));
        let good = json!({"quarantinedAt":150,"mode":"quarantine","entries":[{"relPath":"archived_sessions/one.jsonl","bytes":99,"mtimeMs":1,"physicalRelPaths":["archived_sessions/one.jsonl","archived_sessions/one.jsonl.zst"]}]});
        stage(home, "100", good.clone());
        stage(home, "101-1", good);
        fs::write(home.join(".trash/100/one.jsonl"), "123").unwrap();
        fs::write(home.join(".trash/100/one.jsonl.zst"), "45").unwrap();
        stage(
            home,
            "200",
            json!({"entries":[{"relPath":"private", "bytes":900}]}),
        );
        fs::create_dir(home.join(".trash/300")).unwrap();
        let result = list(home).unwrap();
        let rows = result["entries"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["id"], ".trash/200");
        assert_eq!(rows[0]["bytes"], 0);
        assert_eq!(rows[1]["id"], ".trash/101-1");
        assert_eq!(rows[1]["fileCount"], 2);
        assert_eq!(rows[1]["bytes"], 99);
        assert_eq!(rows[2]["bytes"], 5);
        assert_eq!(rows[2]["fileCount"], 2);
        assert!(!result.to_string().contains(home.to_str().unwrap()));
        assert!(!result.to_string().contains("private"));
        assert!(home.join(".trash/100/one.jsonl").exists());
    }
    #[test]
    fn partial_manifest_never_produces_partial_totals() {
        let root = tempfile::tempdir().unwrap();
        stage(
            root.path(),
            "1",
            json!({"entries":[{"relPath":"one","bytes":5,"mtimeMs":1,"physicalRelPaths":["one"]},{"relPath":"two","bytes":6,"mtimeMs":1,"physicalRelPaths":[null]}]}),
        );
        assert_eq!(list(root.path()).unwrap()["entries"][0]["bytes"], 0);
        for name in ["1-2-3", "", "-1", "1-", "abc"] {
            assert!(!epoch(name));
        }
    }
    #[cfg(unix)]
    #[test]
    fn quarantine_listing_rejects_linked_stages_and_manifests() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".trash")).unwrap();
        std::os::unix::fs::symlink(other.path(), root.path().join(".trash/1")).unwrap();
        assert!(list(root.path()).is_err());
        fs::remove_file(root.path().join(".trash/1")).unwrap();
        stage(root.path(), "1", json!({"entries":[]}));
        fs::remove_file(root.path().join(".trash/1/manifest.json")).unwrap();
        fs::write(other.path().join("private"), "PRIVATE").unwrap();
        std::os::unix::fs::symlink(
            other.path().join("private"),
            root.path().join(".trash/1/manifest.json"),
        )
        .unwrap();
        assert!(list(root.path()).is_err());
    }
}
