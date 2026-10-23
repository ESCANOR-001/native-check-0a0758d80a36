//! Import an external Claude observer's already-committed sequence. A legacy
//! anchor records its source ledger hash, never a fabricated native event hash.
//! Native events after this boundary advance the normal verified cursor.
use crate::{claude_cursor::Cursor, claude_host::Identity, store::Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
const INVALID:&str="The saved Claude resume checkpoint is missing or changed; history remains offline and nothing was replayed";
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claude_resume')",[],|r|r.get(0)).map_err(|_|INVALID.into())
}
fn expected(raw: &[u8], hash: &str) -> Result<Cursor, String> {
    if raw.len() > 16 * 1024 * 1024
        || crate::auth::digest(std::str::from_utf8(raw).map_err(|_| INVALID)?) != hash
    {
        return Err(INVALID.into());
    }
    let source: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    let identity = Identity {
        native_session: source["nativeId"].as_str().ok_or(INVALID)?.into(),
        bridge_session: source["bridgeId"].as_str().ok_or(INVALID)?.into(),
    };
    if source["external"] == true {
        let sequence = source["remoteSequence"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= 9_007_199_254_740_991)
            .ok_or(INVALID)?;
        Cursor::legacy(identity, sequence, hash.into())
    } else {
        // An owned native export has no proven relay offset. Replay from zero
        // with an immutable-row boundary, never skip events using a guessed
        // sequence. A matching complete native export is required separately.
        Cursor::new(identity)
    }
}
fn native_proof(db: &rusqlite::Connection, id: &str, raw: &[u8], hash: &str) -> Result<(), String> {
    let source: Value = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    if source["external"] == true {
        return Ok(());
    }
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claude_native')",[],|r|r.get(0)).map_err(|_|INVALID)?;
    if !exists || !db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_claude_native WHERE chat=?1 AND source_hash=?2 AND phase='prepared')",params![id,hash],|r|r.get::<_,bool>(0)).map_err(|_|INVALID)? {
        return Err("An owned Claude session needs a verified quiesced native-history export before its replay checkpoint can migrate".into());
    }
    Ok(())
}
/// Called by the enclosing metadata verifier. Every cursor must be either absent
/// or exactly the prepared legacy anchor; observed native progress forbids rollback.
pub(crate) fn verify(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM native_sync WHERE chat=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    let saved: Option<(String, String, String)> = if table(db)? {
        db.query_row(
            "SELECT source_hash,cursor,phase FROM legacy_claude_resume WHERE chat=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|_| INVALID)?
    } else {
        None
    };
    let Some((hash, cursor, phase)) = saved else {
        return if count == 0 {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    };
    if phase == "rolled-back" {
        return if count == 0 {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    }
    if phase != "prepared" || count != 1 {
        return Err(INVALID.into());
    }
    let raw: Vec<u8> = db
        .query_row(
            "SELECT raw FROM legacy_claude_import WHERE id=?1 AND phase='prepared'",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    native_proof(db, id, &raw, &hash)?;
    let mut wanted = expected(&raw, &hash)?;
    let before: i64 = db
        .query_row(
            "SELECT COALESCE(MAX(seq),0) FROM chat_rows WHERE chat=?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    wanted.projection = crate::claude_history::relink_projection(&Value::Null, before)?;
    let encoded = serde_json::to_string(&wanted).map_err(|_| INVALID)?;
    if encoded != cursor {
        return Err(INVALID.into());
    }
    let matches: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM native_sync WHERE chat=?1 AND native=?2 AND cursor=?3)",
            params![
                id,
                format!("claude:{}", wanted.identity.native_session),
                cursor
            ],
            |r| r.get(0),
        )
        .map_err(|_| INVALID)?;
    if !matches {
        return Err(INVALID.into());
    }
    Ok(())
}
pub(crate) fn rollback(db: &rusqlite::Connection, id: &str) -> Result<(), String> {
    verify(db, id)?;
    db.execute("DELETE FROM native_sync WHERE chat=?1", [id])
        .map_err(|_| INVALID)?;
    if table(db)? {
        db.execute(
            "UPDATE legacy_claude_resume SET phase='rolled-back' WHERE chat=?1",
            [id],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(())
}
impl Store {
    pub fn prepare_legacy_claude_resume(&mut self, id: &str) -> Result<Value, String> {
        let p = self.legacy_claude_prepared(id)?;
        native_proof(&self.db, id, &p.source, &p.hash)?;
        let mut source = expected(&p.source, &p.hash)?;
        let before: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(MAX(seq),0) FROM chat_rows WHERE chat=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|_| INVALID)?;
        source.projection = crate::claude_history::relink_projection(&Value::Null, before)?;
        let cursor = serde_json::to_string(&source).map_err(|_| INVALID)?;
        self.check_history(cursor.len() as u64 * 3 + 65536)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_claude_resume(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,cursor TEXT NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let phase: Option<String> = tx
            .query_row(
                "SELECT phase FROM legacy_claude_resume WHERE chat=?1",
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
            tx.execute(
                "INSERT INTO native_sync(chat,native,cursor) VALUES(?1,?2,?3)",
                params![
                    id,
                    format!("claude:{}", source.identity.native_session),
                    cursor
                ],
            )
            .map_err(|_| INVALID)?;
            tx.execute(
                "INSERT INTO legacy_claude_resume VALUES(?1,?2,?3,'prepared')",
                params![id, p.hash, cursor],
            )
            .map_err(|_| INVALID)?;
        }
        tx.commit().map_err(|_| INVALID)?;
        Ok(
            json!({"phase":if source.sequence==0 {"owned-replay-prepared"} else {"external-resume-prepared"},"sequence":source.sequence,"migrationComplete":false}),
        )
    }
}
