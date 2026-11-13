//! Display cached Work in the opening snapshot. The actor and durable rows keep
//! their original, small live window and full tool details respectively.
use crate::store::Store;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

// Opening a long chat must not eagerly transfer and lay out several full turns.
// Small cached turns still open complete; larger ones use the existing Work
// loader after the recent snapshot has painted. These are display-only limits.
const BYTES: usize = crate::chat_store::PAGE_BYTES;
const ROWS: usize = 96;

pub(crate) fn compact(row: &mut Value) {
    if !row["kind"].as_str().is_some_and(|k| k.starts_with("tool."))
        || row["payload"]["data"]["providerName"] != "codex"
    {
        return;
    }
    if let Some(data) = row["payload"]["data"].as_object_mut() {
        data.remove("input");
        for key in ["command", "inputDetail"] {
            if let Some(text) = data.get(key).and_then(Value::as_str) {
                data.insert(
                    key.into(),
                    json!(crate::chat_projection::bounded(text, 512)),
                );
            }
        }
    }
    if let Some(text) = row["payload"]["detail"].as_str() {
        row["payload"]["detail"] = json!(crate::chat_projection::bounded(text, 512));
    }
}

/// A completed native scan cannot become incomplete just because a later
/// thread/turns/list summary omits tool items. In-progress scans can change.
pub(crate) fn retain_completed(previous: &Value, incoming: &Value) -> bool {
    let revision = previous["payload"]["revision"].as_str().unwrap_or("");
    previous["kind"] == "work.deferred"
        && incoming["kind"] == "work.deferred"
        && previous["turnId"] == incoming["turnId"]
        && previous["payload"]["revision"] == incoming["payload"]["revision"]
        && [":completed:v", ":interrupted:v", ":failed:v"]
            .iter()
            .any(|status| revision.contains(status))
        && previous["payload"]["loaded"] == true
        && previous["payload"]["remaining"] == 0
        && previous["payload"]["cursor"].is_null()
}

impl Store {
    pub(crate) fn restore_work_snapshot(&self, id: &str, page: &mut Value) -> rusqlite::Result<()> {
        for row in page["activities"].as_array_mut().unwrap() {
            compact(row);
        }
        let mut bytes = page.to_string().len();
        let mut ids = std::collections::HashSet::new();
        let mut sequences = std::collections::HashMap::new();
        for kind in ["messages", "activities", "proposedPlans"] {
            for row in page[kind].as_array().unwrap() {
                ids.insert((kind.to_owned(), row["id"].as_str().unwrap_or("").to_owned()));
            }
        }
        let mut count = ids.len();
        let markers: Vec<_> = page["activities"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .filter(|r| r["kind"] == "work.deferred")
            .filter_map(|r| {
                Some((
                    r["id"].as_str()?.to_owned(),
                    r["turnId"].as_str()?.to_owned(),
                ))
            })
            .take(3)
            .collect();
        for (marker_id, turn) in markers {
            let marker: Option<String> = self
                .db
                .query_row(
                    "SELECT data FROM chat_rows WHERE chat=?1 AND kind='activities' AND id=?2",
                    params![id, marker_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(marker) = marker else { continue };
            let mut marker: Value =
                serde_json::from_str(&marker).map_err(|_| rusqlite::Error::InvalidQuery)?;
            // Count through the turn index before reading JSON bodies. Sorting
            // thousands of old command bodies just to restore a small prefix
            // would move the opening delay from the network into SQLite.
            let restore = count < ROWS
                && bytes < BYTES
                && self.db.query_row(
                    "SELECT count(*) FROM (SELECT 1 FROM chat_rows WHERE chat=?1 AND turn=?2 LIMIT ?3)",
                    params![id, turn, ROWS as i64 + 1],
                    |r| r.get::<_, i64>(0),
                )? <= ROWS as i64;
            let mut complete = false;
            if restore {
                // Stream one durable row at a time. Large command bodies never
                // accumulate in RAM and are retained on disk for getToolDetails.
                // Start beside the already-visible tail, rather than restoring an
                // old prefix separated from it by an unloaded middle. The final
                // sort keeps native display order, including equal timestamps.
                let mut stmt = self.db.prepare("SELECT kind,data,seq FROM chat_rows WHERE chat=?1 AND turn=?2 AND COALESCE(json_extract(data,'$.kind'),'')<>'work.deferred' ORDER BY julianday(json_extract(data,'$.createdAt')) DESC,seq DESC LIMIT ?3")?;
                let mut rows = stmt.query(params![id, turn, ROWS as i64 + 1])?;
                complete = true;
                let mut seen = 0;
                while let Some(r) = rows.next()? {
                    seen += 1;
                    if seen > ROWS {
                        complete = false;
                        break;
                    }
                    let kind: String = r.get(0)?;
                    let raw: String = r.get(1)?;
                    let mut row: Value =
                        serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let key = (kind.clone(), row["id"].as_str().unwrap_or("").to_owned());
                    sequences.insert(key.clone(), r.get::<_, i64>(2)?);
                    if ids.contains(&key) {
                        continue;
                    }
                    compact(&mut row);
                    if kind == "messages" {
                        self.present_legacy_uploads(id, &mut row)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    }
                    crate::desktop_window::sequence(&mut row);
                    let size = row.to_string().len();
                    if count >= ROWS || bytes + size > BYTES {
                        complete = false;
                        break;
                    }
                    count += 1;
                    bytes += size;
                    ids.insert(key);
                    page[&kind].as_array_mut().unwrap().push(row);
                }
            }
            // Cached rows need not cover native history yet. Do not turn a
            // summary-only response into a false native completion signal.
            let covered = complete
                && marker["payload"]["cursor"].is_null()
                && (marker["payload"]["remaining"] == 0
                    || marker["payload"]["source"] == "live-window");
            marker["payload"]["preloaded"] = json!(covered);
            marker["payload"]["loaded"] = json!(covered);
            marker["payload"]["remaining"] = json!(if covered { 0 } else { 1 });
            crate::desktop_window::sequence(&mut marker);
            if let Some(row) = page["activities"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == marker_id)
            {
                *row = marker;
            }
        }
        for kind in ["messages", "activities", "proposedPlans"] {
            // Stable for equal native timestamps; never put all progress text
            // before all tools or change their native timeline positions.
            page[kind].as_array_mut().unwrap().sort_by_key(|r| {
                (
                    r["sequence"].as_i64().unwrap_or_default(),
                    sequences
                        .get(&(kind.to_owned(), r["id"].as_str().unwrap_or("").to_owned()))
                        .copied()
                        .unwrap_or_default(),
                )
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(count: usize, cursor: Value, text_size: usize) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        db.db
            .execute(
                "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                [
                    json!({"conversation":true,"latestTurn":"turn","next":"older-conversation"})
                        .to_string(),
                ],
            )
            .unwrap();
        let marker = json!({"id":"work","turnId":"turn","kind":"work.deferred","createdAt":"2026-10-31T00:00:00Z",
            "payload":{"cursor":cursor,"remaining":if cursor.is_null(){0}else{1},"loaded":cursor.is_null(),"revision":"turn:completed:v8"}});
        let mut state = json!({"agent":"codex","thread":{"id":"chat","messages":[],"activities":[marker]},"queue":[]});
        db.save_chat(&state, None).unwrap();
        for n in 0..count {
            let at = crate::auth::timestamp(1793404801000 + (n / 2) as i64);
            let row = if n % 2 == 0 {
                json!({"id":format!("row-{n}"),"turnId":"turn","kind":"tool.completed","createdAt":at,
                    "payload":{"data":{"providerName":"codex","input":{"script":"PRIVATE_LARGE_BODY".repeat(500)},
                        "inputDetail":"é".repeat(4000),"toolImages":["/saved/image.png"]}}})
            } else {
                json!({"id":format!("row-{n}"),"turnId":"turn","role":"assistant","phase":"commentary","text":"p".repeat(text_size),"createdAt":at})
            };
            state["thread"]["activities"] = if n % 2 == 0 {
                json!([row.clone()])
            } else {
                json!([])
            };
            state["thread"]["messages"] = if n % 2 == 1 { json!([row]) } else { json!([]) };
            db.save_chat(&state, None).unwrap();
        }
        (dir, db)
    }
    #[test]
    fn first_display_contains_saved_tools_and_progress_in_native_order_without_details() {
        let (_dir, db) = fixture(80, Value::Null, 40);
        let actor = db
            .desktop_window_page("chat", None, false)
            .unwrap()
            .unwrap();
        assert!(
            actor["messages"].as_array().unwrap().len()
                + actor["activities"].as_array().unwrap().len()
                <= 25
        );
        let page = db.desktop_window_page("chat", None, true).unwrap().unwrap();
        assert_eq!(page["messages"].as_array().unwrap().len(), 40);
        assert_eq!(page["activities"].as_array().unwrap().len(), 41);
        assert_eq!(page["pageInfo"]["olderCursor"], "older-conversation");
        assert_eq!(page["activities"][0]["payload"]["preloaded"], true);
        assert!(!page.to_string().contains("PRIVATE_LARGE_BODY"));
        for (n, row) in page["activities"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .enumerate()
        {
            assert_eq!(row["id"], format!("row-{}", n * 2));
            assert_eq!(
                row["payload"]["data"]["toolImages"],
                json!(["/saved/image.png"])
            );
            assert!(
                row["payload"]["data"]["inputDetail"]
                    .as_str()
                    .unwrap()
                    .len()
                    <= 512
            );
        }
        assert!(db
            .history_row("chat", "activities", "row-0")
            .unwrap()
            .unwrap()
            .to_string()
            .contains("PRIVATE_LARGE_BODY"));
    }
    #[test]
    fn incomplete_native_scan_keeps_cursor_while_cached_work_is_already_visible() {
        let (_dir, db) = fixture(80, json!("i1:opaque"), 40);
        let page = db.desktop_window_page("chat", None, true).unwrap().unwrap();
        assert_eq!(page["activities"].as_array().unwrap().len(), 41);
        assert_eq!(page["activities"][0]["payload"]["cursor"], "i1:opaque");
        assert_eq!(page["activities"][0]["payload"]["preloaded"], false);
        assert_eq!(page["activities"][0]["payload"]["remaining"], 1);
    }
    #[test]
    fn large_work_stays_bounded_and_can_still_page() {
        let (_dir, db) = fixture(180, Value::Null, 32_000);
        let page = db.desktop_window_page("chat", None, true).unwrap().unwrap();
        assert!(page.to_string().len() < BYTES + 1024);
        assert_eq!(page["activities"][0]["payload"]["remaining"], 1);
        assert_eq!(page["activities"][0]["payload"]["preloaded"], false);
        assert!(
            db.history_page("chat", None, Some("turn")).unwrap()["pageInfo"]["hasOlder"]
                .as_bool()
                .unwrap()
        );
    }
    #[test]
    fn long_open_keeps_a_small_recent_tail_and_every_older_row_remains_pageable() {
        let (_dir, db) = fixture(400, Value::Null, 40);
        // An older page may be imported after the live tail. Its insertion
        // sequence must not promote it into the newest display window.
        let late = json!({"id":"late-old","turnId":"turn","role":"assistant","text":"Older imported progress","createdAt":"2026-10-30T00:00:00Z"});
        db.db.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('chat','messages','late-old','turn',?1)", [late.to_string()]).unwrap();
        let recent = db
            .desktop_window_page("chat", None, false)
            .unwrap()
            .unwrap();
        let page = db.desktop_window_page("chat", None, true).unwrap().unwrap();
        let count = page["messages"].as_array().unwrap().len()
            + page["activities"].as_array().unwrap().len();
        assert!(
            count <= ROWS,
            "The opening budget includes the existing tail"
        );
        assert_eq!(
            count,
            recent["messages"].as_array().unwrap().len()
                + recent["activities"].as_array().unwrap().len(),
            "Long responses keep the existing recent tail while Work is prepared"
        );
        assert!(page.to_string().len() <= BYTES);
        assert_eq!(
            page["messages"].as_array().unwrap().last().unwrap()["id"],
            "row-399"
        );
        assert!(!page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "late-old"));
        assert!(!page["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "row-0"));
        let marker = page["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "work")
            .unwrap();
        assert_eq!(marker["payload"]["preloaded"], false);
        assert_eq!(marker["payload"]["loaded"], false);
        assert_eq!(marker["payload"]["remaining"], 1);
        assert_eq!(page["pageInfo"]["olderCursor"], "older-conversation");

        // Walk the real store paging path used by getTurnWork. A display limit
        // must not delete rows or signal that the missing activity is complete.
        let mut before = None;
        let mut found = std::collections::HashSet::new();
        loop {
            let older = db
                .history_page_display("chat", before, Some("turn"))
                .unwrap();
            assert!(!older.to_string().contains("PRIVATE_LARGE_BODY"));
            for kind in ["messages", "activities"] {
                for row in older[kind].as_array().unwrap() {
                    if row["id"] != "work" {
                        assert!(found.insert(row["id"].as_str().unwrap().to_owned()));
                    }
                }
            }
            let Some(cursor) = older["pageInfo"]["olderCursor"].as_str() else {
                break;
            };
            let next = cursor
                .strip_prefix("v1:chat:")
                .unwrap()
                .parse::<i64>()
                .unwrap();
            assert!(before.is_none_or(|previous| next < previous));
            before = Some(next);
        }
        assert_eq!(found.len(), 401);
        assert!(db
            .history_row("chat", "activities", "row-0")
            .unwrap()
            .unwrap()
            .to_string()
            .contains("PRIVATE_LARGE_BODY"));
    }
    #[test]
    fn completed_coverage_survives_summary_replay_but_not_a_changed_projection() {
        let (_dir, mut db) = fixture(80, Value::Null, 40);
        let mut marker = db
            .history_row("chat", "activities", "work")
            .unwrap()
            .unwrap();
        marker["payload"]["cursor"] = json!("i1:restart");
        marker["payload"]["remaining"] = json!(1);
        marker["payload"]["loaded"] = json!(false);
        let mut state = json!({"agent":"codex","thread":{"id":"chat","activities":[marker],"messages":[]},"queue":[]});
        db.save_chat(&state, None).unwrap();
        let saved = db
            .history_row("chat", "activities", "work")
            .unwrap()
            .unwrap();
        assert_eq!(saved["payload"]["remaining"], 0);
        assert!(saved["payload"]["cursor"].is_null());
        state["thread"]["activities"][0]["payload"]["revision"] = json!("turn:completed:v9");
        db.save_chat(&state, None).unwrap();
        assert_eq!(
            db.history_row("chat", "activities", "work")
                .unwrap()
                .unwrap()["payload"]["remaining"],
            1
        );
        let mut running = saved;
        running["payload"]["revision"] = json!("turn:inProgress:v8");
        assert!(!retain_completed(&running, &running));
    }
}
