//! Authenticated, resumable phone transfers. Only opaque references cross the
//! prompt boundary; client-supplied paths are never used. Receipts survive restart.
use crate::{private_directory, store::Store};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

pub const MAX_FILE: u64 = 100 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 250 * 1024 * 1024;
pub const CHUNK: usize = 512 * 1024;
const TTL: i64 = 24 * 60 * 60 * 1000;

impl Store {
    pub(crate) fn init_phone_uploads(&self) -> Result<(), String> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS phone_uploads(owner TEXT NOT NULL,id TEXT NOT NULL,meta TEXT NOT NULL,path TEXT NOT NULL,expires INTEGER NOT NULL,ready INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(owner,id));").map_err(|e| e.to_string())
    }
    fn phone_upload_row(&self, owner: &str, id: &str) -> Result<(Value, PathBuf, bool), String> {
        let row: Option<(String, String, i64, i64)> = self
            .db
            .query_row(
                "SELECT meta,path,ready,expires FROM phone_uploads WHERE owner=?1 AND id=?2",
                params![owner, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| "Upload store unavailable")?;
        let (meta, path, ready, expires) =
            row.ok_or("Upload expired or unavailable; attach the file again")?;
        if expires < crate::auth::now() {
            return Err("Upload expired; attach the file again".into());
        }
        let path = PathBuf::from(path);
        private_directory::prepare(path.parent().ok_or("Invalid upload directory")?)
            .map_err(|_| "Private upload storage unavailable")?;
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return Err("Upload file unavailable".into());
        }
        Ok((
            serde_json::from_str(&meta).map_err(|_| "Invalid upload record")?,
            path,
            ready == 1,
        ))
    }
    pub(crate) fn phone_upload(&self, owner: &str, request: &Value) -> Result<Value, String> {
        let id = request["id"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 100
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            .ok_or("Invalid upload ID")?;
        let action = request["action"].as_str().ok_or("Upload action required")?;
        if action == "begin" {
            // Delete only unclaimed temporary transfers. Claimed chat files are separate hard links.
            let expired: Vec<(String, String, String)> = self
                .db
                .prepare("SELECT owner,id,path FROM phone_uploads WHERE expires<?1")
                .map_err(|_| "Upload store unavailable")?
                .query_map([crate::auth::now()], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(|_| "Upload store unavailable")?
                .collect::<Result<_, _>>()
                .map_err(|_| "Upload store unavailable")?;
            for (o, i, p) in expired {
                let _ = fs::remove_file(p);
                self.db
                    .execute(
                        "DELETE FROM phone_uploads WHERE owner=?1 AND id=?2",
                        params![o, i],
                    )
                    .map_err(|_| "Upload store unavailable")?;
            }
            let meta = &request["file"];
            let size = meta["sizeBytes"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= MAX_FILE)
                .ok_or("Each file must be between 1 byte and 100 MB")?;
            let name = meta["name"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 1024)
                .ok_or("File name required")?;
            let mime = meta["mimeType"]
                .as_str()
                .filter(|s| s.len() <= 100 && s.contains('/') && !s.chars().any(char::is_control))
                .ok_or("Invalid media type")?;
            let kind = meta["type"]
                .as_str()
                .filter(|s| ["image", "file"].contains(s))
                .ok_or("Invalid file type")?;
            let meta = json!({"type":kind,"name":name,"mimeType":mime,"sizeBytes":size});
            if let Ok((old, path, ready)) = self.phone_upload_row(owner, id) {
                if old != meta {
                    return Err("Upload ID already belongs to another file".into());
                }
                return Ok(
                    json!({"id":id,"offset":fs::metadata(path).map_err(|_|"Upload unavailable")?.len(),"ready":ready}),
                );
            }
            self.check_assets(size, 1)?;
            let root = self.directory.join("incoming-uploads");
            private_directory::prepare(&root).map_err(|_| "Private upload storage unavailable")?;
            let path = root.join(uuid::Uuid::new_v4().to_string());
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&path)
                .map_err(|_| "Cannot create upload file")?;
            if self
                .db
                .execute(
                    "INSERT INTO phone_uploads(owner,id,meta,path,expires) VALUES(?1,?2,?3,?4,?5)",
                    params![
                        owner,
                        id,
                        meta.to_string(),
                        path.to_string_lossy(),
                        crate::auth::now() + TTL
                    ],
                )
                .is_err()
            {
                let _ = fs::remove_file(path);
                return Err("Cannot save upload receipt".into());
            }
            return Ok(json!({"id":id,"offset":0,"ready":false}));
        }
        let (meta, path, mut ready) = self.phone_upload_row(owner, id)?;
        let size = meta["sizeBytes"].as_u64().ok_or("Invalid upload size")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).map_err(|_| "Upload file unavailable")?;
        let mut offset = file.metadata().map_err(|_| "Upload unavailable")?.len();
        match action {
            "status" => {}
            "chunk" => {
                let start = request["offset"].as_u64().ok_or("Upload offset required")?;
                let encoded = request["data"]
                    .as_str()
                    .filter(|v| v.len() <= CHUNK.div_ceil(3) * 4)
                    .ok_or("Upload chunk exceeds 512 KB")?;
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| "Invalid upload chunk")?;
                if bytes.is_empty()
                    || bytes.len() > CHUNK
                    || start.saturating_add(bytes.len() as u64) > size
                {
                    return Err("Invalid upload chunk size".into());
                }
                if start < offset && start + bytes.len() as u64 <= offset {
                    // Lost acknowledgement: accept an identical retry, never append it twice.
                    file.seek(SeekFrom::Start(start))
                        .map_err(|_| "Cannot read upload")?;
                    let mut old = vec![0; bytes.len()];
                    file.read_exact(&mut old)
                        .map_err(|_| "Cannot read upload")?;
                    if old != bytes {
                        return Err("Upload retry content differs".into());
                    }
                } else if !ready && start == offset {
                    self.check_free(bytes.len() as u64)?;
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|_| "Cannot append upload")?;
                    file.write_all(&bytes)
                        .and_then(|_| file.sync_data())
                        .map_err(|_| "Cannot save upload chunk")?;
                    offset += bytes.len() as u64;
                } else {
                    return Err("Upload offset changed; resume from the confirmed offset".into());
                }
            }
            "finish" => {
                if offset != size {
                    return Err("Upload is incomplete".into());
                }
                if !ready {
                    file.seek(SeekFrom::Start(0))
                        .map_err(|_| "Cannot verify upload")?;
                    let mut head = [0; 16];
                    let n = file.read(&mut head).map_err(|_| "Cannot verify upload")?;
                    if meta["type"] == "image"
                        && crate::attachments::image_mime(&head[..n]) != meta["mimeType"].as_str()
                    {
                        return Err(
                            "Unsupported or damaged image; attach it as a file instead".into()
                        );
                    }
                    file.seek(SeekFrom::Start(0))
                        .map_err(|_| "Cannot verify upload")?;
                    let mut hash = Sha256::new();
                    let mut buffer = [0u8; 65536];
                    loop {
                        let n = file.read(&mut buffer).map_err(|_| "Cannot verify upload")?;
                        if n == 0 {
                            break;
                        }
                        hash.update(&buffer[..n]);
                    }
                    let _digest = hash.finalize(); // Read every saved byte before publishing Ready.
                    self.db
                        .execute(
                            "UPDATE phone_uploads SET ready=1 WHERE owner=?1 AND id=?2",
                            params![owner, id],
                        )
                        .map_err(|_| "Cannot confirm upload")?;
                    ready = true;
                }
            }
            "cancel" => {
                drop(file);
                fs::remove_file(path).map_err(|_| "Cannot remove upload")?;
                self.db
                    .execute(
                        "DELETE FROM phone_uploads WHERE owner=?1 AND id=?2",
                        params![owner, id],
                    )
                    .map_err(|_| "Upload store unavailable")?;
                return Ok(json!({"cancelled":true}));
            }
            _ => return Err("Unknown upload action".into()),
        }
        Ok(json!({"id":id,"offset":offset,"ready":ready}))
    }
    pub(crate) fn preflight_phone_attachments(
        &self,
        owner: &str,
        rows: &Value,
    ) -> Result<(), String> {
        let rows = rows.as_array().ok_or("Attachments must be an array")?;
        if rows.len() > 8 {
            return Err("At most eight attachments are allowed".into());
        }
        if rows.is_empty() {
            return Ok(());
        }
        let mut bytes = 0;
        for row in rows {
            if let Some(id) = row["uploadId"].as_str() {
                let (meta, _, ready) = self.phone_upload_row(owner, id)?;
                if !ready {
                    return Err("File is still uploading".into());
                }
                bytes += meta["sizeBytes"].as_u64().ok_or("Invalid upload size")?;
            } else {
                let (size, _) = crate::attachments::preflight(&json!([row]))?;
                bytes += size;
            }
        }
        if bytes > MAX_TOTAL {
            return Err("Attachments can use up to 250 MB per message".into());
        }
        self.check_assets(bytes, rows.len() as u64)
    }
    pub(crate) fn stage_phone_attachments(
        &self,
        owner: &str,
        rows: Value,
    ) -> Result<Vec<Value>, String> {
        self.preflight_phone_attachments(owner, &rows)?;
        let mut staged = Vec::new();
        let result = (|| {
            for row in rows.as_array().unwrap() {
                if let Some(id) = row["uploadId"].as_str() {
                    let (mut meta, path, _) = self.phone_upload_row(owner, id)?;
                    let root = self.directory.join("uploads");
                    private_directory::prepare(&root)
                        .map_err(|_| "Private upload storage unavailable")?;
                    let folder = root.join(uuid::Uuid::new_v4().to_string());
                    private_directory::prepare(&folder)
                        .map_err(|_| "Private upload storage unavailable")?;
                    let name: String = meta["name"]
                        .as_str()
                        .unwrap_or("file")
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                                c
                            } else {
                                '_'
                            }
                        })
                        .take(160)
                        .collect();
                    let target = folder.join(format!("file-{name}"));
                    fs::hard_link(&path, &target)
                        .or_else(|_| fs::copy(&path, &target).map(|_| ()))
                        .map_err(|_| "Cannot attach uploaded file")?;
                    meta["path"] = json!(target);
                    staged.push(meta);
                    self.record_asset(&target)?;
                } else {
                    staged.extend(self.stage_uploads(json!([row]))?);
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.discard_uploads(&staged);
            return Err(e);
        }
        Ok(staged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn begin(store: &Store, owner: &str, id: &str, size: u64) -> Value {
        store.phone_upload(owner,&json!({"action":"begin","id":id,"file":{"name":"phone.txt","type":"file","mimeType":"text/plain","sizeBytes":size}})).unwrap()
    }
    fn chunk(
        store: &Store,
        owner: &str,
        id: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<Value, String> {
        store.phone_upload(
            owner,
            &json!({"action":"chunk","id":id,"offset":offset,"data":STANDARD.encode(bytes)}),
        )
    }
    fn finish(store: &Store, owner: &str, id: &str) -> Result<Value, String> {
        store.phone_upload(owner, &json!({"action":"finish","id":id}))
    }
    #[test]
    fn resumes_after_restart_and_lost_ack_without_duplicate_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        begin(&store, "phone", "abc", 6);
        assert_eq!(
            chunk(&store, "phone", "abc", 0, b"abc").unwrap()["offset"],
            3
        );
        assert!(finish(&store, "phone", "abc").is_err());
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(begin(&store, "phone", "abc", 6)["offset"], 3);
        assert_eq!(
            chunk(&store, "phone", "abc", 0, b"abc").unwrap()["offset"],
            3
        );
        assert!(chunk(&store, "phone", "abc", 0, b"BAD").is_err());
        chunk(&store, "phone", "abc", 3, b"def").unwrap();
        assert_eq!(finish(&store, "phone", "abc").unwrap()["ready"], true);
        let refs = json!([{"uploadId":"abc","path":"/untrusted/path","type":"file","sizeBytes":1}]);
        assert!(store
            .stage_phone_attachments("other", refs.clone())
            .is_err());
        let staged = store
            .stage_phone_attachments("phone", refs.clone())
            .unwrap();
        assert_eq!(
            fs::read(staged[0]["path"].as_str().unwrap()).unwrap(),
            b"abcdef"
        );
        assert_eq!(staged[0]["sizeBytes"], 6);
        store.discard_uploads(&staged);
        assert!(store.stage_phone_attachments("phone", refs).is_ok()); // A failed send remains retryable.
    }
    #[test]
    fn accepts_100_mb_in_bounded_chunks_and_enforces_message_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        begin(&store, "phone", "big", MAX_FILE);
        let bytes = vec![b'a'; CHUNK];
        for offset in (0..MAX_FILE).step_by(CHUNK) {
            chunk(&store, "phone", "big", offset, &bytes).unwrap();
        }
        finish(&store, "phone", "big").unwrap();
        assert!(store
            .preflight_phone_attachments("phone", &json!([{"uploadId":"big"},{"uploadId":"big"}]))
            .is_ok());
        assert!(store
            .preflight_phone_attachments(
                "phone",
                &json!([{"uploadId":"big"},{"uploadId":"big"},{"uploadId":"big"}])
            )
            .is_err());
        assert!(store.phone_upload("phone",&json!({"action":"begin","id":"over","file":{"type":"file","name":"a","mimeType":"text/plain","sizeBytes":MAX_FILE+1}})).is_err());
        assert!(store
            .preflight_phone_attachments("phone", &json!(vec![json!({"uploadId":"big"}); 9]))
            .is_err());
    }
    #[test]
    fn incomplete_and_foreign_uploads_cannot_be_claimed_or_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        begin(&store, "a", "one", 4);
        assert!(store
            .phone_upload("b", &json!({"id":"one","action":"cancel"}))
            .is_err());
        assert!(store
            .preflight_phone_attachments("a", &json!([{"uploadId":"one"}]))
            .is_err());
        assert!(chunk(&store, "a", "one", 1, b"abc").is_err());
        assert!(chunk(&store, "a", "one", 0, b"abcde").is_err());
        store
            .phone_upload("a", &json!({"id":"one","action":"cancel"}))
            .unwrap();
        assert!(finish(&store, "a", "one").is_err());
    }
    #[test]
    fn expiration_removes_only_temporary_copy_and_unknown_actions_fail() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        begin(&store, "a", "one", 3);
        chunk(&store, "a", "one", 0, b"abc").unwrap();
        finish(&store, "a", "one").unwrap();
        let staged = store
            .stage_phone_attachments("a", json!([{"uploadId":"one"}]))
            .unwrap();
        assert!(store
            .phone_upload("a", &json!({"id":"one","action":"nonsense"}))
            .is_err());
        store
            .db
            .execute("UPDATE phone_uploads SET expires=0", [])
            .unwrap();
        begin(&store, "a", "two", 1);
        assert_eq!(
            fs::read(staged[0]["path"].as_str().unwrap()).unwrap(),
            b"abc"
        );
        assert!(finish(&store, "a", "one").is_err());
    }
}
