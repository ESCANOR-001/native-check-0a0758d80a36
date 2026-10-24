//! Offline tool-image conversion. Only explicit exported, content-addressed
//! images are copied; transcript text and the byte-exact source ledger stay intact.
use crate::{attachments::image_mime, auth, storage_stage::open, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const INVALID: &str = "Legacy tool images are missing, changed or unsupported; original files and chat history were not replaced";
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TOTAL: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 2048;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    original: String,
    local: String,
    sha256: String,
    bytes: u64,
}
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='legacy_claude_assets' AND type='table')",[],|r|r.get(0)).map_err(|_|INVALID.into())
}
fn paths(state: &Value) -> Result<BTreeSet<String>, String> {
    let mut found = BTreeSet::new();
    for row in state["thread"]["activities"].as_array().ok_or(INVALID)? {
        let images = &row["payload"]["data"]["toolImages"];
        if images.is_null() {
            continue;
        }
        let images = images.as_array().filter(|v| v.len() <= 8).ok_or(INVALID)?;
        for path in images {
            let path = path
                .as_str()
                .filter(|p| p.len() <= 4096 && !p.chars().any(char::is_control))
                .ok_or(INVALID)?;
            if !Path::new(path).is_absolute() {
                return Err(INVALID.into());
            }
            found.insert(path.to_owned());
            if found.len() > MAX_FILES {
                return Err(INVALID.into());
            }
        }
    }
    Ok(found)
}
fn shape(path: &Path, bucket: &str) -> Result<(String, String, &'static str), String> {
    if path.components().any(|p| {
        matches!(
            p,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) || path
        .parent()
        .and_then(Path::file_name)
        .and_then(|v| v.to_str())
        != Some(bucket)
    {
        return Err(INVALID.into());
    }
    let name = path.file_name().and_then(|n| n.to_str()).ok_or(INVALID)?;
    let (hash, extension) = name.split_once('.').ok_or(INVALID)?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(INVALID.into());
    }
    let mime = match extension {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return Err(INVALID.into()),
    };
    Ok((name.into(), hash.into(), mime))
}
fn read_checked(
    path: &Path,
    hash: &str,
    mime: &str,
    expected: Option<u64>,
    mut output: Option<&mut File>,
    deadline: Instant,
) -> Result<u64, String> {
    open(path.parent().ok_or(INVALID)?, true).map_err(|_| INVALID)?;
    let mut input = open(path, false).map_err(|_| INVALID)?;
    let before = input.metadata().map_err(|_| INVALID)?;
    let identity = crate::storage_stage::identity(&input).map_err(|_| INVALID)?;
    if before.len() == 0 || before.len() > MAX_FILE || expected.is_some_and(|n| n != before.len()) {
        return Err(INVALID.into());
    }
    let mut digest = Sha256::new();
    let mut total = 0;
    let mut buffer = [0u8; 64 * 1024];
    let mut header = Vec::with_capacity(16);
    loop {
        if Instant::now() >= deadline {
            return Err(
                "Tool image migration exceeded its time budget; retry from the unchanged export"
                    .into(),
            );
        }
        let n = input.read(&mut buffer).map_err(|_| INVALID)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > before.len() {
            return Err(INVALID.into());
        }
        let take = n.min(16 - header.len());
        header.extend_from_slice(&buffer[..take]);
        digest.update(&buffer[..n]);
        if let Some(out) = output.as_deref_mut() {
            out.write_all(&buffer[..n]).map_err(|_| INVALID)?;
        }
    }
    let after = open(path, false).map_err(|_| INVALID)?;
    if total != before.len()
        || format!("{:x}", digest.finalize()) != hash
        || image_mime(&header) != Some(mime)
        || crate::storage_stage::identity(&after).map_err(|_| INVALID)? != identity
        || after.metadata().map_err(|_| INVALID)?.len() != before.len()
    {
        return Err(INVALID.into());
    }
    Ok(total)
}
fn ledger(db: &rusqlite::Connection, id: &str) -> Result<Option<Vec<Asset>>, String> {
    if !table(db)? {
        return Ok(None);
    }
    let saved: Option<(String, String)> = db
        .query_row(
            "SELECT data,phase FROM legacy_claude_assets WHERE chat=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|_| INVALID)?;
    saved
        .map(|(raw, phase)| {
            if phase != "prepared" || raw.len() > 1024 * 1024 {
                return Err(INVALID.into());
            }
            let assets: Vec<Asset> = serde_json::from_str(&raw).map_err(|_| INVALID)?;
            if assets.len() > MAX_FILES {
                return Err(INVALID.into());
            }
            Ok(assets)
        })
        .transpose()
}
fn overlay(state: &Value, assets: &[Asset]) -> Result<Value, String> {
    let map: BTreeMap<_, _> = assets
        .iter()
        .map(|a| (a.original.as_str(), a.local.as_str()))
        .collect();
    if map.len() != assets.len()
        || paths(state)? != assets.iter().map(|a| a.original.clone()).collect()
    {
        return Err(INVALID.into());
    }
    let mut state = state.clone();
    for row in state["thread"]["activities"]
        .as_array_mut()
        .ok_or(INVALID)?
    {
        if let Some(paths) = row["payload"]["data"]["toolImages"].as_array_mut() {
            for path in paths {
                *path = json!(map.get(path.as_str().ok_or(INVALID)?).ok_or(INVALID)?);
            }
        }
    }
    Ok(state)
}
pub(crate) fn project(
    db: &rusqlite::Connection,
    directory: &Path,
    state: &Value,
) -> Result<Value, String> {
    let id = state["thread"]["id"].as_str().ok_or(INVALID)?;
    let assets = ledger(db, id)?;
    let mut read = db
        .prepare("SELECT path FROM tool_images WHERE chat=?1 ORDER BY path")
        .map_err(|_| INVALID)?;
    let found: BTreeSet<String> = read
        .query_map([id], |r| r.get(0))
        .map_err(|_| INVALID)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| INVALID)?;
    let Some(assets) = assets else {
        if !found.is_empty() {
            return Err(INVALID.into());
        }
        return Ok(state.clone());
    };
    let expected: BTreeSet<_> = assets.iter().map(|a| a.local.clone()).collect();
    if found != expected || expected.len() != assets.len() {
        return Err(INVALID.into());
    }
    let bucket = auth::digest(id);
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut total = 0u64;
    if !assets.is_empty() {
        open(&directory.join("tool-images"), true).map_err(|_| INVALID)?;
    }
    for asset in &assets {
        let (name, hash, mime) = shape(Path::new(&asset.original), &bucket)?;
        if asset.sha256 != hash
            || Path::new(&asset.local) != directory.join("tool-images").join(&bucket).join(name)
        {
            return Err(INVALID.into());
        }
        total = total
            .checked_add(read_checked(
                Path::new(&asset.local),
                &hash,
                mime,
                Some(asset.bytes),
                None,
                deadline,
            )?)
            .ok_or(INVALID)?;
        if total > MAX_TOTAL {
            return Err(INVALID.into());
        }
    }
    overlay(state, &assets)
}
pub(crate) fn rollback(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    if table(db)? {
        db.execute("UPDATE legacy_claude_assets SET phase='rolled-back' WHERE chat=?1 AND phase='prepared'",[id]).map_err(|_|INVALID)?;
    }
    // Deliberately keep copied blobs. Rollback must not erase files which may
    // be referenced elsewhere; their receipts and the source backup remain.
    Ok(())
}
/// Retained, already-rolled-back blobs remain accounted for. A connection
/// rollback may leave these exact copies in place, but no unrelated asset rows.
pub(crate) fn retained_assets(
    db: &rusqlite::Connection,
    directory: &Path,
    environment: &str,
) -> Result<i64, String> {
    let mut expected = crate::legacy_uploads::retained_asset_rows(db, directory, environment)?;
    if table(db)? {
        let mut q = db
            .prepare("SELECT data FROM legacy_claude_assets WHERE phase='rolled-back' LIMIT 10001")
            .map_err(|_| INVALID)?;
        let rows = q
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|_| INVALID)?;
        for (count, row) in rows.enumerate() {
            if count >= 10_000 {
                return Err(INVALID.into());
            }
            let raw = row.map_err(|_| INVALID)?;
            if raw.len() > 1024 * 1024 {
                return Err(INVALID.into());
            }
            let assets: Vec<Asset> = serde_json::from_str(&raw).map_err(|_| INVALID)?;
            if assets.len() > MAX_FILES {
                return Err(INVALID.into());
            }
            for asset in assets {
                if asset.bytes == 0
                    || asset.bytes > MAX_FILE
                    || expected.insert(asset.local, asset.bytes as i64).is_some()
                {
                    return Err(INVALID.into());
                }
                if expected.len() > 20_000 {
                    return Err(INVALID.into());
                }
            }
        }
    }
    let mut q = db
        .prepare("SELECT path,bytes FROM storage_assets LIMIT 20001")
        .map_err(|_| INVALID)?;
    let rows: BTreeMap<String, i64> = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| INVALID)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| INVALID)?;
    // A stage uses the canonical generation parent, while a later offline
    // rollback can arrive through /var on macOS or a non-verbatim Windows
    // path. Compare the same verified files without rewriting the ledger.
    let root = directory.canonicalize().map_err(|_| INVALID)?;
    if normalized_asset_rows(expected, &root)? != normalized_asset_rows(rows, &root)? {
        return Err(INVALID.into());
    }
    Ok(db
        .query_row("SELECT count(*) FROM storage_assets", [], |r| r.get(0))
        .map_err(|_| INVALID)?)
}

fn normalized_asset_rows(
    rows: BTreeMap<String, i64>,
    root: &Path,
) -> Result<BTreeMap<PathBuf, i64>, String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut result = BTreeMap::new();
    for (raw, bytes) in rows {
        let path = Path::new(&raw);
        if Instant::now() >= deadline
            || bytes < 0
            || !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(INVALID.into());
        }
        let canonical = path.canonicalize().map_err(|_| INVALID)?;
        let relative = canonical.strip_prefix(root).map_err(|_| INVALID)?;
        let depth = relative.components().count();
        if depth == 0 {
            return Err(INVALID.into());
        }
        // Only the profile's parent may have an OS alias. No link or reparse
        // point below the profile is accepted, even if it resolves to a known
        // file. Duplicate aliases must not hide an extra storage charge.
        let prefix = path.ancestors().nth(depth).ok_or(INVALID)?;
        if prefix.canonicalize().map_err(|_| INVALID)? != root {
            return Err(INVALID.into());
        }
        for (i, part) in path.ancestors().take(depth + 1).enumerate() {
            open(part, i != 0).map_err(|_| INVALID)?;
        }
        if result.insert(canonical, bytes).is_some() {
            return Err(INVALID.into());
        }
    }
    Ok(result)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl Store {
    /// The two roots describe the old path prefix and its separate quiesced
    /// export. Never read the serialized original path or fetch a remote URL.
    pub fn prepare_legacy_claude_tool_images(
        &mut self,
        id: &str,
        original_root: &Path,
        export_root: &Path,
    ) -> Result<Value, String> {
        let p = self.legacy_claude_prepared(id)?;
        if let Some(assets) = ledger(&self.db, id)? {
            return Ok(
                json!({"phase":"claude-tool-images-prepared","images":assets.len(),"migrationComplete":false}),
            );
        }
        let originals = paths(&p.state)?;
        if !original_root.is_absolute()
            || !export_root.is_absolute()
            || original_root == export_root
            || original_root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || export_root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(INVALID.into());
        }
        open(export_root, true).map_err(|_| INVALID)?;
        let export = export_root.canonicalize().map_err(|_| INVALID)?;
        if original_root.canonicalize().is_ok_and(|p| p == export)
            || export.starts_with(&self.directory)
        {
            return Err(INVALID.into());
        }
        let bucket = auth::digest(id);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut assets = Vec::new();
        let mut total = 0u64;
        for original in originals {
            let original_path = Path::new(&original);
            let (name, hash, mime) = shape(original_path, &bucket)?;
            if original_path != original_root.join(&bucket).join(&name) {
                return Err(INVALID.into());
            }
            let source = export.join(&bucket).join(&name);
            let bytes = read_checked(&source, &hash, mime, None, None, deadline)?;
            total = total.checked_add(bytes).ok_or(INVALID)?;
            if total > MAX_TOTAL {
                return Err(INVALID.into());
            }
            assets.push(Asset {
                original,
                local: self
                    .directory
                    .join("tool-images")
                    .join(&bucket)
                    .join(name)
                    .to_str()
                    .ok_or(INVALID)?
                    .into(),
                sha256: hash,
                bytes,
            });
        }
        let (mut extra_bytes, mut extra_files) = (0u64, 0u64);
        for asset in &assets {
            match fs::symlink_metadata(&asset.local) {
                Ok(_) => {
                    let (_, hash, mime) = shape(Path::new(&asset.original), &bucket)?;
                    read_checked(
                        Path::new(&asset.local),
                        &hash,
                        mime,
                        Some(asset.bytes),
                        None,
                        deadline,
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    extra_bytes += asset.bytes;
                    extra_files += 1;
                }
                Err(_) => return Err(INVALID.into()),
            }
        }
        self.check_assets(extra_bytes.saturating_mul(2), extra_files)?;
        let root = self.directory.join("tool-images");
        let destination = root.join(&bucket);
        if !assets.is_empty() {
            crate::private_directory::prepare(&root)
                .and_then(|_| crate::private_directory::prepare(&destination))
                .map_err(|_| INVALID)?;
        }
        for asset in &assets {
            let (name, hash, mime) = shape(Path::new(&asset.original), &bucket)?;
            let path = Path::new(&asset.local);
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    read_checked(path, &hash, mime, Some(asset.bytes), None, deadline)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let tmp = Temporary(
                        destination.join(format!(".import-{}.tmp", uuid::Uuid::new_v4())),
                    );
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut output = options.open(&tmp.0).map_err(|_| INVALID)?;
                    read_checked(
                        &export.join(&bucket).join(name),
                        &hash,
                        mime,
                        Some(asset.bytes),
                        Some(&mut output),
                        deadline,
                    )?;
                    output.sync_all().map_err(|_| INVALID)?;
                    drop(output);
                    // create-only publication. A conflict never overwrites bytes.
                    fs::hard_link(&tmp.0, path).map_err(|_| INVALID)?;
                    #[cfg(unix)]
                    open(&destination, true)
                        .and_then(|f| f.sync_all())
                        .map_err(|_| INVALID)?;
                }
                Err(_) => return Err(INVALID.into()),
            }
            self.record_asset(path)?;
        }
        // A failed or interrupted copy may leave unreferenced verified blobs,
        // but no UI row or image capability changes until this transaction.
        self.legacy_claude_prepared(id)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_claude_assets(chat TEXT PRIMARY KEY,data TEXT NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let state = overlay(&p.state, &assets)?;
        let saved = serde_json::to_string(&assets).map_err(|_| INVALID)?;
        if saved.len() > 1024 * 1024 {
            return Err(INVALID.into());
        }
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        for row in state["thread"]["activities"].as_array().ok_or(INVALID)? {
            tx.execute(
                "UPDATE chat_rows SET data=?3 WHERE chat=?1 AND kind='activities' AND id=?2",
                params![id, row["id"].as_str(), row.to_string()],
            )
            .map_err(|_| INVALID)?;
        }
        for asset in &assets {
            tx.execute(
                "INSERT INTO tool_images(chat,path) VALUES(?1,?2)",
                params![id, asset.local],
            )
            .map_err(|_| INVALID)?;
        }
        tx.execute(
            "INSERT INTO legacy_claude_assets VALUES(?1,?2,'prepared')",
            params![id, saved],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(
            json!({"phase":"claude-tool-images-prepared","images":assets.len(),"migrationComplete":false}),
        )
    }
}
