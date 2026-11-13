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

#[derive(Default)]
pub(crate) struct CapturedMedia {
    pub paths: Vec<String>,
    warnings: Vec<&'static str>,
}
impl CapturedMedia {
    fn warn(&mut self, reason: &'static str) {
        if !self.warnings.contains(&reason) {
            self.warnings.push(reason);
        }
    }
    pub fn project_warning(&self, chat: &mut crate::chat::Chat, tool: &str) {
        if self.warnings.is_empty() {
            return;
        }
        let source = chat.thread["activities"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == tool))
            .cloned();
        let id = format!("media-warning-{tool}");
        crate::chat_projection::activity(
            chat,
            &id,
            "session.notice",
            "Some tool images could not be attached",
            "info",
            serde_json::json!({"detail":format!("{} Available images are still shown. Open the original result on your PC or share the image as a file.",self.warnings.join(" "))}),
        );
        if let Some(source) = source {
            if let Some(row) = chat.thread["activities"]
                .as_array_mut()
                .and_then(|rows| rows.iter_mut().find(|r| r["id"] == id))
            {
                row["createdAt"] = source["createdAt"].clone();
                row["turnId"] = source["turnId"].clone();
            }
        }
    }
}
struct CaptureBudget {
    bytes: usize,
    encoded: usize,
    nodes: usize,
}

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
            Some("imageGeneration")
                if item["status"] == "completed" && item["failure"].is_null() =>
            {
                if let Some(data) = item["result"].as_str().filter(|s| !s.is_empty()) {
                    if data.len() <= MAX_BYTES.div_ceil(3) * 4 {
                        if let Ok(bytes) = STANDARD.decode(data) {
                            return self.capture_native_image_bytes(chat, &bytes);
                        }
                    }
                }
                self.capture_native_image_file(chat, workspace, item["savedPath"].as_str())
            }
            Some("imageView") => {
                self.capture_native_image_file(chat, workspace, item["path"].as_str())
            }
            _ => Ok(vec![]),
        }
    }

    // Only called for typed image records received from the configured Codex
    // connection or its verified history reader. A native tool may view an
    // image outside the initial cwd. Copy that exact file into chat-scoped
    // storage; never grant the phone access to its parent directory. MCP text,
    // tool arguments, image URLs and user-supplied paths do not enter here.
    fn capture_native_image_file(
        &mut self,
        chat: &str,
        workspace: &Path,
        path: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let Some(path) = path.filter(|p| p.len() <= 4096 && !p.contains('\0')) else {
            return Ok(vec![]);
        };
        let path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            let Ok(path) = crate::workspace::resolve(workspace, path) else {
                return Ok(vec![]);
            };
            path
        };
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return Ok(vec![]);
        };
        if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_BYTES as u64 {
            return Ok(vec![]);
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let Ok(file) = options.open(path) else {
            return Ok(vec![]);
        };
        if !file
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= MAX_BYTES as u64)
        {
            return Ok(vec![]);
        }
        let mut bytes = Vec::new();
        if file
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Ok(vec![]);
        }
        self.capture_native_image_bytes(chat, &bytes)
    }

    fn capture_native_image_bytes(
        &mut self,
        chat: &str,
        bytes: &[u8],
    ) -> Result<Vec<String>, String> {
        if bytes.len() > MAX_BYTES || image_mime(bytes).is_none() {
            return Ok(vec![]);
        }
        self.prepare_tool_images()?;
        Ok(vec![self.save_tool_image(
            chat,
            bytes,
            image_mime(bytes).unwrap(),
        )?])
    }

    fn prepare_tool_images(&self) -> Result<(), String> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS tool_images (chat TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(chat,path));")
            .map_err(|_| "Cannot save tool image references".into())
    }
    pub fn capture_tool_images(
        &mut self,
        chat: &str,
        content: &Value,
    ) -> Result<Vec<String>, String> {
        Ok(self.capture_claude_tool_images(chat, content)?.paths)
    }

    pub(crate) fn capture_claude_tool_images(
        &mut self,
        chat: &str,
        content: &Value,
    ) -> Result<CapturedMedia, String> {
        self.prepare_tool_images()?;
        let mut result = CapturedMedia::default();
        let mut budget = CaptureBudget {
            bytes: MAX_BYTES,
            encoded: MAX_BYTES,
            nodes: 4096,
        };
        self.visit_image(chat, content, 0, &mut budget, &mut result)?;
        Ok(result)
    }

    fn visit_image(
        &self,
        chat: &str,
        value: &Value,
        depth: usize,
        budget: &mut CaptureBudget,
        result: &mut CapturedMedia,
    ) -> Result<(), String> {
        if value.is_null() {
            return Ok(());
        }
        if depth > 12 || budget.nodes == 0 {
            result.warn("The tool result exceeded the supported nesting or item limit.");
            return Ok(());
        }
        budget.nodes -= 1;
        if let Some(rows) = value.as_array() {
            for row in rows.iter().take(100) {
                self.visit_image(chat, row, depth + 1, budget, result)?;
            }
            if rows.len() > 100 {
                result.warn("Only the first 100 content items could be inspected.");
            }
            return Ok(());
        }
        if !value.is_object() {
            return Ok(());
        }
        if value["type"] == "text" {
            if let Some(text) = value["text"]
                .as_str()
                .filter(|s| s.trim_start().starts_with('{'))
            {
                if text.len() > budget.encoded {
                    result.warn("An embedded result exceeded the 8 MiB decoding limit.");
                } else {
                    budget.encoded -= text.len();
                    if let Ok(decoded) = serde_json::from_str::<Value>(text) {
                        if decoded["content"].is_array() {
                            self.visit_image(chat, &decoded["content"], depth + 1, budget, result)?;
                        }
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
            if !mime.starts_with("image/") {
                return Ok(());
            }
            if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&mime) {
                result.warn("An image uses a format this viewer does not support.");
                return Ok(());
            }
            if result.paths.len() >= MAX_IMAGES {
                result.warn("A tool result contains more than 8 images.");
                return Ok(());
            }
            if data.len() > budget.bytes.div_ceil(3) * 4 {
                result.warn("Tool images exceed the 8 MiB result limit.");
                return Ok(());
            }
            let Ok(bytes) = STANDARD.decode(data) else {
                result.warn("An image contains invalid base64 data.");
                return Ok(());
            };
            if bytes.len() > budget.bytes {
                result.warn("Tool images exceed the 8 MiB result limit.");
                return Ok(());
            }
            if bytes.is_empty() || image_mime(&bytes) != Some(mime) {
                result.warn("An image is empty or its contents do not match its format.");
                return Ok(());
            }
            let path = self.save_tool_image(chat, &bytes, mime)?;
            if !result.paths.contains(&path) {
                result.paths.push(path);
            }
            budget.bytes -= bytes.len();
            return Ok(());
        }
        if value["type"] == "image" {
            result.warn("Claude returned an image without supported inline image data.");
        }
        // Do not fetch tool-supplied URLs or traverse arbitrary local paths.
        for key in ["content", "resource"] {
            self.visit_image(chat, &value[key], depth + 1, budget, result)?;
        }
        Ok(())
    }

    fn save_tool_image(&self, chat: &str, bytes: &[u8], mime: &str) -> Result<String, String> {
        let ext = match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => return Err("Unsupported tool image".into()),
        };
        let dir = self
            .directory
            .join("tool-images")
            .join(format!("{:x}", Sha256::digest(chat.as_bytes())));
        let path = dir.join(format!("{:x}.{ext}", Sha256::digest(bytes)));
        crate::private_directory::prepare(dir.parent().ok_or("Invalid image directory")?)
            .and_then(|_| crate::private_directory::prepare(&dir))
            .map_err(|_| "Cannot prepare tool image storage")?;
        if !matches_bytes(&path, bytes).map_err(|_| "Cannot verify saved tool image")? {
            // Account for peak temporary storage. Repair only from the same
            // authoritative native bytes, never by deleting history.
            self.check_assets(bytes.len() as u64, 1)?;
            save(&dir, &path, |file| file.write_all(bytes))
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
        Ok(path)
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
    fn skipped_claude_media_is_explained_without_losing_valid_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let valid = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")});
        let result = store
            .capture_claude_tool_images(
                "chat",
                &json!([
                    {"type":"image","mimeType":"image/svg+xml","data":"PHN2Zy8+"},
                    {"type":"image","mimeType":"image/png","data":"not base64"},
                    {"type":"image","source":{"type":"url","url":"http://127.0.0.1/private"}}, valid
                ]),
            )
            .unwrap();
        assert_eq!(result.paths.len(), 1);
        assert_eq!(result.warnings.len(), 3);
        assert!(store.owns_tool_image("chat", Path::new(&result.paths[0])));
        assert!(!result.warnings.join(" ").contains("127.0.0.1"));
        let mut chat: crate::chat::Chat = serde_json::from_value(json!({
            "agent":"claude","native_id":"native","resumed":true,
            "thread":{"id":"chat","latestTurn":null,"session":{"status":"idle"},
                "messages":[],"activities":[{"id":"tool","turnId":"turn",
                    "createdAt":"2026-11-01T00:00:00Z"}]}
        }))
        .unwrap();
        result.project_warning(&mut chat, "tool");
        let notice = &chat.thread["activities"][1];
        assert_eq!(
            notice["tone"], "info",
            "Use a supported Android activity tone"
        );
        assert_eq!(notice["turnId"], "turn");
        assert_eq!(notice["createdAt"], "2026-11-01T00:00:00Z");
        assert!(notice["payload"]["detail"]
            .as_str()
            .unwrap()
            .contains("Available images are still shown"));
        let big = json!({"type":"image","mimeType":"image/png","data":"x".repeat(MAX_BYTES.div_ceil(3)*4+4)});
        let result = store.capture_claude_tool_images("chat", &big).unwrap();
        assert!(result.paths.is_empty());
        assert_eq!(
            result.warnings,
            vec!["Tool images exceed the 8 MiB result limit."]
        );
    }
    #[test]
    fn native_generated_and_viewed_images_are_scoped_copies_even_outside_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nnative-image";
        let file = outside.path().join("native.png");
        fs::write(&file, bytes).unwrap();
        let generated = json!({"type":"imageGeneration","status":"completed","result":STANDARD.encode(bytes),"savedPath":file});
        let paths = store
            .codex_tool_images("one", dir.path(), &generated)
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert_ne!(Path::new(&paths[0]), file);
        assert_eq!(fs::read(&paths[0]).unwrap(), bytes);
        assert!(store.owns_tool_image("one", Path::new(&paths[0])));
        assert!(!store.owns_tool_image("two", Path::new(&paths[0])));
        for item in [
            json!({"type":"imageView","path":file}),
            json!({"type":"imageGeneration","status":"completed","savedPath":file}),
        ] {
            assert_eq!(
                store.codex_tool_images("one", dir.path(), &item).unwrap(),
                paths
            );
        }
        fs::remove_file(file).unwrap();
        assert_eq!(fs::read(&paths[0]).unwrap(), bytes);
        assert_eq!(
            store
                .codex_tool_images("one", dir.path(), &generated)
                .unwrap(),
            paths
        );
        assert_eq!(store.storage_status().unwrap()["assetFiles"], 1);
    }
    #[test]
    fn native_image_paths_do_not_enable_arbitrary_tool_text_or_unbounded_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let file = dir.path().join("not-an-image.png");
        fs::write(&file, "private text").unwrap();
        let huge = dir.path().join("huge.png");
        fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        for item in [
            json!({"type":"imageView","path":file}),
            json!({"type":"imageView","path":huge}),
            json!({"type":"imageView","path":dir.path()}),
            json!({"type":"imageView","path":"https://localhost/private.png"}),
            json!({"type":"imageGeneration","status":"failed","result":STANDARD.encode(b"\x89PNG\r\n\x1a\nfailed")}),
            json!({"type":"imageGeneration","status":"inProgress","savedPath":file}),
            json!({"type":"mcpToolCall","result":{"content":[{"type":"image","path":file}]}}),
        ] {
            assert!(store
                .codex_tool_images("chat", dir.path(), &item)
                .unwrap()
                .is_empty());
        }
        #[cfg(unix)]
        {
            let image = dir.path().join("real.png");
            fs::write(&image, b"\x89PNG\r\n\x1a\nimage").unwrap();
            let link = dir.path().join("link.png");
            std::os::unix::fs::symlink(image, &link).unwrap();
            assert!(store
                .codex_tool_images("chat", dir.path(), &json!({"type":"imageView","path":link}))
                .unwrap()
                .is_empty());
        }
    }
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
