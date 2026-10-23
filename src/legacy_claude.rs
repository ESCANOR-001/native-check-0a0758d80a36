//! Offline conversion of settled legacy Claude chats. The connection-migration
//! barrier must already exist and is never lifted here. Native transcripts and
//! assets are qualified separately before any service can use this profile.
use crate::{
    auth,
    chat::{AgentKind, Chat},
    claude_host::Identity,
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;

const INVALID: &str = "Legacy Claude chat is invalid or unsupported; original data was not changed";
const BUSY: &str = "Finish Claude tasks, resolve pending controls and finish or remove queued prompts before upgrading; nothing was imported";
const CONFLICT: &str =
    "Migration destination changed or already contains this chat; nothing was overwritten";
const MAX_FILE: usize = 16 * 1024 * 1024;
// An old receipt has an identity but not a payload digest. A tombstone prevents
// its replay without pretending the new request's payload was verified.
const LEGACY_RECEIPT: &str = "legacy-accepted-payload-unavailable";

fn text(value: &Value, limit: usize) -> Result<&str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .ok_or_else(|| INVALID.into())
}
fn optional_bool(value: &Value) -> Result<bool, String> {
    if value.is_null() {
        Ok(false)
    } else {
        value.as_bool().ok_or_else(|| INVALID.into())
    }
}
fn list<'a>(root: &'a Value, key: &str, limit: usize) -> Result<&'a [Value], String> {
    if root[key].is_null() {
        return Ok(&[]);
    }
    root[key]
        .as_array()
        .filter(|v| v.len() <= limit)
        .map(Vec::as_slice)
        .ok_or_else(|| INVALID.into())
}
fn ordered(state: &Value) -> Result<Vec<(&str, &Value)>, String> {
    let mut rows = Vec::new();
    for kind in ["messages", "activities", "proposedPlans"] {
        for row in state["thread"][kind].as_array().ok_or(INVALID)? {
            let at = time::OffsetDateTime::parse(
                text(&row["createdAt"], 64)?,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|_| INVALID)?;
            rows.push((
                at.unix_timestamp_nanos(),
                row["sequence"].as_u64().unwrap_or(u64::MAX),
                kind,
                row,
            ));
        }
    }
    // Legacy global sequence numbers can restart at one after a PC restart.
    // Timestamps retain cross-restart chronology; sequences order same-time rows.
    rows.sort_by_key(|(at, seq, _, _)| (*at, *seq));
    Ok(rows
        .into_iter()
        .map(|(_, _, kind, row)| (kind, row))
        .collect())
}
pub(crate) struct Prepared {
    source: Vec<u8>,
    hash: String,
    pub(crate) state: Value,
    commands: Vec<String>,
    checkpoints: Vec<(String, String)>,
}
impl Prepared {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_FILE {
            return Err(INVALID.into());
        }
        let root: Value = serde_json::from_slice(bytes).map_err(|_| INVALID)?;
        if root["version"] != 1 || !root["thread"].is_object() {
            return Err(INVALID.into());
        }
        let native = text(&root["nativeId"], 36)?;
        if native.len() != 36 {
            return Err(INVALID.into());
        }
        uuid::Uuid::parse_str(native).map_err(|_| INVALID)?;
        let mut thread = root["thread"].clone();
        text(&thread["id"], 128)?;
        text(&thread["projectId"], 256)?;
        text(&thread["title"], 4096)?;
        text(&thread["worktreePath"], 4096)?;
        if !std::path::Path::new(thread["worktreePath"].as_str().unwrap()).is_absolute()
            || (!thread["nativeSessionId"].is_null() && thread["nativeSessionId"] != native)
            || (!thread["session"]["threadId"].is_null()
                && thread["session"]["threadId"] != thread["id"])
            || thread["modelSelection"]["instanceId"] != "claude-code"
            || !matches!(
                thread["runtimeMode"].as_str(),
                Some("approval-required" | "auto-accept-edits" | "full-access")
            )
            || !matches!(thread["interactionMode"].as_str(), Some("default" | "plan"))
            || !thread["session"].is_object()
        {
            return Err(INVALID.into());
        }
        text(&thread["modelSelection"]["model"], 256)?;
        if matches!(
            thread["session"]["status"].as_str(),
            Some("running" | "starting" | "stopping")
        ) || thread["latestTurn"]["state"] == "running"
            || !thread["session"]["activeTurnId"].is_null()
            || optional_bool(&thread["hasPendingApprovals"])?
            || optional_bool(&thread["hasPendingUserInput"])?
            || optional_bool(&root["branchImportPending"])?
            || !list(&root, "followUps", 32)?.is_empty()
            || list(&root, "deliveries", 100_000)?
                .iter()
                .any(|d| d["state"] == "offered")
        {
            return Err(BUSY.into());
        }
        if !matches!(
            thread["session"]["status"].as_str(),
            Some("idle" | "ready" | "disconnected" | "error" | "interrupted")
        ) || (!thread["latestTurn"].is_null()
            && !matches!(
                thread["latestTurn"]["state"].as_str(),
                Some("completed" | "interrupted" | "error")
            ))
            || list(&root, "deliveries", 100_000)?
                .iter()
                .any(|d| !matches!(d["state"].as_str(), Some("consumed" | "cancelled")))
        {
            return Err(INVALID.into());
        }
        if !list(&root, "permissionRules", 4096)?.is_empty() {
            return Err("Saved Claude session permission rules need explicit conversion before upgrading; nothing was imported".into());
        }
        let mut rows = 0usize;
        for kind in ["messages", "activities", "proposedPlans"] {
            let mut ids = HashSet::new();
            for row in list(&thread, kind, 100_000)? {
                if !row.is_object()
                    || !ids.insert(text(&row["id"], 256)?.to_owned())
                    || row.to_string().len() > crate::chat_store::ROW_BYTES
                {
                    return Err(INVALID.into());
                }
                if optional_bool(&row["streaming"])?
                    || row["phase"] == "queued"
                    || (matches!(
                        row["kind"].as_str(),
                        Some("approval.requested" | "user-input.requested" | "steer.updated")
                    ) && row["payload"]["status"] == "pending")
                {
                    return Err(BUSY.into());
                }
                rows += 1;
                if rows > 100_000 {
                    return Err(INVALID.into());
                }
            }
            if thread[kind].is_null() {
                thread[kind] = json!([]);
            }
        }
        let identity = if root["bridgeId"].is_null() {
            None
        } else {
            let value = Identity {
                native_session: native.into(),
                bridge_session: text(&root["bridgeId"], 128)?.into(),
            };
            value.validate().map_err(|_| INVALID)?;
            Some(value)
        };
        let mut commands = Vec::new();
        let mut seen = HashSet::new();
        for key in list(&root, "commands", 100_000)? {
            let key = text(key, 512)?;
            if !key.contains(':') {
                return Err(INVALID.into());
            }
            if seen.insert(key) {
                commands.push(format!("claude-code:{key}"));
            }
        }
        let mut checkpoints = Vec::new();
        let mut seen = HashSet::new();
        for row in list(&root, "fileCheckpoints", 100_000)? {
            let id = text(&row["id"], 256)?;
            if !seen.insert(id) {
                return Err(INVALID.into());
            }
            checkpoints.push((id.into(), text(&row["label"], 4096)?.into()));
        }
        thread["nativeSessionId"] = json!(native);
        thread["session"]["status"] = json!("disconnected");
        thread["session"]["statusConfidence"] = json!("unknown");
        thread["session"]["lastError"] = Value::Null;
        thread["session"]["activeTurnId"] = Value::Null;
        // These are connection metadata, not transcript/tool output. Old
        // connected flags must not mask the first native handshake.
        let activities = thread["activities"].as_array_mut().unwrap();
        let last_sync = activities
            .iter()
            .rposition(|v| v["kind"] == "claude.desktop-sync.updated")
            .map(|i| activities[i].clone());
        activities.retain(|v| v["kind"] != "claude.desktop-sync.updated");
        if let Some(mut sync) = last_sync {
            sync["id"] = json!("claude-native-sync");
            sync["payload"] = json!({"status":"disconnected","detail":"Waiting for the original Claude connection after upgrade."});
            activities.push(sync);
        }
        let plan = (thread["interactionMode"] == "plan")
            .then(|| thread["runtimeMode"].as_str().unwrap().to_owned());
        let chat = Chat {
            agent: AgentKind::Claude,
            thread,
            native_id: native.into(),
            resumed: optional_bool(&root["resumed"])?,
            claude_plan_access: plan,
            remote_identity: identity,
            external_claude: optional_bool(&root["external"])?,
            external_stop: None,
            native_control: None,
            fork_from: None,
            sync_replacement: None,
            queue: vec![],
            paused: true,
            disabled_mcp: vec![],
            stream_id: String::new(),
            pending_message: Value::Null,
            stream_model: String::new(),
        };
        let state = serde_json::to_value(chat).map_err(|_| INVALID)?;
        ordered(&state)?;
        Ok(Self {
            source: bytes.into(),
            hash: auth::digest(std::str::from_utf8(bytes).map_err(|_| INVALID)?),
            state,
            commands,
            checkpoints,
        })
    }
    fn report(&self) -> Value {
        json!({"phase":"claude-metadata-prepared","messages":self.state["thread"]["messages"].as_array().unwrap().len(),"activities":self.state["thread"]["activities"].as_array().unwrap().len(),"commandReceipts":self.commands.len(),"fileCheckpoints":self.checkpoints.len(),"migrationComplete":false})
    }
}

impl Store {
    pub(crate) fn legacy_claude_prepared(&self, id: &str) -> Result<Prepared, String> {
        let allowed: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import WHERE id=1 AND phase='prepared')",[],|r|r.get(0)).map_err(|_|CONFLICT)?;
        if !allowed {
            return Err(CONFLICT.into());
        }
        let (digest, raw, state, phase): (String, Vec<u8>, String, String) = self
            .db
            .query_row(
                "SELECT digest,raw,state,phase FROM legacy_claude_import WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|_| CONFLICT)?;
        let p = Prepared::parse(&raw)?;
        let expected = p.state.to_string();
        if digest != p.hash
            || state != expected
            || phase != "prepared"
            || p.state["thread"]["id"] != id
        {
            return Err(CONFLICT.into());
        }
        verify_inactive(&self.db, id)?;
        verify(&self.db, &p, &self.directory)?;
        Ok(p)
    }
    /// Call under the offline migration profile lease. Original JSON is retained
    /// byte-for-byte in the same transaction as rows and receipt tombstones.
    pub fn prepare_legacy_claude(&mut self, raw: &[u8]) -> Result<Value, String> {
        let prepared = Prepared::parse(raw)?;
        let allowed: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import WHERE id=1 AND phase='prepared')",[],|r|r.get(0)).map_err(|_|"Prepare an offline connection migration first")?;
        if !allowed {
            return Err("Prepare an offline connection migration first".into());
        }
        self.check_history(
            (raw.len() as u64)
                .saturating_mul(3)
                .saturating_add(1024 * 1024),
        )?;
        self.init_chats().map_err(|_| INVALID)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_claude_import(id TEXT PRIMARY KEY,digest TEXT NOT NULL,raw BLOB NOT NULL,state TEXT NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let id = prepared.state["thread"]["id"].as_str().unwrap();
        let prior: Option<(String, Vec<u8>, String, String)> = tx
            .query_row(
                "SELECT digest,raw,state,phase FROM legacy_claude_import WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| INVALID)?;
        if let Some((hash, raw, state, phase)) = prior {
            let expected_state = prepared.state.to_string();
            if phase != "prepared"
                || hash != prepared.hash
                || raw != prepared.source
                || state != expected_state
            {
                return Err(CONFLICT.into());
            }
            verify_inactive(&tx, id)?;
            verify(&tx, &prepared, &self.directory)?;
            return Ok(prepared.report());
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 OR json_extract(data,'$.native_id')=?2)",params![id,prepared.state["native_id"].as_str()],|r|r.get::<_,bool>(0)).map_err(|_|INVALID)? { return Err(CONFLICT.into()); }
        let mut header = prepared.state.clone();
        for (kind, row) in ordered(&prepared.state)? {
            tx.execute(
                "INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES(?1,?2,?3,?4,?5)",
                params![
                    id,
                    kind,
                    row["id"].as_str(),
                    row["turnId"].as_str(),
                    row.to_string()
                ],
            )
            .map_err(|_| CONFLICT)?;
        }
        for kind in ["messages", "activities", "proposedPlans"] {
            header["thread"][kind] = json!([]);
        }
        header["thread"]
            .as_object_mut()
            .unwrap()
            .remove("historyPage");
        tx.execute(
            "INSERT INTO chats VALUES(?1,'claude',?2)",
            params![id, header.to_string()],
        )
        .map_err(|_| CONFLICT)?;
        for key in &prepared.commands {
            tx.execute(
                "INSERT INTO chat_commands VALUES(?1,?2)",
                params![key, LEGACY_RECEIPT],
            )
            .map_err(|_| CONFLICT)?;
        }
        for (point, label) in &prepared.checkpoints {
            tx.execute(
                "INSERT INTO native_checkpoints(chat,id,label) VALUES(?1,?2,?3)",
                params![id, point, label],
            )
            .map_err(|_| CONFLICT)?;
        }
        tx.execute(
            "INSERT INTO legacy_claude_import VALUES(?1,?2,?3,?4,'prepared')",
            params![
                id,
                prepared.hash,
                prepared.source,
                prepared.state.to_string()
            ],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(prepared.report())
    }
    /// Remove only an unchanged, unactivated conversion. Retain its exact source
    /// and receipt; retries cannot erase new messages or native side effects.
    pub fn rollback_legacy_claude(&mut self, id: &str) -> Result<Value, String> {
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let allowed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import WHERE id=1 AND phase='prepared')",[],|r|r.get(0)).map_err(|_|CONFLICT)?;
        if !allowed {
            return Err(CONFLICT.into());
        }
        let (digest, raw, state, phase): (String, Vec<u8>, String, String) = tx
            .query_row(
                "SELECT digest,raw,state,phase FROM legacy_claude_import WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|_| CONFLICT)?;
        let p = Prepared::parse(&raw)?;
        let expected_state = p.state.to_string();
        if digest != p.hash || state != expected_state || p.state["thread"]["id"] != id {
            return Err(CONFLICT.into());
        }
        verify_inactive(&tx, id)?;
        if phase == "prepared" {
            verify(&tx, &p, &self.directory)?;
            crate::legacy_claude_assets::rollback(&tx, id)?;
            for table in ["chat_rows", "native_checkpoints", "tool_images"] {
                tx.execute(&format!("DELETE FROM {table} WHERE chat=?1"), [id])
                    .map_err(|_| INVALID)?;
            }
            tx.execute("DELETE FROM chats WHERE id=?1", [id])
                .map_err(|_| INVALID)?;
            for key in &p.commands {
                tx.execute("DELETE FROM chat_commands WHERE id=?1", [key])
                    .map_err(|_| INVALID)?;
            }
            tx.execute(
                "UPDATE legacy_claude_import SET phase='rolled-back' WHERE id=?1",
                [id],
            )
            .map_err(|_| INVALID)?;
        } else if phase == "rolled-back" {
            let occupied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1) OR EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1) OR EXISTS(SELECT 1 FROM native_checkpoints WHERE chat=?1) OR EXISTS(SELECT 1 FROM tool_images WHERE chat=?1)",[id],|r|r.get(0)).map_err(|_|CONFLICT)?;
            if occupied {
                return Err(CONFLICT.into());
            }
            for key in &p.commands {
                if tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM chat_commands WHERE id=?1)",
                        [key],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(|_| CONFLICT)?
                {
                    return Err(CONFLICT.into());
                }
            }
        } else {
            return Err(CONFLICT.into());
        }
        tx.commit().map_err(|_| INVALID)?;
        Ok(json!({"phase":"rolled-back","backupRetained":true,"migrationComplete":false}))
    }
}
fn verify_inactive(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    for table in ["native_turns", "native_sync", "claude_outbox"] {
        let present: bool = db
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE chat=?1)"),
                [id],
                |r| r.get(0),
            )
            .map_err(|_| CONFLICT)?;
        if present {
            return Err(CONFLICT.into());
        }
    }
    let revised:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM chat_revisions WHERE id=?1 OR source=?1) OR EXISTS(SELECT 1 FROM native_tool_actions) OR EXISTS(SELECT 1 FROM native_worktrees)",[id],|r|r.get(0)).map_err(|_|CONFLICT)?;
    if revised {
        return Err(CONFLICT.into());
    }
    Ok(())
}
fn verify(
    db: &rusqlite::Connection,
    p: &Prepared,
    directory: &std::path::Path,
) -> Result<(), String> {
    let id = p.state["thread"]["id"].as_str().unwrap();
    let state = crate::legacy_claude_assets::project(db, directory, &p.state)?;
    let mut header = state.clone();
    let mut total = 0;
    for kind in ["messages", "activities", "proposedPlans"] {
        for row in state["thread"][kind].as_array().unwrap() {
            let (found, turn): (String, Option<String>) = db
                .query_row(
                    "SELECT data,turn FROM chat_rows WHERE chat=?1 AND kind=?2 AND id=?3",
                    params![id, kind, row["id"].as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| CONFLICT)?;
            let expected = row.to_string();
            if found != expected || turn.as_deref() != row["turnId"].as_str() {
                return Err(CONFLICT.into());
            }
            total += 1;
        }
        header["thread"][kind] = json!([]);
    }
    header["thread"]
        .as_object_mut()
        .unwrap()
        .remove("historyPage");
    let found: String = db
        .query_row(
            "SELECT data FROM chats WHERE id=?1 AND agent='claude'",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| CONFLICT)?;
    let count: i64 = db
        .query_row("SELECT count(*) FROM chat_rows WHERE chat=?1", [id], |r| {
            r.get(0)
        })
        .map_err(|_| CONFLICT)?;
    let expected_header = header.to_string();
    if found != expected_header || count != total {
        return Err(CONFLICT.into());
    }
    let expected: Vec<_> = ordered(&state)?
        .into_iter()
        .map(|(k, r)| (k.to_owned(), r["id"].as_str().unwrap().to_owned()))
        .collect();
    let mut statement = db
        .prepare("SELECT kind,id FROM chat_rows WHERE chat=?1 ORDER BY seq")
        .map_err(|_| CONFLICT)?;
    let order: Vec<(String, String)> = statement
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| CONFLICT)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| CONFLICT)?;
    if order != expected {
        return Err(CONFLICT.into());
    }
    for key in &p.commands {
        let found: String = db
            .query_row("SELECT digest FROM chat_commands WHERE id=?1", [key], |r| {
                r.get(0)
            })
            .map_err(|_| CONFLICT)?;
        if found != LEGACY_RECEIPT {
            return Err(CONFLICT.into());
        }
    }
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM native_checkpoints WHERE chat=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| CONFLICT)?;
    if count != p.checkpoints.len() as i64 {
        return Err(CONFLICT.into());
    }
    for (point, label) in &p.checkpoints {
        let found: String = db
            .query_row(
                "SELECT label FROM native_checkpoints WHERE chat=?1 AND id=?2",
                params![id, point],
                |r| r.get(0),
            )
            .map_err(|_| CONFLICT)?;
        if found != *label {
            return Err(CONFLICT.into());
        }
    }
    Ok(())
}
