//! Explicit offline headers for Desktop Codex chats missing a legacy alias.
//! Never queries a running server or rewrites the original connection export.
use crate::{auth, legacy_codex, store::Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const INVALID:&str="The offline Codex header export is incomplete, active or changed; no original chat was modified";
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) fn table(db: &rusqlite::Connection, name: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )
    .map_err(|_| INVALID.into())
}
fn source(db: &rusqlite::Connection) -> Result<(String, Value), String> {
    let (digest, environment, raw, phase): (String, String, Vec<u8>, String) = db
        .query_row(
            "SELECT digest,environment,raw,phase FROM legacy_connection_import WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| INVALID)?;
    if phase != "prepared"
        || raw.len() > 16 * 1024 * 1024
        || auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != digest
    {
        return Err(INVALID.into());
    }
    crate::legacy_connection::Prepared::parse(&raw, &environment)?;
    Ok((digest, serde_json::from_slice(&raw).map_err(|_| INVALID)?))
}
fn parse(raw: &[u8], root: &Value) -> Result<Vec<Value>, String> {
    if raw.is_empty() || raw.len() > MAX_BYTES {
        return Err(INVALID.into());
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    if value["version"] != 1 {
        return Err(INVALID.into());
    }
    let rows = value["headers"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or(INVALID)?;
    let old = root["threadAliases"].as_array();
    if old.map_or(0, Vec::len) + rows.len() > 4096 {
        return Err(INVALID.into());
    }
    let mut natives = BTreeSet::new();
    let mut public = BTreeSet::new();
    for alias in old.into_iter().flatten() {
        natives.insert(alias["nativeThreadId"].as_str().ok_or(INVALID)?);
        public.insert(alias["remoteThreadId"].as_str().ok_or(INVALID)?);
    }
    let mut aliases = Vec::with_capacity(rows.len());
    for row in rows {
        let alias = &row["alias"];
        let native = &row["native"];
        let id = alias["nativeThreadId"].as_str().ok_or(INVALID)?;
        let remote = alias["remoteThreadId"].as_str().ok_or(INVALID)?;
        if !natives.insert(id)
            || !public.insert(remote)
            || native["id"] != id
            || native["cwd"] != alias["cwd"]
            || !matches!(native["modelProvider"].as_str(), Some("openai" | "codex"))
            || !matches!(
                native["status"]["type"].as_str(),
                Some("idle" | "notLoaded")
            )
            || row["hasPendingApprovals"] != false
            || row["hasPendingUserInput"] != false
            || row["hasQueuedPrompts"] != false
            || row["hasActionableProposedPlan"] != false
            || row["activeTurnId"] != Value::Null
            || !row
                .as_object()
                .is_some_and(|r| r.contains_key("activeTurnId"))
            || !row.as_object().is_some_and(|r| {
                r.contains_key("archivedAt")
                    && r.contains_key("deletedAt")
                    && r.contains_key("branch")
            })
        {
            return Err(INVALID.into());
        }
        let mut alias = alias.clone();
        for key in ["archivedAt", "deletedAt", "branch"] {
            if alias.get(key).is_some_and(|v| v != &row[key]) {
                return Err(INVALID.into());
            }
            alias[key] = row[key].clone();
        }
        aliases.push(alias);
    }
    Ok(aliases)
}
pub(crate) fn read(
    db: &rusqlite::Connection,
    digest: &str,
    root: &Value,
    rollback: bool,
) -> Result<(String, Vec<Value>), String> {
    if !table(db, "legacy_codex_headers")? {
        return Ok((digest.into(), vec![]));
    }
    let record: Option<(String, String, Vec<u8>, String)> = db
        .query_row(
            "SELECT source_hash,digest,raw,phase FROM legacy_codex_headers WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|_| INVALID)?;
    let Some((source_hash, hash, raw, phase)) = record else {
        return Ok((digest.into(), vec![]));
    };
    if source_hash != digest
        || raw.len() > MAX_BYTES
        || auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != hash
        || !(phase == "prepared" || rollback && phase == "rolled-back")
    {
        return Err(INVALID.into());
    }
    let aliases = parse(&raw, root)?;
    Ok((
        auth::digest(&json!({"connection":digest,"headers":hash}).to_string()),
        aliases,
    ))
}
impl Store {
    /// `headers` comes from a quiesced export. Alias fields preserve the old
    /// public projection, including its project ID; native cwd is explicit even
    /// when No Project hid it in the UI. This stages no prompts or workers.
    pub fn prepare_legacy_codex_headers(&mut self, headers: &[u8]) -> Result<Value, String> {
        let (source_hash, root) = source(&self.db)?;
        let rows = parse(headers, &root)?;
        let hash = auth::digest(std::str::from_utf8(headers).map_err(|_| INVALID)?);
        self.check_history(headers.len() as u64 * 3 + 1024 * 1024)?;
        self.init_chats().map_err(|_| INVALID)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_codex_headers(id INTEGER PRIMARY KEY,source_hash TEXT NOT NULL,digest TEXT NOT NULL,raw BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let saved: Option<(String, String, Vec<u8>, String)> = tx
            .query_row(
                "SELECT source_hash,digest,raw,phase FROM legacy_codex_headers WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| INVALID)?;
        if let Some((source, old, raw, phase)) = saved {
            if source != source_hash || old != hash || raw != headers || phase != "prepared" {
                return Err(INVALID.into());
            }
        } else {
            if table(&tx, "legacy_codex_import")? {
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM legacy_codex_import)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|_| INVALID)?;
                if exists {
                    return Err(
                        "Export missing Codex headers before converting chat metadata".into(),
                    );
                }
            }
            tx.execute(
                "INSERT INTO legacy_codex_headers VALUES(1,?1,?2,?3,'prepared')",
                params![source_hash, hash, headers],
            )
            .map_err(|_| INVALID)?;
        }
        // Validate the entire combined projection before committing even one
        // header, including selections for other previously unaliased chats.
        legacy_codex::validate_headers(&tx)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(json!({"phase":"codex-headers-prepared","chats":rows.len(),"migrationComplete":false}))
    }
    pub fn rollback_legacy_codex_headers(&mut self) -> Result<Value, String> {
        let (digest, root) = source(&self.db)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let (_, rows) = read(&tx, &digest, &root, true)?;
        if rows.is_empty() {
            return Err(INVALID.into());
        }
        legacy_codex::headers_can_rollback(&tx)?;
        tx.execute(
            "UPDATE legacy_codex_headers SET phase='rolled-back' WHERE id=1",
            [],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(json!({"phase":"rolled-back","sourceRetained":true,"migrationComplete":false}))
    }
}
