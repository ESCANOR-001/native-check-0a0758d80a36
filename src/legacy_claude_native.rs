//! Stage a verified native Claude history export without starting a worker or
//! writing to the original CLI profile. Unknown exported files remain intact.
use crate::{migration_snapshot, profile_snapshot, storage_stage::open, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Component, Path},
    time::{Duration, Instant},
};

const INVALID: &str = "Claude native history or file checkpoints are incomplete or changed; the original profile remains untouched and migration stays offline";
const MAX_ROW: u64 = 8 * 1024 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Report {
    sha256: String,
    bytes: u64,
    rows: usize,
    prompts: usize,
    replies: usize,
    checkpoints: usize,
}
fn fingerprint(path: &Path) -> Result<String, String> {
    let raw = profile_snapshot::bytes(path, MAX_ROW).map_err(|_| INVALID)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| INVALID)?;
    Ok(crate::auth::digest(&value.to_string()))
}
fn relative_path(path: &str, native: &str) -> Result<(), String> {
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 3
        || parts[0] != "projects"
        || parts[1].is_empty()
        || parts[2] != format!("{native}.jsonl")
        || path.len() > 2048
        || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(INVALID.into());
    }
    Ok(())
}
struct Required {
    users: BTreeSet<String>,
    replies: BTreeSet<String>,
    checkpoints: BTreeSet<String>,
}
fn required(raw: &Value) -> Result<Required, String> {
    let mut users = BTreeSet::new();
    let mut replies = BTreeSet::new();
    for message in raw["thread"]["messages"].as_array().ok_or(INVALID)? {
        let id = message["id"].as_str().ok_or(INVALID)?;
        match message["role"].as_str() {
            Some("user") => {
                let mapped = raw["deliveries"]
                    .as_array()
                    .and_then(|v| v.iter().find(|d| d["messageId"] == id));
                if mapped.is_some_and(|d| d["state"] == "cancelled") {
                    continue;
                }
                let native = mapped.and_then(|d| d["uuid"].as_str()).unwrap_or(id);
                uuid::Uuid::parse_str(native).map_err(|_| "An older prompt has no native identity in this export; reconcile its original delivery ledger before upgrading")?;
                users.insert(native.to_owned());
            }
            Some("assistant") => {
                replies.insert(id.to_owned());
            }
            _ => return Err(INVALID.into()),
        }
    }
    let checkpoints = raw["fileCheckpoints"]
        .as_array()
        .map(|v| {
            v.iter()
                .map(|p| p["id"].as_str().map(str::to_owned).ok_or(INVALID))
                .collect()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(Required {
        users,
        replies,
        checkpoints,
    })
}
fn inspect(data: &Path, relative: &str, raw: &[u8]) -> Result<Report, String> {
    let source: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    if source["external"] == true {
        return Err("Externally owned Claude history must remain with its owner".into());
    }
    let native = source["nativeId"].as_str().ok_or(INVALID)?;
    uuid::Uuid::parse_str(native).map_err(|_| INVALID)?;
    relative_path(relative, native)?;
    let Required {
        mut users,
        mut replies,
        mut checkpoints,
    } = required(&source)?;
    let (prompts, reply_count, checkpoint_count) = (users.len(), replies.len(), checkpoints.len());
    let path = data.join(relative);
    open(data, true).map_err(|_| INVALID)?;
    open(path.parent().ok_or(INVALID)?, true).map_err(|_| INVALID)?;
    let (file, meta) = profile_snapshot::open(&path, MAX_FILE).map_err(|_| INVALID)?;
    let identity = crate::storage_stage::identity(&file).map_err(|_| INVALID)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut row = Vec::new();
    let mut bytes = 0;
    let mut rows = 0;
    let mut hash = Sha256::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        row.clear();
        let n = reader
            .by_ref()
            .take(MAX_ROW + 1)
            .read_until(b'\n', &mut row)
            .map_err(|_| INVALID)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        rows += 1;
        if n as u64 > MAX_ROW
            || bytes > meta.len()
            || rows > 200_000
            || row.last() != Some(&b'\n')
            || Instant::now() >= deadline
        {
            return Err(INVALID.into());
        }
        hash.update(&row);
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = serde_json::from_slice(&row).map_err(|_| INVALID)?;
        if !v.is_object() || (v.get("sessionId").is_some() && v["sessionId"] != native) {
            return Err(INVALID.into());
        }
        if v["isSidechain"] == true {
            continue;
        }
        if v["type"] == "user"
            && v["isMeta"] != true
            && v["isSynthetic"] != true
            && v["message"]["role"] == "user"
        {
            if let Some(id) = v["uuid"].as_str() {
                users.remove(id);
            }
        }
        if v["type"] == "assistant" && v["message"]["role"] == "assistant" {
            if let Some(id) = v["message"]["id"].as_str() {
                replies.remove(id);
            }
        }
        if v["type"] == "file-history-snapshot" {
            let snapshot = v["snapshot"].as_object().ok_or(INVALID)?;
            let tracked = snapshot
                .get("trackedFileBackups")
                .and_then(Value::as_object)
                .ok_or(INVALID)?;
            if tracked.len() > 20_000 {
                return Err(INVALID.into());
            }
            for backup in tracked.values() {
                let name = backup
                    .as_object()
                    .and_then(|v| v.get("backupFileName"))
                    .ok_or(INVALID)?;
                if name.is_null() {
                    continue;
                } // a file that did not exist yet
                let name = name
                    .as_str()
                    .filter(|s| {
                        !s.is_empty()
                            && s.len() <= 256
                            && s.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
                            && *s != "."
                            && *s != ".."
                    })
                    .ok_or(INVALID)?;
                let folder = data.join("file-history").join(native);
                open(&folder, true).map_err(|_| INVALID)?;
                open(&folder.join(name), false).map_err(|_| INVALID)?;
            }
            if let Some(id) = v["messageId"].as_str() {
                checkpoints.remove(id);
            }
        }
    }
    let (after, end) = profile_snapshot::open(&path, MAX_FILE).map_err(|_| INVALID)?;
    if bytes != meta.len()
        || bytes == 0
        || end.len() != bytes
        || crate::storage_stage::identity(&after).map_err(|_| INVALID)? != identity
        || !users.is_empty()
        || !replies.is_empty()
        || !checkpoints.is_empty()
    {
        return Err(INVALID.into());
    }
    Ok(Report {
        sha256: format!("{:x}", hash.finalize()),
        bytes,
        rows,
        prompts,
        replies: reply_count,
        checkpoints: checkpoint_count,
    })
}
fn exists(db: &rusqlite::Connection) -> Result<bool, String> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='legacy_claude_native' AND type='table')", [], |r|r.get(0)).map_err(|_| INVALID.into())
}
// Include incomplete copies left by a crash, not only committed receipts.
// This staging area cannot grow without bounds across failed imports.
fn staged_bytes(root: &Path) -> Result<u64, String> {
    let mut pending = vec![(root.to_owned(), 0)];
    let mut bytes = 0u64;
    let mut entries = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Some((dir, depth)) = pending.pop() {
        open(&dir, true).map_err(|_| INVALID)?;
        for entry in fs::read_dir(dir).map_err(|_| INVALID)? {
            let entry = entry.map_err(|_| INVALID)?;
            entries += 1;
            if entries > 80_000 || depth >= 64 || Instant::now() >= deadline {
                return Err(INVALID.into());
            }
            let is_dir = entry.file_type().map_err(|_| INVALID)?.is_dir();
            let file = open(&entry.path(), is_dir).map_err(|_| INVALID)?;
            if is_dir {
                pending.push((entry.path(), depth + 1));
            } else {
                bytes = bytes
                    .checked_add(file.metadata().map_err(|_| INVALID)?.len())
                    .ok_or(INVALID)?;
                if bytes > 2 * 1024 * 1024 * 1024 {
                    return Err(INVALID.into());
                }
            }
        }
    }
    Ok(bytes)
}
pub(crate) fn verify(db: &rusqlite::Connection, directory: &Path, id: &str) -> Result<(), String> {
    if !exists(db)? {
        return Ok(());
    }
    let record: Option<(String,String,String,String,String)> = db.query_row("SELECT source_hash,bundle,transcript,report,phase FROM legacy_claude_native WHERE chat=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_| INVALID)?;
    let Some((source_hash, bundle, transcript, report, phase)) = record else {
        return Ok(());
    };
    if phase == "rolled-back" {
        return Ok(());
    }
    if phase != "prepared"
        || bundle.len() != 64
        || !bundle
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(INVALID.into());
    }
    let (hash, raw): (String, Vec<u8>) = db
        .query_row(
            "SELECT digest,raw FROM legacy_claude_import WHERE id=?1 AND phase='prepared'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| INVALID)?;
    if hash != source_hash
        || crate::auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != hash
    {
        return Err(INVALID.into());
    }
    let environment: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    let restored = directory.join("legacy-native").join(bundle.clone());
    migration_snapshot::verify_restored(&restored, &environment).map_err(|_| INVALID)?;
    if fingerprint(&restored.join("restored.json"))? != bundle {
        return Err(INVALID.into());
    }
    let expected = inspect(&restored.join("data"), &transcript, &raw)?;
    if serde_json::from_str::<Report>(&report).map_err(|_| INVALID)? != expected {
        return Err(INVALID.into());
    }
    Ok(())
}
pub(crate) fn rollback(
    db: &rusqlite::Connection,
    directory: &Path,
    id: &str,
) -> Result<(), String> {
    verify(db, directory, id)?;
    if exists(db)? {
        db.execute(
            "UPDATE legacy_claude_native SET phase='rolled-back' WHERE chat=?1",
            [id],
        )
        .map_err(|_| INVALID)?;
    }
    // Preserve the verified export for rollback and other imported chats. Never
    // erase a bundle that may also contain unrelated, unknown source files.
    Ok(())
}
impl Store {
    /// Accepts only an explicitly captured offline snapshot. No original profile
    /// is discovered, no credentials are activated, and no CLI is launched.
    pub fn prepare_legacy_claude_native(
        &mut self,
        id: &str,
        snapshot: &Path,
        transcript: &str,
    ) -> Result<Value, String> {
        let prepared = self.legacy_claude_prepared(id)?;
        crate::legacy_claude_bindings::validate(&prepared.source)?;
        required(&serde_json::from_slice(&prepared.source).map_err(|_| INVALID)?)?;
        let native = prepared.state["native_id"].as_str().ok_or(INVALID)?;
        relative_path(transcript, native)?;
        if prepared.state["external_claude"] == true {
            return Err("Externally owned Claude history must remain with its owner".into());
        }
        let environment = self.environment_id().map_err(|_| INVALID)?;
        let summary = migration_snapshot::verify(snapshot, &environment).map_err(|_| INVALID)?;
        let bundle = fingerprint(&snapshot.join("snapshot.json"))?;
        let parent = self.directory.join("legacy-native");
        if !parent.exists() {
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let mut builder = builder;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&parent).map_err(|_| INVALID)?;
        }
        open(&parent, true).map_err(|_| INVALID)?;
        let restored = parent.join(&bundle);
        if restored.exists() {
            migration_snapshot::verify_restored(&restored, &environment).map_err(|_| INVALID)?;
        } else {
            let retained = staged_bytes(&parent)?;
            let requested = retained
                .saturating_add(summary.bytes)
                .saturating_add(MAX_ROW);
            if requested > 2 * 1024 * 1024 * 1024 {
                return Err(INVALID.into());
            }
            self.check_history(requested)?;
            migration_snapshot::restore(snapshot, &restored, &environment).map_err(|_| INVALID)?;
        }
        if fingerprint(&restored.join("restored.json"))? != bundle {
            return Err(INVALID.into());
        }
        let report = inspect(&restored.join("data"), transcript, &prepared.source)?;
        // Reverify the entire bundle after inspecting JSON and backup references.
        migration_snapshot::verify_restored(&restored, &environment).map_err(|_| INVALID)?;
        let encoded = serde_json::to_string(&report).map_err(|_| INVALID)?;
        self.check_history(encoded.len() as u64 + 65536)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_claude_native(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,bundle TEXT NOT NULL,transcript TEXT NOT NULL,report TEXT NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let old: Option<(String,String,String,String,String)>=tx.query_row("SELECT source_hash,bundle,transcript,report,phase FROM legacy_claude_native WHERE chat=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_|INVALID)?;
        if let Some(old) = old {
            if old
                != (
                    prepared.hash.clone(),
                    bundle.clone(),
                    transcript.to_owned(),
                    encoded.clone(),
                    "prepared".into(),
                )
            {
                return Err(INVALID.into());
            }
        } else {
            tx.execute(
                "INSERT INTO legacy_claude_native VALUES(?1,?2,?3,?4,?5,'prepared')",
                params![id, prepared.hash, bundle, transcript, encoded],
            )
            .map_err(|_| INVALID)?;
        }
        verify(&tx, &self.directory, id)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(
            json!({"phase":"claude-native-history-prepared","rows":report.rows,"bytes":report.bytes,"prompts":report.prompts,"replies":report.replies,"fileCheckpoints":report.checkpoints,"migrationComplete":false}),
        )
    }
}
