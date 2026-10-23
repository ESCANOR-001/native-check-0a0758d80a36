//! Preserve prior bridge prompt aliases without fabricating native delivery
//! receipts or a hash for an unavailable wire payload. Offline migration only.
use crate::{claude_outbox::Record, claude_turns::Input, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
const INVALID:&str="Saved Claude prompt identities are incomplete or changed; the original history remains offline";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    native: String,
    message: String,
    turn: String,
    steer: bool,
    state: String,
}
fn text(v: &Value, limit: usize) -> Result<String, String> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or(INVALID.into())
}
fn expected(raw: &[u8]) -> Result<(String, Vec<Binding>), String> {
    if raw.len() > 16 * 1024 * 1024 {
        return Err(INVALID.into());
    }
    let root: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    let session = text(&root["nativeId"], 36)?;
    uuid::Uuid::parse_str(&session).map_err(|_| INVALID)?;
    let Some(rows) = root.get("deliveries").filter(|v| !v.is_null()) else {
        return Ok((session, vec![]));
    };
    let rows = rows.as_array().filter(|v| v.len() <= 512).ok_or(INVALID)?;
    let messages = root["thread"]["messages"].as_array().ok_or(INVALID)?;
    let mut by_native = BTreeMap::new();
    let mut by_message = BTreeSet::new();
    for row in rows {
        let native = text(&row["uuid"], 36)?;
        uuid::Uuid::parse_str(&native).map_err(|_| INVALID)?;
        let message = text(&row["messageId"], 128)?;
        let turn = text(&row["turnId"], 128)?;
        let steer = if row["steer"].is_null() {
            false
        } else {
            row["steer"].as_bool().ok_or(INVALID)?
        };
        let state = match row["state"].as_str() {
            Some("consumed") => "legacy-consumed",
            Some("cancelled") => "cancelled",
            _ => return Err(INVALID.into()),
        }
        .to_owned();
        if !messages.iter().any(|m| {
            m["id"] == message
                && m["role"] == "user"
                && m["turnId"] == turn
                && m["streaming"] != true
                && m["phase"] != "queued"
        }) {
            return Err(INVALID.into());
        }
        let binding = Binding {
            native: native.clone(),
            message: message.clone(),
            turn,
            steer,
            state,
        };
        if let Some(old) = by_native.get(&native) {
            if old != &binding {
                return Err(INVALID.into());
            }
            continue;
        }
        if !by_message.insert(message) {
            return Err(INVALID.into());
        }
        by_native.insert(native, binding);
    }
    Ok((session, by_native.into_values().collect()))
}
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claude_bindings')",[],|r|r.get(0)).map_err(|_|INVALID.into())
}
pub(crate) fn verify(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    if !table(db)? {
        return Ok(());
    }
    let saved: Option<(String, String)> = db
        .query_row(
            "SELECT source_hash,phase FROM legacy_claude_binding_import WHERE chat=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|_| INVALID)?;
    let rows:Vec<(String,Binding)>=db.prepare("SELECT session,native,message,turn,steer,state FROM legacy_claude_bindings WHERE chat=?1 ORDER BY native").map_err(|_|INVALID)?.query_map([id],|r|Ok((r.get(0)?,Binding{native:r.get(1)?,message:r.get(2)?,turn:r.get(3)?,steer:r.get(4)?,state:r.get(5)?}))).map_err(|_|INVALID)?.collect::<Result<_,_>>().map_err(|_|INVALID)?;
    let Some((hash, phase)) = saved else {
        return if rows.is_empty() {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    };
    if phase == "rolled-back" {
        return if rows.is_empty() {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    }
    if phase != "prepared" {
        return Err(INVALID.into());
    }
    let (source_hash, raw): (String, Vec<u8>) = db
        .query_row(
            "SELECT digest,raw FROM legacy_claude_import WHERE id=?1 AND phase='prepared'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| INVALID)?;
    if source_hash != hash
        || crate::auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != hash
    {
        return Err(INVALID.into());
    }
    let (session, wanted) = expected(&raw)?;
    if rows
        != wanted
            .into_iter()
            .map(|b| (session.clone(), b))
            .collect::<Vec<_>>()
    {
        return Err(INVALID.into());
    }
    Ok(())
}
pub(crate) fn rollback(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    verify(db, id)?;
    if !table(db)? {
        return Ok(());
    }
    db.execute("DELETE FROM legacy_claude_bindings WHERE chat=?1", [id])
        .map_err(|_| INVALID)?;
    db.execute(
        "UPDATE legacy_claude_binding_import SET phase='rolled-back' WHERE chat=?1",
        [id],
    )
    .map_err(|_| INVALID)?;
    Ok(())
}
pub(crate) fn lookup(
    db: &rusqlite::Connection,
    chat: &str,
    session: &str,
    native: &str,
) -> rusqlite::Result<Option<Record>> {
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claude_bindings')",[],|r|r.get(0))?;
    if !exists {
        return Ok(None);
    }
    db.query_row("SELECT b.message,b.turn,b.steer,b.state FROM legacy_claude_bindings b JOIN legacy_claude_binding_import i ON i.chat=b.chat WHERE b.chat=?1 AND b.session=?2 AND b.native=?3 AND i.phase='prepared'",params![chat,session,native],|r|Ok(Record{input:Input{native:native.into(),message:r.get(0)?,public_turn:r.get(1)?,local:true,steer:r.get(2)?},state:r.get(3)?})).optional()
}
pub(crate) fn reserved(
    db: &rusqlite::Connection,
    chat: &str,
    session: &str,
    native: &str,
    message: &str,
) -> rusqlite::Result<bool> {
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claude_bindings')",[],|r|r.get(0))?;
    if !exists {
        return Ok(false);
    }
    db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_claude_bindings b JOIN legacy_claude_binding_import i ON i.chat=b.chat WHERE b.chat=?1 AND b.session=?2 AND (b.native=?3 OR b.message=?4) AND i.phase='prepared')",params![chat,session,native,message],|r|r.get(0))
}
impl Store {
    pub fn prepare_legacy_claude_bindings(&mut self, id: &str) -> Result<Value, String> {
        let p = self.legacy_claude_prepared(id)?;
        let (session, bindings) = expected(&p.source)?;
        self.check_history(65536 + bindings.len() as u64 * 1024)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_claude_binding_import(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,phase TEXT NOT NULL);CREATE TABLE IF NOT EXISTS legacy_claude_bindings(chat TEXT NOT NULL,session TEXT NOT NULL,native TEXT NOT NULL,message TEXT NOT NULL,turn TEXT NOT NULL,steer INTEGER NOT NULL,state TEXT NOT NULL,PRIMARY KEY(chat,session,native),UNIQUE(chat,session,message));").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let phase: Option<String> = tx
            .query_row(
                "SELECT phase FROM legacy_claude_binding_import WHERE chat=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| INVALID)?;
        if let Some(phase) = phase {
            if phase != "prepared" {
                return Err(INVALID.into());
            }
            verify(&tx, id)?;
        } else {
            for b in &bindings {
                tx.execute(
                    "INSERT INTO legacy_claude_bindings VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![id, session, b.native, b.message, b.turn, b.steer, b.state],
                )
                .map_err(|_| INVALID)?;
            }
            tx.execute(
                "INSERT INTO legacy_claude_binding_import VALUES(?1,?2,'prepared')",
                params![id, p.hash],
            )
            .map_err(|_| INVALID)?;
            verify(&tx, id)?;
        }
        tx.commit().map_err(|_| INVALID)?;
        Ok(
            json!({"phase":"claude-prompt-bindings-prepared","bindings":bindings.len(),"migrationComplete":false}),
        )
    }
}
