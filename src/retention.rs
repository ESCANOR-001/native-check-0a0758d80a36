//! Incremental cleanup of abandoned staging files. Conversation history, user
//! workspaces and CLI profiles are never retention targets. Live/queued/history
//! references win over age, including references outside the in-memory page.
use crate::store::Store;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const GRACE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Default)]
pub struct Scanner {
    root: Option<fs::ReadDir>,
    bucket: Option<(PathBuf, fs::ReadDir)>,
    kind: usize,
}
#[derive(Default, Debug)]
pub struct Report {
    pub removed: usize,
    pub bytes: u64,
    pub complete: bool,
}
fn directory(path: &Path) -> Option<fs::ReadDir> {
    fs::symlink_metadata(path).ok().filter(|m| m.is_dir())?;
    fs::read_dir(path).ok()
}
fn bucket_name(name: &str, kind: usize) -> bool {
    if kind == 0 {
        uuid::Uuid::parse_str(name).is_ok()
    } else {
        name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
    }
}
impl Scanner {
    /// At most 64 filesystem entries / four old files per slice. Directory
    /// iterators survive between slices instead of loading a directory tree.
    pub fn step(&mut self, store: &Store, now: SystemTime) -> rusqlite::Result<Report> {
        let mut report = Report::default();
        let started = Instant::now();
        let mut inspected = 0;
        let mut old_files = 0;
        while inspected < 64 && old_files < 4 && started.elapsed() < Duration::from_millis(100) {
            inspected += 1;
            if let Some((folder, entries)) = &mut self.bucket {
                // A replaced directory must not turn the iterator's paths into
                // traversal through a symlink on a later maintenance slice.
                if !fs::symlink_metadata(folder.parent().unwrap()).is_ok_and(|m| m.is_dir())
                    || !fs::symlink_metadata(&*folder).is_ok_and(|m| m.is_dir())
                {
                    self.bucket = None;
                    continue;
                }
                let Some(entry) = entries.next() else {
                    let _ = fs::remove_dir(&*folder); // succeeds only when empty
                    self.bucket = None;
                    continue;
                };
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if !meta.is_file()
                    || meta
                        .modified()
                        .ok()
                        .and_then(|m| now.duration_since(m).ok())
                        .is_none_or(|age| age < GRACE)
                {
                    continue;
                }
                old_files += 1;
                // JSON escaping matters on Windows and for attachment names.
                // Search on disk, never deserialize entire transcripts for GC.
                let quoted = serde_json::to_string(&path.to_string_lossy()).unwrap();
                let needle = &quoted[1..quoted.len() - 1];
                let referenced: bool = store.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM chats WHERE instr(data,?1)>0) OR EXISTS(SELECT 1 FROM chat_rows WHERE instr(data,?1)>0)",
                    [needle], |r| r.get(0))?;
                if referenced {
                    continue;
                }
                // No database row is removed if the file cannot be deleted.
                if fs::remove_file(&path).is_ok() {
                    store.forget_asset(&path)?;
                    if self.kind == 1 {
                        store.db.execute(
                            "DELETE FROM tool_images WHERE path=?1",
                            [path.to_string_lossy()],
                        )?;
                    }
                    report.removed += 1;
                    report.bytes += meta.len();
                }
                continue;
            }
            if self.kind >= 2 {
                report.complete = true;
                break;
            }
            let root = store.directory.join(if self.kind == 0 {
                "uploads"
            } else {
                "tool-images"
            });
            if self.root.is_none() {
                self.root = directory(&root);
            }
            let Some(entries) = &mut self.root else {
                self.kind += 1;
                continue;
            };
            if !fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
                self.root = None;
                self.kind += 1;
                continue;
            }
            let Some(entry) = entries.next() else {
                self.root = None;
                self.kind += 1;
                continue;
            };
            let Ok(entry) = entry else { continue };
            if !bucket_name(&entry.file_name().to_string_lossy(), self.kind) {
                continue;
            }
            let path = entry.path();
            if let Some(entries) = directory(&path) {
                self.bucket = Some((path, entries));
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn upload(s: &Store, text: &str) -> PathBuf {
        let dir = s
            .directory
            .join("uploads")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        fs::write(&path, text).unwrap();
        path
    }
    fn sweep(s: &Store, now: SystemTime) -> Report {
        let mut scan = Scanner::default();
        let mut report = Report::default();
        for _ in 0..100 {
            let r = scan.step(s, now).unwrap();
            report.removed += r.removed;
            report.bytes += r.bytes;
            if r.complete {
                report.complete = true;
                return report;
            }
        }
        panic!("cleanup did not finish")
    }
    #[test]
    fn keeps_history_and_queue_references_and_only_reaps_old_orphans() {
        let temp = tempfile::tempdir().unwrap();
        let mut s = Store::open(temp.path()).unwrap();
        s.init_chats().unwrap();
        let history = upload(&s, "history");
        let queued = upload(&s, "queue");
        let orphan = upload(&s, "orphan");
        let mut rows = vec![json!({"id":"old","text":format!("[file](<{}>)",history.display())})];
        for i in 0..100 {
            rows.push(json!({"id":format!("row-{i}"),"text":"later"}));
        }
        let state = json!({"agent":"claude","thread":{"id":"chat","messages":rows,"activities":[],"proposedPlans":[]},"queue":[{"message":{"attachments":[{"path":queued}]}}]});
        s.save_chat(&state, None).unwrap();
        assert_eq!(sweep(&s, SystemTime::now()).removed, 0);
        let result = sweep(&s, SystemTime::now() + GRACE + Duration::from_secs(1));
        assert!(result.complete);
        assert_eq!(result.removed, 1);
        assert_eq!(result.bytes, 6);
        assert!(!orphan.exists());
        assert!(history.exists());
        assert!(queued.exists());
        assert_eq!(
            s.load_chat("chat").unwrap()["thread"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            24
        );
    }
    #[test]
    fn unknown_directories_and_symlinks_are_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let mut s = Store::open(temp.path()).unwrap();
        s.init_chats().unwrap();
        let unrelated = temp.path().join("uploads/user-folder/keep.txt");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "keep").unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("keep.txt"), "keep").unwrap();
            std::os::unix::fs::symlink(
                outside.path(),
                temp.path()
                    .join("uploads")
                    .join(uuid::Uuid::new_v4().to_string()),
            )
            .unwrap();
            sweep(&s, SystemTime::now() + GRACE + Duration::from_secs(1));
            assert!(outside.path().join("keep.txt").exists());
        }
        assert!(unrelated.exists());
    }
    #[test]
    fn orphan_tool_images_are_removed_but_history_previews_stay_available() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let temp = tempfile::tempdir().unwrap();
        let mut s = Store::open(temp.path()).unwrap();
        s.init_chats().unwrap();
        let content = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")});
        let kept = s.capture_tool_images("kept", &content).unwrap()[0].clone();
        let orphan = s.capture_tool_images("orphan", &content).unwrap()[0].clone();
        let state = json!({"agent":"claude","thread":{"id":"kept","messages":[],"activities":[{"id":"tool","payload":{"imagePaths":[kept]}}],"proposedPlans":[]}});
        s.save_chat(&state, None).unwrap();
        let report = sweep(&s, SystemTime::now() + GRACE + Duration::from_secs(1));
        assert_eq!(report.removed, 1);
        assert!(Path::new(&kept).exists());
        assert!(!Path::new(&orphan).exists());
        assert!(s.owns_tool_image("kept", Path::new(&kept)));
        assert!(!s.owns_tool_image("orphan", Path::new(&orphan)));
    }
}
