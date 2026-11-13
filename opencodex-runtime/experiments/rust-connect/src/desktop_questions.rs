//! Desktop's asynchronous questions are conversation items, not blocking RPCs.
//! Keep their exact reply IDs and native receipts independently of turn lifetime.
use crate::{
    chat::Chat,
    chat_projection::{activity, message, s},
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

fn request_id(item: &str) -> String {
    format!("desktop-question:{item}")
}

/// An unanswered async card stays in history, but only the latest turn can
/// request attention in the chat list. Completion alone does not answer it.
/// Blocking CLI controls retain their separate, live request lifecycle.
pub fn refresh_pending_status(
    db: &rusqlite::Connection,
    chat: &str,
    state: &mut Value,
) -> rusqlite::Result<()> {
    if state["agent"] != "codex" {
        return Ok(());
    }
    let has_async: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='activities' AND json_extract(data,'$.payload.data.desktopAsync')=1)", [chat], |r| r.get(0))?;
    if !has_async {
        return Ok(());
    }
    let latest = state["thread"]["latestTurn"]["turnId"].as_str();
    let (pending_input, pending_blocking): (bool, bool) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='activities'
         AND json_extract(data,'$.kind')='user-input.requested'
         AND json_extract(data,'$.payload.status')='pending'
         AND (COALESCE(json_extract(data,'$.payload.data.desktopAsync'),0)<>1 OR turn=?2)),
         EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='activities'
         AND json_extract(data,'$.kind')='user-input.requested'
         AND json_extract(data,'$.payload.status')='pending'
         AND COALESCE(json_extract(data,'$.payload.data.desktopAsync'),0)<>1)",
        params![chat, latest],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    state["thread"]["hasPendingUserInput"] = json!(pending_input);
    state["thread"]["hasPendingNonBlockingUserInput"] = json!(pending_input && !pending_blocking);
    crate::task_state::refresh(state);
    Ok(())
}

/// Correct pre-fix cached badges before the first chat list is published.
/// Process one metadata record at a time; never change question rows/receipts.
pub fn repair_pending_status(db: &rusqlite::Connection) -> rusqlite::Result<()> {
    let ids = db.prepare("SELECT id FROM chats c WHERE agent='codex' AND EXISTS(SELECT 1 FROM chat_rows r WHERE r.chat=c.id AND r.kind='activities' AND json_extract(r.data,'$.payload.data.desktopAsync')=1)")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let original: String =
            db.query_row("SELECT data FROM chats WHERE id=?1", [&id], |r| r.get(0))?;
        let mut state: Value =
            serde_json::from_str(&original).map_err(|_| rusqlite::Error::InvalidQuery)?;
        refresh_pending_status(db, &id, &mut state)?;
        let updated = state.to_string();
        if updated != original {
            db.execute("UPDATE chats SET data=?2 WHERE id=?1", params![id, updated])?;
        }
    }
    Ok(())
}

/// A recovered card belongs at its native item time, never the time of recovery.
/// Compact summaries have no item clock; retain a previously verified one or
/// use the original turn's start until its work page supplies the exact time.
pub fn stamp_history(
    chat: &mut Chat,
    item: &str,
    native_ms: Option<i64>,
    started: &str,
    previous: Option<&Value>,
) {
    let exact = native_ms
        .filter(|v| *v > 0)
        .map(crate::auth::timestamp)
        .or_else(|| {
            previous
                .and_then(|v| v["payload"]["data"]["nativeCreatedAt"].as_str())
                .filter(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .is_ok()
                })
                .map(str::to_owned)
        });
    if let Some(row) = chat.thread["activities"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == request_id(item))
    {
        row["createdAt"] = json!(exact.as_deref().unwrap_or(started));
        row.as_object_mut().unwrap().remove("sequence");
        if let Some(at) = exact {
            row["payload"]["data"]["nativeCreatedAt"] = json!(at);
        }
    }
}

/// Repair only the bridge's pre-fix recovery timestamps, using the stored
/// original turn boundary. Native transcripts and question identities stay put.
pub fn repair_timestamps(db: &rusqlite::Connection) -> rusqlite::Result<()> {
    db.execute("UPDATE chat_rows SET data=json_set(json_remove(data,'$.sequence'),'$.createdAt',json_extract(data,'$.payload.turnStartedAt'))
        WHERE kind='activities' AND json_extract(data,'$.payload.data.desktopAsync')=1
        AND json_extract(data,'$.kind')='user-input.requested'
        AND json_type(data,'$.payload.data.nativeCreatedAt') IS NULL
        AND julianday(json_extract(data,'$.payload.turnStartedAt'))<=julianday(json_extract(data,'$.payload.turnCompletedAt'))
        AND julianday(json_extract(data,'$.createdAt'))>julianday(json_extract(data,'$.payload.turnCompletedAt'))", [])?;
    Ok(())
}

pub fn questions(item: &Value) -> Option<(String, Value)> {
    let kind = s(&item["type"]).replace('_', "").to_lowercase();
    let (id, args) = if kind == "agentmessage" {
        (s(&item["id"]), item.clone())
    } else if ["functioncall", "dynamictoolcall"].contains(&kind.as_str())
        && item.get("name").or_else(|| item.get("tool"))?.as_str()? == "request_user_input_async"
    {
        let id = item
            .get("call_id")
            .or_else(|| item.get("callId"))
            .or_else(|| item.get("id"))?
            .as_str()?;
        let args = if let Some(raw) = item["arguments"].as_str() {
            if raw.len() > 64 * 1024 {
                return None;
            }
            serde_json::from_str(raw).ok()?
        } else {
            item["arguments"].clone()
        };
        (id, args)
    } else {
        return None;
    };
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    let source = args["questions"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 16)?;
    let mut result = vec![];
    for (index, question) in source.iter().enumerate() {
        let title = question["title"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 8192)?;
        let options = match question.get("options") {
            None | Some(Value::Null) => vec![],
            Some(value) => value
                .as_array()
                .filter(|a| a.len() <= 32)?
                .iter()
                .map(|v| {
                    let label = v
                        .as_str()
                        .filter(|s| !s.trim().is_empty() && s.len() <= 2048)?;
                    Some(json!({"label":label,"description":""}))
                })
                .collect::<Option<Vec<_>>>()?,
        };
        result.push(json!({"id":json!(["request_user_input_async",id,index]).to_string(),
            "header":"Question","question":title,"options":options,"multiSelect":false,"isOther":true,"isSecret":false}));
    }
    let result = json!(result);
    (result.to_string().len() <= 64 * 1024).then(|| (id.into(), result))
}

pub fn replies(text: &str) -> Vec<Value> {
    let text = text.trim();
    if text.len() > 120_000 {
        return vec![];
    }
    let Some(raw) = text
        .strip_prefix("<send_user_message_question_reply>")
        .and_then(|s| s.strip_suffix("</send_user_message_question_reply>"))
    else {
        return vec![];
    };
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    if rows.is_empty() || rows.len() > 32 {
        return vec![];
    }
    rows.into_iter()
        .filter(|r| {
            question_item(s(&r["questionItemId"])).is_some()
                && !s(&r["question"]).trim().is_empty()
                && !s(&r["answer"]).trim().is_empty()
        })
        .collect()
}

fn question_item(id: &str) -> Option<String> {
    let v: Value = serde_json::from_str(id).ok()?;
    let a = v.as_array().filter(|a| a.len() == 3)?;
    let item = a[1].as_str().filter(|s| !s.is_empty() && s.len() <= 128)?;
    let canonical = serde_json::to_string(&v).ok()?;
    (a[0] == "request_user_input_async" && a[2].as_u64().is_some_and(|v| v < 16) && canonical == id)
        .then(|| item.to_owned())
}

/// Keep only public question controls/reply envelopes from the owner snapshot.
pub fn thin(item: &Value) -> Option<Value> {
    if let Some((id, qs)) = questions(item) {
        return Some(
            json!({"type":"agentMessage","id":id,"delivery":"async","text":"",
            "questions":qs.as_array()?.iter().map(|q|json!({"title":q["question"],"options":q["options"].as_array().unwrap().iter().map(|o|o["label"].clone()).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
        );
    }
    let kind = s(&item["type"]).replace('_', "").to_lowercase();
    if !(kind == "usermessage"
        || (kind == "message" && item["role"] == "user")
        || (kind == "steeringusermessage" && item["status"] == "accepted"))
    {
        return None;
    }
    let text = item
        .get("content")
        .or_else(|| item.get("input"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|p| ["text", "input_text"].contains(&s(&p["type"])))
                .map(|p| s(&p["text"]))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| s(&item["text"]).into());
    if replies(&text).is_empty() {
        return None;
    }
    Some(
        json!({"id":item["id"],"type":"userMessage","text":text,"content":[{"type":"text","text":text}]}),
    )
}

pub fn is_request(row: &Value) -> bool {
    row["payload"]["data"]["desktopAsync"] == true
}

pub fn project(chat: &mut Chat, item: &Value) -> bool {
    if let Some((id, questions)) = questions(item) {
        let rid = request_id(&id);
        activity(
            chat,
            &rid,
            "user-input.requested",
            "Question",
            "info",
            json!({"requestId":rid,"status":"pending","questions":questions,"data":{"desktopAsync":true,"itemId":id}}),
        );
        chat.thread["hasPendingUserInput"] = json!(true);
        return true;
    }
    let kind = s(&item["type"]).replace('_', "").to_lowercase();
    if kind == "usermessage"
        || (kind == "message" && item["role"] == "user")
        || (kind == "steeringusermessage" && item["status"] == "accepted")
    {
        let text = item
            .get("content")
            .or_else(|| item.get("input"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|p| ["text", "input_text"].contains(&s(&p["type"])))
                    .map(|p| s(&p["text"]))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| s(&item["text"]).into());
        record_replies(chat, &text, s(&item["id"]));
    }
    false
}

/// Called only for native user items or an acknowledged start/steer response.
pub fn record_replies(chat: &mut Chat, text: &str, source: &str) {
    use sha2::{Digest, Sha256};
    let rows = replies(text);
    if rows.is_empty() {
        return;
    }
    let id = format!(
        "desktop-answer:{:x}",
        Sha256::digest(format!("{source}:{text}"))
    );
    activity(
        chat,
        &id,
        "user-input.resolved",
        "Answer received",
        "info",
        json!({"status":"completed","data":{"desktopQuestionReplies":rows}}),
    );
    if let Some(row) = chat.thread["activities"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == id)
    {
        row["turnId"] = Value::Null;
        row["createdAt"] = json!(crate::auth::timestamp(0));
    }
}

pub fn accept(chat: &mut Chat, input: &Value) {
    let text = s(&input["text"]);
    if replies(text).is_empty() {
        return;
    }
    message(chat, s(&input["messageId"]), text, "user", false, false);
    record_replies(chat, text, s(&input["messageId"]));
    if let Some(skipped) = input["desktopQuestionSkips"].as_array() {
        record_skips(chat, skipped);
    }
}

/// Desktop async Skip is a dismissal, not an empty message or a new turn.
/// Persist a receipt so replaying the native question cannot resurrect its UI.
pub fn record_skips(chat: &mut Chat, skipped: &[Value]) {
    if skipped.is_empty() {
        return;
    }
    use sha2::{Digest, Sha256};
    let id = format!(
        "desktop-skip:{:x}",
        Sha256::digest(json!(skipped).to_string())
    );
    activity(
        chat,
        &id,
        "user-input.resolved",
        "Question skipped",
        "info",
        json!({"status":"completed","data":{"desktopQuestionSkips":skipped}}),
    );
}

/// Call only after `answer` has validated the complete question/answer mapping.
pub fn skipped(row: &Value, answers: &Value) -> Vec<Value> {
    row["payload"]["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let raw = &answers[s(&q["id"])];
            let values = raw.as_array().or_else(|| raw["answers"].as_array());
            values
                .filter(|v| v.is_empty())
                .map(|_| json!({"questionItemId":q["id"],"skipped":true}))
        })
        .collect()
}

pub fn answer(row: &Value, answers: &Value) -> Result<String, String> {
    if !is_request(row)
        || row["kind"] != "user-input.requested"
        || row["payload"]["status"] != "pending"
    {
        return Err("This question was already answered".into());
    }
    // Reuse the CLI's strict key/size/answer validation, but preserve Desktop's
    // envelope: sending a made-up JSON-RPC response would never reach the model.
    let mapped = crate::questions::answer(
        &json!({"method":"item/tool/requestUserInput",
        "params":{"questions":row["payload"]["questions"]}}),
        answers,
    )?;
    let replies: Vec<_> = row["payload"]["questions"].as_array().ok_or("Question unavailable")?.iter().filter_map(|q| {
        let values = mapped["result"]["answers"][s(&q["id"])]["answers"].as_array().unwrap();
        (!values.is_empty()).then(|| json!({"questionItemId":q["id"],"question":q["question"],"answer":values.iter().map(s).collect::<Vec<_>>().join("\n")}))
    }).collect();
    if replies.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
        json!(replies)
    ))
}

/// Receipts precede reconciliation, so reverse paging and native replays cannot
/// resurrect questions. Work is bounded by changed rows, not the whole history.
pub fn save(tx: &Transaction<'_>, chat: &str, state: &Value) -> rusqlite::Result<bool> {
    let mut touched = BTreeSet::new();
    for row in state["thread"]["activities"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if is_request(row) {
            touched.insert(s(&row["id"]).to_owned());
        }
        for reply in row["payload"]["data"]["desktopQuestionReplies"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                row["payload"]["data"]["desktopQuestionSkips"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            )
        {
            if let Some(item) = question_item(s(&reply["questionItemId"])) {
                tx.execute("INSERT INTO desktop_question_answers(chat,question,answer) VALUES(?1,?2,?3) ON CONFLICT(chat,question) DO UPDATE SET answer=excluded.answer WHERE json_extract(desktop_question_answers.answer,'$.skipped')=1 AND json_extract(excluded.answer,'$.answer') IS NOT NULL", params![chat,s(&reply["questionItemId"]),reply.to_string()])?;
                touched.insert(request_id(&item));
            }
        }
    }
    for rid in touched {
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM chat_rows WHERE chat=?1 AND kind='activities' AND id=?2",
                params![chat, rid],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            continue;
        };
        let mut row: Value =
            serde_json::from_str(&data).map_err(|_| rusqlite::Error::InvalidQuery)?;
        if !is_request(&row) {
            continue;
        }
        let mut remaining = vec![];
        let mut answered = Map::new();
        for question in row["payload"]["questions"].as_array().into_iter().flatten() {
            let answer: Option<String> = tx
                .query_row(
                    "SELECT answer FROM desktop_question_answers WHERE chat=?1 AND question=?2",
                    params![chat, s(&question["id"])],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(answer) = answer {
                let answer: Value =
                    serde_json::from_str(&answer).map_err(|_| rusqlite::Error::InvalidQuery)?;
                answered.insert(
                    s(&question["id"]).into(),
                    json!({"answers":if answer["skipped"]==true {json!([])}else{json!([answer["answer"]])}}),
                );
            } else {
                remaining.push(question.clone());
            }
        }
        if !answered.is_empty() {
            if remaining.is_empty() {
                row["kind"] = json!("user-input.resolved");
                row["payload"]["status"] = json!("completed");
                row["payload"]["answers"] = json!(answered);
            } else {
                row["payload"]["questions"] = json!(remaining);
            }
            tx.execute(
                "UPDATE chat_rows SET data=?3 WHERE chat=?1 AND kind='activities' AND id=?2",
                params![chat, rid, row.to_string()],
            )?;
        }
    }
    tx.query_row("SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='activities' AND json_extract(data,'$.payload.data.desktopAsync')=1 AND json_extract(data,'$.kind')='user-input.requested' AND json_extract(data,'$.payload.status')='pending')",[chat],|r|r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat::finish, store::Store};
    fn chat() -> Chat {
        serde_json::from_value(json!({"agent":"codex","native_id":"native","resumed":true,"thread":{"id":"chat","messages":[],"activities":[],"proposedPlans":[],"session":{"status":"running","statusConfidence":"confirmed"},"latestTurn":{"turnId":"turn","state":"running"}},"queue":[]})).unwrap()
    }
    fn item() -> Value {
        json!({"id":"call-a","type":"agentMessage","text":"Pick one","delivery":"async","questions":[{"title":"Pick one","options":["Blue","Green"]},{"title":"Anything else?"}]})
    }
    fn save_chat(db: &mut Store, c: &Chat) {
        db.save_chat(&serde_json::to_value(c).unwrap(), None)
            .unwrap();
    }
    fn reply(q: usize, answer: &str) -> String {
        format!(
            "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
            json!([{"questionItemId":json!(["request_user_input_async","call-a",q]).to_string(),"question":"Question","answer":answer}])
        )
    }

    #[test]
    fn mixed_skip_and_answer_only_sends_actual_text_and_persists_after_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        let row = c.thread["activities"][0].clone();
        let q = &row["payload"]["questions"];
        let answers =
            json!({s(&q[0]["id"]): {"answers":[]},s(&q[1]["id"]):{"answers":["Real answer"]}});
        let text = answer(&row, &answers).unwrap();
        let parsed = replies(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["answer"], "Real answer");
        assert_eq!(parsed[0]["questionItemId"], q[1]["id"]);
        let skipped = skipped(&row, &answers);
        assert_eq!(skipped.len(), 1);
        save_chat(&mut db, &c);
        // A failed native send must leave both questions unresolved.
        assert_eq!(c.thread["activities"][0]["kind"], "user-input.requested");
        accept(
            &mut c,
            &json!({"messageId":"actual-answer","text":text,"desktopQuestionSkips":skipped}),
        );
        save_chat(&mut db, &c);
        let saved = db.load_chat("chat").unwrap();
        let row = saved["thread"]["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "desktop-question:call-a")
            .unwrap();
        assert_eq!(row["kind"], "user-input.resolved");
        assert_eq!(
            row["payload"]["answers"][s(&q[0]["id"])]["answers"],
            json!([])
        );
        // Replaying the native question after reconnect does not reopen it.
        let mut fresh = chat();
        project(&mut fresh, &item());
        save_chat(&mut db, &fresh);
        let replay = db.load_chat("chat").unwrap();
        assert_eq!(replay["thread"]["hasPendingUserInput"], false);
        // A later explicit Desktop answer supersedes a local skip, never vice versa.
        record_replies(
            &mut fresh,
            &reply(0, "Desktop answered later"),
            "desktop-later",
        );
        save_chat(&mut db, &fresh);
        let saved = db.load_chat("chat").unwrap();
        let row = saved["thread"]["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "desktop-question:call-a")
            .unwrap();
        assert_eq!(
            row["payload"]["answers"][s(&q[0]["id"])]["answers"],
            json!(["Desktop answered later"])
        );
    }

    #[test]
    fn canonical_and_raw_items_use_the_same_native_question_ids() {
        let (_, q) = questions(&item()).unwrap();
        assert_eq!(q[0]["id"], "[\"request_user_input_async\",\"call-a\",0]");
        assert_eq!(q[0]["isOther"], true);
        for kind in ["function_call", "dynamicToolCall"] {
            let raw = json!({"type":kind,"call_id":"call-a","name":"request_user_input_async","arguments":json!({"questions":item()["questions"]}).to_string()});
            assert_eq!(questions(&raw).unwrap().1, q);
        }
        assert!(
            questions(&json!({"id":"x","type":"agentMessage","text":"A normal response"}))
                .is_none()
        );
    }

    #[test]
    fn answer_preserves_exact_ids_and_rejects_foreign_or_missing_answers() {
        let mut c = chat();
        project(&mut c, &item());
        let row = &c.thread["activities"][0];
        let qs = &row["payload"]["questions"];
        let answer = answer(
            row,
            &json!({s(&qs[0]["id"]):["Blue"],s(&qs[1]["id"]):{"answers":["My own answer"]}}),
        )
        .unwrap();
        assert_eq!(replies(&answer)[1]["answer"], "My own answer");
        assert!(super::answer(row, &json!({"foreign":["Blue"]})).is_err());
        assert!(replies(&format!("quoted {answer}")).is_empty());
        assert!(replies(&reply(50, "bad")).is_empty());
    }

    #[test]
    fn pending_cards_stay_in_their_turn_across_completion_restart_and_recent_windows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        finish(&mut c, "completed", None);
        save_chat(&mut db, &c);
        assert_eq!(
            db.history_row("chat", "activities", "desktop-question:call-a")
                .unwrap()
                .unwrap()["kind"],
            "user-input.requested"
        );
        c.thread["activities"] = json!([]);
        c.thread["messages"] = json!((0..100)
            .map(|n| json!({"id":format!("m{n}"),"text":"later in same turn","turnId":"turn"}))
            .collect::<Vec<_>>());
        save_chat(&mut db, &c);
        drop(db);
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let display = db.load_chat_display("chat", &[]).unwrap();
        assert_eq!(display["thread"]["hasPendingUserInput"], true);
        assert!(display["thread"]["messages"].as_array().unwrap().len() <= 24);
        assert_eq!(
            display["thread"]["activities"][0]["payload"]["questions"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Loading a new response must not pin this old question to its tail.
        c.thread["latestTurn"]["turnId"] = json!("later");
        c.thread["messages"] = json!((0..100)
            .map(|n| json!({"id":format!("new{n}"),"role":"user","text":"new response","turnId":"later"}))
            .collect::<Vec<_>>());
        save_chat(&mut db, &c);
        let display = db.load_chat_display("chat", &[]).unwrap();
        assert_eq!(display["thread"]["hasPendingUserInput"], false);
        assert_eq!(display["thread"]["hasPendingNonBlockingUserInput"], false);
        assert!(!display["thread"]["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(is_request));
        // The unanswered card remains available in its original turn's history.
        let mut cursor = None;
        let mut found = false;
        for _ in 0..8 {
            let page = db
                .history_page_display("chat", cursor, Some("turn"))
                .unwrap();
            found |= page["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(is_request);
            let Some(next) = page["pageInfo"]["olderCursor"].as_str() else {
                break;
            };
            cursor = Some(next.rsplit(':').next().unwrap().parse().unwrap());
        }
        assert!(found);
    }

    #[test]
    fn latest_question_badge_survives_completion_but_not_a_newer_turn_or_old_replay() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        save_chat(&mut db, &c);
        let old_question = db
            .history_row("chat", "activities", "desktop-question:call-a")
            .unwrap()
            .unwrap();

        c.thread["latestTurn"] = json!({"turnId":"new-turn","state":"running"});
        c.thread["activities"] = json!([old_question.clone()]);
        save_chat(&mut db, &c);
        let saved = db.load_chat("chat").unwrap();
        assert_eq!(saved["thread"]["hasPendingUserInput"], false);
        assert_eq!(saved["thread"]["session"]["executionState"], "running");

        let mut newest = item();
        newest["id"] = json!("call-new");
        project(&mut c, &newest);
        finish(&mut c, "completed", None);
        save_chat(&mut db, &c);
        let saved = db.load_chat("chat").unwrap();
        assert_eq!(saved["thread"]["hasPendingUserInput"], true);
        assert_eq!(saved["thread"]["hasPendingNonBlockingUserInput"], true);
        assert_eq!(saved["thread"]["session"]["executionState"], "idle");

        for n in 0..2 {
            let text = reply(n, "Blue").replace("call-a", "call-new");
            record_replies(&mut c, &text, &format!("new-answer-{n}"));
        }
        save_chat(&mut db, &c);
        assert_eq!(
            db.load_chat("chat").unwrap()["thread"]["hasPendingUserInput"],
            false
        );
        assert_eq!(
            db.history_row("chat", "activities", "desktop-question:call-a")
                .unwrap()
                .unwrap(),
            old_question
        );
    }

    #[test]
    fn startup_repairs_stale_badges_without_answering_or_removing_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        save_chat(&mut db, &c);
        let old_question = db
            .history_row("chat", "activities", "desktop-question:call-a")
            .unwrap()
            .unwrap();
        // Simulate metadata persisted by an older bridge while its question
        // belongs to an earlier turn. This chat need not be opened to repair it.
        db.db.execute("UPDATE chats SET data=json_set(data,'$.thread.latestTurn.turnId','new-turn','$.thread.hasPendingUserInput',json('true'),'$.thread.hasPendingNonBlockingUserInput',json('true')) WHERE id='chat'", []).unwrap();
        drop(db);
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        assert_eq!(
            db.chat_headers().unwrap()[0]["thread"]["hasPendingUserInput"],
            false
        );
        assert_eq!(
            db.load_chat_display("chat", &[]).unwrap()["thread"]["hasPendingNonBlockingUserInput"],
            false
        );
        assert_eq!(
            db.history_row("chat", "activities", "desktop-question:call-a")
                .unwrap()
                .unwrap(),
            old_question
        );
        let first = db.chat_headers().unwrap();
        db.init_chats().unwrap();
        assert_eq!(db.chat_headers().unwrap(), first);
    }

    #[test]
    fn native_history_time_survives_summary_refresh_and_later_turns() {
        let mut c = chat();
        project(&mut c, &item());
        stamp_history(
            &mut c,
            "call-a",
            Some(1700000005000),
            "2023-11-14T22:13:20Z",
            None,
        );
        let original = c.thread["activities"][0].clone();
        assert_eq!(original["createdAt"], crate::auth::timestamp(1700000005000));
        c.thread["activities"] = json!([]);
        project(&mut c, &item());
        stamp_history(
            &mut c,
            "call-a",
            None,
            "2023-11-14T22:13:20Z",
            Some(&original),
        );
        assert_eq!(
            c.thread["activities"][0]["createdAt"],
            original["createdAt"]
        );
        assert_eq!(c.thread["activities"][0]["turnId"], "turn");
        assert!(c.thread["activities"][0].get("sequence").is_none());
    }

    #[test]
    fn reopening_repairs_only_recovered_question_times_outside_their_original_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        let row = &mut c.thread["activities"][0];
        row["createdAt"] = json!("2026-10-26T02:55:54.551Z");
        row["payload"]["turnStartedAt"] = json!("2026-10-26T00:52:46Z");
        row["payload"]["turnCompletedAt"] = json!("2026-10-26T01:07:41Z");
        let mut correct = row.clone();
        correct["id"] = json!("correct");
        correct["createdAt"] = json!("2026-10-26T01:00:00Z");
        let mut tool = row.clone();
        tool["id"] = json!("ordinary-tool");
        tool["payload"]["data"] = json!({});
        // Turn completion is second precision; an authoritative item clock may
        // be a few milliseconds after that boundary. Never rewrite that clock.
        let mut exact = row.clone();
        exact["id"] = json!("native-precise");
        exact["createdAt"] = json!("2026-10-26T01:07:41.551Z");
        exact["payload"]["data"]["nativeCreatedAt"] = exact["createdAt"].clone();
        c.thread["activities"].as_array_mut().unwrap().extend([
            correct.clone(),
            tool.clone(),
            exact.clone(),
        ]);
        save_chat(&mut db, &c);
        drop(db);
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let repaired = db
            .history_row("chat", "activities", "desktop-question:call-a")
            .unwrap()
            .unwrap();
        assert_eq!(repaired["createdAt"], "2026-10-26T00:52:46Z");
        assert_eq!(repaired["turnId"], "turn");
        assert_eq!(repaired["payload"]["status"], "pending");
        assert_eq!(
            db.history_row("chat", "activities", "correct")
                .unwrap()
                .unwrap(),
            correct
        );
        assert_eq!(
            db.history_row("chat", "activities", "ordinary-tool")
                .unwrap()
                .unwrap(),
            tool
        );
        assert_eq!(
            db.history_row("chat", "activities", "native-precise")
                .unwrap()
                .unwrap(),
            exact
        );
        let changes = db.db.total_changes();
        db.init_chats().unwrap();
        assert_eq!(db.db.total_changes(), changes);
    }

    #[test]
    fn pinned_question_uses_the_native_window_clock_and_original_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        stamp_history(
            &mut c,
            "call-a",
            Some(1700000005000),
            "2023-11-14T22:13:20Z",
            None,
        );
        save_chat(&mut db, &c);
        // A bounded native window has only the initiating prompt, not the card.
        db.db
            .execute(
                "INSERT INTO native_recent(chat,data) VALUES('chat',?1)",
                [json!({"latestTurn":"turn","conversation":false,"next":null}).to_string()],
            )
            .unwrap();
        c.thread["activities"] = json!([]);
        c.thread["messages"] = json!([{"id":"prompt","turnId":"turn","role":"user","text":"ask","createdAt":"2023-11-14T22:13:20Z"}]);
        save_chat(&mut db, &c);
        let display = db.load_chat_display("chat", &[]).unwrap();
        let q = display["thread"]["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| is_request(r))
            .unwrap();
        assert_eq!(q["sequence"], 1700000005000i64);
        assert_eq!(q["turnId"], "turn");
        assert_eq!(q["createdAt"], crate::auth::timestamp(1700000005000));
    }

    #[test]
    fn partial_desktop_replies_and_reverse_paging_never_resurrect_answers() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        record_replies(&mut c, &reply(0, "Blue"), "native-reply");
        save_chat(&mut db, &c);
        project(&mut c, &item());
        save_chat(&mut db, &c);
        let row = db
            .history_row("chat", "activities", "desktop-question:call-a")
            .unwrap()
            .unwrap();
        assert_eq!(row["payload"]["questions"].as_array().unwrap().len(), 1);
        assert_eq!(row["payload"]["questions"][0]["question"], "Anything else?");
        record_replies(&mut c, &reply(1, "Custom answer"), "native-reply-2");
        save_chat(&mut db, &c);
        c.thread["activities"] = json!([]);
        project(&mut c, &item());
        save_chat(&mut db, &c);
        assert_eq!(
            db.load_chat_display("chat", &[]).unwrap()["thread"]["hasPendingUserInput"],
            false
        );
        assert_eq!(
            db.history_row("chat", "activities", "desktop-question:call-a")
                .unwrap()
                .unwrap()["kind"],
            "user-input.resolved"
        );
    }

    #[test]
    fn pending_async_question_does_not_claim_the_running_model_is_paused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let mut c = chat();
        project(&mut c, &item());
        save_chat(&mut db, &c);
        assert_eq!(
            db.load_chat("chat").unwrap()["thread"]["session"]["executionState"],
            "running"
        );
        c.thread["activities"].as_array_mut().unwrap().push(
            json!({"id":"blocking","kind":"user-input.requested","payload":{"status":"pending"}}),
        );
        save_chat(&mut db, &c);
        assert_eq!(
            db.load_chat("chat").unwrap()["thread"]["session"]["executionState"],
            "paused"
        );
    }
}
