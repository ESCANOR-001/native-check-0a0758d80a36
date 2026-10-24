//! Bounded private bridge storage. Quotas never delete conversations or user
//! files. A reserve permits final error/Stop receipts after admission closes.
use crate::{storage_disk::available, store::Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::Path};

const MIB: u64 = 1024 * 1024;
const HISTORY_RESERVE: u64 = 64 * MIB;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub history_bytes: u64,
    pub asset_bytes: u64,
    pub asset_files: u64,
    pub free_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            history_bytes: 1024 * MIB,
            asset_bytes: 2048 * MIB,
            asset_files: 65536,
            free_bytes: 256 * MIB,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<(), String> {
        if !(MIB..=64 * 1024 * MIB).contains(&self.history_bytes)
            || !(1..=64 * 1024 * MIB).contains(&self.asset_bytes)
            || !(1..=1_000_000).contains(&self.asset_files)
            || self.free_bytes > 64 * 1024 * MIB
        {
            return Err("Invalid native storage limits".into());
        }
        Ok(())
    }
}
fn integer(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|_| rusqlite::Error::InvalidQuery)
}
fn failure() -> String {
    "Cannot verify private storage capacity; no new task was started".into()
}

impl Store {
    pub(crate) fn init_storage(&mut self) -> Result<(), String> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS storage_assets(path TEXT PRIMARY KEY, bytes INTEGER NOT NULL CHECK(bytes>=0));
        CREATE TABLE IF NOT EXISTS storage_totals(id INTEGER PRIMARY KEY CHECK(id=1),bytes INTEGER NOT NULL,files INTEGER NOT NULL);
        INSERT OR IGNORE INTO storage_totals VALUES(1,0,0);
        CREATE TRIGGER IF NOT EXISTS storage_asset_add AFTER INSERT ON storage_assets BEGIN UPDATE storage_totals SET bytes=bytes+new.bytes,files=files+1 WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS storage_asset_delete AFTER DELETE ON storage_assets BEGIN UPDATE storage_totals SET bytes=bytes-old.bytes,files=files-1 WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS storage_asset_update AFTER UPDATE OF bytes ON storage_assets BEGIN UPDATE storage_totals SET bytes=bytes+new.bytes-old.bytes WHERE id=1; END;").map_err(|_| failure())?;
        // Recover files written before a crash/receipt. Stream two fixed levels,
        // not an in-memory file list. This runs once before serving any clients.
        let tx = self.db.transaction().map_err(|_| failure())?;
        tx.execute("DELETE FROM storage_assets", [])
            .map_err(|_| failure())?;
        let mut entries = 0usize;
        for kind in ["uploads", "tool-images"] {
            let root = self.directory.join(kind);
            match fs::symlink_metadata(&root) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(meta) if meta.is_dir() => {}
                _ => return Err("Private asset directory is unavailable or linked".into()),
            }
            for bucket in fs::read_dir(&root).map_err(|_| failure())? {
                let bucket = bucket.map_err(|_| failure())?;
                entries += 1;
                if entries > 1_100_000 {
                    return Err("Private storage index exceeds its entry budget".into());
                }
                if !bucket.file_type().map_err(|_| failure())?.is_dir() {
                    return Err("Unexpected file or link in private asset storage".into());
                }
                for file in fs::read_dir(bucket.path()).map_err(|_| failure())? {
                    let file = file.map_err(|_| failure())?;
                    entries += 1;
                    if entries > 1_100_000 {
                        return Err("Private storage index exceeds its entry budget".into());
                    }
                    let meta = fs::symlink_metadata(file.path()).map_err(|_| failure())?;
                    if !meta.is_file() {
                        return Err("Unexpected directory or link in private asset storage".into());
                    }
                    tx.execute(
                        "INSERT INTO storage_assets VALUES(?1,?2)",
                        params![
                            file.path().to_string_lossy(),
                            i64::try_from(meta.len()).map_err(|_| failure())?
                        ],
                    )
                    .map_err(|_| failure())?;
                }
            }
        }
        crate::migration_assets::index(&tx, &self.directory)?;
        tx.commit().map_err(|_| failure())?;
        self.configure_storage(Limits::default())
    }
    pub fn configure_storage(&mut self, limits: Limits) -> Result<(), String> {
        limits.validate()?;
        let size: u64 = self
            .db
            .query_row("PRAGMA page_size", [], |r| integer(r, 0))
            .map_err(|_| failure())?;
        let pages = (limits.history_bytes + HISTORY_RESERVE).div_ceil(size);
        self.db
            .query_row(&format!("PRAGMA max_page_count={pages}"), [], |r| {
                integer(r, 0)
            })
            .map_err(|_| failure())?;
        self.storage = limits;
        Ok(())
    }
    fn history_bytes(&self) -> Result<u64, String> {
        let (pages, free, size): (u64,u64,u64) = self.db.query_row(
            "SELECT (SELECT page_count FROM pragma_page_count),(SELECT freelist_count FROM pragma_freelist_count),(SELECT page_size FROM pragma_page_size)",[],
            |r| Ok((integer(r, 0)?,integer(r, 1)?,integer(r, 2)?))).map_err(|_| failure())?;
        Ok(pages.saturating_sub(free).saturating_mul(size))
    }
    pub fn storage_status(&self) -> Result<Value, String> {
        let (bytes, files): (u64, u64) = self
            .db
            .query_row(
                "SELECT bytes,files FROM storage_totals WHERE id=1",
                [],
                |r| Ok((integer(r, 0)?, integer(r, 1)?)),
            )
            .map_err(|_| failure())?;
        let history = self.history_bytes()?;
        let free = available(&self.directory)?;
        Ok(
            json!({"historyBytes":history,"assetBytes":bytes,"assetFiles":files,"availableBytes":free,"limits":self.storage,
            "historyFull":history>=self.storage.history_bytes,"assetsFull":bytes>=self.storage.asset_bytes || files>=self.storage.asset_files,"lowDiskSpace":free<self.storage.free_bytes,
            "scope":"Private bridge history and copied assets; project files and CLI profiles are separate."}),
        )
    }
    pub(crate) fn check_history(&self, extra: u64) -> Result<(), String> {
        if self.history_bytes()?.saturating_add(extra) >= self.storage.history_bytes {
            return Err("Not enough space within the bridge history storage limit. Existing history is kept. Increase the storage limit before continuing.".into());
        }
        self.check_free(extra)
    }
    fn check_free(&self, extra: u64) -> Result<(), String> {
        if available(&self.directory)? < self.storage.free_bytes.saturating_add(extra) {
            return Err("The PC is low on disk space. Existing history is kept. Free some space before continuing.".into());
        }
        Ok(())
    }
    pub(crate) fn check_assets(&self, bytes: u64, files: u64) -> Result<(), String> {
        let (used, count): (u64, u64) = self
            .db
            .query_row(
                "SELECT bytes,files FROM storage_totals WHERE id=1",
                [],
                |r| Ok((integer(r, 0)?, integer(r, 1)?)),
            )
            .map_err(|_| failure())?;
        if used.saturating_add(bytes) > self.storage.asset_bytes
            || count.saturating_add(files) > self.storage.asset_files
        {
            return Err("The bridge attachment storage limit was reached. Saved images and history are kept. Increase the storage limit before adding more.".into());
        }
        self.check_history(bytes.min(MIB))?;
        self.check_free(bytes)
    }
    pub(crate) fn record_asset(&self, path: &Path) -> Result<(), String> {
        let bytes = fs::symlink_metadata(path).map_err(|_| failure())?;
        if !bytes.is_file() {
            return Err("Private asset is not a regular file".into());
        }
        self.db.execute("INSERT INTO storage_assets VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET bytes=excluded.bytes",params![path.to_string_lossy(),i64::try_from(bytes.len()).map_err(|_| failure())?]).map_err(|_|failure())?;
        Ok(())
    }
    pub(crate) fn forget_asset(&self, path: &Path) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM storage_assets WHERE path=?1",
            [path.to_string_lossy()],
        )?;
        Ok(())
    }
    pub(crate) fn preflight_uploads(&self, rows: &Value) -> Result<(), String> {
        let (bytes, count) = crate::attachments::preflight(rows)?;
        if count > 0 {
            self.check_assets(bytes, count)?;
        }
        Ok(())
    }
    pub fn stage_uploads(&self, rows: Value) -> Result<Vec<Value>, String> {
        let mut bytes = 0u64;
        for row in rows.as_array().ok_or("Attachments must be an array")? {
            let data = row["dataUrl"]
                .as_str()
                .and_then(|v| v.split_once(";base64,"))
                .ok_or("Attachment data is missing")?
                .1;
            // Decoded size before allocation; actual base64 validation stays in stage.
            bytes = bytes.saturating_add(
                ((data.len() as u64).div_ceil(4) * 3).saturating_sub(
                    data.bytes()
                        .rev()
                        .take_while(|b| *b == b'=')
                        .take(2)
                        .count() as u64,
                ),
            );
        }
        let count = rows.as_array().unwrap().len() as u64;
        if count == 0 {
            return Ok(vec![]);
        }
        self.check_assets(bytes, count)?;
        let staged = crate::attachments::stage(&self.directory.join("uploads"), rows)?;
        for row in &staged {
            if let Err(e) = self.record_asset(Path::new(row["path"].as_str().unwrap())) {
                self.discard_uploads(&staged);
                return Err(e);
            }
        }
        Ok(staged)
    }
    pub(crate) fn discard_uploads(&self, rows: &[Value]) {
        crate::attachments::discard(rows);
        for row in rows {
            if let Some(path) = row["path"].as_str().map(Path::new) {
                if !path.exists() {
                    let _ = self.forget_asset(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    fn upload(bytes: &[u8]) -> Value {
        json!([{"type":"file","name":"sample.txt","dataUrl":format!("data:text/plain;base64,{}",STANDARD.encode(bytes))}])
    }
    #[cfg(unix)]
    #[test]
    fn changed_private_storage_directories_are_rejected_without_writing_elsewhere() {
        use sha2::Digest;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        symlink(outside.path(), dir.path().join("uploads")).unwrap();
        assert!(store.stage_uploads(upload(b"keep private")).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_file(dir.path().join("uploads")).unwrap();
        symlink(outside.path(), dir.path().join("tool-images")).unwrap();
        let content = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")});
        assert!(store.capture_tool_images("test", &content).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_file(dir.path().join("tool-images")).unwrap();
        fs::create_dir(dir.path().join("tool-images")).unwrap();
        let bucket = dir
            .path()
            .join("tool-images")
            .join(format!("{:x}", sha2::Sha256::digest(b"test")));
        symlink(outside.path(), bucket).unwrap();
        assert!(store.capture_tool_images("test", &content).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
    #[test]
    fn asset_limits_survive_restart_and_recover_unindexed_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .configure_storage(Limits {
                asset_bytes: 4,
                asset_files: 2,
                ..Default::default()
            })
            .unwrap();
        let accepted = store.stage_uploads(upload(b"abcd")).unwrap();
        assert_eq!(store.storage_status().unwrap()["assetBytes"], 4);
        assert!(store
            .stage_uploads(upload(b"x"))
            .unwrap_err()
            .contains("attachment storage limit"));
        assert_eq!(
            fs::read(accepted[0]["path"].as_str().unwrap()).unwrap(),
            b"abcd"
        );
        // Simulate a crash after file creation but before quota indexing.
        let folder = dir
            .path()
            .join("uploads")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("unindexed"), b"one").unwrap();
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        store
            .configure_storage(Limits {
                asset_bytes: 8,
                asset_files: 2,
                ..Default::default()
            })
            .unwrap();
        let status = store.storage_status().unwrap();
        assert_eq!(status["assetBytes"], 7);
        assert_eq!(status["assetFiles"], 2);
        assert!(store.stage_uploads(upload(b"x")).is_err());
        store.discard_uploads(&accepted);
        assert_eq!(store.storage_status().unwrap()["assetBytes"], 3);
        store.stage_uploads(upload(b"12345")).unwrap();
        assert!(folder.join("unindexed").exists());
    }
    #[test]
    fn invalid_upload_and_failed_dispatch_release_their_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .configure_storage(Limits {
                asset_bytes: 20,
                asset_files: 2,
                ..Default::default()
            })
            .unwrap();
        let mut invalid = upload(b"x");
        invalid
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"file","name":"bad","dataUrl":"data:text/plain;base64,!!!!"}));
        assert!(store.stage_uploads(invalid).is_err());
        assert_eq!(store.storage_status().unwrap()["assetFiles"], 0);
        let staged = store
            .stage_uploads(upload(b"12345678901234567890"))
            .unwrap();
        store.discard_uploads(&staged);
        assert_eq!(store.storage_status().unwrap()["assetFiles"], 0);
        assert_eq!(store.storage_status().unwrap()["assetBytes"], 0);
        store
            .stage_uploads(upload(b"12345678901234567890"))
            .unwrap();
    }
    #[test]
    fn tool_images_share_upload_budget_and_existing_preview_stays_available() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .configure_storage(Limits {
                asset_bytes: 14,
                asset_files: 1,
                ..Default::default()
            })
            .unwrap();
        let image = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(b"\x89PNG\r\n\x1a\nimage!")});
        let original = store.capture_tool_images("chat", &image).unwrap();
        assert_eq!(store.storage_status().unwrap()["assetBytes"], 14);
        assert_eq!(store.capture_tool_images("chat", &image).unwrap(), original);
        assert!(store.stage_uploads(upload(b"x")).is_err());
        assert!(store
            .capture_tool_images("another-chat", &image)
            .unwrap_err()
            .contains("attachment storage limit"));
        assert!(store.owns_tool_image("chat", Path::new(&original[0])));
        assert!(Path::new(&original[0]).is_file());
    }
    #[test]
    fn history_full_blocks_new_work_without_erasing_history_or_final_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        store
            .configure_storage(Limits {
                history_bytes: MIB,
                ..Default::default()
            })
            .unwrap();
        let mut state = json!({"agent":"claude","thread":{"id":"chat","messages":[{"id":"old","text":"x".repeat(1100*1024)}],"activities":[],"proposedPlans":[]}});
        store.save_chat(&state, None).unwrap();
        assert!(store
            .check_history(0)
            .unwrap_err()
            .contains("history storage limit"));
        assert_eq!(
            store
                .history_row("chat", "messages", "old")
                .unwrap()
                .unwrap()["text"]
                .as_str()
                .unwrap()
                .len(),
            1100 * 1024
        );
        state["thread"]["messages"] = json!([]);
        state["thread"]["session"] = json!({"status":"error","lastError":"storage full"});
        store
            .save_chat(&state, Some(("final-status", "digest")))
            .unwrap();
        assert_eq!(
            store.command_digest("final-status").unwrap().as_deref(),
            Some("digest")
        );
        let mut child = state.clone();
        child["thread"]["id"] = json!("child");
        assert!(store
            .save_branch(&child, "fork", "digest", &json!({}), "chat")
            .is_err());
        assert!(store.load_chat("child").is_err());
        store
            .configure_storage(Limits {
                history_bytes: 8 * MIB,
                ..Default::default()
            })
            .unwrap();
        store.check_history(MIB).unwrap();
        assert!(store
            .history_row("chat", "messages", "old")
            .unwrap()
            .is_some());
    }
    #[test]
    fn low_disk_space_is_not_confused_with_history_or_asset_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert!(available(dir.path()).unwrap() > 0);
        store.storage.free_bytes = u64::MAX; // inject an impossible free-space floor
        assert!(store.check_history(0).unwrap_err().contains("disk space"));
        assert!(store
            .stage_uploads(upload(b"x"))
            .unwrap_err()
            .contains("disk space"));
        assert_eq!(store.storage_status().unwrap()["assetBytes"], 0);
    }
}
