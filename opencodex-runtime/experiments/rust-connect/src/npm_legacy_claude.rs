//! Export settled historical Claude caches using the existing Rust validators.
//! Original session files/transcripts/assets are read only; all generated
//! inventories go into the private upgrade copy.
use super::{hash, read, write, ERROR};
use crate::{profile_snapshot, storage_stage};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}
fn empty(row: &Value) -> bool {
    row["thread"]["latestTurn"].is_null()
        && row["bridgeId"].is_null()
        && row["external"] != true
        && ["messages", "activities", "proposedPlans"]
            .iter()
            .all(|key| row["thread"][key].as_array().is_some_and(Vec::is_empty))
        && ["followUps", "deliveries", "fileCheckpoints", "userUuids"]
            .iter()
            .all(|key| row[key].is_null() || row[key].as_array().is_some_and(Vec::is_empty))
        && (row["thread"]["checkpoints"].is_null()
            || row["thread"]["checkpoints"]
                .as_array()
                .is_some_and(Vec::is_empty))
}
fn transcript(profile: &Path, native: &str) -> Result<Option<PathBuf>, String> {
    uuid::Uuid::parse_str(native).map_err(|_| ERROR)?;
    let root = profile.join("projects");
    if !root.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    storage_stage::open(&root, true).map_err(|_| ERROR)?;
    let mut found = None;
    for (count, entry) in fs::read_dir(root).map_err(|_| ERROR)?.enumerate() {
        if count >= 20_000 {
            return Err(ERROR.into());
        }
        let entry = entry.map_err(|_| ERROR)?;
        if !entry.file_type().map_err(|_| ERROR)?.is_dir() {
            continue;
        }
        storage_stage::open(&entry.path(), true).map_err(|_| ERROR)?;
        let file = entry.path().join(format!("{native}.jsonl"));
        if file.try_exists().map_err(|_| ERROR)? {
            storage_stage::open(&file, false).map_err(|_| ERROR)?;
            if found.replace(file).is_some() {
                return Err(ERROR.into());
            }
        }
    }
    Ok(found)
}
fn checkpoints(path: &Path, row: &mut Value) -> Result<String, String> {
    let (file, before) = profile_snapshot::open(path, 512 * 1024 * 1024).map_err(|_| ERROR)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut ids = BTreeSet::new();
    let mut line = Vec::new();
    let deadline = std::time::Instant::now();
    let mut count = 0;
    loop {
        line.clear();
        (&mut reader)
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| ERROR)?;
        if line.is_empty() {
            break;
        }
        count += 1;
        size += line.len() as u64;
        if line.len() > 8 * 1024 * 1024
            || line.last() != Some(&b'\n')
            || size > before.len()
            || count > 200_000
            || deadline.elapsed().as_secs() > 120
        {
            return Err(ERROR.into());
        }
        digest.update(&line);
        let native: Value = serde_json::from_slice(&line).map_err(|_| ERROR)?;
        if !native["sessionId"].is_null() && native["sessionId"] != row["nativeId"] {
            return Err(ERROR.into());
        }
        if native["type"] == "file-history-snapshot" {
            if let Some(id) = native["messageId"].as_str() {
                ids.insert(id.to_owned());
            }
        }
    }
    let after = reader.get_ref().metadata().map_err(|_| ERROR)?;
    if size != before.len()
        || after.len() != before.len()
        || after.modified().map_err(|_| ERROR)? != before.modified().map_err(|_| ERROR)?
    {
        return Err(ERROR.into());
    }
    if let Some(saved) = row["fileCheckpoints"].as_array().cloned() {
        let (keep, missing): (Vec<_>, Vec<_>) = saved
            .into_iter()
            .partition(|p| p["id"].as_str().is_some_and(|id| ids.contains(id)));
        row["fileCheckpoints"] = json!(keep);
        if !missing.is_empty() {
            let mut retained = row["retainedFileCheckpoints"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            retained.extend(missing);
            row["retainedFileCheckpoints"] = json!(retained);
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn copy(
    root: &Path,
    original: &str,
    destination: &Path,
    limit: u64,
) -> Result<(String, String, u64), String> {
    let root = plain(root);
    let source = Path::new(original);
    let relative = source.strip_prefix(&root).map_err(|_| ERROR)?;
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(ERROR.into());
    }
    let mut part = root.clone();
    storage_stage::open(&part, true).map_err(|_| ERROR)?;
    let names: Vec<_> = relative.components().collect();
    for (index, name) in names.iter().enumerate() {
        part.push(name);
        storage_stage::open(&part, index + 1 < names.len()).map_err(|_| ERROR)?;
    }
    let data = read(source, limit)?;
    let name = relative.to_str().ok_or(ERROR)?.replace('\\', "/");
    let out = destination.join(relative);
    prepare_asset_parent(destination, relative)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|_| ERROR)?;
    file.write_all(&data).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    Ok((name, hash(&data), data.len() as u64))
}
fn prepare_asset_parent(root: &Path, relative: &Path) -> Result<(), String> {
    crate::private_directory::prepare(root).map_err(|_| ERROR)?;
    let mut directory = root.to_owned();
    for component in relative.parent().ok_or(ERROR)?.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(ERROR.into());
        }
        directory.push(component);
        crate::private_directory::prepare(&directory).map_err(|_| ERROR)?;
    }
    Ok(())
}
fn paths(row: &Value, upload_root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut paths = BTreeMap::new();
    for message in row["thread"]["messages"].as_array().ok_or(ERROR)? {
        let id = message["id"].as_str().ok_or(ERROR)?;
        if let Some(attachments) = message.get("attachments").filter(|v| !v.is_null()) {
            for attachment in attachments.as_array().ok_or(ERROR)? {
                paths
                    .entry(attachment["path"].as_str().ok_or(ERROR)?.to_owned())
                    .or_insert_with(|| id.to_owned());
            }
        }
        let text = message["text"].as_str().unwrap_or("");
        if message["role"] == "user" {
            if let Some((_, suffix)) = text.rsplit_once("\n\nAttached files on this PC:\n") {
                for path in suffix.lines() {
                    if !Path::new(path).is_absolute() {
                        return Err(ERROR.into());
                    }
                    paths.entry(path.into()).or_insert_with(|| id.into());
                }
            }
        }
        // Preserve the historical raw Markdown path spelling. Unrecognised
        // old upload references are refused by validate() below.
        for link in text.split("](").skip(1) {
            let path = if let Some(rest) = link.strip_prefix('<') {
                rest.split_once(">)").map(|(p, _)| p)
            } else {
                link.split_once(')').map(|(p, _)| p)
            };
            if let Some(path) = path {
                if Path::new(path).starts_with(upload_root) || path.contains("android-remote-files")
                {
                    paths.entry(path.into()).or_insert_with(|| id.into());
                }
            }
        }
    }
    if paths.len() > 2048 {
        return Err(ERROR.into());
    }
    Ok(paths)
}
fn native_assets(
    path: &Path,
    row: &Value,
    uploads: &Path,
    target: &Path,
    budget: &mut u64,
) -> Result<(BTreeMap<String, String>, Vec<Value>), String> {
    let users: Vec<_> = row["thread"]["messages"]
        .as_array()
        .ok_or(ERROR)?
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    let mut mapped = BTreeMap::new();
    for user in &users {
        mapped.insert(user["id"].as_str().ok_or(ERROR)?.to_owned(), *user);
    }
    if let Some(deliveries) = row["deliveries"].as_array() {
        for d in deliveries {
            if d["state"] != "cancelled" {
                if let Some(user) = users.iter().find(|m| m["id"] == d["messageId"]) {
                    mapped.insert(d["uuid"].as_str().ok_or(ERROR)?.to_owned(), *user);
                }
            }
        }
    }
    let ordered: Vec<_> = if row["deliveries"].as_array().is_none_or(Vec::is_empty) {
        row["userUuids"]
            .as_array()
            .filter(|r| r.len() == users.len())
            .cloned()
            .unwrap_or_default()
    } else {
        vec![]
    };
    let mut expected = Vec::new();
    let mut seen = BTreeSet::new();
    for (id, user) in ordered.iter().zip(&users) {
        let id = id.as_str().ok_or(ERROR)?;
        if !seen.insert(id) {
            return Err(ERROR.into());
        }
        if mapped.get(id).is_some_and(|old| old["id"] != user["id"]) {
            return Err(ERROR.into());
        }
        mapped.insert(id.into(), *user);
        expected.push(id.to_owned());
    }
    let (file, before) = profile_snapshot::open(path, 512 * 1024 * 1024).map_err(|_| ERROR)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut total = 0u64;
    let mut paths = BTreeMap::new();
    let mut embedded = BTreeMap::new();
    let mut observed = Vec::new();
    let deadline = std::time::Instant::now();
    loop {
        line.clear();
        (&mut reader)
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| ERROR)?;
        if line.is_empty() {
            break;
        }
        total += line.len() as u64;
        if line.len() > 8 * 1024 * 1024
            || line.last() != Some(&b'\n')
            || total > before.len()
            || deadline.elapsed().as_secs() > 120
        {
            return Err(ERROR.into());
        }
        let native: Value = serde_json::from_slice(&line).map_err(|_| ERROR)?;
        if native["type"] != "user"
            || native["isMeta"] == true
            || native["isSynthetic"] == true
            || native["isSidechain"] == true
            || !native["parent_tool_use_id"].is_null()
            || native["sessionId"] != row["nativeId"]
            || native["message"]["role"] != "user"
        {
            continue;
        }
        let uuid = native["uuid"].as_str().ok_or(ERROR)?;
        if expected.iter().any(|s| s == uuid) {
            observed.push(uuid.to_owned());
        }
        let Some(blocks) = native["message"]["content"].as_array() else {
            continue;
        };
        if blocks.is_empty() || blocks[0]["type"] != "text" {
            continue;
        }
        let text = blocks[0]["text"].as_str().ok_or(ERROR)?;
        if !text.contains("\n\nAttached files on this PC:\n")
            && !blocks.iter().any(|b| b["type"] == "image")
        {
            continue;
        }
        let message=mapped.get(uuid).ok_or("A native Claude attachment has no verified message identity; original history was retained")?;
        let id = message["id"].as_str().ok_or(ERROR)?;
        let prompt = message["text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("Inspect the attached files.");
        if text != prompt {
            let suffix = text
                .strip_prefix(&format!("{prompt}\n\nAttached files on this PC:\n"))
                .ok_or(ERROR)?;
            for path in suffix.lines() {
                if !Path::new(path).starts_with(uploads) {
                    return Err(ERROR.into());
                }
                paths.entry(path.into()).or_insert_with(|| id.into());
            }
        }
        for block in blocks.iter().skip(1) {
            let source = &block["source"];
            if block["type"] != "image" || source["type"] != "base64" {
                return Err(ERROR.into());
            }
            let raw = source["data"].as_str().ok_or(ERROR)?;
            if raw.len() > 14 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .map_err(|_| ERROR)?;
            if bytes.is_empty()
                || bytes.len() > 10 * 1024 * 1024
                || base64::engine::general_purpose::STANDARD.encode(&bytes) != raw
            {
                return Err(ERROR.into());
            }
            let extension = match source["media_type"].as_str() {
                Some("image/png") => "png",
                Some("image/jpeg") => "jpg",
                Some("image/gif") => "gif",
                Some("image/webp") => "webp",
                _ => return Err(ERROR.into()),
            };
            let digest = hash(&bytes);
            let key =
                hash(format!("{}\0{uuid}", row["nativeId"].as_str().ok_or(ERROR)?).as_bytes());
            let bucket = format!(
                "{}-{}-{}-{}-{}",
                &key[..8],
                &key[8..12],
                &key[12..16],
                &key[16..20],
                &key[20..32]
            );
            let relative = format!("recovered-native/{bucket}/{digest}.{extension}");
            if embedded.contains_key(&relative) {
                continue;
            }
            *budget += bytes.len() as u64;
            if *budget > 512 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            let out = target.join(&relative);
            prepare_asset_parent(target, Path::new(&relative))?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)
                .map_err(|_| ERROR)?;
            file.write_all(&bytes).map_err(|_| ERROR)?;
            file.sync_all().map_err(|_| ERROR)?;
            embedded.insert(relative.clone(),json!({"original":uploads.join(&relative),"message":id,"relative":relative,"bytes":bytes.len(),"sha256":digest}));
            if embedded.len() + paths.len() > 2048 {
                return Err(ERROR.into());
            }
        }
    }
    let after = reader.get_ref().metadata().map_err(|_| ERROR)?;
    if total != before.len()
        || after.len() != before.len()
        || after.modified().map_err(|_| ERROR)? != before.modified().map_err(|_| ERROR)?
        || observed != expected
    {
        return Err(ERROR.into());
    }
    Ok((paths, embedded.into_values().collect()))
}
fn assets(
    root: &Path,
    export: &Path,
    index: usize,
    row: &Value,
    transcript: &Path,
    budget: &mut u64,
) -> Result<Value, String> {
    let uploads = plain(&root.join("android-remote-files"));
    let images = plain(&root.join("tool-images"));
    let mut result = json!({});
    let directory = format!("claude-uploads-{index}");
    let (native, mut manifest) =
        native_assets(transcript, row, &uploads, &export.join(&directory), budget)?;
    let mut files = paths(row, &uploads)?;
    for (path, message) in native {
        files.entry(path).or_insert(message);
    }
    if *budget > 512 * 1024 * 1024 {
        return Err(ERROR.into());
    }
    if !files.is_empty() || !manifest.is_empty() {
        for (path, message) in files {
            let (relative, digest, bytes) =
                copy(&uploads, &path, &export.join(&directory), 10 * 1024 * 1024)?;
            *budget += bytes;
            if *budget > 512 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            manifest.push(json!({"original":path,"message":message,"relative":relative,"bytes":bytes,"sha256":digest}));
        }
        let name = format!("claude-uploads-{index}.json");
        write(&export.join(&name), &json!({"version":1,"files":manifest}))?;
        result["uploads"] = json!({"original_root":uploads,"export":directory,"manifest":name});
    }
    let mut pictures = BTreeSet::new();
    for activity in row["thread"]["activities"].as_array().ok_or(ERROR)? {
        if let Some(paths) = activity["payload"]["data"]
            .get("toolImages")
            .filter(|v| !v.is_null())
        {
            for path in paths.as_array().ok_or(ERROR)? {
                pictures.insert(path.as_str().ok_or(ERROR)?.to_owned());
            }
        }
    }
    if pictures.len() > 2048 {
        return Err(ERROR.into());
    }
    if !pictures.is_empty() {
        let directory = format!("claude-images-{index}");
        let bucket = hash(row["thread"]["id"].as_str().ok_or(ERROR)?.as_bytes());
        for path in pictures {
            let (relative, digest, bytes) =
                copy(&images, &path, &export.join(&directory), 16 * 1024 * 1024)?;
            *budget += bytes;
            if *budget > 512 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            let relative = Path::new(&relative);
            if relative.components().count() != 2
                || relative.parent() != Some(Path::new(&bucket))
                || relative.file_stem().and_then(|s| s.to_str()) != Some(digest.as_str())
            {
                return Err(ERROR.into());
            }
        }
        result["images"] = json!({"original_root":images,"export":directory});
    }
    crate::legacy_export_assets::validate(
        row,
        "claude",
        export,
        result
            .get("uploads")
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| ERROR))
            .transpose()?
            .as_ref(),
        result
            .get("images")
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| ERROR))
            .transpose()?
            .as_ref(),
    )?;
    Ok(result)
}
pub(super) fn export(
    source: &Path,
    profile: Option<&Path>,
    target: &Path,
    connection: &mut Value,
) -> Result<Vec<Value>, String> {
    let root = source.join("managed-claude-code");
    let sessions = root.join("sessions");
    if !sessions.try_exists().map_err(|_| ERROR)? {
        return Ok(vec![]);
    }
    storage_stage::open(&sessions, true).map_err(|_| ERROR)?;
    let mut chats = Vec::new();
    let mut drafts = Vec::new();
    let mut seen = BTreeSet::new();
    let mut budget = 0;
    let deadline = std::time::Instant::now();
    for (index, file) in fs::read_dir(&sessions).map_err(|_| ERROR)?.enumerate() {
        if index >= 4096 || deadline.elapsed().as_secs() > 120 {
            return Err(ERROR.into());
        }
        let file = file.map_err(|_| ERROR)?;
        if file.path().extension().is_none_or(|s| s != "json") {
            return Err(ERROR.into());
        }
        let bytes = read(&file.path(), 16 * 1024 * 1024)?;
        budget += bytes.len() as u64;
        if budget > 512 * 1024 * 1024 {
            return Err(ERROR.into());
        }
        let mut row: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
        crate::legacy_claude::Prepared::parse(&serde_json::to_vec(&row).map_err(|_| ERROR)?)?;
        let id = row["thread"]["id"].as_str().ok_or(ERROR)?.to_owned();
        if !seen.insert(id.clone()) {
            return Err(ERROR.into());
        }
        let native = row["nativeId"].as_str().ok_or(ERROR)?;
        let profile =
            profile.ok_or("The saved Claude profile is required before upgrading these chats")?;
        let Some(path) = transcript(profile, native)? else {
            if empty(&row) {
                drafts.push(row);
                continue;
            }
            return Err("A saved Claude transcript is missing; its original cache and pairing were retained".into());
        };
        let digest = checkpoints(&path, &mut row)?;
        row["upgradePreserveUnmappedHistory"] = json!(true);
        if row["external"] == true
            && (row["bridgeId"].is_null() || row["remoteSequence"].as_u64().is_none_or(|s| s == 0))
        {
            row["upgradeNativeReplay"] = json!(true);
        }
        let mut receipt = assets(&root, target, index, &row, &path, &mut budget)?;
        let history = format!("claude-{index}.json");
        write(&target.join(&history), &row)?;
        let relative = path
            .strip_prefix(profile)
            .map_err(|_| ERROR)?
            .to_str()
            .ok_or(ERROR)?
            .replace('\\', "/");
        let map = receipt.as_object_mut().ok_or(ERROR)?;
        map.extend(json!({"agent":"claude","id":id,"history":history,"transcript":relative,"sha256":digest}).as_object().ok_or(ERROR)?.clone());
        chats.push(receipt);
    }
    connection["retainedClaudeDrafts"] = json!(drafts);
    Ok(chats)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exports_settled_cache_and_assets_without_changing_originals_and_refuses_busy_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = plain(&dir.path().canonicalize().unwrap());
        let source = root.join("old");
        let sessions = source.join("managed-claude-code/sessions");
        let uploads = source.join("managed-claude-code/android-remote-files");
        let profile = root.join("profile");
        let project = profile.join("projects/fixture");
        let relative = format!("client/{}/document.txt", uuid::Uuid::new_v4());
        for path in [
            &sessions,
            &uploads.join(&relative).parent().unwrap().to_owned(),
            &project,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let native = uuid::Uuid::new_v4().to_string();
        let input = uuid::Uuid::new_v4().to_string();
        let at = "2026-01-01T12:00:00.000Z";
        let attachment = uploads.join(&relative);
        fs::write(&attachment, b"original attachment").unwrap();
        let mut saved = json!({"version":1,"nativeId":native,"userUuids":[input],"fileCheckpoints":[],"thread":{
            "id":"phone-chat","nativeSessionId":native,"projectId":"project","title":"Saved chat","worktreePath":root,
            "modelSelection":{"instanceId":"claude-code","model":"sonnet"},"runtimeMode":"approval-required","interactionMode":"default",
            "createdAt":at,"updatedAt":at,"session":{"threadId":"phone-chat","status":"ready"},
            "messages":[{"id":"prompt","role":"user","text":"Read this","createdAt":at,"attachments":[{"path":attachment}]}],"activities":[],"proposedPlans":[]}});
        let history = sessions.join("chat.json");
        let transcript = project.join(format!("{native}.jsonl"));
        let native_row = json!({"type":"user","sessionId":native,"uuid":input,"message":{"role":"user","content":[{"type":"text","text":"Read this"}]}});
        let native_bytes = format!("{native_row}\n");
        fs::write(&transcript, &native_bytes).unwrap();
        let original = serde_json::to_vec(&saved).unwrap();
        fs::write(&history, &original).unwrap();
        let output = root.join("export");
        fs::create_dir(&output).unwrap();
        let mut connection = json!({});
        let receipts = export(&source, Some(&profile), &output, &mut connection).unwrap();
        assert_eq!(receipts.len(), 1);
        let result = read(&output.join("claude-0.json"), 1024 * 1024).unwrap();
        let prepared = crate::legacy_claude::Prepared::parse(&result).unwrap();
        assert_eq!(
            prepared.state["thread"]["messages"],
            saved["thread"]["messages"]
        );
        assert_eq!(
            fs::read(output.join("claude-uploads-0").join(&relative)).unwrap(),
            b"original attachment"
        );
        let environment = "opencodex-0123456789abcdef01234567";
        let mut store = crate::store::Store::open(&root.join("imported")).unwrap();
        let connection = json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],"threadAliases":[],"taskSelections":[]});
        store
            .prepare_legacy_connection(
                &crate::legacy_connection::Prepared::parse(
                    connection.to_string().as_bytes(),
                    environment,
                )
                .unwrap(),
            )
            .unwrap();
        store.prepare_legacy_claude(&result).unwrap();
        let snapshot = root.join("upload-snapshot");
        crate::migration_snapshot::capture(
            &output.join("claude-uploads-0"),
            &snapshot,
            environment,
        )
        .unwrap();
        let manifest = fs::read(output.join("claude-uploads-0.json")).unwrap();
        store
            .prepare_legacy_uploads("phone-chat", &uploads, &snapshot, &manifest)
            .unwrap();
        let restored = store
            .resolve_legacy_upload("phone-chat", attachment.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(restored).unwrap(), b"original attachment");
        assert_eq!(fs::read(&history).unwrap(), original);
        assert_eq!(fs::read(&transcript).unwrap(), native_bytes.as_bytes());
        assert_eq!(fs::read(&attachment).unwrap(), b"original attachment");
        saved["thread"]["hasPendingUserInput"] = json!(true);
        fs::write(&history, serde_json::to_vec(&saved).unwrap()).unwrap();
        let bad = root.join("busy");
        fs::create_dir(&bad).unwrap();
        assert!(export(&source, Some(&profile), &bad, &mut json!({}))
            .unwrap_err()
            .contains("Finish Claude tasks"));
        assert_eq!(fs::read_dir(bad).unwrap().count(), 0);
    }
    #[test]
    fn recovers_embedded_images_by_saved_uuid_without_writing_original_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let input = uuid::Uuid::new_v4().to_string();
        let image = b"exact native image bytes";
        let mut saved = json!({"nativeId":native,"userUuids":[input],"thread":{"messages":[{"id":"public-prompt","role":"user","text":"Look"}]}});
        let transcript = root.join("native.jsonl");
        let row = json!({"type":"user","sessionId":native,"uuid":input,"message":{"role":"user","content":[{"type":"text","text":"Look"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":base64::engine::general_purpose::STANDARD.encode(image)}}]}});
        fs::write(&transcript, format!("{row}\n")).unwrap();
        let uploads = plain(&root.join("original/android-remote-files"));
        let output = root.join("export");
        let mut budget = 0;
        let (_, files) =
            native_assets(&transcript, &saved, &uploads, &output, &mut budget).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["message"], "public-prompt");
        assert_eq!(
            fs::read(output.join(files[0]["relative"].as_str().unwrap())).unwrap(),
            image
        );
        assert!(!uploads.exists());
        assert_eq!(budget, image.len() as u64);
        saved["userUuids"] = json!([uuid::Uuid::new_v4().to_string()]);
        assert!(native_assets(&transcript, &saved, &uploads, &root.join("bad"), &mut 0).is_err());
    }
    #[test]
    fn keeps_only_native_proven_file_restore_points_and_retains_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let row = json!({"type":"file-history-snapshot","messageId":"proved","sessionId":"native"});
        let bytes = format!("{row}\n");
        fs::write(&path, &bytes).unwrap();
        let mut saved =
            json!({"nativeId":"native","fileCheckpoints":[{"id":"proved"},{"id":"unproved"}]});
        assert_eq!(
            checkpoints(&path, &mut saved).unwrap(),
            hash(bytes.as_bytes())
        );
        assert_eq!(saved["fileCheckpoints"], json!([{"id":"proved"}]));
        assert_eq!(saved["retainedFileCheckpoints"], json!([{"id":"unproved"}]));
        saved["nativeId"] = json!("other");
        assert!(checkpoints(&path, &mut saved).is_err());
    }
    #[test]
    fn copies_only_an_explicit_asset_inside_its_original_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = plain(&dir.path().canonicalize().unwrap());
        let source = root.join("assets");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("image.png"), b"original").unwrap();
        let result = copy(
            &source,
            source.join("image.png").to_str().unwrap(),
            &root.join("copy"),
            100,
        )
        .unwrap();
        assert_eq!(result.1, hash(b"original"));
        fs::write(root.join("outside"), b"outside").unwrap();
        assert!(copy(
            &source,
            root.join("outside").to_str().unwrap(),
            &root.join("other"),
            100
        )
        .is_err());
    }
}
