//! Offline, chat-scoped attachment relocation. Public messages and their links
//! stay byte-exact; only a verified resource alias points at the retained copy.
use crate::{auth, migration_snapshot, profile_snapshot, storage_stage::open, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const INVALID: &str = "Legacy attachment export is incomplete or changed; original messages and files were not replaced";
const MAX_PLAN: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 2048;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct File {
    pub(crate) message: String,
    pub(crate) original: String,
    pub(crate) relative: String,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    files: Vec<File>,
}
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    crate::legacy_codex_headers::table(db, "legacy_uploads")
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn absolute(p: &Path) -> bool {
    p.is_absolute()
        && p.to_str()
            .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
        && !p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}
fn shape(file: &File, root: &Path) -> Result<(), String> {
    let parts: Vec<_> = file.relative.split('/').collect();
    if parts.len() != 3
        || parts.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || s.contains(['\\', ':'])
                || s.chars().any(char::is_control)
        })
        || parts[0].len() > 256
        || !parts[0]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || uuid::Uuid::parse_str(parts[1]).is_err()
        || parts[2].len() > 256
        || !parts[2]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._ -".contains(&b))
        || !digest(&file.sha256)
        || file.bytes == 0
        || file.bytes > crate::attachments::MAX_FILE as u64
        || file.message.is_empty()
        || file.message.len() > 256
        || !absolute(Path::new(&file.original))
        || Path::new(&file.original) != root.join(&file.relative)
    {
        return Err(INVALID.into());
    }
    Ok(())
}
fn source(db: &rusqlite::Connection, id: &str) -> Result<(String, Value), String> {
    let agent: String = db
        .query_row("SELECT agent FROM chats WHERE id=?1", [id], |r| r.get(0))
        .map_err(|_| INVALID)?;
    let (table, key) = match agent.as_str() {
        "claude" => ("legacy_claude_import", "id"),
        "codex" => ("legacy_codex_history", "chat"),
        _ => return Err(INVALID.into()),
    };
    let (hash, raw): (String, Vec<u8>) = db
        .query_row(
            &format!("SELECT digest,raw FROM {table} WHERE {key}=?1 AND phase='prepared'"),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| INVALID)?;
    if raw.len() > 16 * 1024 * 1024
        || auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != hash
    {
        return Err(INVALID.into());
    }
    let mut raw: Value = serde_json::from_slice(&raw).map_err(|_| INVALID)?;
    Ok((
        hash,
        if agent == "claude" {
            raw["thread"].take()
        } else {
            raw
        },
    ))
}
pub(crate) fn referenced(message: &Value, original: &str) -> bool {
    message["attachments"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|r| r["path"] == original))
        || message["text"].as_str().is_some_and(|text| {
            // Match complete Markdown destinations, never substrings or bare
            // prose paths. The export is also restricted to the old upload root.
            text.contains(&format!("](<{original}>)")) || text.contains(&format!("]({original})"))
        })
}
fn plan(raw: &[u8], root: &Path, thread: &Value) -> Result<Vec<File>, String> {
    if raw.len() > MAX_PLAN || !absolute(root) {
        return Err(INVALID.into());
    }
    let p: Plan = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    if p.version != 1 || p.files.len() > MAX_FILES {
        return Err(INVALID.into());
    }
    let messages: BTreeMap<_, _> = thread["messages"]
        .as_array()
        .ok_or(INVALID)?
        .iter()
        .filter_map(|m| m["id"].as_str().map(|id| (id, m)))
        .collect();
    let mut paths = BTreeSet::new();
    for f in &p.files {
        shape(f, root)?;
        if !paths.insert(&f.original) || !messages.contains_key(f.message.as_str()) {
            return Err(INVALID.into());
        }
    }
    Ok(p.files)
}
fn checked(root: &Path, path: &Path, file: &File) -> Result<(), String> {
    // Verify each ancestor without following reparse points/symlinks.
    if !path.starts_with(root) {
        return Err(INVALID.into());
    }
    for parent in path.ancestors().skip(1) {
        open(parent, true).map_err(|_| INVALID)?;
        if parent == root {
            break;
        }
    }
    let (mut input, meta) =
        profile_snapshot::open(path, crate::attachments::MAX_FILE as u64).map_err(|_| INVALID)?;
    if meta.len() != file.bytes {
        return Err(INVALID.into());
    }
    let identity = crate::storage_stage::identity(&input).map_err(|_| INVALID)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            return Err(INVALID.into());
        }
        let n = input.read(&mut buffer).map_err(|_| INVALID)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > file.bytes {
            return Err(INVALID.into());
        }
        hash.update(&buffer[..n]);
    }
    let after = open(path, false).map_err(|_| INVALID)?;
    if total != file.bytes
        || format!("{:x}", hash.finalize()) != file.sha256
        || crate::storage_stage::identity(&after).map_err(|_| INVALID)? != identity
    {
        return Err(INVALID.into());
    }
    Ok(())
}
fn manifest(path: &Path) -> Result<String, String> {
    Ok(auth::digest(&manifest_json(path)?))
}
fn manifest_json(path: &Path) -> Result<String, String> {
    let bytes = profile_snapshot::bytes(path, 8 * 1024 * 1024).map_err(|_| INVALID)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
    Ok(v.to_string())
}
type Receipt = (String, String, String, Vec<u8>, String);
fn receipt(db: &rusqlite::Connection, id: &str) -> Result<Option<Receipt>, String> {
    if !table(db)? {
        return Ok(None);
    }
    db.query_row(
        "SELECT source_hash,original_root,bundle,plan,phase FROM legacy_uploads WHERE chat=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()
    .map_err(|_| INVALID.into())
}
pub(crate) fn verify(db: &rusqlite::Connection, directory: &Path, id: &str) -> Result<(), String> {
    let Some((hash, root, bundle, raw, phase)) = receipt(db, id)? else {
        return Ok(());
    };
    if phase == "rolled-back" {
        return Ok(());
    }
    if phase != "prepared" || !digest(&bundle) {
        return Err(INVALID.into());
    }
    let (expected, thread) = source(db, id)?;
    if expected != hash {
        return Err(INVALID.into());
    }
    let files = plan(&raw, Path::new(&root), &thread)?;
    crate::legacy_upload_native::verify(db, directory, id, &thread, &files)?;
    let path = directory.join("legacy-uploads").join(&bundle);
    let environment: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    migration_snapshot::verify_restored(&path, &environment).map_err(|_| INVALID)?;
    if manifest(&path.join("restored.json"))? != bundle {
        return Err(INVALID.into());
    }
    for f in &files {
        checked(directory, &path.join("data").join(&f.relative), f)?;
    }
    crate::legacy_upload_display::verify(db, directory, id, &bundle, &files)?;
    Ok(())
}
pub(crate) fn rollback(
    db: &rusqlite::Connection,
    directory: &Path,
    id: &str,
) -> Result<(), String> {
    verify(db, directory, id)?;
    if table(db)? {
        db.execute(
            "UPDATE legacy_uploads SET phase='rolled-back' WHERE chat=?1 AND phase='prepared'",
            [id],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(())
}

/// Retain verified recovery copies after child-chat rollback. This grants no
/// resource access and never reads the original upload directory.
pub(crate) fn retained_asset_rows(
    db: &rusqlite::Connection,
    directory: &Path,
    environment: &str,
) -> Result<BTreeMap<String, i64>, String> {
    let mut result = BTreeMap::new();
    if !table(db)? {
        return Ok(result);
    }
    let mut query = db
        .prepare("SELECT chat FROM legacy_uploads ORDER BY chat LIMIT 1025")
        .map_err(|_| INVALID)?;
    let ids = query
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| INVALID)?;
    let mut bundles = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(300);
    for (count, id) in ids.enumerate() {
        if count >= 1024 || Instant::now() >= deadline {
            return Err(INVALID.into());
        }
        let id = id.map_err(|_| INVALID)?;
        let (hash, root, bundle, raw, phase) = receipt(db, &id)?.ok_or(INVALID)?;
        if phase != "rolled-back" || !digest(&bundle) {
            return Err(INVALID.into());
        }
        let mut source = None;
        for (table, key, claude) in [
            ("legacy_codex_history", "chat", false),
            ("legacy_claude_import", "id", true),
        ] {
            if !crate::legacy_codex_headers::table(db, table)? {
                continue;
            }
            let saved: Option<(String,Vec<u8>)> = db.query_row(
                &format!("SELECT digest,raw FROM {table} WHERE {key}=?1 AND phase='rolled-back' AND length(raw)<=16777216"),
                [&id], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional().map_err(|_|INVALID)?;
            if let Some((digest, bytes)) = saved {
                if source.is_some()
                    || digest != hash
                    || auth::digest(std::str::from_utf8(&bytes).map_err(|_| INVALID)?) != hash
                {
                    return Err(INVALID.into());
                }
                let mut v: Value = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
                source = Some(if claude { v["thread"].take() } else { v });
            }
        }
        let files = plan(&raw, Path::new(&root), &source.ok_or(INVALID)?)?;
        let path = directory.join("legacy-uploads").join(&bundle);
        if manifest(&path.join("restored.json"))? != bundle {
            return Err(INVALID.into());
        }
        for file in &files {
            checked(directory, &path.join("data").join(&file.relative), file)?;
        }
        if bundles.insert(bundle) {
            for (path, bytes) in
                migration_snapshot::retained_asset_rows(&path, environment).map_err(|_| INVALID)?
            {
                if result.insert(path, bytes).is_some() || result.len() > 20_000 {
                    return Err(INVALID.into());
                }
            }
        }
    }
    Ok(result)
}

pub(crate) fn inactive_metadata(db: &rusqlite::Connection, name: &str) -> Result<bool, String> {
    if !matches!(
        name,
        "legacy_upload_evidence" | "legacy_upload_display" | "legacy_upload_display_import"
    ) {
        return Ok(false);
    }
    if !table(db)? {
        return Ok(false);
    }
    let orphan:bool = db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {name} d LEFT JOIN legacy_uploads u ON u.chat=d.chat WHERE u.chat IS NULL OR u.phase<>'rolled-back')"),
        [], |r|r.get(0),
    ).map_err(|_|INVALID)?;
    Ok(!orphan)
}
impl Store {
    pub fn read_legacy_upload_text(
        &self,
        id: &str,
        original: &str,
    ) -> Result<Option<Value>, String> {
        let Some(path) = self.resolve_legacy_upload(id, original)? else {
            return Ok(None);
        };
        let mut result = crate::workspace::read_chat(
            path.parent().ok_or(INVALID)?,
            path.to_str().ok_or(INVALID)?,
        )?;
        result["path"] = json!(original);
        result["relativePath"] = json!(original);
        Ok(Some(result))
    }
    /// Stage explicitly bound files from a verified snapshot. This never reads
    /// the original upload root and never enables the migration startup gate.
    pub fn prepare_legacy_uploads(
        &mut self,
        id: &str,
        original_root: &Path,
        snapshot: &Path,
        raw: &[u8],
    ) -> Result<Value, String> {
        let allowed: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import WHERE id=1 AND phase='prepared')",[],|r|r.get(0)).map_err(|_|INVALID)?;
        if !allowed {
            return Err(INVALID.into());
        }
        let (hash, thread) = source(&self.db, id)?;
        let agent: String = self
            .db
            .query_row("SELECT agent FROM chats WHERE id=?1", [id], |r| r.get(0))
            .map_err(|_| INVALID)?;
        match agent.as_str() {
            "claude" => {
                self.legacy_claude_prepared(id)?;
            }
            "codex" => {
                self.legacy_codex_base(id)?;
            }
            _ => return Err(INVALID.into()),
        }
        let files = plan(raw, original_root, &thread)?;
        let evidence =
            crate::legacy_upload_native::collect(&self.db, &self.directory, id, &thread, &files)?;
        let environment = self.environment_id().map_err(|_| INVALID)?;
        let summary = migration_snapshot::verify(snapshot, &environment).map_err(|_| INVALID)?;
        let bundle = manifest(&snapshot.join("snapshot.json"))?;
        if let Some((prior, root, saved, plan, phase)) = receipt(&self.db, id)? {
            if prior != hash
                || Path::new(&root) != original_root
                || saved != bundle
                || plan != raw
                || phase != "prepared"
            {
                return Err(INVALID.into());
            }
            self.check_history(crate::legacy_upload_display::additional_bytes(
                &self.db,
                &self.directory,
                id,
                &bundle,
                &files,
            )?)?;
            let tx = self.db.transaction().map_err(|_| INVALID)?;
            crate::legacy_upload_display::prepare(&tx, &self.directory, id, &bundle, &files)?;
            verify(&tx, &self.directory, id)?;
            tx.commit().map_err(|_| INVALID)?;
            return Ok(
                json!({"phase":"uploads-prepared","files":files.len(),"migrationComplete":false}),
            );
        }
        let parent = self.directory.join("legacy-uploads");
        crate::private_directory::prepare(&parent).map_err(|_| INVALID)?;
        let destination = parent.join(&bundle);
        self.reindex_legacy_uploads()?;
        // Count partial restores too; a failed copy cannot evade the budget.
        let retained = crate::legacy_claude_native::staged_bytes(&parent)?;
        let other = if destination.exists() {
            retained
                .checked_sub(crate::legacy_claude_native::staged_bytes(&destination)?)
                .ok_or(INVALID)?
        } else {
            retained
        };
        let required = other
            .saturating_add(summary.bytes)
            .saturating_add(24 * 1024 * 1024);
        if required > 2 * 1024 * 1024 * 1024 {
            return Err("Legacy attachment copies exceed the 2 GiB staging budget".into());
        }
        // Both restore manifests are retained alongside the data. Charge old
        // bundles and interrupted copies too, but do not double-charge the
        // verified destination when resuming a prior failed import.
        let manifest_bytes = manifest_json(&snapshot.join("snapshot.json"))?.len() as u64;
        let (present_bytes, present_files) = self.legacy_upload_bundle_usage(&destination)?;
        self.check_assets(
            summary
                .bytes
                .saturating_add(manifest_bytes * 2)
                .saturating_sub(present_bytes),
            (summary.files as u64 + 2).saturating_sub(present_files),
        )?;
        let copied = if destination.exists() {
            migration_snapshot::resume_restore(snapshot, &destination, &environment)
        } else {
            migration_snapshot::restore(snapshot, &destination, &environment)
        };
        // A failed restore may leave a recovery copy. It consumes capacity even
        // though it has no alias receipt and cannot be opened by a phone.
        self.reindex_legacy_uploads()?;
        copied.map_err(|_| INVALID)?;
        for f in &files {
            checked(
                &self.directory,
                &destination.join("data").join(&f.relative),
                f,
            )?;
        }
        // Revalidate before granting any alias. Files alone confer no access.
        let (current, _) = source(&self.db, id)?;
        if current != hash {
            return Err(INVALID.into());
        }
        self.check_history(
            (raw.len() as u64)
                .saturating_add(crate::legacy_upload_native::storage_bytes(&evidence))
                .saturating_add(files.len() as u64 * 6144)
                .saturating_add(8192),
        )?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_uploads(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,original_root TEXT NOT NULL,bundle TEXT NOT NULL,plan BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        crate::legacy_upload_native::save(&tx, id, &evidence)?;
        tx.execute(
            "INSERT INTO legacy_uploads VALUES(?1,?2,?3,?4,?5,'prepared')",
            params![
                id,
                hash,
                original_root.to_str().ok_or(INVALID)?,
                bundle,
                raw
            ],
        )
        .map_err(|_| INVALID)?;
        crate::legacy_upload_display::prepare(&tx, &self.directory, id, &bundle, &files)?;
        verify(&tx, &self.directory, id)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(json!({"phase":"uploads-prepared","files":files.len(),"migrationComplete":false}))
    }
    /// Resolve only one exact historical link for this chat. No original path
    /// is opened, and copying an alias into a different chat grants no access.
    pub fn resolve_legacy_upload(
        &self,
        id: &str,
        original: &str,
    ) -> Result<Option<PathBuf>, String> {
        let Some((hash, root, bundle, raw, phase)) = receipt(&self.db, id)? else {
            return Ok(None);
        };
        if phase == "rolled-back" {
            return Ok(None);
        }
        if phase != "prepared" || !digest(&bundle) {
            return Err(INVALID.into());
        }
        let agent: String = self
            .db
            .query_row("SELECT agent FROM chats WHERE id=?1", [id], |r| r.get(0))
            .map_err(|_| INVALID)?;
        let (source_table, key) = match agent.as_str() {
            "claude" => ("legacy_claude_import", "id"),
            "codex" => ("legacy_codex_history", "chat"),
            _ => return Err(INVALID.into()),
        };
        let expected: String = self
            .db
            .query_row(
                &format!("SELECT digest FROM {source_table} WHERE {key}=?1 AND phase='prepared'"),
                [id],
                |r| r.get(0),
            )
            .map_err(|_| INVALID)?;
        if hash != expected {
            return Err(INVALID.into());
        }
        // The startup/import verifier checks the complete immutable source.
        // A tap reads at most the bounded alias plan and one public message,
        // never rehydrates the whole old conversation.
        if raw.len() > MAX_PLAN || !absolute(Path::new(&root)) {
            return Err(INVALID.into());
        }
        let plan: Plan = serde_json::from_slice(&raw).map_err(|_| INVALID)?;
        if plan.version != 1 || plan.files.len() > MAX_FILES {
            return Err(INVALID.into());
        }
        let files = plan.files;
        if files.iter().filter(|f| f.original == original).count() > 1 {
            return Err(INVALID.into());
        }
        let Some(f) = files.iter().find(|f| f.original == original) else {
            return Ok(None);
        };
        shape(f, Path::new(&root))?;
        let raw: String = self.db.query_row("SELECT data FROM chat_rows WHERE chat=?1 AND kind='messages' AND id=?2 AND length(data)<=?3",params![id,f.message,crate::chat_store::ROW_BYTES as i64],|r|r.get(0)).map_err(|_|INVALID)?;
        let message: Value = serde_json::from_str(&raw).map_err(|_| INVALID)?;
        if !referenced(&message, original)
            && !crate::legacy_upload_native::allows(&self.db, id, f, &message)?
        {
            return Err(INVALID.into());
        }
        let path = self
            .directory
            .join("legacy-uploads")
            .join(bundle)
            .join("data")
            .join(&f.relative);
        checked(&self.directory, &path, f)?;
        Ok(Some(path))
    }
}
