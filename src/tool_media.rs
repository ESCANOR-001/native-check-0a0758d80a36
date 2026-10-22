//! Tool images stay on disk. Snapshots contain exact, chat-scoped references;
//! bytes are served only through an expiring capability requested by the UI.
use crate::{attachments::image_mime, store::Store};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGES: usize = 8;

impl Store {
    pub(crate) fn codex_tool_images(
        &mut self,
        chat: &str,
        workspace: &Path,
        item: &Value,
    ) -> Result<Vec<String>, String> {
        match item["type"].as_str() {
            Some("mcpToolCall" | "dynamicToolCall") => {
                let content = if item["result"].is_object() {
                    &item["result"]
                } else {
                    &item["contentItems"]
                };
                self.capture_tool_images(chat, content)
            }
            Some("imageView") => Ok(item["path"]
                .as_str()
                .and_then(|p| crate::workspace::resolve(workspace, p).ok())
                .filter(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .into_iter()
                .collect()),
            _ => Ok(vec![]),
        }
    }
    pub fn capture_tool_images(
        &mut self,
        chat: &str,
        content: &Value,
    ) -> Result<Vec<String>, String> {
        if self.db.execute_batch("CREATE TABLE IF NOT EXISTS tool_images (chat TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(chat,path));").is_err() {
            return Err("Cannot save tool image references".into());
        }
        let mut paths = Vec::new();
        let mut remaining = MAX_BYTES;
        let mut encoded_remaining = MAX_BYTES;
        self.visit_image(
            chat,
            content,
            0,
            &mut remaining,
            &mut encoded_remaining,
            &mut paths,
        )?;
        Ok(paths)
    }

    fn visit_image(
        &self,
        chat: &str,
        value: &Value,
        depth: usize,
        remaining: &mut usize,
        encoded_remaining: &mut usize,
        paths: &mut Vec<String>,
    ) -> Result<(), String> {
        if depth > 12 || *remaining == 0 || paths.len() >= MAX_IMAGES {
            return Ok(());
        }
        if let Some(rows) = value.as_array() {
            for row in rows.iter().take(100) {
                self.visit_image(chat, row, depth + 1, remaining, encoded_remaining, paths)?;
            }
            return Ok(());
        }
        if !value.is_object() {
            return Ok(());
        }
        // Some CLI versions wrap an MCP content result as JSON text (sometimes
        // twice). Extract only embedded image blocks from that exact envelope.
        // Plain text, remote URLs and arbitrary local paths remain inert. The
        // cumulative parsing budget also bounds nested wrapper amplification.
        if value["type"] == "text" {
            if let Some(text) = value["text"]
                .as_str()
                .filter(|s| s.len() <= *encoded_remaining && s.trim_start().starts_with('{'))
            {
                *encoded_remaining -= text.len();
                if let Ok(decoded) = serde_json::from_str::<Value>(text) {
                    if decoded["content"].is_array() {
                        self.visit_image(
                            chat,
                            &decoded["content"],
                            depth + 1,
                            remaining,
                            encoded_remaining,
                            paths,
                        )?;
                    }
                }
            }
            return Ok(());
        }
        let source = &value["source"];
        let (mime, data) = if value["type"] == "image" && source["type"] == "base64" {
            (source["media_type"].as_str(), source["data"].as_str())
        } else if value["type"] == "image" {
            (value["mimeType"].as_str(), value["data"].as_str())
        } else {
            (value["mimeType"].as_str(), value["blob"].as_str())
        };
        if let (Some(mime), Some(data)) = (mime, data) {
            let ext = match mime {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => return Ok(()),
            };
            if data.len() > remaining.div_ceil(3) * 4 {
                return Ok(());
            }
            let Ok(bytes) = STANDARD.decode(data) else {
                return Ok(());
            };
            if bytes.is_empty() || bytes.len() > *remaining || image_mime(&bytes) != Some(mime) {
                return Ok(());
            }
            let dir = self
                .directory
                .join("tool-images")
                .join(format!("{:x}", Sha256::digest(chat.as_bytes())));
            let path = dir.join(format!("{:x}.{ext}", Sha256::digest(&bytes)));
            crate::private_directory::prepare(dir.parent().ok_or("Invalid image directory")?)
                .and_then(|_| crate::private_directory::prepare(&dir))
                .map_err(|_| "Cannot prepare tool image storage")?;
            if !matches_bytes(&path, &bytes).map_err(|_| "Cannot verify saved tool image")? {
                // Account for peak temporary storage. Repair only from the same
                // authoritative native bytes, never by deleting history.
                self.check_assets(bytes.len() as u64, 1)?;
                save(&dir, &path, |file| file.write_all(&bytes))
                    .map_err(|_| "Cannot save tool image")?;
            }
            self.record_asset(&path)?;
            let path = path.to_string_lossy().into_owned();
            if self
                .db
                .execute(
                    "INSERT OR IGNORE INTO tool_images(chat,path) VALUES(?1,?2)",
                    params![chat, path],
                )
                .is_err()
            {
                return Err("Cannot save tool image reference".into());
            }
            if !paths.contains(&path) {
                paths.push(path);
            }
            *remaining -= bytes.len();
            return Ok(());
        }
        // No remote fetch, arbitrary filesystem path, or tool-input traversal.
        for key in ["content", "resource"] {
            self.visit_image(
                chat,
                &value[key],
                depth + 1,
                remaining,
                encoded_remaining,
                paths,
            )?;
        }
        Ok(())
    }

    pub fn owns_tool_image(&self, chat: &str, path: &Path) -> bool {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_images WHERE chat=?1 AND path=?2)",
                params![chat, path.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap_or(false)
    }
}

fn matches_bytes(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(meta) if !meta.is_file() => return Err(io::ErrorKind::InvalidData.into()),
        Ok(meta) if meta.len() != bytes.len() as u64 => return Ok(false),
        _ => {}
    }
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    for chunk in bytes.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..chunk.len()])?;
        if chunk != &buffer[..chunk.len()] {
            return Ok(false);
        }
    }
    Ok(true)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn save(
    dir: &Path,
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    crate::private_directory::prepare(dir.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    crate::private_directory::prepare(dir)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let temporary = Temporary(dir.join(format!(".image-{}.tmp", uuid::Uuid::new_v4())));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary.0)?;
    write(&mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary.0, path)?;
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn nested_native_json_text_images_are_bounded_without_interpreting_paths_or_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nwrapped";
        let content = json!({"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(bytes)},
            {"type":"image","source":{"type":"url","url":"http://127.0.0.1/private"}},
            {"type":"image","path":"/private/not-to-read.png"}]});
        let wrap = |v: &Value| json!({"content":[{"type":"text","text":v.to_string()}]});
        let paths = store
            .capture_tool_images("chat", &wrap(&wrap(&content)))
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(fs::read(&paths[0]).unwrap(), bytes);
        assert!(store
            .capture_tool_images(
                "chat",
                &json!({"type":"text","text":json!({"arbitrary":content}).to_string()})
            )
            .unwrap()
            .is_empty());
        let mut deep = content.clone();
        for _ in 0..8 {
            deep = wrap(&deep);
        }
        assert!(store.capture_tool_images("chat", &deep).unwrap().is_empty());
        let big = json!({"type":"text","text":format!("{{{}", " ".repeat(MAX_BYTES))});
        assert!(store.capture_tool_images("chat", &big).unwrap().is_empty());
    }
    #[test]
    fn failed_image_writes_leave_original_intact_and_clean_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("images").join("bucket");
        let path = dir.join("image.png");
        save(&dir, &path, |f| f.write_all(b"original")).unwrap();
        assert!(save(&dir, &path, |f| {
            f.write_all(b"partial")?;
            Err(io::ErrorKind::WriteZero.into())
        })
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let missing = dir.join("new.png");
        assert!(save(&dir, &missing, |f| {
            f.write_all(b"partial")?;
            Err(io::ErrorKind::WriteZero.into())
        })
        .is_err());
        assert!(!missing.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }
    #[test]
    fn native_replay_repairs_partial_or_same_length_corrupt_images_without_duplicate_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        let content = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(bytes)});
        let paths = store.capture_tool_images("chat", &content).unwrap();
        for damaged in [b"partial".to_vec(), vec![0; bytes.len()]] {
            fs::write(&paths[0], damaged).unwrap();
            assert_eq!(store.capture_tool_images("chat", &content).unwrap(), paths);
            assert_eq!(fs::read(&paths[0]).unwrap(), bytes);
            assert_eq!(store.storage_status().unwrap()["assetFiles"], 1);
        }
    }
    #[test]
    fn images_are_bounded_deduplicated_and_scoped_without_fetching_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let data = STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture");
        let block =
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}});
        let paths = store
            .capture_tool_images("../one", &json!([block, block]))
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert!(store.owns_tool_image("../one", Path::new(&paths[0])));
        assert!(!store.owns_tool_image("two", Path::new(&paths[0])));
        assert_eq!(store.capture_tool_images("../one", &block).unwrap(), paths);
        assert!(store
            .capture_tool_images(
                "one",
                &json!({"type":"image","source":{"type":"url","url":"http://localhost/secret"}})
            )
            .unwrap()
            .is_empty());
        assert!(store
            .capture_tool_images(
                "one",
                &json!({"type":"image","mimeType":"image/png","data":"c2VjcmV0"})
            )
            .unwrap()
            .is_empty());
    }
}
