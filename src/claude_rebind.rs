//! Follow a locally proven replacement relay for the same, settled native chat.
//! This never creates a session, replays a delivery, or controls its owner.
use crate::{claude_cursor::Cursor, claude_link::Link, store::Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

impl Store {
    pub(crate) fn claude_relink_request(
        &self,
        chat: &str,
        before: i64,
        native: &str,
        turn: &str,
        tool: &Value,
    ) -> rusqlite::Result<Option<Value>> {
        // Earlier resolved rows kept the tool ID but omitted the native request
        // ID. Only an exact nonempty tool + turn match repairs that legacy case.
        self.db.query_row("SELECT data FROM chat_rows WHERE chat=?1 AND kind='activities' AND seq<=?2 AND (json_extract(data,'$.payload.data.nativeRequestId')=?3 OR (json_extract(data,'$.payload.data.nativeRequestId') IS NULL AND json_type(?5,'$')='text' AND length(json_extract(?5,'$'))>0)) AND turn=?4 AND json_extract(data,'$.payload.data.toolCallId') IS json_extract(?5,'$') AND json_extract(data,'$.kind') IN ('approval.resolved','user-input.resolved') ORDER BY seq DESC LIMIT 1",params![chat,before,native,turn,tool.to_string()],|r|r.get::<_,String>(0)).optional()?.map(|s|serde_json::from_str(&s).map_err(|_|rusqlite::Error::InvalidQuery)).transpose()
    }
    pub(crate) fn claude_relink_rows(
        &self,
        chat: &str,
        before: i64,
        keys: &[(&'static str, String)],
    ) -> rusqlite::Result<Vec<(&'static str, Value)>> {
        let mut read = self.db.prepare_cached(
            "SELECT data FROM chat_rows WHERE chat=?1 AND kind=?2 AND id=?3 AND seq<=?4",
        )?;
        let mut rows = Vec::new();
        for (kind, id) in keys {
            if *kind == "activities" && id == "claude-native-sync" {
                continue;
            }
            if let Some(raw) = read
                .query_row(params![chat, kind, id, before], |r| r.get::<_, String>(0))
                .optional()?
            {
                rows.push((
                    *kind,
                    serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?,
                ));
            }
        }
        Ok(rows)
    }
    pub(crate) fn rebind_claude_link(
        &mut self,
        id: &str,
        before: &Cursor,
        link: &Link,
    ) -> Result<(Value, Cursor), String> {
        before.validate()?;
        link.identity.validate()?;
        if before.identity.native_session != link.identity.native_session
            || before.identity == link.identity
        {
            return Err("Claude recovery must keep the original native session".into());
        }
        self.check_history(256 * 1024)?;
        let tx = self
            .db
            .transaction()
            .map_err(|_| "Cannot begin Claude recovery")?;
        let current: Option<String> = tx
            .query_row(
                "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                params![id, format!("claude:{}", before.identity.native_session)],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "Claude's saved cursor is unavailable")?;
        let current: Cursor = current
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|_| "Claude's saved cursor is invalid")?
            .unwrap_or(Cursor::new(before.identity.clone())?);
        if current != *before {
            return Err("Claude's saved cursor changed during recovery".into());
        }
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM claude_outbox WHERE chat=?1 AND session=?2 AND state NOT IN ('completed','cancelled','orphaned'))",params![id,before.identity.native_session],|r|r.get(0))
            .map_err(|_| "Claude delivery state is unavailable")?;
        let raw: String = tx
            .query_row("SELECT data FROM chats WHERE id=?1", [id], |r| r.get(0))
            .map_err(|_| "Claude chat is unavailable")?;
        let mut state: Value =
            serde_json::from_str(&raw).map_err(|_| "Invalid saved Claude chat")?;
        if state["agent"] != "claude"
            || state["external_claude"] != true
            || state["native_id"] != before.identity.native_session
            || state["remote_identity"] != serde_json::to_value(&before.identity).unwrap()
            || !state["thread"]["archivedAt"].is_null()
        {
            return Err("Claude's saved session binding changed during recovery".into());
        }
        let cwd = state["thread"]["worktreePath"]
            .as_str()
            .ok_or("Claude's original folder is unavailable")?;
        if std::path::Path::new(cwd)
            .canonicalize()
            .map_err(|_| "Claude's original folder is unavailable")?
            != link.cwd
        {
            return Err("Claude's new link belongs to a different folder; history was kept".into());
        }
        if pending
            || state["thread"]["latestTurn"]["state"] == "running"
            || state["thread"]["session"]["status"] == "running"
            || state["thread"]["hasPendingApprovals"] == true
            || state["thread"]["hasPendingUserInput"] == true
            || !state["external_stop"].is_null()
            || (!state["native_control"].is_null() && state["native_control"]["result"].is_null())
        {
            return Err("Claude's link changed while work or a delivery is unresolved. The original connection and history were kept; nothing was resent.".into());
        }
        let high: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq),0) FROM chat_rows WHERE chat=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|_| "Claude history is unavailable")?;
        let mut after = Cursor::new(link.identity.clone())?;
        after.projection = crate::claude_history::relink_projection(&before.projection, high)?;
        after.validate()?;
        let prefix = format!("claude-replaced:{}:", before.identity.native_session);
        let old_key = format!("{prefix}{}", before.identity.bridge_session);
        let new_key = format!("{prefix}{}", after.identity.bridge_session);
        let seen: Option<String> = tx
            .query_row(
                "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                params![id, new_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "Claude link history is unavailable")?;
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM native_sync WHERE chat=?1 AND substr(native,1,?2)=?3",
                params![id, prefix.len() as i64, prefix],
                |r| r.get(0),
            )
            .map_err(|_| "Claude link history is unavailable")?;
        if seen.is_some() || count >= 16 {
            return Err(
                "Claude's link history needs review; an older link was not replayed".into(),
            );
        }
        state["remote_identity"] = json!(link.identity);
        state["paused"] = json!(true);
        state["thread"]["session"]["statusConfidence"] = json!("unavailable");
        state["thread"]["session"]["lastError"] =
            json!("Reconnecting to Claude's updated link; saved history is retained");
        // Metadata, archived old cursor and new cursor commit together. Transcript,
        // outbox, approvals, command receipts and queued messages are untouched.
        tx.execute(
            "INSERT INTO native_sync(chat,native,cursor) VALUES(?1,?2,?3)",
            params![id, old_key, serde_json::to_string(before).unwrap()],
        )
        .map_err(|_| "Cannot retain Claude's previous cursor")?;
        tx.execute("INSERT INTO native_sync(chat,native,cursor) VALUES(?1,?2,?3) ON CONFLICT(chat,native) DO UPDATE SET cursor=excluded.cursor",params![id,format!("claude:{}",after.identity.native_session),serde_json::to_string(&after).unwrap()])
            .map_err(|_| "Cannot save Claude's new cursor")?;
        tx.execute(
            "UPDATE chats SET data=?2 WHERE id=?1",
            params![id, state.to_string()],
        )
        .map_err(|_| "Cannot save Claude's updated binding")?;
        tx.commit()
            .map_err(|_| "Cannot commit Claude connection recovery")?;
        Ok((state, after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_host::Identity;
    fn fixture() -> (tempfile::TempDir, Store, Cursor, Link) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let identity = Identity {
            native_session: uuid::Uuid::new_v4().to_string(),
            bridge_session: "cse_old_fixture".into(),
        };
        let cursor = Cursor::new(identity.clone()).unwrap();
        let link = Link {
            identity: Identity {
                bridge_session: "cse_new_fixture".into(),
                ..identity.clone()
            },
            cwd: dir.path().canonicalize().unwrap(),
            title: "Ignored title".into(),
            internal_inputs: vec![],
        };
        store.save_chat(&json!({"agent":"claude","native_id":identity.native_session,"remote_identity":identity,"external_claude":true,"resumed":true,"paused":false,"queue":[{"message":"Never automatically replay"}],"thread":{"id":"chat","title":"Keep title","worktreePath":link.cwd,"session":{"status":"idle","statusConfidence":"confirmed"},"messages":[{"id":"old","text":"Keep history","role":"assistant","streaming":false}],"activities":[],"proposedPlans":[],"latestTurn":null}}),None).unwrap();
        (dir, store, cursor, link)
    }
    fn raw(store: &Store, table: &str) -> Vec<String> {
        store
            .db
            .prepare(&format!("SELECT data FROM {table} ORDER BY rowid"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
    #[test]
    fn relink_preserves_history_and_queue_and_is_atomic_and_durable() {
        let (dir, mut store, before, link) = fixture();
        let rows = raw(&store, "chat_rows");
        let chats = raw(&store, "chats");
        store.db.execute_batch("CREATE TRIGGER fail_binding BEFORE UPDATE ON chats BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;").unwrap();
        assert!(store.rebind_claude_link("chat", &before, &link).is_err());
        assert_eq!(raw(&store, "chats"), chats);
        assert_eq!(
            store
                .claude_remote_cursor("chat", before.identity.clone())
                .unwrap(),
            before
        );
        let count: i64 = store
            .db
            .query_row("SELECT COUNT(*) FROM native_sync", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "failed commit must not leave a partial epoch");
        store
            .db
            .execute_batch("DROP TRIGGER fail_binding;")
            .unwrap();
        let (state, after) = store.rebind_claude_link("chat", &before, &link).unwrap();
        assert_eq!(state["native_id"], before.identity.native_session);
        assert_eq!(state["thread"]["title"], "Keep title");
        assert_eq!(state["paused"], true);
        assert_eq!(state["queue"][0]["message"], "Never automatically replay");
        assert_eq!(
            state["thread"]["session"]["statusConfidence"],
            "unavailable"
        );
        assert_eq!(after.sequence, 0);
        assert_eq!(raw(&store, "chat_rows"), rows);
        assert!(store.rebind_claude_link("chat", &before, &link).is_err());
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        assert_eq!(
            store
                .claude_remote_cursor("chat", link.identity.clone())
                .unwrap(),
            after
        );
        assert_eq!(raw(&store, "chat_rows"), rows);
        let archived: String = store
            .db
            .query_row(
                "SELECT cursor FROM native_sync WHERE native LIKE 'claude-replaced:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Cursor>(&archived).unwrap(), before);
        let mut old_link = link;
        old_link.identity = before.identity;
        assert!(store
            .rebind_claude_link("chat", &after, &old_link)
            .unwrap_err()
            .contains("older link"));
    }
    #[test]
    fn relink_refuses_unresolved_deliveries_controls_wrong_session_and_folder() {
        for field in [
            "running", "approval", "question", "stop", "control", "outbox", "native", "folder",
            "archived",
        ] {
            let (_dir, mut store, before, mut link) = fixture();
            let mut state = store.load_chat("chat").unwrap();
            match field {
                "running" => state["thread"]["latestTurn"] = json!({"state":"running"}),
                "approval" => state["thread"]["hasPendingApprovals"] = json!(true),
                "question" => state["thread"]["hasPendingUserInput"] = json!(true),
                "stop" => state["external_stop"] = json!({"turn":"pending"}),
                "control" => state["native_control"] = json!({"result":null}),
                "archived" => state["thread"]["archivedAt"] = json!("saved"),
                "native" => link.identity.native_session = uuid::Uuid::new_v4().to_string(),
                "folder" => link.cwd = link.cwd.join("other"),
                "outbox" => {
                    let input = crate::claude_turns::Input {
                        native: uuid::Uuid::new_v4().to_string(),
                        message: "local".into(),
                        public_turn: "local-turn".into(),
                        local: true,
                        steer: false,
                    };
                    let frame = json!({"type":"user","uuid":input.native,"session_id":before.identity.native_session,"message":{"role":"user","content":"Keep uncertain"}});
                    store
                        .offer_claude_input("chat", &before.identity.native_session, &input, &frame)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            if field == "control" {
                // A pending native control needs more fields for the ordinary
                // commit path. This fixture only exercises the rebind guard.
                store.db.execute("UPDATE chats SET data=json_set(data,'$.native_control',json('{\"result\":null}')) WHERE id='chat'", []).unwrap();
            } else {
                store.save_chat(&state, None).unwrap();
            }
            let original = raw(&store, "chats");
            assert!(
                store.rebind_claude_link("chat", &before, &link).is_err(),
                "{field}"
            );
            assert_eq!(raw(&store, "chats"), original, "{field}");
            assert_eq!(
                store
                    .claude_remote_cursor("chat", before.identity.clone())
                    .unwrap(),
                before,
                "{field}"
            );
        }
    }
}
