//! Transcript rows are durable independently of the bounded live projection.
//! Receipt, metadata, and changed rows commit together; paging never changes the
//! live sequence or loads the complete transcript into memory.
use crate::store::Store;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

pub const PAGE_BYTES: usize = 160 * 1024;
pub const ROW_BYTES: usize = 384 * 1024;
const PAGE_ROWS: usize = 24;
enum Sync<'a> {
    Codex(&'a crate::shared_history::Batch),
    Claude(crate::claude_cursor::Batch<'a>),
}
fn parse(text: String) -> rusqlite::Result<Value> {
    serde_json::from_str(&text).map_err(|_| rusqlite::Error::InvalidQuery)
}
impl Store {
    pub(crate) fn empty_shared_session(&self, id: &str, native: &str) -> rusqlite::Result<bool> {
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM chats c WHERE id=?1 AND agent='codex' AND json_extract(data,'$.native_id')=?2 AND json_extract(data,'$.thread.latestTurn.startedAt') IS NULL AND json_extract(data,'$.thread.session.status') IN ('error','interrupted') AND NOT EXISTS(SELECT 1 FROM native_turns WHERE chat=c.id) AND NOT EXISTS(SELECT 1 FROM chat_rows WHERE chat=c.id AND NOT (kind='activities' AND id='empty-session-recovered') AND (kind<>'messages' OR COALESCE(json_extract(data,'$.role'),'')<>'user')))",params![id,native],|r|r.get(0))
    }
    pub fn init_chats(&mut self) -> rusqlite::Result<()> {
        self.init_claude_outbox()?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS chats (id TEXT PRIMARY KEY, agent TEXT NOT NULL, data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS chat_commands (id TEXT PRIMARY KEY, digest TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS chat_rows (seq INTEGER PRIMARY KEY AUTOINCREMENT, chat TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL, turn TEXT, data TEXT NOT NULL, UNIQUE(chat,kind,id));
        CREATE INDEX IF NOT EXISTS chat_rows_page ON chat_rows(chat,seq);
        CREATE INDEX IF NOT EXISTS chat_rows_turn ON chat_rows(chat,turn,seq);")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS tool_images (chat TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(chat,path));")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_tool_actions (id TEXT PRIMARY KEY, digest TEXT NOT NULL, result TEXT NOT NULL);")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_worktrees (id TEXT PRIMARY KEY, digest TEXT NOT NULL, data TEXT NOT NULL);")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_checkpoints(seq INTEGER PRIMARY KEY AUTOINCREMENT,chat TEXT NOT NULL,id TEXT NOT NULL,label TEXT NOT NULL,UNIQUE(chat,id)); CREATE INDEX IF NOT EXISTS native_checkpoints_chat ON native_checkpoints(chat,seq);")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_turns(chat TEXT NOT NULL,turn TEXT NOT NULL,native TEXT NOT NULL,PRIMARY KEY(chat,turn)); CREATE TABLE IF NOT EXISTS chat_revisions(id TEXT PRIMARY KEY,source TEXT NOT NULL,data TEXT NOT NULL);")?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_sync(chat TEXT NOT NULL,native TEXT NOT NULL,cursor TEXT NOT NULL,PRIMARY KEY(chat,native));")?;
        // Migrate previous experimental blobs one chat at a time, transactionally.
        let ids = self
            .db
            .prepare("SELECT id FROM chats")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            let state = parse(self.db.query_row(
                "SELECT data FROM chats WHERE id=?1",
                [&id],
                |r| r.get(0),
            )?)?;
            if ["messages", "activities", "proposedPlans"]
                .iter()
                .any(|k| state["thread"][k].as_array().is_some_and(|a| !a.is_empty()))
            {
                self.save_chat(&state, None)?;
            }
        }
        Ok(())
    }
    pub fn chats(&self) -> rusqlite::Result<Vec<Value>> {
        let ids = self
            .db
            .prepare("SELECT id FROM chats ORDER BY id LIMIT 10001")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.load_chat(id)).collect()
    }
    pub fn chat_headers(&self) -> rusqlite::Result<Vec<Value>> {
        self.db
            .prepare("SELECT data FROM chats ORDER BY id LIMIT 10001")?
            .query_map([], |r| parse(r.get(0)?))?
            .collect()
    }
    pub fn load_chat(&self, id: &str) -> rusqlite::Result<Value> {
        self.load_chat_page(id, false)
    }
    fn load_chat_page(&self, id: &str, display: bool) -> rusqlite::Result<Value> {
        let mut state = parse(self.db.query_row(
            "SELECT data FROM chats WHERE id=?1",
            [id],
            |r| r.get(0),
        )?)?;
        let page = self.history_page_inner(id, None, None, display)?;
        state["thread"]["messages"] = page["messages"].clone();
        state["thread"]["activities"] = page["activities"].clone();
        state["thread"]["proposedPlans"] = page["proposedPlans"].clone();
        state["thread"]["historyPage"] = page["pageInfo"].clone();
        Ok(state)
    }
    pub fn history_page(
        &self,
        id: &str,
        before: Option<i64>,
        turn: Option<&str>,
    ) -> rusqlite::Result<Value> {
        self.history_page_inner(id, before, turn, false)
    }
    pub fn history_page_display(
        &self,
        id: &str,
        before: Option<i64>,
        turn: Option<&str>,
    ) -> rusqlite::Result<Value> {
        self.history_page_inner(id, before, turn, true)
    }
    fn history_page_inner(
        &self,
        id: &str,
        before: Option<i64>,
        turn: Option<&str>,
        display: bool,
    ) -> rusqlite::Result<Value> {
        let mut stmt = self.db.prepare("SELECT seq,kind,data FROM chat_rows WHERE chat=?1 AND seq<?2 AND (?3 IS NULL OR turn=?3) ORDER BY seq DESC LIMIT 25")?;
        let mut rows = stmt.query(params![id, before.unwrap_or(i64::MAX), turn])?;
        let mut selected = Vec::new();
        let mut bytes = 0;
        let mut older = false;
        while let Some(row) = rows.next()? {
            let text: String = row.get(2)?;
            let mut row_bytes = text.len();
            let kind: String = row.get(1)?;
            let mut value = parse(text)?;
            if display && kind == "messages" {
                self.present_legacy_uploads(id, &mut value)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                row_bytes = value.to_string().len();
            }
            if !selected.is_empty()
                && (bytes + row_bytes > PAGE_BYTES || selected.len() >= PAGE_ROWS)
            {
                older = true;
                break;
            }
            bytes += row_bytes;
            selected.push((row.get::<_, i64>(0)?, kind, value));
        }
        let cursor = if older {
            selected.last().map(|r| format!("v1:{id}:{}", r.0))
        } else {
            None
        };
        selected.reverse();
        let mut messages = Vec::new();
        let mut activities = Vec::new();
        let mut plans = Vec::new();
        for (seq, kind, mut row) in selected {
            row["sequence"] = json!(seq);
            if kind == "messages" {
                messages.push(row);
            } else if kind == "proposedPlans" {
                plans.push(row);
            } else {
                activities.push(row);
            }
        }
        Ok(
            json!({"threadId":id,"messages":messages,"activities":activities,"proposedPlans":plans,"pageInfo":{"hasOlder":older,"olderCursor":cursor}}),
        )
    }
    pub fn load_chat_display(&self, id: &str, pending: &[String]) -> rusqlite::Result<Value> {
        if pending.len() > 16 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let mut state = self.load_chat_page(id, true)?;
        let claude = state["agent"] == "claude";
        let rows = state["thread"]["activities"].as_array_mut().unwrap();
        let mut query = self.db.prepare(
            "SELECT seq,data FROM chat_rows WHERE chat=?1 AND kind='activities' AND id=?2",
        )?;
        for request in pending
            .iter()
            .map(String::as_str)
            .chain(claude.then_some("claude-native-sync"))
        {
            if rows.iter().any(|row| row["id"] == request) {
                continue;
            }
            let value = query
                .query_row(params![id, request], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .optional()?;
            if let Some((seq, data)) = value {
                let mut value = parse(data)?;
                if (request == "claude-native-sync"
                    && value["kind"] == "claude.desktop-sync.updated")
                    || (value["payload"]["status"] == "pending"
                        && matches!(
                            value["kind"].as_str(),
                            Some("user-input.requested" | "approval.requested")
                        ))
                {
                    value["sequence"] = json!(seq);
                    rows.push(value);
                }
            }
        }
        rows.sort_by_key(|r| r["sequence"].as_i64().unwrap_or_default());
        Ok(state)
    }
    pub fn history_row(&self, chat: &str, kind: &str, id: &str) -> rusqlite::Result<Option<Value>> {
        self.db
            .query_row(
                "SELECT data FROM chat_rows WHERE chat=?1 AND kind=?2 AND id=?3",
                params![chat, kind, id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()
    }
    pub(crate) fn pending_native_control(
        &self,
        chat: &str,
        turn: &str,
        native: &Value,
    ) -> rusqlite::Result<Option<Value>> {
        self.db.query_row("SELECT data FROM chat_rows WHERE chat=?1 AND turn=?2 AND kind='activities' AND json_extract(data,'$.kind') IN ('approval.requested','user-input.requested') AND json_extract(data,'$.payload.data.nativeRequestId')=json_extract(?3,'$') LIMIT 1",params![chat,turn,native.to_string()],|r|r.get::<_,String>(0)).optional()?.map(parse).transpose()
    }
    pub(crate) fn pending_shared_controls(
        &self,
        chat: &str,
        turn: &str,
    ) -> rusqlite::Result<Vec<Value>> {
        let rows: Vec<String> = self.db.prepare("SELECT data FROM chat_rows WHERE chat=?1 AND turn=?2 AND kind='activities' AND json_extract(data,'$.kind') IN ('approval.requested','user-input.requested') AND json_extract(data,'$.payload.status')='pending' AND json_extract(data,'$.payload.data.nativeRequestId') IS NOT NULL LIMIT 17")?
            .query_map(params![chat,turn], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        if rows.len() > 16 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        rows.into_iter().map(parse).collect()
    }
    pub fn command_digest(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.db
            .query_row("SELECT digest FROM chat_commands WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
    }
    pub(crate) fn save_shared_history(
        &mut self,
        state: &Value,
        sync: &crate::shared_history::Batch,
    ) -> rusqlite::Result<()> {
        self.save_chat_change(state, None, None, None, None, Some(Sync::Codex(sync)))
    }
    pub fn claude_remote_cursor(
        &self,
        chat: &str,
        identity: crate::claude_host::Identity,
    ) -> rusqlite::Result<crate::claude_cursor::Cursor> {
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                params![chat, format!("claude:{}", identity.native_session)],
                |r| r.get(0),
            )
            .optional()?;
        let cursor: crate::claude_cursor::Cursor = raw
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .unwrap_or(
                crate::claude_cursor::Cursor::new(identity.clone())
                    .map_err(rusqlite::Error::InvalidParameterName)?,
            );
        cursor
            .validate()
            .map_err(rusqlite::Error::InvalidParameterName)?;
        if cursor.identity != identity {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(cursor)
    }
    pub fn save_claude_remote(
        &mut self,
        state: &Value,
        before: &crate::claude_cursor::Cursor,
        after: &crate::claude_cursor::Cursor,
    ) -> rusqlite::Result<()> {
        self.save_claude_remote_receipt(state, before, after, None)
    }
    pub fn save_claude_remote_receipt(
        &mut self,
        state: &Value,
        before: &crate::claude_cursor::Cursor,
        after: &crate::claude_cursor::Cursor,
        receipt: Option<&crate::claude_host::Receipt>,
    ) -> rusqlite::Result<()> {
        self.save_chat_change(
            state,
            None,
            None,
            None,
            None,
            Some(Sync::Claude(crate::claude_cursor::Batch {
                before,
                after,
                receipt,
            })),
        )
    }
    pub fn save_chat(
        &mut self,
        state: &Value,
        receipt: Option<(&str, &str)>,
    ) -> rusqlite::Result<()> {
        self.save_chat_change(state, receipt, None, None, None, None)
    }
    pub fn tool_action(&self, id: &str) -> rusqlite::Result<Option<(String, Value)>> {
        self.db
            .query_row(
                "SELECT digest,result FROM native_tool_actions WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, parse(row.get(1)?)?)),
            )
            .optional()
    }
    pub fn save_tool_action(
        &mut self,
        state: &Value,
        key: &str,
        digest: &str,
        result: &Value,
    ) -> rusqlite::Result<()> {
        self.check_history(16 * 1024)
            .map_err(rusqlite::Error::InvalidParameterName)?;
        self.save_chat_change(state, None, Some((key, digest, result)), None, None, None)
    }
    pub(crate) fn save_branch(
        &mut self,
        state: &Value,
        key: &str,
        digest: &str,
        result: &Value,
        source: &str,
    ) -> rusqlite::Result<()> {
        let bytes: i64 = self.db.query_row(
            "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM chat_rows WHERE chat=?1",
            [source],
            |r| r.get(0),
        )?;
        // Include index/page overhead for the transactional history copy.
        self.check_history(
            (bytes.max(0) as u64)
                .saturating_mul(2)
                .saturating_add(1024 * 1024),
        )
        .map_err(rusqlite::Error::InvalidParameterName)?;
        self.save_chat_change(
            state,
            None,
            Some((key, digest, result)),
            Some(source),
            None,
            None,
        )
    }
    pub(crate) fn save_edit(
        &mut self,
        state: &Value,
        receipt: Option<(&str, &str)>,
        target: &crate::editing::Target,
    ) -> rusqlite::Result<()> {
        self.edit_target(state["thread"]["id"].as_str().unwrap_or(""), &target.turn)
            .map_err(rusqlite::Error::InvalidParameterName)?;
        self.save_chat_change(state, receipt, None, None, Some(target), None)
    }
    fn save_chat_change(
        &mut self,
        state: &Value,
        receipt: Option<(&str, &str)>,
        action: Option<(&str, &str, &Value)>,
        copy_from: Option<&str>,
        edit: Option<&crate::editing::Target>,
        sync: Option<Sync<'_>>,
    ) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        let id = state["thread"]["id"]
            .as_str()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        if let Some(target) = edit {
            let old: String =
                tx.query_row("SELECT data FROM chats WHERE id=?1", [id], |r| r.get(0))?;
            let revision = format!("edit-revision-{}", uuid::Uuid::new_v4());
            tx.execute(
                "INSERT INTO chat_revisions(id,source,data) VALUES(?1,?2,?3)",
                params![revision, id, old],
            )?;
            tx.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) SELECT ?1,kind,id,turn,data FROM chat_rows WHERE chat=?2 ORDER BY seq",params![revision,id])?;
            tx.execute(
                "INSERT INTO tool_images(chat,path) SELECT ?1,path FROM tool_images WHERE chat=?2",
                params![revision, id],
            )?;
            tx.execute("INSERT INTO native_turns(chat,turn,native) SELECT ?1,turn,native FROM native_turns WHERE chat=?2",params![revision,id])?;
            // All statements share the receipt transaction. Failed saves retain the
            // original chat, its CLI identity and every transcript row.
            tx.execute("DELETE FROM native_turns WHERE chat=?1 AND turn IN (SELECT turn FROM chat_rows WHERE chat=?1 AND seq>=?2)",params![id,target.cut])?;
            tx.execute("DELETE FROM chat_rows WHERE chat=?1 AND (seq>=?2 OR turn IN (SELECT turn FROM chat_rows WHERE chat=?1 AND seq>=?2))",params![id,target.cut])?;
            tx.execute("DELETE FROM native_checkpoints WHERE chat=?1", [id])?;
        }
        if let Some(Sync::Codex(sync)) = &sync {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                    params![id, sync.native],
                    |r| r.get(0),
                )
                .optional()?;
            let previous: crate::shared_history::Cursor = previous
                .map(|v| serde_json::from_str(&v))
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?
                .unwrap_or_default();
            if previous != sync.before {
                return Err(rusqlite::Error::InvalidQuery);
            }
            tx.execute("INSERT INTO native_sync(chat,native,cursor) VALUES(?1,?2,?3) ON CONFLICT(chat,native) DO UPDATE SET cursor=excluded.cursor",params![id,sync.native,serde_json::to_string(&sync.after).map_err(|_|rusqlite::Error::InvalidQuery)?])?;
            if let Some(turn) = sync.turn.as_ref() {
                let public = sync
                    .public_turn
                    .as_deref()
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                tx.execute("INSERT INTO native_turns(chat,turn,native) VALUES(?1,?2,?3) ON CONFLICT(chat,turn) DO NOTHING",params![id,public,turn["id"].as_str().unwrap_or("")])?;
                let saved: String = tx.query_row(
                    "SELECT native FROM native_turns WHERE chat=?1 AND turn=?2",
                    params![id, public],
                    |r| r.get(0),
                )?;
                if turn["id"] != saved {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
        }
        if let Some(Sync::Claude(batch)) = &sync {
            let before = batch.before;
            let after = batch.after;
            let bound: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 AND agent='claude' AND json_extract(data,'$.native_id')=?2 AND json_extract(data,'$.remote_identity.bridge_session')=?3)",params![id,after.identity.native_session,after.identity.bridge_session],|r|r.get(0))?;
            if !bound {
                return Err(rusqlite::Error::InvalidQuery);
            }
            before
                .validate()
                .and_then(|_| after.validate())
                .map_err(rusqlite::Error::InvalidParameterName)?;
            if before.identity != after.identity
                || after.sequence < before.sequence
                || (after.sequence == before.sequence
                    && (batch.receipt.is_none()
                        || after.event_id != before.event_id
                        || after.digest != before.digest
                        || after.legacy_source != before.legacy_source))
                || state["native_id"] != after.identity.native_session
                || state["remote_identity"] != serde_json::to_value(&after.identity).unwrap()
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let previous: Option<String> = tx
                .query_row(
                    "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                    params![id, format!("claude:{}", after.identity.native_session)],
                    |r| r.get(0),
                )
                .optional()?;
            let previous: crate::claude_cursor::Cursor = previous
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?
                .unwrap_or(
                    crate::claude_cursor::Cursor::new(after.identity.clone())
                        .map_err(rusqlite::Error::InvalidParameterName)?,
                );
            if previous != *before {
                return Err(rusqlite::Error::InvalidQuery);
            }
            tx.execute("INSERT INTO native_sync(chat,native,cursor) VALUES(?1,?2,?3) ON CONFLICT(chat,native) DO UPDATE SET cursor=excluded.cursor",params![id,format!("claude:{}",after.identity.native_session),serde_json::to_string(after).map_err(|_|rusqlite::Error::InvalidQuery)?])?;
            if let Some(receipt) = batch.receipt {
                crate::claude_outbox::apply_receipt(
                    &tx,
                    id,
                    &after.identity.native_session,
                    receipt,
                )?;
            }
        }
        let mut metadata = state.clone();
        {
            // Stream snapshots contain the bounded recent page. Do not allocate
            // AUTOINCREMENT ids or dirty its pages for rows that did not change.
            // Reuse the statement across rows/events; FULL durability is retained.
            let mut upsert = tx.prepare_cached("INSERT INTO chat_rows(chat,kind,id,turn,data) SELECT ?1,?2,?3,?4,?5 WHERE NOT EXISTS (SELECT 1 FROM chat_rows WHERE chat=?1 AND kind=?2 AND id=?3 AND data=?5) ON CONFLICT(chat,kind,id) DO UPDATE SET turn=excluded.turn,data=CASE
                        WHEN excluded.turn=chat_rows.turn AND excluded.kind='activities'
                        AND json_type(chat_rows.data,'$.payload.turnDurationMs')='integer'
                        AND json_extract(chat_rows.data,'$.payload.turnDurationMs')>=0
                        AND json_type(excluded.data,'$.payload.turnDurationMs') IS NULL
                        AND COALESCE(json_type(excluded.data,'$.payload'),'null') IN ('object','null')
                        AND length(CAST(excluded.data AS BLOB))+360<=?6
                        THEN json_patch(excluded.data,json_object('payload',json_object(
                            'turnStartedAt',json_extract(chat_rows.data,'$.payload.turnStartedAt'),
                            'turnCompletedAt',json_extract(chat_rows.data,'$.payload.turnCompletedAt'),
                            'turnDurationMs',json_extract(chat_rows.data,'$.payload.turnDurationMs'))))
                        ELSE excluded.data END WHERE chat_rows.data<>excluded.data")?;
            for kind in ["messages", "activities", "proposedPlans"] {
                if let Some(rows) = state["thread"][kind].as_array() {
                    for row in rows {
                        upsert.execute(params![
                            id,
                            kind,
                            row["id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?,
                            row["turnId"].as_str(),
                            row.to_string(),
                            ROW_BYTES as i64
                        ])?;
                    }
                }
                metadata["thread"][kind] = json!([]);
            }
        }
        metadata["thread"]
            .as_object_mut()
            .unwrap()
            .remove("historyPage");
        let turn = &metadata["thread"]["latestTurn"];
        if let (Some(turn_id), Some(state)) = (turn["turnId"].as_str(), turn["state"].as_str()) {
            if ["completed", "interrupted", "error"].contains(&state) {
                if let Some(timing) = crate::turn_timing::Timing::read(turn) {
                    // Stamp spilled rows in-place using the turn index. Replaying
                    // older in-memory rows must not erase this completion data.
                    tx.execute("UPDATE chat_rows SET data=json_set(data,'$.payload.turnStartedAt',?3,'$.payload.turnCompletedAt',?4,'$.payload.turnDurationMs',?5)
                        WHERE chat=?1 AND turn=?2 AND kind='activities'
                        AND COALESCE(json_type(data,'$.payload'),'null') IN ('object','null')
                        AND length(CAST(data AS BLOB))+360<=?6
                        AND (json_extract(data,'$.payload.turnDurationMs') IS NOT ?5
                        OR json_extract(data,'$.payload.turnCompletedAt') IS NOT ?4)",params![id,turn_id,timing.started,timing.completed,timing.millis,ROW_BYTES as i64])?;
                }
                // A pending control/tool row may be older than the live window.
                // Finalize it on disk without materializing the whole turn.
                tx.execute("UPDATE chat_rows SET data=json_set(data,'$.streaming',json('false')) WHERE chat=?1 AND turn=?2 AND kind='messages' AND json_extract(data,'$.streaming')=1",params![id,turn_id])?;
                tx.execute("UPDATE chat_rows SET data=json_set(data,'$.payload.status',?3) WHERE chat=?1 AND turn=?2 AND kind='activities' AND json_extract(data,'$.payload.status')='running'",params![id,turn_id,if state=="completed"{"completed"}else{"stopped"}])?;
                tx.execute("UPDATE chat_rows SET data=json_set(data,'$.kind',CASE json_extract(data,'$.kind') WHEN 'approval.requested' THEN 'approval.resolved' ELSE 'user-input.resolved' END,'$.payload.status','stopped') WHERE chat=?1 AND turn=?2 AND kind='activities' AND json_extract(data,'$.kind') IN ('approval.requested','user-input.requested')",params![id,turn_id])?;
                tx.execute("UPDATE chat_rows SET data=json_set(data,'$.payload.status','failed','$.payload.detail','Delivery was not confirmed. Review the chat before resending.') WHERE chat=?1 AND turn=?2 AND kind='activities' AND json_extract(data,'$.kind')='steer.updated' AND json_extract(data,'$.payload.status')='pending'",params![id,turn_id])?;
            }
        }
        let actionable: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='proposedPlans' AND json_extract(data,'$.completed')=1 AND json_extract(data,'$.implementedAt') IS NULL)",
            [id], |r|r.get(0))?;
        metadata["thread"]["hasActionableProposedPlan"] = json!(actionable);
        tx.execute("INSERT INTO chats(id,agent,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![id,state["agent"].as_str().unwrap_or(""),metadata.to_string()])?;
        if let Some(source) = copy_from {
            tx.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) SELECT ?1,kind,id,turn,data FROM chat_rows WHERE chat=?2 AND (kind='messages' OR (kind='activities' AND json_extract(data,'$.kind') NOT IN ('context-window.updated','provider.usage.updated','claude.desktop-sync.updated','approval.requested','user-input.requested'))) ORDER BY seq",params![id,source])?;
            tx.execute(
                "INSERT INTO tool_images(chat,path) SELECT ?1,path FROM tool_images WHERE chat=?2",
                params![id, source],
            )?;
        }
        if let Some((id, digest)) = receipt {
            tx.execute(
                "INSERT INTO chat_commands(id,digest) VALUES(?1,?2)",
                params![id, digest],
            )?;
        }
        if let Some((key, digest, result)) = action {
            tx.execute(
                "INSERT INTO native_tool_actions(id,digest,result) VALUES(?1,?2,?3)",
                params![key, digest, result.to_string()],
            )?;
        }
        crate::native_control::commit(&tx, state)?;
        tx.commit()
    }
}
