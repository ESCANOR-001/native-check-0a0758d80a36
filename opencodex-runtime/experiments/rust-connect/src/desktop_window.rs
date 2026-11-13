//! A recent Desktop transcript window is separate from the durable history.
//! Loading an older page must not become a new live message or move the viewport.
use crate::store::Store;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(crate) fn sequence(row: &mut Value) {
    if let Some(at) = row["createdAt"].as_str().and_then(|s| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    }) {
        row["sequence"] = json!(at.unix_timestamp_nanos() / 1_000_000);
    } else {
        row.as_object_mut().unwrap().remove("sequence");
    }
}
pub(crate) fn save(
    tx: &Transaction<'_>,
    id: &str,
    state: &Value,
    window: Option<&crate::shared_history::WindowPage>,
) -> rusqlite::Result<()> {
    if window.is_some_and(|w| w.older_request) {
        return Ok(());
    }
    let previous: Option<String> = tx
        .query_row("SELECT data FROM native_recent WHERE chat=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    if previous.is_none() && window.is_none() {
        return Ok(());
    }
    let mut recent = if let Some(w) = window {
        tx.execute("DELETE FROM native_recent_rows WHERE chat=?1", [id])?;
        json!({"next":w.next,"conversation":w.conversation})
    } else {
        serde_json::from_str(previous.as_deref().unwrap())
            .map_err(|_| rusqlite::Error::InvalidQuery)?
    };
    let turn = &state["thread"]["latestTurn"];
    if let Some(current) = turn["turnId"].as_str().filter(|id| !id.is_empty()) {
        // Live follower events do not carry a new WindowPage. They still move
        // the current response boundary; retaining the old boundary treats all
        // new commentary as conversation summaries and drops its tools.
        recent["latestTurn"] = json!(current);
        if recent["conversation"] == true {
            let marker = format!("desktop-work-{current}");
            let revision = crate::shared_history::work_revision(
                current,
                turn["state"].as_str().unwrap_or("running"),
            );
            let at = turn["startedAt"]
                .as_str()
                .or_else(|| turn["requestedAt"].as_str())
                .unwrap_or("");
            let row = json!({"id":marker,"turnId":current,"kind":"work.deferred","summary":"Load activity","tone":"info","createdAt":at,
                "payload":{"cursor":null,"remaining":1,"loaded":false,"revision":revision,"source":"live-window"}});
            // Native paging owns existing native cursors. Only locally created
            // live disclosures change revision when that response completes.
            tx.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) SELECT ?1,'activities',?2,?3,?4 WHERE NOT EXISTS (SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='activities' AND id=?2 AND (COALESCE(json_extract(data,'$.payload.source'),'')<>'live-window' OR json_extract(data,'$.payload.revision')=json_extract(?4,'$.payload.revision'))) ON CONFLICT(chat,kind,id) DO UPDATE SET data=excluded.data", params![id,marker,current,row.to_string()])?;
            tx.execute(
                "INSERT OR IGNORE INTO native_recent_rows(chat,kind,id) VALUES(?1,'activities',?2)",
                params![id, marker],
            )?;
        }
    }
    let serialized = recent.to_string();
    if previous.as_deref() != Some(&serialized) {
        tx.execute("INSERT INTO native_recent(chat,data) VALUES(?1,?2) ON CONFLICT(chat) DO UPDATE SET data=excluded.data",params![id,serialized])?;
    }
    // Keep membership on disk: a long live task must not lose the gap between
    // its original recent window and newer events. Only display pages enter RAM.
    for kind in ["messages", "activities", "proposedPlans"] {
        for row in state["thread"][kind].as_array().into_iter().flatten() {
            tx.execute(
                "INSERT OR IGNORE INTO native_recent_rows(chat,kind,id) VALUES(?1,?2,?3)",
                params![
                    id,
                    kind,
                    row["id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?
                ],
            )?;
        }
    }
    Ok(())
}
impl Store {
    pub(crate) fn desktop_window_page(
        &self,
        id: &str,
        before: Option<i64>,
        display: bool,
    ) -> rusqlite::Result<Option<Value>> {
        let data: Option<String> = self
            .db
            .query_row("SELECT data FROM native_recent WHERE chat=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(data) = data else { return Ok(None) };
        let mut data: Value =
            serde_json::from_str(&data).map_err(|_| rusqlite::Error::InvalidQuery)?;
        // Repair the read boundary for installations that saved stale window
        // metadata, even before the next follower event updates it on disk.
        let current: Option<String> = self
            .db
            .query_row(
                "SELECT json_extract(data,'$.thread.latestTurn.turnId') FROM chats WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(current) = current.filter(|id| !id.is_empty()) {
            data["latestTurn"] = json!(current);
        }
        let anchor=before.map(|seq|self.db.query_row("SELECT json_extract(data,'$.createdAt') FROM chat_rows WHERE chat=?1 AND seq=?2",params![id,seq],|r|r.get::<_,String>(0))).transpose()?;
        let mut stmt=self.db.prepare("SELECT r.seq,r.kind,r.data FROM native_recent_rows w JOIN chat_rows r ON r.chat=w.chat AND r.kind=w.kind AND r.id=w.id WHERE w.chat=?1 AND (?2 IS NULL OR (julianday(json_extract(r.data,'$.createdAt')),r.seq)<(julianday(?2),?3)) ORDER BY julianday(json_extract(r.data,'$.createdAt')) DESC,r.seq DESC LIMIT 25")?;
        let mut rows = stmt.query(params![id, anchor, before])?;
        let mut selected = vec![];
        let mut bytes = 0;
        let mut more = false;
        while let Some(r) = rows.next()? {
            let kind: String = r.get(1)?;
            let raw: String = r.get(2)?;
            let mut value: Value =
                serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
            if display && kind == "messages" {
                self.present_legacy_uploads(id, &mut value)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
            }
            let len = value.to_string().len();
            if !selected.is_empty()
                && (selected.len() >= 24 || bytes + len > crate::chat_store::PAGE_BYTES)
            {
                more = true;
                break;
            }
            bytes += len;
            sequence(&mut value);
            selected.push((r.get::<_, i64>(0)?, kind, value));
        }
        // Prompts and the activity disclosure are part of the conversation,
        // even when a long live response fills the bounded activity tail.
        if data["conversation"] == true {
            let mut q=self.db.prepare("SELECT r.seq,r.kind,r.data FROM native_recent_rows w JOIN chat_rows r ON r.chat=w.chat AND r.kind=w.kind AND r.id=w.id WHERE w.chat=?1 AND (json_extract(r.data,'$.role')='user' OR json_extract(r.data,'$.kind')='work.deferred' OR (w.kind='messages' AND r.turn<>?2)) ORDER BY julianday(json_extract(r.data,'$.createdAt')) DESC LIMIT 201")?;
            let rows = q.query_map(
                params![id, data["latestTurn"].as_str().unwrap_or("")],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?;
            for row in rows {
                let (seq, kind, raw) = row?;
                let mut row: Value =
                    serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                if selected
                    .iter()
                    .any(|(_, k, v)| k == &kind && v["id"] == row["id"])
                {
                    continue;
                }
                if display && kind == "messages" {
                    self.present_legacy_uploads(id, &mut row)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                }
                sequence(&mut row);
                selected.push((seq, kind, row));
            }
            // Restore a contiguous, bounded part of the current response, not
            // a separate list of its assistant paragraphs. Otherwise restoring
            // text alone strands it beside unrelated text while its intervening
            // tools stay unloaded. Older response activity is loaded together
            // through the existing Work disclosure; conversation paging stays
            // independent and continues to show the latest three exchanges.
            let current = data["latestTurn"].as_str().unwrap_or("");
            selected.retain(|(_, _, row)| {
                row["turnId"] != current || row["role"] == "user" || row["kind"] == "work.deferred"
            });
            let mut q = self.db.prepare("SELECT seq,kind,data FROM chat_rows WHERE chat=?1 AND turn=?2 AND COALESCE(json_extract(data,'$.kind'),'')<>'work.deferred' ORDER BY julianday(json_extract(data,'$.createdAt')) DESC,seq DESC LIMIT 25")?;
            let mut rows = q.query(params![id, current])?;
            let mut current_bytes = 0;
            let mut current_rows = 0;
            let mut hidden = false;
            while let Some(r) = rows.next()? {
                let raw: String = r.get(2)?;
                if current_rows >= 24
                    || (current_rows > 0
                        && current_bytes + raw.len() > crate::chat_store::PAGE_BYTES)
                {
                    hidden = true;
                    break;
                }
                current_bytes += raw.len();
                current_rows += 1;
                let kind: String = r.get(1)?;
                let mut row: Value =
                    serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                if selected
                    .iter()
                    .any(|(_, k, v)| k == &kind && v["id"] == row["id"])
                {
                    continue;
                }
                if display && kind == "messages" {
                    self.present_legacy_uploads(id, &mut row)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                }
                sequence(&mut row);
                selected.push((r.get(0)?, kind, row));
            }
            if hidden {
                // A prior full Work read may have exhausted its native cursor.
                // A cold reader can still page those retained items from disk.
                for (_, _, row) in &mut selected {
                    if row["turnId"] == current
                        && row["kind"] == "work.deferred"
                        && row["payload"]["remaining"] == 0
                    {
                        row["payload"]["remaining"] = json!(1);
                        row["payload"]["loaded"] = json!(false);
                        row["payload"]["cursor"] = Value::Null;
                    }
                }
            }
            selected.sort_by(|a, b| {
                b.2["sequence"]
                    .as_i64()
                    .cmp(&a.2["sequence"].as_i64())
                    .then(b.0.cmp(&a.0))
            });
        }
        let next = if more && data["conversation"] != true {
            selected.last().map(|r| format!("w1:{id}:{}", r.0))
        } else {
            data["next"].as_str().map(str::to_owned)
        };
        let mut page = json!({"threadId":id,"messages":[],"activities":[],"proposedPlans":[],"pageInfo":{"hasOlder":next.is_some(),"olderCursor":next}});
        for (_, kind, row) in selected.into_iter().rev() {
            page[&kind]
                .as_array_mut()
                .ok_or(rusqlite::Error::InvalidQuery)?
                .push(row);
        }
        if display && before.is_none() && data["conversation"] == true {
            self.restore_work_snapshot(id, &mut page)?;
        }
        Ok(Some(page))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_metrics_are_retained_outside_the_visible_turn_and_message_page() {
        for native_window in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(temp.path()).unwrap();
            store.init_chats().unwrap();
            let mut state = json!({"agent":"codex","thread":{"id":"chat","latestTurn":{"turnId":"old"},"messages":[],"activities":[
                {"id":"context-window-chat","turnId":"old","kind":"context-window.updated","createdAt":"2026-10-25T00:00:00Z","payload":{"usedTokens":42000,"maxTokens":258400}},
                {"id":"provider-usage-chat","turnId":"old","kind":"provider.usage.updated","createdAt":"2026-10-25T00:00:00Z","payload":{"windows":[{"label":"Weekly","usedPercent":5}]}}
            ],"proposedPlans":[]}});
            store.save_chat(&state, None).unwrap();
            if native_window {
                store
                    .db
                    .execute(
                        "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                        [
                            json!({"conversation":true,"latestTurn":"new","next":"older"})
                                .to_string(),
                        ],
                    )
                    .unwrap();
            }
            state["thread"]["latestTurn"] = json!({"turnId":"new"});
            state["thread"]["activities"] = json!([]);
            for n in 0..40 {
                state["thread"]["messages"] = json!([{"id":format!("m{n}"),"turnId":"new","role":"assistant","text":"progress","createdAt":crate::auth::timestamp(1792972800000+n*1000)}]);
                store.save_chat(&state, None).unwrap();
            }
            let display = store.load_chat_display("chat", &[]).unwrap();
            let rows = display["thread"]["activities"].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .filter(|r| r["kind"] == "context-window.updated")
                    .count(),
                1
            );
            assert_eq!(
                rows.iter()
                    .filter(|r| r["kind"] == "provider.usage.updated")
                    .count(),
                1
            );
            assert_eq!(
                rows.iter()
                    .find(|r| r["kind"] == "context-window.updated")
                    .unwrap()["payload"]["usedTokens"],
                42000
            );
            // Native display restores the current response's cached progress;
            // the ordinary actor window and older conversation stay bounded.
            if native_window {
                assert_eq!(display["thread"]["messages"].as_array().unwrap().len(), 40);
                assert!(display["thread"]["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["turnId"] == "new"));
            } else {
                assert!(display["thread"]["messages"].as_array().unwrap().len() <= 24);
            }
        }
    }

    #[test]
    fn new_live_turn_advances_the_window_and_exposes_its_saved_activity() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.init_chats().unwrap();
        store.db.execute("INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
            [json!({"conversation":true,"latestTurn":"previous","next":"older-conversation"}).to_string()]).unwrap();
        let mut state = json!({"agent":"codex","thread":{"id":"chat","latestTurn":{"turnId":"new-live","state":"running","startedAt":"2026-10-26T00:00:00Z"},"messages":[],"activities":[],"proposedPlans":[]}});
        // Exactly the follower path: many events, no fresh native WindowPage.
        for n in 0..60 {
            let at = crate::auth::timestamp(1792972801000 + n * 1000);
            let (kind, row) = if n % 10 == 5 {
                (
                    "messages",
                    json!({"id":format!("progress-{n}"),"role":"assistant","text":"Public progress","turnId":"new-live","createdAt":at,"streaming":false}),
                )
            } else {
                (
                    "activities",
                    json!({"id":format!("tool-{n}"),"kind":"tool.completed","summary":"Command Execution","turnId":"new-live","createdAt":at}),
                )
            };
            state["thread"]["messages"] = json!([]);
            state["thread"]["activities"] = json!([]);
            state["thread"][kind] = json!([row]);
            store.save_chat(&state, None).unwrap();
        }
        let seq: i64 = store
            .db
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='chat_rows'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store.save_chat(&state, None).unwrap();
        assert_eq!(
            seq,
            store
                .db
                .query_row(
                    "SELECT seq FROM sqlite_sequence WHERE name='chat_rows'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap()
        );
        let raw: String = store
            .db
            .query_row(
                "SELECT data FROM native_recent WHERE chat='chat'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let metadata: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(metadata["latestTurn"], "new-live");
        assert_eq!(metadata["next"], "older-conversation");
        // Also reproduce a cold open with metadata written by the older bridge.
        store.db.execute("UPDATE native_recent SET data=json_set(data,'$.latestTurn','previous') WHERE chat='chat'", []).unwrap();
        for _ in 0..2 {
            let page = store
                .desktop_window_page("chat", None, false)
                .unwrap()
                .unwrap();
            assert_eq!(page["messages"].as_array().unwrap().len(), 2);
            assert_eq!(page["messages"][0]["id"], "progress-45");
            assert_eq!(page["messages"][1]["id"], "progress-55");
            let tools = page["activities"].as_array().unwrap();
            assert_eq!(
                tools
                    .iter()
                    .filter(|r| r["kind"] == "tool.completed")
                    .count(),
                22
            );
            let marker = tools.iter().find(|r| r["kind"] == "work.deferred").unwrap();
            assert_eq!(marker["payload"]["source"], "live-window");
            assert_eq!(page["pageInfo"]["olderCursor"], "older-conversation");
            for n in [45, 55] {
                let before = tools
                    .iter()
                    .find(|r| r["id"] == format!("tool-{}", n - 1))
                    .unwrap();
                let after = tools
                    .iter()
                    .find(|r| r["id"] == format!("tool-{}", n + 1))
                    .unwrap();
                let text = page["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["id"] == format!("progress-{n}"))
                    .unwrap();
                assert!(before["sequence"].as_i64() < text["sequence"].as_i64());
                assert!(text["sequence"].as_i64() < after["sequence"].as_i64());
            }
        }
        let page = store.history_page("chat", None, Some("new-live")).unwrap();
        let marker = page["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "work.deferred")
            .unwrap();
        assert_eq!(marker["payload"]["cursor"], page["pageInfo"]["olderCursor"]);
        assert!(marker["payload"]["cursor"]
            .as_str()
            .unwrap()
            .starts_with("v1:"));
        state["thread"]["latestTurn"]["state"] = json!("completed");
        store.save_chat(&state, None).unwrap();
        let marker = store
            .history_row("chat", "activities", "desktop-work-new-live")
            .unwrap()
            .unwrap();
        assert_eq!(
            marker["payload"]["revision"],
            crate::shared_history::work_revision("new-live", "completed")
        );
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM chat_rows WHERE kind='messages'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            6
        );
    }

    #[test]
    fn recent_refresh_keeps_progress_between_its_tools_without_loading_other_turns() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.init_chats().unwrap();
        store
            .db
            .execute(
                "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                [
                    json!({"conversation":true,"latestTurn":"current","next":"older-cursor"})
                        .to_string(),
                ],
            )
            .unwrap();
        for (id, turn) in [("progress", "current"), ("unrelated", "old")] {
            let row = json!({"id":id,"turnId":turn,"role":"assistant","text":"Public progress update","createdAt":"2026-10-25T00:00:00Z","streaming":false});
            store.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat','messages',?1,?2,?3)",params![id,turn,row.to_string()]).unwrap();
        }
        for (id, at) in [
            ("before", "2026-10-24T23:59:59Z"),
            ("after", "2026-10-25T00:00:01Z"),
        ] {
            let row = json!({"id":id,"turnId":"current","kind":"tool.completed","summary":id,"createdAt":at});
            store.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat','activities',?1,'current',?2)",params![id,row.to_string()]).unwrap();
        }
        // None of these rows belongs to the fresh native tail anymore.
        for _ in 0..2 {
            let page = store
                .desktop_window_page("chat", None, false)
                .unwrap()
                .unwrap();
            assert_eq!(page["messages"].as_array().unwrap().len(), 1);
            assert_eq!(page["messages"][0]["id"], "progress");
            assert_eq!(page["activities"][0]["id"], "before");
            assert_eq!(page["activities"][1]["id"], "after");
            assert!(
                page["activities"][0]["sequence"].as_i64()
                    < page["messages"][0]["sequence"].as_i64()
            );
            assert!(
                page["messages"][0]["sequence"].as_i64()
                    < page["activities"][1]["sequence"].as_i64()
            );
            assert_eq!(page["pageInfo"]["olderCursor"], "older-cursor");
        }
        drop(store);
        let mut store = Store::open(temp.path()).unwrap();
        store.init_chats().unwrap();
        assert_eq!(
            store
                .desktop_window_page("chat", None, false)
                .unwrap()
                .unwrap()["messages"][0]["id"],
            "progress"
        );
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM chat_rows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4, "Display reads must preserve durable history");
    }

    #[test]
    fn current_response_restoration_has_a_bounded_page_budget() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.init_chats().unwrap();
        store
            .db
            .execute(
                "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                [json!({"conversation":true,"latestTurn":"current"}).to_string()],
            )
            .unwrap();
        for n in 0..20 {
            let id = format!("progress-{n}");
            let row = json!({"id":id,"turnId":"current","role":"assistant","text":"x".repeat(32_000),"createdAt":"2026-10-25T00:00:00Z"});
            store.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat','messages',?1,'current',?2)",params![id,row.to_string()]).unwrap();
        }
        let page = store
            .desktop_window_page("chat", None, false)
            .unwrap()
            .unwrap();
        assert!(page["messages"].as_array().unwrap().len() <= 5);
        assert!(page.to_string().len() < crate::chat_store::PAGE_BYTES + 1024);
    }
    #[test]
    fn cold_response_pages_restore_tools_and_progress_as_one_ordered_window() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.init_chats().unwrap();
        store
            .db
            .execute(
                "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                [
                    json!({"conversation":true,"latestTurn":"current","next":"older-conversation"})
                        .to_string(),
                ],
            )
            .unwrap();
        let marker = json!({"id":"desktop-work-current","turnId":"current","kind":"work.deferred","createdAt":"2026-10-25T00:00:00Z","payload":{"remaining":0,"loaded":true,"revision":"current:running"}});
        store.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat','activities','desktop-work-current','current',?1)", [marker.to_string()]).unwrap();
        store.db.execute("INSERT INTO native_recent_rows(chat,kind,id) VALUES('chat','activities','desktop-work-current')", []).unwrap();
        // Reverse insertion order makes storage row IDs differ from provider order.
        for n in (0..70).rev() {
            let id = format!("row-{n}");
            let kind = if n % 2 == 0 { "activities" } else { "messages" };
            let at = crate::auth::timestamp(1792886401000 + n * 1000);
            let row = if n % 2 == 0 {
                json!({"id":id,"turnId":"current","kind":"tool.completed","summary":"Read file","createdAt":at})
            } else {
                json!({"id":id,"turnId":"current","role":"assistant","text":"Public progress","createdAt":at})
            };
            store.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat',?1,?2,'current',?3)", params![kind,id,row.to_string()]).unwrap();
        }
        let recent = store
            .desktop_window_page("chat", None, false)
            .unwrap()
            .unwrap();
        assert_eq!(recent["pageInfo"]["olderCursor"], "older-conversation");
        let mut visible = recent["messages"].as_array().unwrap().clone();
        visible.extend(
            recent["activities"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["kind"] != "work.deferred")
                .cloned(),
        );
        visible.sort_by_key(|row| row["sequence"].as_i64().unwrap());
        assert_eq!(visible.len(), 24);
        for (row, n) in visible.iter().zip(46..70) {
            assert_eq!(row["id"], format!("row-{n}"));
        }
        let marker = recent["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == "work.deferred")
            .unwrap();
        assert_eq!(marker["payload"]["remaining"], 1);
        assert!(marker["payload"]["cursor"].is_null());
        let mut cursor = None;
        let mut all = std::collections::BTreeMap::new();
        loop {
            let page = store.history_page("chat", cursor, Some("current")).unwrap();
            for kind in ["messages", "activities"] {
                for row in page[kind]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["kind"] != "work.deferred")
                {
                    let n: i64 = row["id"]
                        .as_str()
                        .unwrap()
                        .strip_prefix("row-")
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert_eq!(row["sequence"], 1792886401000_i64 + n * 1000);
                    all.insert(n, row.clone());
                }
            }
            let marker = page["activities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["kind"] == "work.deferred")
                .unwrap();
            assert_eq!(marker["payload"]["cursor"], page["pageInfo"]["olderCursor"]);
            if page["pageInfo"]["hasOlder"] == false {
                assert_eq!(marker["payload"]["remaining"], 0);
                break;
            }
            cursor = Some(
                page["pageInfo"]["olderCursor"]
                    .as_str()
                    .unwrap()
                    .rsplit(':')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        }
        assert_eq!(all.len(), 70);
    }
}
