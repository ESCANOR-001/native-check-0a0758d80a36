//! Preserve the old public Codex transcript during an offline upgrade. Native
//! profile handoff and cursor qualification are separate activation gates.
use crate::{auth, legacy_codex_headers::table, store::Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
const INVALID:&str="The exported Codex transcript is incomplete, active or changed; its original history remains intact";
const MAX: usize = 16 * 1024 * 1024;
fn text(v: &Value, limit: usize) -> Result<&str, String> {
    v.as_str()
        .filter(|v| !v.is_empty() && v.len() <= limit && !v.chars().any(char::is_control))
        .ok_or(INVALID.into())
}
fn time(v: &Value) -> Result<i128, String> {
    time::OffsetDateTime::parse(text(v, 64)?, &time::format_description::well_known::Rfc3339)
        .map(|v| v.unix_timestamp_nanos())
        .map_err(|_| INVALID.into())
}
struct Prepared {
    header: Value,
    rows: Vec<(String, Value)>,
    turns: BTreeMap<String, String>,
}
type Receipt = (String, String, Vec<u8>, String);
fn parse(base: &Value, raw: &[u8]) -> Result<Prepared, String> {
    if raw.is_empty() || raw.len() > MAX {
        return Err(INVALID.into());
    }
    let v: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    if v["version"] != 1
        || v["threadId"] != base["thread"]["id"]
        || v["nativeId"] != base["native_id"]
        || !matches!(v["nativeStatus"].as_str(), Some("idle" | "notLoaded"))
        || v["hasPendingApprovals"] != false
        || v["hasPendingUserInput"] != false
        || v["hasQueuedPrompts"] != false
        || v["hasActionableProposedPlan"] != false
    {
        return Err(INVALID.into());
    }
    let mut turns = BTreeMap::new();
    let mut natives = BTreeSet::new();
    let mut statuses = BTreeMap::new();
    for t in v["turns"]
        .as_array()
        .filter(|v| v.len() <= 100_000)
        .ok_or(INVALID)?
    {
        let public = text(&t["publicId"], 256)?;
        let native = text(&t["nativeId"], 256)?;
        if !matches!(
            t["status"].as_str(),
            Some("completed" | "interrupted" | "error")
        ) || turns.insert(public.to_owned(), native.to_owned()).is_some()
            || !natives.insert(native)
        {
            return Err(INVALID.into());
        }
        statuses.insert(public, &t["status"]);
    }
    let mut ordered = vec![];
    for kind in ["messages", "activities", "proposedPlans"] {
        let mut ids = BTreeSet::new();
        for row in v[kind]
            .as_array()
            .filter(|v| v.len() <= 100_000)
            .ok_or(INVALID)?
        {
            let id = text(&row["id"], 256)?;
            let created = time(&row["createdAt"])?;
            if !row.is_object()
                || !ids.insert(id)
                || row.to_string().len() > crate::chat_store::ROW_BYTES
                || row
                    .get("streaming")
                    .is_some_and(|v| !matches!(v, Value::Bool(false) | Value::Null))
                || row["phase"] == "queued"
                || (kind == "messages"
                    && !matches!(row["role"].as_str(), Some("user" | "assistant")))
                || (matches!(
                    row["kind"].as_str(),
                    Some("approval.requested" | "user-input.requested" | "steer.updated")
                ) && row["payload"]["status"] == "pending")
            {
                return Err(INVALID.into());
            }
            if !row["updatedAt"].is_null() {
                time(&row["updatedAt"])?;
            }
            if kind == "messages" || !row["turnId"].is_null() {
                let turn = text(&row["turnId"], 256)?;
                if !turns.contains_key(turn) {
                    return Err(INVALID.into());
                }
            }
            ordered.push((
                created,
                row["sequence"].as_u64().unwrap_or(u64::MAX),
                kind.to_owned(),
                row.clone(),
            ));
            if ordered.len() > 100_000 {
                return Err(INVALID.into());
            }
        }
    }
    let latest = &v["latestTurn"];
    if !latest.is_null() {
        let id = text(&latest["turnId"], 256)?;
        if statuses.get(id).is_none_or(|s| **s != latest["state"]) {
            return Err(INVALID.into());
        }
        for key in ["requestedAt", "startedAt", "completedAt"] {
            if !latest[key].is_null() {
                time(&latest[key])?;
            }
        }
        if !latest["assistantMessageId"].is_null()
            && !v["messages"].as_array().unwrap().iter().any(|m| {
                m["id"] == latest["assistantMessageId"]
                    && m["role"] == "assistant"
                    && m["turnId"] == id
            })
        {
            return Err(INVALID.into());
        }
    } else if !turns.is_empty() || !ordered.is_empty() {
        return Err(INVALID.into());
    }
    if time(&v["updatedAt"])? < time(&base["thread"]["createdAt"])? {
        return Err(INVALID.into());
    }
    if !v["latestUserMessageAt"].is_null() {
        time(&v["latestUserMessageAt"])?;
    }
    let mut header = base.clone();
    for key in ["latestTurn", "updatedAt", "latestUserMessageAt"] {
        header["thread"][key] = v[key].clone();
    }
    ordered.sort_by_key(|(created, seq, _, _)| (*created, *seq));
    Ok(Prepared {
        header,
        rows: ordered
            .into_iter()
            .map(|(_, _, kind, row)| (kind, row))
            .collect(),
        turns,
    })
}
fn receipt(db: &rusqlite::Connection, id: &str) -> Result<Option<Receipt>, String> {
    if !table(db, "legacy_codex_history")? {
        return Ok(None);
    }
    db.query_row(
        "SELECT source_hash,digest,raw,phase FROM legacy_codex_history WHERE chat=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
    .map_err(|_| INVALID.into())
}
pub(crate) fn verify(db: &rusqlite::Connection, base: &Value) -> Result<(Value, bool), String> {
    let id = text(&base["thread"]["id"], 128)?;
    let Some((source_hash, digest, raw, phase)) = receipt(db, id)? else {
        return Ok((base.clone(), false));
    };
    if phase != "prepared"
        || raw.len() > MAX
        || source_hash != auth::digest(&base.to_string())
        || digest != auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?)
    {
        return Err(INVALID.into());
    }
    let p = parse(base, &raw)?;
    let mut q = db
        .prepare("SELECT kind,id,turn,data FROM chat_rows WHERE chat=?1 ORDER BY seq")
        .map_err(|_| INVALID)?;
    let mut rows = q.query([id]).map_err(|_| INVALID)?;
    for (kind, row) in &p.rows {
        let saved = rows.next().map_err(|_| INVALID)?.ok_or(INVALID)?;
        let expected = row.to_string();
        if saved.get::<_, String>(0).map_err(|_| INVALID)? != *kind
            || saved.get::<_, String>(1).map_err(|_| INVALID)? != row["id"]
            || saved
                .get::<_, Option<String>>(2)
                .map_err(|_| INVALID)?
                .as_deref()
                != row["turnId"].as_str()
            || saved.get::<_, String>(3).map_err(|_| INVALID)? != expected
        {
            return Err(INVALID.into());
        }
    }
    if rows.next().map_err(|_| INVALID)?.is_some() {
        return Err(INVALID.into());
    }
    let mut q = db
        .prepare("SELECT turn,native FROM native_turns WHERE chat=?1 LIMIT 100001")
        .map_err(|_| INVALID)?;
    let saved: BTreeMap<String, String> = q
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| INVALID)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| INVALID)?;
    if saved != p.turns {
        return Err(INVALID.into());
    }
    Ok((p.header, true))
}
pub(crate) fn rollback(db: &rusqlite::Connection, base: &Value) -> Result<(), String> {
    let id = text(&base["thread"]["id"], 128)?;
    let (_, present) = verify(db, base)?;
    if present {
        db.execute("DELETE FROM chat_rows WHERE chat=?1", [id])
            .map_err(|_| INVALID)?;
        db.execute("DELETE FROM native_turns WHERE chat=?1", [id])
            .map_err(|_| INVALID)?;
        db.execute(
            "UPDATE legacy_codex_history SET phase='rolled-back' WHERE chat=?1",
            [id],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(())
}
impl Store {
    pub fn prepare_legacy_codex_history(&mut self, id: &str, raw: &[u8]) -> Result<Value, String> {
        let base = self.legacy_codex_base(id)?;
        let p = parse(&base, raw)?;
        let hash = auth::digest(std::str::from_utf8(raw).map_err(|_| INVALID)?);
        self.check_history(
            (raw.len() as u64)
                .saturating_mul(4)
                .saturating_add(1024 * 1024),
        )?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_codex_history(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,digest TEXT NOT NULL,raw BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        if let Some((_, digest, saved, phase)) = receipt(&tx, id)? {
            if digest != hash || saved != raw || phase != "prepared" {
                return Err(INVALID.into());
            }
            verify(&tx, &base)?;
        } else {
            for (kind, row) in &p.rows {
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
                .map_err(|_| INVALID)?;
            }
            for (public, native) in &p.turns {
                tx.execute(
                    "INSERT INTO native_turns VALUES(?1,?2,?3)",
                    params![id, public, native],
                )
                .map_err(|_| INVALID)?;
            }
            tx.execute(
                "UPDATE chats SET data=?1 WHERE id=?2 AND agent='codex'",
                params![p.header.to_string(), id],
            )
            .map_err(|_| INVALID)?;
            tx.execute(
                "INSERT INTO legacy_codex_history VALUES(?1,?2,?3,?4,'prepared')",
                params![id, auth::digest(&base.to_string()), hash, raw],
            )
            .map_err(|_| INVALID)?;
        }
        tx.commit().map_err(|_| INVALID)?;
        Ok(
            json!({"phase":"codex-history-prepared","rows":p.rows.len(),"turns":p.turns.len(),"nativeHandoffPending":true,"migrationComplete":false}),
        )
    }
}
