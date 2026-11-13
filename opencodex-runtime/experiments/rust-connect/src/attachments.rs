//! Phone uploads are decoded one file at a time into the isolated private store.
//! Chat/queue state contains only metadata, never base64 or image bytes.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
pub const MAX_FILE: usize = 10 * 1024 * 1024;
pub const MAX_TOTAL: usize = 40 * 1024 * 1024;
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
fn decode(row: &Value) -> Result<(&str, &str, Vec<u8>), String> {
    let kind = row["type"].as_str().unwrap_or("");
    if !["image", "file"].contains(&kind) {
        return Err("Unsupported attachment type".into());
    }
    let data = row["dataUrl"]
        .as_str()
        .ok_or("Attachment data is missing")?;
    let (mime, encoded) = data
        .strip_prefix("data:")
        .and_then(|s| s.split_once(";base64,"))
        .ok_or("Attachment must be a base64 data URL")?;
    if mime.len() > 100 || encoded.len() > MAX_FILE.div_ceil(3) * 4 {
        return Err("Attachment exceeds 10 MiB".into());
    }
    if row["mimeType"].as_str().is_some_and(|s| s != mime) {
        return Err("Attachment media type mismatch".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid attachment base64")?;
    if bytes.is_empty() || bytes.len() > MAX_FILE {
        return Err("Attachment must be between 1 byte and 10 MiB".into());
    }
    if kind == "image" && image_mime(&bytes) != Some(mime) {
        return Err("Invalid PNG, JPEG, GIF or WebP attachment".into());
    }
    if row
        .get("sizeBytes")
        .is_some_and(|v| v.as_u64() != Some(bytes.len() as u64))
    {
        return Err("Attachment size mismatch".into());
    }
    Ok((kind, mime, bytes))
}
/// Validate a request before preparing a new native history. Decode one file at
/// a time, and leave both the private upload store and CLI profiles unchanged.
pub(crate) fn preflight(rows: &Value) -> Result<(u64, u64), String> {
    let rows = rows.as_array().ok_or("Attachments must be an array")?;
    if rows.len() > 8 {
        return Err("At most eight attachments are allowed".into());
    }
    let mut total = 0u64;
    for row in rows {
        total += decode(row)?.2.len() as u64;
        if total > MAX_TOTAL as u64 {
            return Err("Combined attachments exceed 40 MiB".into());
        }
    }
    Ok((total, rows.len() as u64))
}
pub fn stage(root: &Path, attachments: Value) -> Result<Vec<Value>, String> {
    let rows = attachments
        .as_array()
        .ok_or("Attachments must be an array")?;
    if rows.len() > 8 {
        return Err("At most eight attachments are allowed".into());
    }
    if rows.is_empty() {
        return Ok(vec![]);
    }
    crate::private_directory::prepare(root).map_err(|_| "Private upload storage is unavailable")?;
    let folder = root.join(uuid::Uuid::new_v4().to_string());
    crate::private_directory::prepare(&folder)
        .map_err(|_| "Cannot create private upload directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect upload directory")?;
    }
    let result = (|| {
        let mut total = 0;
        let mut staged = Vec::new();
        for row in rows {
            let (kind, mime, bytes) = decode(row)?;
            total += bytes.len();
            if total > MAX_TOTAL {
                return Err("Combined attachments exceed 40 MiB".into());
            }
            let name: String = row["name"]
                .as_str()
                .unwrap_or("attachment")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "._ -".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .take(160)
                .collect();
            let name = name.trim_matches('.');
            let name = if name.is_empty() { "attachment" } else { name };
            let path = folder.join(format!("{}-{name}", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path).map_err(|_| "Cannot stage attachment")?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "Cannot save attachment")?;
            staged.push(json!({"type":kind,"name":name,"mimeType":mime,"sizeBytes":bytes.len(),"path":path}));
        }
        Ok(staged)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(folder);
    }
    result
}
pub fn discard(rows: &[Value]) {
    for row in rows {
        if let Some(p) = row["path"].as_str() {
            let _ = fs::remove_file(p);
        }
    }
    if let Some(p) = rows
        .first()
        .and_then(|v| v["path"].as_str())
        .and_then(|p| Path::new(p).parent())
    {
        let _ = fs::remove_dir(p);
    }
}
pub fn display_message(message: &Value) -> String {
    let mut text = message["text"].as_str().unwrap_or("").to_owned();
    for row in message["attachments"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "\n\n{}[{}](<{}>)",
            if row["type"] == "image" { "!" } else { "" },
            row["name"].as_str().unwrap_or("Attachment"),
            row["path"].as_str().unwrap_or("")
        ));
    }
    text
}

impl crate::store::Store {
    /// A saved upload link authorizes only this exact private file in this
    /// chat. It does not make the upload bucket a workspace browsing root.
    pub(crate) fn read_upload_text(
        &self,
        chat: &str,
        requested: &str,
    ) -> Result<Option<Value>, String> {
        let Some(path) = self.resolve_upload_file(chat, requested)? else {
            return Ok(None);
        };
        crate::workspace::read_chat(path.parent().ok_or("Attachment unavailable")?, requested)
            .map(Some)
    }

    pub(crate) fn resolve_upload_file(
        &self,
        chat: &str,
        requested: &str,
    ) -> Result<Option<PathBuf>, String> {
        let path = Path::new(requested);
        let root = self.directory.join("uploads");
        let Ok(relative) = path.strip_prefix(&root) else {
            return Ok(None);
        };
        let parts: Vec<_> = relative.components().collect();
        if parts.len() != 2
            || parts
                .iter()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("Invalid attachment path".into());
        }
        let parent = path.parent().ok_or("Attachment unavailable")?;
        for directory in [&root, parent] {
            let metadata = fs::symlink_metadata(directory).map_err(|_| "Attachment unavailable")?;
            if !metadata.is_dir() || !crate::profile_snapshot::regular(&metadata) {
                return Err("Attachment directory is unavailable or linked".into());
            }
        }
        let metadata = fs::symlink_metadata(path).map_err(|_| "Attachment unavailable")?;
        if !metadata.is_file() || !crate::profile_snapshot::regular(&metadata) {
            return Err("Attachment must be a regular file".into());
        }
        let indexed: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM storage_assets WHERE path=?1)",
                [requested],
                |r| r.get(0),
            )
            .map_err(|_| "Attachment history unavailable")?;
        if !indexed {
            return Err("Attachment is not in private storage".into());
        }
        // Stream only matching bounded message rows. Do not hydrate the chat
        // or accept a partial path mentioned in arbitrary prose.
        let mut statement = self.db.prepare(
            "SELECT data FROM chat_rows WHERE chat=?1 AND kind='messages' AND length(data)<=?2 AND instr(data,?3)>0",
        ).map_err(|_| "Attachment history unavailable")?;
        // Stored rows are JSON, so Windows separators and quoted filenames
        // must be escaped for the SQL prefilter, then checked after decoding.
        let encoded = serde_json::to_string(requested).map_err(|_| "Invalid attachment path")?;
        let needle = &encoded[1..encoded.len() - 1];
        let rows = statement
            .query_map(
                rusqlite::params![chat, crate::chat_store::ROW_BYTES as i64, needle],
                |r| r.get::<_, String>(0),
            )
            .map_err(|_| "Attachment history unavailable")?;
        for row in rows {
            let raw = row.map_err(|_| "Attachment history unavailable")?;
            let message: Value =
                serde_json::from_str(&raw).map_err(|_| "Attachment history unavailable")?;
            if crate::legacy_uploads::referenced(&message, requested) {
                return Ok(Some(path.to_owned()));
            }
        }
        Err("Attachment is not part of this chat".into())
    }
}

pub fn input(message: &Value, codex: bool) -> Result<Value, String> {
    let mut text = message["text"].as_str().unwrap_or("").to_owned();
    let mut images = Vec::new();
    for row in message["attachments"].as_array().into_iter().flatten() {
        let path = PathBuf::from(row["path"].as_str().ok_or("Missing staged attachment")?);
        if !fs::symlink_metadata(&path)
            .is_ok_and(|m| m.is_file() && m.len() == row["sizeBytes"].as_u64().unwrap_or(0))
        {
            return Err("Staged attachment is unavailable; reattach it before sending".into());
        }
        if row["type"] == "image" {
            // A transport limit is not a model limit. Preserve the original and
            // use a bounded preview for large inline images; tools can read the original.
            let (image_path, image_mime) = if row["sizeBytes"].as_u64().unwrap_or(0) > 1024 * 1024 {
                text.push_str(&format!(
                    "\n\nOriginal image uploaded from Android: {}",
                    path.display()
                ));
                match model_preview(&path) {
                    Ok(preview) => (preview, "image/png"),
                    Err(_) => {
                        text.push_str(" (read this file with image/file tools; it exceeds the inline preview budget)");
                        continue;
                    }
                }
            } else {
                (
                    path.clone(),
                    row["mimeType"].as_str().unwrap_or("image/jpeg"),
                )
            };
            if codex {
                images.push(json!({"type":"localImage","path":image_path}));
            } else {
                let bytes = fs::read(&image_path).map_err(|_| "Cannot read staged image")?;
                images.push(json!({"type":"image","source":{"type":"base64","media_type":image_mime,"data":STANDARD.encode(bytes)}}));
            }
        } else {
            text.push_str(&format!(
                "\n\nFile uploaded from Android: {}",
                path.display()
            ));
        }
    }
    let mut input = vec![json!({"type":"text","text":text})];
    input.extend(images);
    Ok(json!(input))
}

pub fn open_image(path: &Path) -> Result<std::fs::File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "Image unavailable")?;
    let meta = file.metadata().map_err(|_| "Image unavailable")?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > crate::phone_uploads::MAX_FILE {
        return Err("Preview must be a regular image up to 100 MB".into());
    }
    Ok(file)
}

/// Decode one still frame, with a strict pixel budget before allocating pixels.
/// The HTTP layer serializes conversions; no decoded images are retained.
fn model_preview(path: &Path) -> Result<PathBuf, String> {
    use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
    let mut preview_name = path.file_name().ok_or("Invalid image path")?.to_os_string();
    preview_name.push(".preview.png");
    let existing = path.with_file_name(preview_name);
    if fs::symlink_metadata(&existing).is_ok_and(|m| m.is_file() && m.len() <= 1024 * 1024) {
        return Ok(existing);
    }
    let mut reader = ImageReader::new(std::io::BufReader::new(open_image(path)?))
        .with_guessed_format()
        .map_err(|_| "Invalid image")?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(80 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "Unsupported image")?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > 16_000_000 || decoder.total_bytes() > 64 * 1024 * 1024
    {
        return Err("Image exceeds preview pixel budget".into());
    }
    let orientation = decoder.orientation().map_err(|_| "Invalid orientation")?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(|_| "Invalid image")?;
    let mut small = decoded.thumbnail(1568, 1568);
    drop(decoded);
    small.apply_orientation(orientation);
    let target = existing;
    let mut bytes = Vec::new();
    for bound in [1568, 768, 512, 256] {
        let resized = small.thumbnail(bound, bound);
        let mut output = std::io::Cursor::new(Vec::new());
        resized
            .write_to(&mut output, image::ImageFormat::Png)
            .map_err(|_| "Cannot encode preview")?;
        bytes = output.into_inner();
        if bytes.len() <= 1024 * 1024 {
            break;
        }
    }
    if bytes.len() > 1024 * 1024 {
        return Err("Image exceeds inline preview budget".into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&target)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|_| "Cannot save preview")?;
    Ok(target)
}

pub fn thumbnail(path: &Path) -> Result<Vec<u8>, String> {
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
    use std::io::{BufReader, Cursor};
    let mut reader = ImageReader::new(BufReader::new(open_image(path)?))
        .with_guessed_format()
        .map_err(|_| "Invalid image")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(80 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "Unsupported image")?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > 16_000_000 || decoder.total_bytes() > 64 * 1024 * 1024
    {
        return Err("Image exceeds the thumbnail pixel budget".into());
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| "Invalid image orientation")?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(|_| "Invalid image pixels")?;
    let mut small = decoded.thumbnail(256.min(width), 256.min(height));
    drop(decoded);
    small.apply_orientation(orientation);
    let mut output = Cursor::new(Vec::new());
    small
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| "Cannot encode thumbnail")?;
    Ok(output.into_inner())
}

#[cfg(test)]
mod upload_preview_tests {
    use super::*;
    #[test]
    fn large_images_in_one_folder_keep_distinct_transparent_previews_for_both_agents() {
        let root = tempfile::tempdir().unwrap();
        let mut attachments = Vec::new();
        for (name, color) in [("one.png", [255, 0, 0, 128]), ("two.png", [0, 0, 255, 64])] {
            let path = root.path().join(name);
            image::RgbaImage::from_pixel(32, 32, image::Rgba(color))
                .save(&path)
                .unwrap();
            // A valid PNG with a large trailing payload exercises the large-file branch.
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&vec![0; 1024 * 1024])
                .unwrap();
            attachments.push(json!({"type":"image","path":path,"mimeType":"image/png","sizeBytes":fs::metadata(&path).unwrap().len()}));
        }
        let message = json!({"text":"Compare these images", "attachments":attachments});
        let codex = input(&message, true).unwrap();
        assert_ne!(codex[1]["path"], codex[2]["path"]);
        let claude = input(&message, false).unwrap();
        for (index, expected) in [(1, [255, 0, 0, 128]), (2, [0, 0, 255, 64])] {
            let bytes = STANDARD
                .decode(claude[index]["source"]["data"].as_str().unwrap())
                .unwrap();
            assert!(bytes.len() <= 1024 * 1024);
            let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
            assert_eq!(decoded.get_pixel(0, 0).0, expected);
            assert_eq!(
                fs::read(codex[index]["path"].as_str().unwrap()).unwrap(),
                bytes
            );
            assert!(
                fs::metadata(message["attachments"][index - 1]["path"].as_str().unwrap())
                    .unwrap()
                    .len()
                    > 1024 * 1024
            );
        }
    }
}
