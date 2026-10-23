//! Materialize an owned recovery session before starting Remote Control. CLI
//! --fork-session is deferred until the first prompt, which is too late: an
//! idle shared handshake would otherwise attach to the source session.
use crate::{branches::Fork, chat_projection::s};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_ROW: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;
const SEAL: &str = "remodex-recovery-checkpoint";

fn identities(target: &str, fork: &Fork) -> Result<(), String> {
    for id in [target, &fork.session, &fork.point] {
        uuid::Uuid::parse_str(id).map_err(|_| "Invalid Claude recovery identity")?;
    }
    if target == fork.session {
        return Err("Recovery must keep the original Claude session separate".into());
    }
    Ok(())
}
fn open(path: &Path) -> Result<File, String> {
    if !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
        return Err("Recovery history is not a regular file".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot inspect recovery history")?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect recovery history")?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err("Recovery history exceeds its verification budget".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("Recovery history cannot be a reparse point".into());
        }
    }
    Ok(file)
}
// Two bounded passes retain only UUID links, never the conversation bodies.
fn scan(
    path: &Path,
    mut visit: impl FnMut(&[u8], Value) -> Result<(), String>,
) -> Result<String, String> {
    let file = open(path)?;
    let size = file.metadata().map_err(|_| "History unavailable")?.len();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut row = Vec::new();
    let mut total = 0u64;
    let mut hash = Sha256::new();
    loop {
        row.clear();
        let n = reader
            .by_ref()
            .take(MAX_ROW + 1)
            .read_until(b'\n', &mut row)
            .map_err(|_| "Cannot read recovery history")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if n as u64 > MAX_ROW || row.last() != Some(&b'\n') || total > size {
            return Err(
                "Recovery history is incomplete or changing; nothing was overwritten".into(),
            );
        }
        hash.update(&row);
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = serde_json::from_slice(&row)
            .map_err(|_| "Recovery history is invalid; nothing was overwritten")?;
        if !v.is_object() {
            return Err("Invalid recovery history entry".into());
        }
        visit(&row, v)?;
    }
    if total != size {
        return Err("Recovery history changed; retry safely".into());
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn ready(profile: &Path, target: &str, fork: &Fork) -> Result<bool, String> {
    identities(target, fork)?;
    let Some(path) = crate::branches::transcript_optional(profile, target)? else {
        return Ok(false);
    };
    let mut hash = Sha256::new();
    let mut count = 0usize;
    let mut sealed = false;
    let mut point = false;
    scan(&path, |raw, v| {
        if v.get("sessionId").is_some() && v["sessionId"] != target {
            return Err("Recovery history belongs to another session".into());
        }
        if v["type"] == SEAL {
            if sealed
                || v["source"] != fork.session
                || v["point"] != fork.point
                || v["sessionId"] != target
                || v["version"] != 1
                || v["rows"] != count
                || v["sha256"] != format!("{:x}", hash.clone().finalize())
                || !point
            {
                return Err(
                    "Recovery history checkpoint does not match; it was not overwritten".into(),
                );
            }
            sealed = true;
        } else if !sealed {
            hash.update(raw);
            count += 1;
            point |= v["forkedFrom"]["sessionId"] == fork.session
                && v["forkedFrom"]["messageUuid"] == fork.point;
        }
        Ok(())
    })?;
    if !sealed {
        return Err(
            "Recovery history is incomplete; the original is intact and no session was overwritten"
                .into(),
        );
    }
    Ok(true)
}

#[derive(Clone)]
struct Link {
    parent: Option<String>,
    progress: bool,
}
fn is_message(v: &Value) -> bool {
    matches!(
        s(&v["type"]),
        "user" | "assistant" | "system" | "attachment" | "progress"
    ) && v["isSidechain"] != true
}
fn remap(target: &str, old: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(target.as_bytes());
    hash.update([0]);
    hash.update(old.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize()[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
fn parent(target: &str, link: &Link, links: &HashMap<String, Link>) -> Result<Value, String> {
    let mut p = link.parent.as_deref();
    let mut seen = HashSet::new();
    while let Some(id) = p {
        if !seen.insert(id) {
            return Err("Claude history has a cyclic parent chain".into());
        }
        let Some(next) = links.get(id) else {
            return Ok(Value::Null);
        };
        if !next.progress {
            return Ok(json!(remap(target, id)));
        }
        p = next.parent.as_deref();
    }
    Ok(Value::Null)
}
fn map_reference(value: &mut Value, target: &str, links: &HashMap<String, Link>) {
    if let Some(items) = value.as_array_mut() {
        for item in items {
            map_reference(item, target, links);
        }
    } else if let Some(id) = value.as_str().filter(|id| links.contains_key(*id)) {
        *value = json!(remap(target, id));
    }
}
fn transform(
    mut v: Value,
    target: &str,
    fork: &Fork,
    links: &HashMap<String, Link>,
) -> Result<Value, String> {
    let old = s(&v["uuid"]).to_owned();
    v["uuid"] = json!(remap(target, &old));
    v["parentUuid"] = parent(
        target,
        links
            .get(&old)
            .ok_or("Claude history changed while preparing recovery")?,
        links,
    )?;
    if v.get("logicalParentUuid").is_some() {
        v["logicalParentUuid"] = v["logicalParentUuid"]
            .as_str()
            .filter(|id| links.contains_key(*id))
            .map(|id| json!(remap(target, id)))
            .unwrap_or(Value::Null);
    }
    v["sessionId"] = json!(target);
    v["isSidechain"] = json!(false);
    v["forkedFrom"] = json!({"sessionId":fork.session,"messageUuid":old});
    for key in [
        "teamName",
        "agentName",
        "sessionKind",
        "slug",
        "sourceToolAssistantUUID",
        "summarizedTurnPromptUuids",
    ] {
        v.as_object_mut().unwrap().remove(key);
    }
    if v["type"] == "system" && v["subtype"] == "model_refusal_fallback" {
        v["neutralizedByFork"] = json!(true);
    }
    if let Some(compact) = v.get_mut("compactMetadata").and_then(Value::as_object_mut) {
        compact.remove("runningTurnSends");
        for (group, fields) in [
            (
                "preservedMessages",
                &["anchorUuid", "uuids", "allUuids"][..],
            ),
            (
                "preservedSegment",
                &["headUuid", "anchorUuid", "tailUuid"][..],
            ),
        ] {
            if let Some(obj) = compact.get_mut(group).and_then(Value::as_object_mut) {
                for field in fields {
                    if let Some(value) = obj.get_mut(*field) {
                        map_reference(value, target, links);
                    }
                }
            }
        }
    }
    if let Some(a) = v.get_mut("attachment").and_then(Value::as_object_mut) {
        if let Some(value) = a.get_mut("source_uuid") {
            map_reference(value, target, links);
        }
        if a.get("type").and_then(Value::as_str) == Some("deferred_tools_record") {
            if let Some(names) = a
                .get_mut("nameOnlyAnnouncements")
                .and_then(Value::as_array_mut)
            {
                names.retain(|v| v.as_str().is_some_and(|id| links.contains_key(id)));
                for name in names {
                    map_reference(name, target, links);
                }
            }
        }
    }
    Ok(v)
}
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) fn materialize(profile: &Path, target: &str, fork: &Fork) -> Result<(), String> {
    if ready(profile, target, fork)? {
        return Ok(());
    }
    let source = crate::branches::transcript(profile, &fork.session)?;
    let mut links = HashMap::new();
    let mut last = String::new();
    let hash = scan(&source, |_, v| {
        if !is_message(&v) {
            return Ok(());
        }
        if v["sessionId"] != fork.session {
            return Err("Claude source history contains a different session".into());
        }
        let id = s(&v["uuid"]);
        uuid::Uuid::parse_str(id).map_err(|_| "Invalid source history identity")?;
        if links.len() >= MAX_ENTRIES {
            return Err("Recovery history exceeds the entry budget".into());
        }
        let progress = v["type"] == "progress";
        links.insert(
            id.to_owned(),
            Link {
                parent: v["parentUuid"].as_str().map(str::to_owned),
                progress,
            },
        );
        if !progress {
            last = id.into();
        }
        Ok(())
    })?;
    // Recovery is only for an idle owner. Refuse newly arrived work rather than
    // silently forking a stale prefix (including compaction made after the pin).
    if last != fork.point {
        return Err(
            "Claude history advanced after recovery was requested; no new link was opened".into(),
        );
    }
    let stage = Staged(source.with_file_name(format!(
        ".remodex-recovery-{target}-{}.tmp",
        uuid::Uuid::new_v4()
    )));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options
        .open(&stage.0)
        .map_err(|_| "Cannot stage Claude recovery history")?;
    let mut out_hash = Sha256::new();
    let mut count = 0usize;
    let check_hash = scan(&source, |_, mut v| {
        if is_message(&v) && v["type"] != "progress" {
            v = transform(v, target, fork, &links)?;
        } else if matches!(
            s(&v["type"]),
            "history-suppression" | "content-replacement" | "atis-latch" | "relocated"
        ) {
            if v.get("sessionId").is_some() && v["sessionId"] != fork.session {
                return Err("Foreign Claude history metadata".into());
            }
            v["sessionId"] = json!(target);
            if v.get("uuid").is_some() {
                v["uuid"] = json!(uuid::Uuid::new_v4().to_string());
            }
        } else {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec(&v).map_err(|_| "Cannot encode recovery history")?;
        bytes.push(b'\n');
        out.write_all(&bytes)
            .map_err(|_| "Cannot save recovery history")?;
        out_hash.update(&bytes);
        count += 1;
        Ok(())
    })?;
    if hash != check_hash {
        return Err("Claude source history changed; no replacement was installed".into());
    }
    let seal = json!({"type":SEAL,"version":1,"sessionId":target,"source":fork.session,"point":fork.point,"sourceSha256":hash,"rows":count,"sha256":format!("{:x}",out_hash.finalize())});
    writeln!(out, "{seal}")
        .and_then(|_| out.sync_all())
        .map_err(|_| "Cannot commit recovery history")?;
    drop(out);
    let destination = source.with_file_name(format!("{target}.jsonl"));
    // A hard link is an atomic create-without-replace on Linux, macOS and NTFS.
    // Unsupported filesystems fail safely; neither history file is overwritten.
    match std::fs::hard_link(&stage.0, &destination) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {
            return Err("Cannot atomically install recovery history on this filesystem".into())
        }
    }
    #[cfg(unix)]
    File::open(destination.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(|_| "Cannot persist recovery history directory")?;
    if !ready(profile, target, fork)? {
        return Err("Recovery history was not committed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Fork, String, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/fixture");
        std::fs::create_dir_all(&folder).unwrap();
        let fork = Fork {
            session: uuid::Uuid::new_v4().to_string(),
            point: uuid::Uuid::new_v4().to_string(),
        };
        let target = uuid::Uuid::new_v4().to_string();
        let path = folder.join(format!("{}.jsonl", fork.session));
        (dir, fork, target, path)
    }
    #[test]
    fn materialization_keeps_tools_images_compaction_and_source_bytes_without_old_links() {
        let (dir, fork, target, source) = fixture();
        let user = uuid::Uuid::new_v4().to_string();
        let progress = uuid::Uuid::new_v4().to_string();
        let rows = [
            json!({"type":"user","uuid":user,"parentUuid":null,"sessionId":fork.session,"message":{"content":[{"type":"image","source":{"type":"base64","data":"keep-bytes"}}]}}),
            json!({"type":"progress","uuid":progress,"parentUuid":user,"sessionId":fork.session}),
            json!({"type":"system","uuid":fork.point,"parentUuid":progress,"sessionId":fork.session,"compactMetadata":{"preservedMessages":{"anchorUuid":user,"uuids":[user]},"runningTurnSends":["discard"]},"message":{"content":[{"type":"tool_result","tool_use_id":"tool-identity-stays","content":"exact output"}]}}),
            json!({"type":"bridge-session","sessionId":fork.session,"bridgeSessionId":"cse_old_link"}),
            json!({"type":"file-history-snapshot","snapshot":{"trackedFileBackups":{"keep-original":"old"}}}),
            json!({"type":"content-replacement","sessionId":fork.session,"replacements":[{"hash":"preserved"}]}),
        ];
        let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
        std::fs::write(&source, &bytes).unwrap();
        materialize(dir.path(), &target, &fork).unwrap();
        let dst = source.with_file_name(format!("{target}.jsonl"));
        let copied = std::fs::read_to_string(&dst).unwrap();
        let values: Vec<Value> = copied
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(values.len(), 4);
        assert_eq!(values[0]["message"], rows[0]["message"]);
        assert_eq!(values[1]["message"], rows[2]["message"]);
        assert_eq!(values[1]["parentUuid"], values[0]["uuid"]);
        assert_eq!(
            values[1]["compactMetadata"]["preservedMessages"]["anchorUuid"],
            values[0]["uuid"]
        );
        assert!(values[1]["compactMetadata"]["runningTurnSends"].is_null());
        assert!(!copied.contains("cse_old_link") && !copied.contains("file-history-snapshot"));
        materialize(dir.path(), &target, &fork).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), copied);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), bytes);
        // Native appends are permitted, but do not weaken the sealed prefix.
        writeln!(
            OpenOptions::new().append(true).open(&dst).unwrap(),
            "{}",
            json!({"type":"user","sessionId":target,"message":{"content":"new"}})
        )
        .unwrap();
        assert!(ready(dir.path(), &target, &fork).unwrap());
    }
    #[test]
    fn partial_wrong_or_modified_destinations_never_get_overwritten() {
        let (dir, fork, target, source) = fixture();
        std::fs::write(&source, format!("{}\n", json!({"type":"assistant","uuid":fork.point,"sessionId":fork.session,"parentUuid":null}))).unwrap();
        let path = source.with_file_name(format!("{target}.jsonl"));
        for bad in [
            String::new(),
            "{\"partial\":".into(),
            format!("{}\n", json!({"sessionId":fork.session})),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(materialize(dir.path(), &target, &fork).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
        std::fs::remove_file(&path).unwrap();
        materialize(dir.path(), &target, &fork).unwrap();
        let corrupted = std::fs::read_to_string(&path)
            .unwrap()
            .replacen("assistant", "user", 1);
        std::fs::write(&path, &corrupted).unwrap();
        assert!(ready(dir.path(), &target, &fork).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupted);
    }
    #[test]
    fn source_advance_cycles_and_partial_rows_cannot_start_empty_replacements() {
        let (dir, fork, target, source) = fixture();
        for bad in [
            "{\"partial\":".to_owned(),
            format!(
                "{}\n",
                json!({"type":"assistant","uuid":uuid::Uuid::new_v4().to_string(),"sessionId":fork.session,"parentUuid":null})
            ),
            format!(
                "{}\n{}\n",
                json!({"type":"progress","uuid":target,"sessionId":fork.session,"parentUuid":target}),
                json!({"type":"assistant","uuid":fork.point,"sessionId":fork.session,"parentUuid":target})
            ),
        ] {
            std::fs::write(&source, &bad).unwrap();
            assert!(materialize(dir.path(), &target, &fork).is_err());
            assert!(!source.with_file_name(format!("{target}.jsonl")).exists());
            assert_eq!(std::fs::read_to_string(&source).unwrap(), bad);
        }
    }
}
