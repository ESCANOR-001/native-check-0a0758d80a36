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
            if codex {
                images.push(json!({"type":"localImage","path":path}));
            } else {
                let bytes = fs::read(&path).map_err(|_| "Cannot read staged image")?;
                images.push(json!({"type":"image","source":{"type":"base64","media_type":row["mimeType"],"data":STANDARD.encode(bytes)}}));
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
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_FILE as u64 {
        return Err("Preview must be a regular image up to 10 MiB".into());
    }
    Ok(file)
}

/// Decode one still frame, with a strict pixel budget before allocating pixels.
/// The HTTP layer serializes conversions; no decoded images are retained.
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
