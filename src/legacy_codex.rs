//! Offline native Codex alias and selection conversion. It never connects to
//! an app-server or releases the history/asset migration startup barrier.
use crate::{
    auth,
    chat::{AgentKind, Chat},
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
const INVALID: &str =
    "Unsupported or changed legacy Codex metadata; the original installation is unchanged";
const CONFLICT: &str = "The Codex migration destination changed; no chat was replaced";
fn text(v: &Value, limit: usize) -> Result<&str, String> {
    v.as_str()
        .filter(|v| !v.is_empty() && v.len() <= limit && !v.chars().any(char::is_control))
        .ok_or_else(|| INVALID.into())
}
fn uuid(v: &Value) -> Result<&str, String> {
    let v = text(v, 36)?;
    if v.len() != 36 || uuid::Uuid::parse_str(v).is_err() {
        return Err(INVALID.into());
    }
    Ok(v)
}
fn timestamp(v: &Value) -> Result<&str, String> {
    let s = text(v, 64)?;
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|_| INVALID)?;
    Ok(s)
}
fn native_provider(v: &Value) -> bool {
    v.is_null() || matches!(v.as_str(), Some("openai" | "codex"))
}
fn array<'a>(root: &'a Value, key: &str, limit: usize) -> Result<&'a [Value], String> {
    if root[key].is_null() {
        return Ok(&[]);
    }
    root[key]
        .as_array()
        .filter(|v| v.len() <= limit)
        .map(Vec::as_slice)
        .ok_or_else(|| INVALID.into())
}
// Normalize the two historical forms without guessing conflicting aliases.
pub(crate) fn options(value: &Value) -> Result<Vec<Value>, String> {
    let entries: Vec<(&str, &Value)> = if value.is_null() {
        vec![]
    } else if let Some(rows) = value.as_array() {
        if rows.len() > 32 {
            return Err(INVALID.into());
        }
        rows.iter()
            .map(|r| Ok((text(&r["id"], 64)?, &r["value"])))
            .collect::<Result<_, String>>()?
    } else if let Some(rows) = value.as_object() {
        if rows.len() > 32 {
            return Err(INVALID.into());
        }
        rows.iter().map(|(k, v)| (k.as_str(), v)).collect()
    } else {
        return Err(INVALID.into());
    };
    let mut normalized = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (key, value) in entries {
        if !matches!(key, "effort" | "reasoningEffort" | "serviceTier") || !seen.insert(key) {
            return Err(INVALID.into());
        }
        // Null represents no override in alias metadata; unsupported nonstrings
        // remain in the retained source and must not become a guessed default.
        if value.is_null() {
            continue;
        }
        let value = text(value, 128)?;
        let key = if key == "reasoningEffort" {
            "effort"
        } else {
            key
        };
        if normalized
            .insert(key, value)
            .is_some_and(|old| old != value)
        {
            return Err(INVALID.into());
        }
    }
    Ok(normalized
        .into_iter()
        .map(|(id, value)| json!({"id":id,"value":value}))
        .collect())
}
fn no_effects(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    for table in [
        "chat_rows",
        "native_turns",
        "native_checkpoints",
        "native_sync",
        "tool_images",
        "claude_outbox",
    ] {
        let used: bool = db
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE chat=?1)"),
                [id],
                |r| r.get(0),
            )
            .map_err(|_| CONFLICT)?;
        if used {
            return Err(CONFLICT.into());
        }
    }
    let changed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM chat_revisions WHERE id=?1 OR source=?1) OR EXISTS(SELECT 1 FROM native_tool_actions) OR EXISTS(SELECT 1 FROM native_worktrees)",[id],|r|r.get(0)).map_err(|_|CONFLICT)?;
    if changed {
        return Err(CONFLICT.into());
    }
    Ok(())
}
struct Prepared {
    digest: String,
    states: Vec<Value>,
}
impl Prepared {
    fn read(db: &rusqlite::Connection) -> Result<Self, String> {
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
        let root: Value = serde_json::from_slice(&raw).map_err(|_| INVALID)?;
        let aliases = array(&root, "threadAliases", 1024)?;
        let selections = array(&root, "taskSelections", 4096)?;
        let mut selected = BTreeMap::new();
        let mut selected_public = BTreeSet::new();
        for row in selections {
            let native = uuid(&row["nativeThreadId"])?;
            let public = text(&row["remoteThreadId"], 128)?;
            if !native_provider(&row["providerInstanceId"])
                || row["providerInstanceId"].is_null()
                || !matches!(
                    row["source"].as_str(),
                    Some("android" | "desktop" | "migration")
                )
                || row["revision"]
                    .as_u64()
                    .is_none_or(|v| v == 0 || v > i64::MAX as u64)
                || selected.insert(native, row).is_some()
                || !selected_public.insert(public)
            {
                return Err(INVALID.into());
            }
            text(&row["model"], 256)?;
            text(&row["updateId"], 256)?;
            timestamp(&row["updatedAt"])?;
        }
        let mut ids = BTreeSet::new();
        let mut natives = BTreeSet::new();
        let mut states = Vec::new();
        for alias in aliases {
            let id = text(&alias["remoteThreadId"], 128)?;
            let native = uuid(&alias["nativeThreadId"])?;
            let project = text(&alias["projectId"], 256)?;
            let cwd = text(&alias["cwd"], 4096)?;
            let created = timestamp(&alias["createdAt"])?;
            let updated = timestamp(&alias["updatedAt"])?;
            if !ids.insert(id)
                || !natives.insert(native)
                || !std::path::Path::new(cwd).is_absolute()
                || !native_provider(&alias["instanceId"])
                || !native_provider(&alias["nativeModelProvider"])
                || !matches!(
                    alias["runtimeMode"].as_str(),
                    Some("approval-required" | "auto-accept-edits" | "full-access")
                )
                || !matches!(alias["interactionMode"].as_str(), Some("default" | "plan"))
                || (!alias["workspaceKind"].is_null() && alias["workspaceKind"] != "projectless")
                || (alias["workspaceKind"] == "projectless" && project != "codex-project-chats")
            {
                return Err(INVALID.into());
            }
            let title = text(&alias["title"], 4096)?;
            let mut selection = json!({"instanceId":"codex","model":text(&alias["model"],256)?});
            let source = selected.remove(native);
            if let Some(row) = source {
                if row["remoteThreadId"] != id {
                    return Err(INVALID.into());
                }
                selection["model"] = row["model"].clone();
                selection["options"] = json!(options(&row["options"])?);
            } else {
                selection["options"] = json!(options(
                    &json!({"reasoningEffort":alias["nativeReasoningEffort"],"serviceTier":alias["nativeServiceTier"]})
                )?);
            }
            let mut thread = json!({"id":id,"nativeSessionId":native,"projectId":project,"title":title,"worktreePath":cwd,
                "modelSelection":selection,"runtimeMode":alias["runtimeMode"],"interactionMode":alias["interactionMode"],
                "createdAt":created,"updatedAt":updated,"branch":null,"latestTurn":null,"archivedAt":null,"deletedAt":null,
                "messages":[],"activities":[],"proposedPlans":[],"checkpoints":[],"hasPendingApprovals":false,"hasPendingUserInput":false,"hasActionableProposedPlan":false,
                "session":{"threadId":id,"providerName":"codex","providerInstanceId":"codex","status":"disconnected","statusConfidence":"unknown","runtimeMode":alias["runtimeMode"],"interactionMode":alias["interactionMode"],"activeTurnId":null,"lastError":null,"updatedAt":updated}});
            if project == "codex-project-chats" {
                thread["workspaceKind"] = json!("projectless");
            }
            let chat = Chat {
                agent: AgentKind::Codex,
                thread,
                native_id: native.into(),
                resumed: true,
                claude_plan_access: None,
                remote_identity: None,
                external_claude: false,
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
            states.push(serde_json::to_value(chat).map_err(|_| INVALID)?);
        }
        if !selected.is_empty() {
            return Err("Some saved Codex selections belong to Desktop chats without a saved Android alias. Export those native chat headers before upgrading; no selection was discarded.".into());
        }
        Ok(Self { digest, states })
    }
    fn report(&self) -> Value {
        json!({"phase":"codex-metadata-prepared","chats":self.states.len(),"historyPending":self.states.len(),"serviceTierPending":self.states.iter().filter(|s|s["thread"]["modelSelection"]["options"].as_array().is_some_and(|v|v.iter().any(|o|o["id"]=="serviceTier"))).count(),"migrationComplete":false})
    }
    fn verify(&self, db: &rusqlite::Connection) -> Result<(), String> {
        for state in &self.states {
            let id = state["thread"]["id"].as_str().ok_or(INVALID)?;
            let found: String = db
                .query_row(
                    "SELECT data FROM chats WHERE id=?1 AND agent='codex'",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|_| CONFLICT)?;
            let expected = state.to_string();
            if found != expected {
                return Err(CONFLICT.into());
            }
            no_effects(db, id)?;
        }
        Ok(())
    }
}
impl Store {
    pub fn prepare_legacy_codex(&mut self) -> Result<Value, String> {
        let p = Prepared::read(&self.db)?;
        let data = serde_json::to_string(&p.states).map_err(|_| INVALID)?;
        self.check_history(data.len() as u64 * 3 + 1024 * 1024)?;
        self.init_chats().map_err(|_| INVALID)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_codex_import(id INTEGER PRIMARY KEY,digest TEXT NOT NULL,data TEXT NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let saved: Option<(String, String, String)> = tx
            .query_row(
                "SELECT digest,data,phase FROM legacy_codex_import WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| INVALID)?;
        if let Some((digest, old, phase)) = saved {
            if phase != "prepared" || digest != p.digest || old != data {
                return Err(CONFLICT.into());
            }
            p.verify(&tx)?;
            return Ok(p.report());
        }
        for state in &p.states {
            let id = state["thread"]["id"].as_str().ok_or(INVALID)?;
            let occupied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 OR json_extract(data,'$.native_id')=?2)",params![id,state["native_id"].as_str()],|r|r.get(0)).map_err(|_|INVALID)?;
            if occupied {
                return Err(CONFLICT.into());
            }
            tx.execute(
                "INSERT INTO chats(id,agent,data) VALUES(?1,'codex',?2)",
                params![id, state.to_string()],
            )
            .map_err(|_| CONFLICT)?;
        }
        tx.execute(
            "INSERT INTO legacy_codex_import VALUES(1,?1,?2,'prepared')",
            params![p.digest, data],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(p.report())
    }
    pub fn rollback_legacy_codex(&mut self) -> Result<Value, String> {
        let p = Prepared::read(&self.db)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let (digest, data, phase): (String, String, String) = tx
            .query_row(
                "SELECT digest,data,phase FROM legacy_codex_import WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| CONFLICT)?;
        let expected = serde_json::to_string(&p.states).map_err(|_| INVALID)?;
        if digest != p.digest || data != expected {
            return Err(CONFLICT.into());
        }
        if phase == "prepared" {
            p.verify(&tx)?;
            for state in &p.states {
                tx.execute(
                    "DELETE FROM chats WHERE id=?1",
                    [state["thread"]["id"].as_str().ok_or(INVALID)?],
                )
                .map_err(|_| INVALID)?;
            }
            tx.execute(
                "UPDATE legacy_codex_import SET phase='rolled-back' WHERE id=1",
                [],
            )
            .map_err(|_| INVALID)?;
        } else if phase == "rolled-back" {
            for state in &p.states {
                let id = state["thread"]["id"].as_str().ok_or(INVALID)?;
                let present: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1)",
                        [id],
                        |r| r.get(0),
                    )
                    .map_err(|_| CONFLICT)?;
                if present {
                    return Err(CONFLICT.into());
                }
                no_effects(&tx, id)?;
            }
        } else {
            return Err(CONFLICT.into());
        }
        tx.commit().map_err(|_| INVALID)?;
        Ok(json!({"phase":"rolled-back","sourceRetained":true,"migrationComplete":false}))
    }
}
