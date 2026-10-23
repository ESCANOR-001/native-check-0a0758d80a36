//! Bounded projection of a verified Claude observer stream. The caller commits
//! the returned rows and reducer state with the stream cursor in one transaction.
use crate::{
    auth_service::AuthService,
    chat::{AgentKind, Chat},
    chat_projection::{self as projection, now, s},
    claude_cursor::Cursor,
    claude_remote::Envelope,
    claude_turns::{Input, Router},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Default, Serialize, Deserialize)]
struct State {
    router: Router,
    history_turn: Option<String>,
    #[serde(default)]
    history_started_at: Option<String>,
    #[serde(default)]
    stream_id: String,
    #[serde(default)]
    stream_model: String,
    #[serde(default)]
    requests: HashMap<String, Value>,
    #[serde(default)]
    processed_steers: Vec<Input>,
    #[serde(default)]
    processed_inputs: Vec<String>,
    #[serde(default)]
    historical_result: Option<String>,
    /// Worker process verified at the last committed owner checkpoint.
    #[serde(default)]
    owner_pid: Option<u64>,
}
pub(crate) const OWNER_RESTARTED: &str = "The original Claude session restarted before this task finished. Its saved output is kept; nothing was resent.";

async fn delivery_receipts(
    db: &AuthService,
    chat: &mut Chat,
    reduced: &mut State,
    event: &Envelope,
) -> Result<(), String> {
    if event.deliveries.len() > 64 {
        return Err("Too many Claude delivery receipts".into());
    }
    for receipt in &event.deliveries {
        if receipt.processed
            && (reduced.router.pending().any(|i| i.native == receipt.input)
                || event.payload.as_ref().is_some_and(|p| {
                    event.source == "client" && p["type"] == "user" && p["uuid"] == receipt.input
                }))
            && !reduced.processed_inputs.contains(&receipt.input)
        {
            if reduced.processed_inputs.len() >= 64 {
                return Err("Too many Claude processing receipts".into());
            }
            reduced.processed_inputs.push(receipt.input.clone());
        }
        let cid = s(&chat.thread["id"]).to_owned();
        let sid = chat.native_id.clone();
        let native = receipt.input.clone();
        let Some(record) = db
            .call(move |a| a.store.claude_input(&cid, &sid, &native))
            .await?
            .map_err(|_| "Claude receipt mapping unavailable")?
        else {
            continue;
        };
        if !record.input.local || !record.input.steer || record.state == "cancelled" {
            continue;
        }
        let input = record.input;
        if !chat.thread["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == input.native)
        {
            let cid = s(&chat.thread["id"]).to_owned();
            let native = input.native.clone();
            if let Some(row) = db
                .call(move |a| a.store.history_row(&cid, "activities", &native))
                .await?
                .map_err(|_| "Claude steering history unavailable")?
            {
                chat.thread["activities"].as_array_mut().unwrap().push(row);
            }
        }
        if let Some(row) = chat.thread["activities"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| {
                r["id"] == input.native
                    && r["kind"] == "steer.updated"
                    && r["turnId"] == input.public_turn
            })
        {
            row["summary"] = json!("Steering message received");
            row["payload"]["status"] = json!("completed");
            row["payload"]["detail"] = json!("Confirmed by Claude delivery receipt");
        }
        if receipt.processed
            && !reduced
                .processed_steers
                .iter()
                .any(|i| i.native == input.native)
        {
            if reduced.processed_steers.len() >= 64 {
                return Err("Too many unsettled Claude steering receipts".into());
            }
            reduced.processed_steers.push(input);
        }
    }
    Ok(())
}
fn settle_deliveries(state: &mut State) {
    if state.historical_result.as_deref().is_some_and(|turn| {
        state
            .router
            .settle_historical_result(turn, &state.processed_inputs)
    }) {
        state.historical_result = None;
    }
    state
        .processed_steers
        .retain(|input| !state.router.settle_processed_steer(input));
    state
        .processed_inputs
        .retain(|id| state.router.pending().any(|i| i.native == *id));
}

async fn reconcile_stop(
    db: &AuthService,
    chat: &mut Chat,
    state: &mut State,
    event: &Envelope,
) -> Result<(), String> {
    if event.source == "client" {
        if let Some(v) = event
            .payload
            .as_ref()
            .filter(|p| p["type"] == "control_request" && p["request"]["subtype"] == "interrupt")
        {
            if let Some(turn) = state.router.active() {
                let request = s(&v["request_id"]);
                if !request.is_empty()
                    && request.len() <= 256
                    && chat
                        .external_stop
                        .as_ref()
                        .is_none_or(|s| s.request != request || s.turn != turn.public_turn)
                {
                    chat.external_stop = Some(crate::chat::StopIntent {
                        turn: turn.public_turn.clone(),
                        request: request.into(),
                        acknowledged: false,
                        result_seen: false,
                        background: Vec::new(),
                    });
                    chat.paused = true;
                }
            }
        }
    }
    let Some(stop) = chat.external_stop.as_mut() else {
        return Ok(());
    };
    let Some(active) = state
        .router
        .active()
        .filter(|t| t.public_turn == stop.turn)
        .cloned()
    else {
        return Ok(());
    };
    if event.source == "worker" && event.sequence.is_some() {
        if let Some(value) = &event.payload {
            state.router.acknowledge_task_stop(value, &stop.background);
            if value["type"] == "control_response"
                && value["response"]["request_id"] == stop.request
                && value["response"]["subtype"] == "success"
            {
                stop.acknowledged = true;
            }
            if value["type"] == "result"
                && value["parent_tool_use_id"].is_null()
                && value.get("user_message_uuid").is_none()
                && value.get("user_message_uuids").is_none()
            {
                stop.result_seen = true;
            }
        }
    }
    if !stop.acknowledged || !stop.result_seen {
        return Ok(());
    }
    for input in &active.inputs {
        if state.processed_inputs.contains(input) {
            continue;
        }
        let cid = s(&chat.thread["id"]).to_owned();
        let sid = chat.native_id.clone();
        let iid = input.clone();
        if !db
            .call(move |a| a.store.claude_input(&cid, &sid, &iid))
            .await?
            .map_err(|_| "Claude Stop receipt unavailable")?
            .is_some_and(|r| r.state == "completed")
        {
            return Ok(());
        }
    }
    if state.router.confirm_stopped(&active.public_turn) {
        crate::chat::finish(chat, "interrupted", None);
        state.requests.clear();
        chat.external_stop = None;
        settle_deliveries(state);
    }
    Ok(())
}

pub fn control_state(projection: &Value) -> Result<(Router, HashMap<String, Value>), String> {
    let state: State = if projection.is_null() {
        State::default()
    } else {
        serde_json::from_value(projection.clone())
            .map_err(|_| "Invalid Claude control projection")?
    };
    Ok((
        state.router,
        state
            .requests
            .into_values()
            .map(|v| (s(&v["publicId"]).to_owned(), v))
            .collect(),
    ))
}

pub async fn project(
    db: &AuthService,
    state: Value,
    cursor: &Cursor,
    event: &Envelope,
    catchup: bool,
) -> Result<(Value, Value), String> {
    let mut chat: Chat =
        serde_json::from_value(state).map_err(|_| "Invalid observer chat metadata")?;
    if chat.agent != AgentKind::Claude
        || chat.native_id != cursor.identity.native_session
        || chat.remote_identity.as_ref() != Some(&cursor.identity)
    {
        return Err("Observer projection belongs to another chat".into());
    }
    let mut reduced: State = if cursor.projection.is_null() {
        State::default()
    } else {
        serde_json::from_value(cursor.projection.clone())
            .map_err(|_| "Saved Claude projection is invalid")?
    };
    chat.stream_id = reduced.stream_id.clone();
    chat.stream_model = reduced.stream_model.clone();
    delivery_receipts(db, &mut chat, &mut reduced, event).await?;
    reconcile_stop(db, &mut chat, &mut reduced, event).await?;
    if event.source == "local" {
        let pid = event
            .payload
            .as_ref()
            .filter(|p| p["type"] == "remodex_owner")
            .ok_or("Unknown local Claude checkpoint")?["pid"]
            .as_u64();
        if matches!((reduced.owner_pid, pid), (Some(a), Some(b)) if a != b) {
            // The process that held this turn is gone without a result.
            reduced.router.owner_gone();
            reduced.requests.clear();
            chat.external_stop = None;
            chat.thread["hasPendingApprovals"] = json!(false);
            chat.thread["hasPendingUserInput"] = json!(false);
            if chat.thread["latestTurn"]["state"] == "running" {
                crate::chat::finish(&mut chat, "interrupted", Some(OWNER_RESTARTED));
            }
        }
        if let Some(ids) = event
            .payload
            .as_ref()
            .and_then(|p| p.get("internal_inputs"))
        {
            let ids: Vec<String> = serde_json::from_value(ids.clone())
                .map_err(|_| "Invalid Claude internal input checkpoint")?;
            reduced.router.retire_internal_inputs(&ids)?;
        }
        reduced.owner_pid = pid.or(reduced.owner_pid);
        settle_deliveries(&mut reduced);
        return Ok((
            serde_json::to_value(chat).unwrap(),
            serde_json::to_value(reduced).unwrap(),
        ));
    }
    let Some(value) = &event.payload else {
        settle_deliveries(&mut reduced);
        return Ok((
            serde_json::to_value(chat).unwrap(),
            serde_json::to_value(reduced).unwrap(),
        ));
    };
    if event.sequence.is_none() && catchup {
        return Ok((
            serde_json::to_value(chat).unwrap(),
            serde_json::to_value(reduced).unwrap(),
        ));
    }
    let mut value = value.clone();
    if event.source == "worker" && event.sequence.is_some() {
        crate::native_control::apply(&mut chat, &value);
    }
    if event.source == "worker"
        && value["type"] == "user"
        && chat.external_stop.as_ref().is_some_and(|s| s.acknowledged)
        && value["message"]["content"].as_array().is_some_and(|a| {
            a.len() == 1
                && a[0]["type"] == "text"
                && matches!(
                    a[0]["text"].as_str(),
                    Some(
                        "[Request interrupted by user]"
                            | "[Request interrupted by user for tool use]"
                    )
                )
        })
    {
        // Native interruption notices are not newly submitted human prompts.
        value["isSynthetic"] = json!(true);
        value["shouldQuery"] = json!(false);
    }
    if value["session_id"]
        .as_str()
        .is_some_and(|s| s != cursor.identity.native_session && s != cursor.identity.bridge_session)
    {
        return Err("Claude output belongs to another session".into());
    }
    if value.get("session_id").is_some() {
        value["session_id"] = json!(chat.native_id);
    }
    let event_time = event
        .created_at
        .as_deref()
        .or_else(|| value["timestamp"].as_str())
        .filter(|s| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).is_ok()
        })
        .map(str::to_owned)
        .unwrap_or_else(now);
    let id = s(&chat.thread["id"]).to_owned();
    // Fetch exact spilled rows, never the full transcript or tool output.
    let mut keys = vec![
        s(&value["uuid"]).to_owned(),
        s(&value["message"]["id"]).to_owned(),
        chat.stream_id.clone(),
        format!(
            "claude-thinking-{}-{}",
            chat.stream_id,
            value["event"]["index"].as_u64().unwrap_or(0)
        ),
    ];
    if let Some(blocks) = value["message"]["content"].as_array() {
        for b in blocks.iter().take(64) {
            keys.push(s(&b["id"]).into());
            keys.push(s(&b["tool_use_id"]).into());
        }
    }
    for key in keys.into_iter().filter(|k| !k.is_empty()) {
        for kind in ["messages", "activities", "proposedPlans"] {
            if chat.thread[kind]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == key)
            {
                continue;
            }
            let cid = id.clone();
            let rid = key.clone();
            if let Some(row) = db
                .call(move |a| a.store.history_row(&cid, kind, &rid))
                .await?
                .map_err(|_| "Observer history row unavailable")?
            {
                chat.thread[kind].as_array_mut().unwrap().push(row);
            }
        }
    }
    // Native start can arrive before the visible echo. Restore local mapping
    // from the durable outbox before interpreting any consumption receipt.
    let mut display_input = None;
    if let Some(native) = crate::claude_turns::input_identity(&value)? {
        let cid = id.clone();
        let sid = chat.native_id.clone();
        let key = native.clone();
        let saved = db
            .call(move |a| a.store.claude_input(&cid, &sid, &key))
            .await?
            .map_err(|_| "Claude input mapping unavailable")?;
        let input = saved.map(|r| r.input).unwrap_or(Input {
            native: native.clone(),
            message: native.clone(),
            public_turn: format!("claude-{native}"),
            local: false,
            steer: false,
        });
        display_input = Some(input.clone());
        if !reduced.router.pending().any(|i| i.native == native) {
            reduced.router.register(input.clone())?;
        }
    }
    if let Some((native, text)) = crate::claude_turns::user_input(&value)? {
        let input = display_input;
        // Completed input echoes are deduplicated without resurrecting a turn.
        let input = input.unwrap_or(Input {
            native: native.clone(),
            message: native.clone(),
            public_turn: format!("claude-{native}"),
            local: false,
            steer: false,
        });
        if reduced.history_turn.as_deref() != Some(&input.public_turn) {
            reduced.history_started_at = Some(event_time.clone());
        }
        reduced.history_turn = Some(input.public_turn.clone());
        let cid = id.clone();
        let mid = input.message.clone();
        if !chat.thread["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == input.message)
        {
            if let Some(row) = db
                .call(move |a| a.store.history_row(&cid, "messages", &mid))
                .await?
                .map_err(|_| "Claude input history unavailable")?
            {
                chat.thread["messages"].as_array_mut().unwrap().push(row);
            }
        }
        projection::message(&mut chat, &input.message, &text, "user", false, false);
        let row = chat.thread["messages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == input.message)
            .unwrap();
        row["turnId"] = json!(input.public_turn);
        if row["sequence"].is_null() {
            if let Some(time) = event
                .created_at
                .as_deref()
                .or_else(|| value["timestamp"].as_str())
                .filter(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .is_ok()
                })
            {
                row["createdAt"] = json!(time);
                row["updatedAt"] = json!(time);
            }
        }
        chat.thread["latestUserMessageAt"] = json!(event_time);
    }
    let change = reduced.router.observe(&value)?;
    if catchup
        && event.source == "worker"
        && value["type"] == "result"
        && value["parent_tool_use_id"].is_null()
        && reduced.router.active().is_none()
        && !reduced.router.background().busy()
        && reduced.router.pending().count() == 1
    {
        reduced.historical_result = reduced.history_turn.clone();
    }
    if let Some(turn) = &change.turn {
        for native in &turn.inputs {
            if chat.thread["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == *native && r["kind"] == "steer.updated")
            {
                projection::activity(
                    &mut chat,
                    native,
                    "steer.updated",
                    "Steering message received",
                    "info",
                    json!({"status":"completed","detail":"Confirmed by Claude in the active task"}),
                );
            }
        }
    }
    if let Some(turn) = &change.turn {
        if chat.thread["latestTurn"]["turnId"] != turn.public_turn {
            chat.thread["latestTurn"] = json!({"turnId":turn.public_turn,"state":"running","requestedAt":reduced.history_started_at.as_deref().unwrap_or(&event_time),"startedAt":event_time,"completedAt":null,"assistantMessageId":null});
        }
        if change.started && chat.thread["latestTurn"]["startedAt"].is_null() {
            chat.thread["latestTurn"]["startedAt"] = json!(event_time);
        }
        chat.thread["session"]["status"] = json!("running");
        chat.thread["session"]["activeTurnId"] = json!(turn.public_turn);
        chat.thread["sharedTurnOwner"] = json!(if turn.local { "android" } else { "desktop" });
        for mid in &change.messages {
            if let Some(row) = chat.thread["messages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == *mid)
            {
                row["turnId"] = json!(turn.public_turn);
            }
        }
    } else if catchup && reduced.router.active().is_none() && value["type"] == "assistant" {
        // Historical peer output may predate consumption receipts. This is
        // display-only grouping; it grants no ownership/control of live work.
        if let Some(turn) = &reduced.history_turn {
            if chat.thread["latestTurn"]["turnId"] != *turn {
                let start = reduced.history_started_at.as_deref().unwrap_or(&event_time);
                chat.thread["latestTurn"] = json!({"turnId":turn,"state":"running","requestedAt":start,"startedAt":start,"completedAt":null,"assistantMessageId":null});
            }
        }
    }
    if event.source == "worker" && value["type"] == "system" && value["subtype"] == "init" {
        if let Some(model) = value["model"]
            .as_str()
            .filter(|m| !m.is_empty() && m.len() <= 256)
        {
            chat.thread["modelSelection"]["model"] = json!(model);
        }
        crate::native_control::observe_permission(&mut chat, s(&value["permissionMode"]));
    }
    if event.source == "worker"
        && value["type"] == "control_request"
        && value["request"]["subtype"] == "can_use_tool"
    {
        let native = value["request_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or("Claude permission request has no identity")?;
        if reduced.requests.len() >= 16 && !reduced.requests.contains_key(native) {
            return Err("Too many pending Claude requests; reconnect to reconcile them".into());
        }
        let rid = format!("claude-request-{}", uuid::Uuid::new_v4());
        let previous = reduced.requests.get(native);
        let rid = previous
            .and_then(|v| v["publicId"].as_str())
            .unwrap_or(&rid)
            .to_owned();
        let mut request = value.clone();
        request["publicId"] = json!(rid);
        reduced.requests.insert(native.into(), request);
        if crate::questions::is_input(&value) {
            projection::activity(
                &mut chat,
                &rid,
                "user-input.requested",
                "Your input is needed",
                "info",
                json!({"requestId":rid,"status":"pending","questions":crate::questions::questions(&value)?,"data":{"nativeRequestId":native,"toolCallId":value["request"]["tool_use_id"]}}),
            );
        } else {
            let detail = projection::approval_detail(&value, false);
            projection::activity(
                &mut chat,
                &rid,
                "approval.requested",
                "Approval required",
                "approval",
                json!({"requestId":rid,"status":"pending","requestKind":"command","providerName":"claude-code","canApproveForSession":false,"detail":detail,"command":value["request"]["input"]["command"],"availableDecisions":["approved","declined","cancel"],"data":{"nativeRequestId":native,"toolCallId":value["request"]["tool_use_id"]}}),
            );
        }
    }
    if value["type"] == "control_response" || value["type"] == "control_cancel_request" {
        let native = value["response"]["request_id"]
            .as_str()
            .or_else(|| value["request_id"].as_str())
            .unwrap_or("");
        if let Some(request) = reduced.requests.remove(native) {
            let rid = s(&request["publicId"]).to_owned();
            let cid = id.clone();
            let key = rid.clone();
            if !chat.thread["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == rid)
            {
                if let Some(row) = db
                    .call(move |a| a.store.history_row(&cid, "activities", &key))
                    .await?
                    .map_err(|_| "Claude request history unavailable")?
                {
                    chat.thread["activities"].as_array_mut().unwrap().push(row);
                }
            }
            if let Some(row) = chat.thread["activities"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == rid)
            {
                row["kind"] = json!(if row["kind"] == "user-input.requested" {
                    "user-input.resolved"
                } else {
                    "approval.resolved"
                });
                row["payload"]["status"] = json!("completed");
            }
        }
    }
    let existing_rows: HashMap<(String, String), Value> =
        ["messages", "activities", "proposedPlans"]
            .into_iter()
            .flat_map(|kind| {
                chat.thread[kind].as_array().unwrap().iter().map(move |r| {
                    (
                        (kind.to_owned(), s(&r["id"]).to_owned()),
                        r["createdAt"].clone(),
                    )
                })
            })
            .collect();
    projection::project(&mut chat, &value);
    // Replaying history must not make old output look newly created, or turn
    // a two-minute operation into a multi-hour "Working" interval.
    for kind in ["messages", "activities", "proposedPlans"] {
        for row in chat.thread[kind].as_array_mut().unwrap() {
            if !existing_rows.contains_key(&(kind.into(), s(&row["id"]).into())) {
                row["createdAt"] = json!(event_time);
                row["updatedAt"] = json!(event_time);
            }
        }
    }
    if event.source == "worker" && value["type"] == "user" && value["parent_tool_use_id"].is_null()
    {
        for block in value["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_result")
            .take(32)
        {
            let cid = id.clone();
            let content = block["content"].clone();
            let paths = db
                .call(move |a| a.store.capture_tool_images(&cid, &content))
                .await??;
            if let Some(row) = chat.thread["activities"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == block["tool_use_id"])
            {
                if !paths.is_empty() {
                    row["payload"]["data"]["toolImages"] = json!(paths);
                }
            }
        }
    }
    if change.completed
        || (catchup
            && value["type"] == "result"
            && reduced.router.active().is_none()
            && !reduced.router.background().busy()
            && chat.thread["latestTurn"]["state"] == "running"
            && reduced.history_turn.is_some())
    {
        let stopping = chat.external_stop.as_ref().is_some_and(|s| {
            change
                .turn
                .as_ref()
                .is_some_and(|t| t.public_turn == s.turn)
        });
        crate::chat::finish(
            &mut chat,
            if change.outcome.as_deref() == Some("error") || value["is_error"] == true {
                "error"
            } else if stopping
                || change.outcome.as_deref() == Some("interrupted")
                || ["aborted_tools", "aborted_streaming"].contains(&s(&value["terminal_reason"]))
            {
                "interrupted"
            } else {
                "completed"
            },
            None,
        );
        chat.thread["latestTurn"]["completedAt"] = json!(event_time);
        chat.thread["session"]["updatedAt"] = json!(event_time);
        crate::turn_timing::complete(&mut chat.thread);
        reduced.requests.clear();
    }
    settle_deliveries(&mut reduced);
    chat.thread["hasPendingApprovals"] = json!(reduced
        .requests
        .values()
        .any(|r| !crate::questions::is_input(r)));
    chat.thread["hasPendingUserInput"] =
        json!(reduced.requests.values().any(crate::questions::is_input));
    reduced.stream_id = chat.stream_id.clone();
    reduced.stream_model = chat.stream_model.clone();
    let reduced = serde_json::to_value(reduced).unwrap();
    if reduced.to_string().len() > 128 * 1024
        || ["messages", "activities", "proposedPlans"].iter().any(|k| {
            chat.thread[k]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.to_string().len() > crate::chat_store::ROW_BYTES)
        })
    {
        return Err(
            "Claude projection exceeds its bounded display budget; existing history is retained"
                .into(),
        );
    }
    Ok((serde_json::to_value(chat).unwrap(), reduced))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Auth, claude_host::Identity, store::Store};
    async fn setup() -> (tempfile::TempDir, AuthService, Cursor, String) {
        let dir = tempfile::tempdir().unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let identity = Identity {
            native_session: native.clone(),
            bridge_session: "cse_history_fixture".into(),
        };
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        store.save_chat(&json!({"agent":"claude","native_id":native,"resumed":true,"remote_identity":identity,"thread":{"id":"history-chat","modelSelection":{"instanceId":"claude-code","model":"test"},"messages":[],"activities":[],"proposedPlans":[],"session":{"status":"idle"},"latestTurn":null}}),None).unwrap();
        (
            dir,
            AuthService::new(Auth::new(store)).unwrap(),
            Cursor::new(identity).unwrap(),
            native,
        )
    }
    #[tokio::test]
    async fn internal_task_checkpoint_repairs_legacy_pending_without_changing_history() {
        let (_dir, db, mut cursor, _) = setup().await;
        let old = uuid::Uuid::new_v4().to_string();
        let human = uuid::Uuid::new_v4().to_string();
        // Simulate an old cursor that classified a native notice as human input.
        step(&db,&mut cursor,1,json!({"type":"user","uuid":old,"message":{"role":"user","content":"An already saved row"}})).await;
        step(&db,&mut cursor,2,json!({"type":"user","uuid":human,"message":{"role":"user","content":"Real pending prompt"}})).await;
        let before = db
            .call(|a| a.store.load_chat("history-chat"))
            .await
            .unwrap()
            .unwrap();
        let mut checkpoint = Envelope::owner(None);
        checkpoint.payload.as_mut().unwrap()["internal_inputs"] = json!([old]);
        apply_delivery(&db, &mut cursor, &checkpoint).await;
        let identity = cursor.identity.clone();
        cursor = db
            .call(move |a| a.store.claude_remote_cursor("history-chat", identity))
            .await
            .unwrap()
            .unwrap();
        let router = control_state(&cursor.projection).unwrap().0;
        assert_eq!(router.pending().count(), 1);
        assert_eq!(router.pending().next().unwrap().native, human);
        let next=step(&db,&mut cursor,3,json!({"type":"user","uuid":old,"origin":{"kind":"task-notification","producer":"session-task","runId":"run"},"message":{"role":"user","content":"<task-notification><task-id>t</task-id><status>completed</status></task-notification>"}})).await;
        assert_eq!(before["thread"]["messages"], next["thread"]["messages"]);
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            1
        );
        step(
            &db,
            &mut cursor,
            4,
            json!({"type":"assistant","user_message_uuid":human}),
        )
        .await;
        let done = step(
            &db,
            &mut cursor,
            5,
            json!({"type":"result","user_message_uuid":human}),
        )
        .await;
        assert_eq!(done["thread"]["latestTurn"]["state"], "completed");
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            0
        );
    }
    #[tokio::test]
    async fn shared_background_completion_survives_cursor_reload_without_ending_early() {
        let (_dir, db, mut cursor, _native) = setup().await;
        let input = uuid::Uuid::new_v4().to_string();
        step(&db,&mut cursor,1,json!({"type":"user","uuid":input,"message":{"role":"user","content":"Run a background tool"}})).await;
        step(&db,&mut cursor,2,json!({"type":"assistant","user_message_uuid":input,"message":{"id":"reply","content":[{"type":"text","text":"Starting"}]}})).await;
        step(
            &db,
            &mut cursor,
            3,
            json!({"type":"system","subtype":"task_started","task_id":"tool","run_id":"2"}),
        )
        .await;
        let held = step(
            &db,
            &mut cursor,
            4,
            json!({"type":"result","user_message_uuid":input,"is_error":false}),
        )
        .await;
        assert_eq!(held["thread"]["latestTurn"]["state"], "running");
        let identity = cursor.identity.clone();
        cursor = db
            .call(move |a| a.store.claude_remote_cursor("history-chat", identity))
            .await
            .unwrap()
            .unwrap();
        let old=step(&db,&mut cursor,5,json!({"type":"system","subtype":"task_notification","task_id":"tool","run_id":"1","status":"completed"})).await;
        assert_eq!(old["thread"]["latestTurn"]["state"], "running");
        let done=step(&db,&mut cursor,6,json!({"type":"system","subtype":"task_notification","task_id":"tool","run_id":"2","status":"completed"})).await;
        assert_eq!(done["thread"]["latestTurn"]["state"], "completed");
        assert!(control_state(&cursor.projection)
            .unwrap()
            .0
            .active()
            .is_none());
        assert_eq!(held["thread"]["messages"], done["thread"]["messages"]);
    }

    #[tokio::test]
    async fn shared_background_stop_requires_reserved_durable_worker_ack_after_reload() {
        let (_dir, db, mut cursor, _) = setup().await;
        let input = uuid::Uuid::new_v4().to_string();
        step(
            &db,
            &mut cursor,
            1,
            json!({"type":"user","uuid":input,"message":{"role":"user","content":"Background"}}),
        )
        .await;
        step(&db,&mut cursor,2,json!({"type":"assistant","user_message_uuid":input,"message":{"id":"answer","content":[{"type":"text","text":"Started"}]}})).await;
        step(
            &db,
            &mut cursor,
            3,
            json!({"type":"system","subtype":"task_started","task_id":"task","run_id":"001"}),
        )
        .await;
        step(
            &db,
            &mut cursor,
            4,
            json!({"type":"result","user_message_uuid":input}),
        )
        .await;
        let router = control_state(&cursor.projection).unwrap().0;
        let targets = router.background().stop_targets();
        let target = targets[0].clone();
        let turn = router.active().unwrap().public_turn.clone();
        db.call(move |a| {
            let mut chat=a.store.load_chat("history-chat").unwrap();
            chat["external_stop"]=json!({"turn":turn,"request":"interrupt","acknowledged":false,"result_seen":false,"background":targets});
            a.store.save_chat(&chat,None).unwrap();
        }).await.unwrap();
        let identity = cursor.identity.clone();
        cursor = db
            .call(move |a| a.store.claude_remote_cursor("history-chat", identity))
            .await
            .unwrap()
            .unwrap();
        let reply = json!({"type":"control_response","response":{"request_id":target.request,"subtype":"success","response":{}}});
        let event = Envelope {
            sequence: Some(5),
            event_id: Some("spoofed-stop".into()),
            source: "client".into(),
            payload: Some(reply.clone()),
            created_at: None,
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let unchanged = commit_event(&db, &mut cursor, event).await;
        assert_eq!(unchanged["thread"]["latestTurn"]["state"], "running");
        let event = Envelope {
            sequence: None,
            event_id: None,
            source: "worker".into(),
            payload: Some(reply.clone()),
            created_at: None,
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let (unchanged, _) = project(&db, unchanged, &cursor, &event, false)
            .await
            .unwrap();
        assert_eq!(unchanged["thread"]["latestTurn"]["state"], "running");
        let done = step(&db, &mut cursor, 6, reply).await;
        assert_eq!(done["thread"]["latestTurn"]["state"], "interrupted");
        assert!(!control_state(&cursor.projection)
            .unwrap()
            .0
            .background()
            .busy());
    }

    #[tokio::test]
    async fn shared_tool_control_accepts_only_durable_original_worker_acknowledgements() {
        let (_dir, db, mut cursor, _) = setup().await;
        db.call(|a| {
            let mut state = a.store.load_chat("history-chat").unwrap();
            state["native_control"] = json!({"key":"control","digest":"digest","request":"request","action":{"Toggle":{"name":"fixture","enabled":false}},"deadline":0,"result":null});
            a.store.save_tool_action(&state,"control","digest",&json!({"nativePending":true,"requestId":"request"})).unwrap();
        }).await.unwrap();
        let reply = json!({"type":"control_response","response":{"request_id":"request","subtype":"success","response":{}}});
        let event = Envelope {
            sequence: Some(1),
            event_id: Some("client-echo".into()),
            source: "client".into(),
            payload: Some(reply.clone()),
            created_at: None,
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let echoed = commit_event(&db, &mut cursor, event).await;
        assert!(echoed["native_control"]["result"].is_null());
        let event = Envelope {
            sequence: None,
            event_id: None,
            source: "worker".into(),
            payload: Some(reply.clone()),
            created_at: None,
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let (ephemeral, _) = project(&db, echoed, &cursor, &event, false).await.unwrap();
        assert!(ephemeral["native_control"]["result"].is_null());
        let confirmed = step(&db, &mut cursor, 2, reply).await;
        assert_eq!(confirmed["native_control"]["result"]["updated"], true);
        assert_eq!(confirmed["disabled_mcp"], json!(["fixture"]));
        assert!(confirmed["thread"]["messages"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            db.call(|a| a.store.tool_action("control"))
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .1,
            json!({"updated":true})
        );
    }
    #[tokio::test]
    async fn historical_human_result_without_uuid_requires_its_own_processed_receipt() {
        let (_dir, db, mut cursor, _) = setup().await;
        let input = uuid::Uuid::new_v4().to_string();
        step(&db,&mut cursor,1,json!({"type":"user","uuid":input,"origin":{"kind":"human"},"client_platform":"unknown","message":{"role":"user","content":"The remote prompt"}})).await;
        step(&db,&mut cursor,2,json!({"type":"assistant","message":{"id":"remote-reply","content":[{"type":"text","text":"The reply"}]}})).await;
        let saved = step(
            &db,
            &mut cursor,
            3,
            json!({"type":"result","is_error":false,"num_turns":1}),
        )
        .await;
        assert_eq!(saved["thread"]["latestTurn"]["state"], "completed");
        assert_eq!(
            saved["thread"]["latestTurn"]["turnId"],
            format!("claude-{input}")
        );
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            1
        );
        apply_delivery(
            &db,
            &mut cursor,
            &Envelope::receipts(vec![crate::claude_remote::DeliveryReceipt {
                input: uuid::Uuid::new_v4().to_string(),
                processed: true,
            }]),
        )
        .await;
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            1
        );
        apply_delivery(
            &db,
            &mut cursor,
            &Envelope::receipts(vec![crate::claude_remote::DeliveryReceipt {
                input: input.clone(),
                processed: true,
            }]),
        )
        .await;
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            0
        );
        let newer = uuid::Uuid::new_v4().to_string();
        step(
            &db,
            &mut cursor,
            4,
            json!({"type":"user","uuid":newer,"message":{"role":"user","content":"A newer task"}}),
        )
        .await;
        apply_delivery(
            &db,
            &mut cursor,
            &Envelope::receipts(vec![crate::claude_remote::DeliveryReceipt {
                input,
                processed: true,
            }]),
        )
        .await;
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn steer_receipts_before_or_after_result_settle_once_without_finishing_another_turn() {
        for late in [false, true] {
            let (_dir, db, mut cursor, native) = setup().await;
            let base = uuid::Uuid::new_v4().to_string();
            step(&db,&mut cursor,1,json!({"type":"user","uuid":base,"message":{"role":"user","content":"Run the test"}})).await;
            step(&db,&mut cursor,2,json!({"type":"assistant","user_message_uuid":base,"message":{"id":"working","content":[{"type":"text","text":"Working"}]}})).await;
            let input = Input {
                native: uuid::Uuid::new_v4().to_string(),
                message: "phone-steer".into(),
                public_turn: format!("claude-{base}"),
                local: true,
                steer: true,
            };
            let payload = json!({"type":"user","uuid":input.native,"session_id":native,"message":{"role":"user","content":"Change the final marker"}});
            let (record, frame, session) = (input.clone(), payload.clone(), native.clone());
            db.call(move |a| {
                a.store
                    .offer_claude_input("history-chat", &session, &record, &frame)
            })
            .await
            .unwrap()
            .unwrap();
            let mut old = db
                .call(|a| a.store.load_chat("history-chat"))
                .await
                .unwrap()
                .unwrap();
            old["thread"]["activities"].as_array_mut().unwrap().push(json!({"id":input.native,"turnId":input.public_turn,"kind":"steer.updated","summary":"Steering message sent","payload":{"status":"pending"}}));
            db.call(move |a| a.store.save_chat(&old, None))
                .await
                .unwrap()
                .unwrap();
            step(&db, &mut cursor, 3, payload).await;
            let event = Envelope::receipts(vec![crate::claude_remote::DeliveryReceipt {
                input: input.native.clone(),
                processed: true,
            }]);
            if late {
                step(
                    &db,
                    &mut cursor,
                    4,
                    json!({"type":"result","user_message_uuid":base,"is_error":false}),
                )
                .await;
            }
            apply_delivery(&db, &mut cursor, &event).await;
            if !late {
                assert!(control_state(&cursor.projection)
                    .unwrap()
                    .0
                    .active()
                    .is_some());
                step(
                    &db,
                    &mut cursor,
                    4,
                    json!({"type":"result","user_message_uuid":base,"is_error":false}),
                )
                .await;
            }
            let router = control_state(&cursor.projection).unwrap().0;
            assert!(router.active().is_none());
            assert_eq!(router.pending().count(), 0);
            let sid = native.clone();
            let iid = input.native.clone();
            assert_eq!(
                db.call(move |a| a.store.claude_input("history-chat", &sid, &iid))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .state,
                "completed"
            );
            let old = db
                .call(|a| a.store.load_chat("history-chat"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                old["thread"]["activities"][0]["summary"],
                "Steering message received"
            );
            assert_eq!(
                old["thread"]["activities"][0]["payload"]["status"],
                "completed"
            );
            let next = uuid::Uuid::new_v4().to_string();
            step(
                &db,
                &mut cursor,
                5,
                json!({"type":"user","uuid":next,"message":{"role":"user","content":"Next task"}}),
            )
            .await;
            step(&db,&mut cursor,6,json!({"type":"assistant","user_message_uuid":next,"message":{"id":"next-output","content":[{"type":"text","text":"Next working"}]}})).await;
            apply_delivery(&db, &mut cursor, &event).await;
            let router = control_state(&cursor.projection).unwrap().0;
            assert_eq!(
                router.active().unwrap().public_turn,
                format!("claude-{next}")
            );
            assert_eq!(router.pending().count(), 1);
        }
    }
    async fn apply_delivery(db: &AuthService, cursor: &mut Cursor, event: &Envelope) {
        let old = db
            .call(|a| a.store.load_chat("history-chat"))
            .await
            .unwrap()
            .unwrap();
        let (state, projection) = project(db, old, cursor, event, false).await.unwrap();
        let mut after = cursor.clone();
        after.projection = projection;
        let before = cursor.clone();
        let saved = after.clone();
        let receipt = crate::claude_host::Receipt {
            consumed: event.deliveries.iter().map(|r| r.input.clone()).collect(),
            completed: event
                .deliveries
                .iter()
                .filter(|r| r.processed)
                .map(|r| r.input.clone())
                .collect(),
            ..Default::default()
        };
        db.call(move |a| {
            a.store
                .save_claude_remote_receipt(&state, &before, &saved, Some(&receipt))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(after.sequence, cursor.sequence);
        *cursor = after;
    }
    #[tokio::test]
    async fn untagged_stop_result_needs_exact_ack_and_processing_proof_in_either_order() {
        for late in [false, true] {
            let (_dir, db, mut cursor, native) = setup().await;
            let input = Input {
                native: uuid::Uuid::new_v4().to_string(),
                message: "stop-input".into(),
                public_turn: "stopped-turn".into(),
                local: true,
                steer: false,
            };
            let payload = json!({"type":"user","uuid":input.native,"session_id":native,"message":{"role":"user","content":"Compute until stopped"}});
            let (record, frame, sid) = (input.clone(), payload.clone(), native.clone());
            db.call(move |a| {
                a.store
                    .offer_claude_input("history-chat", &sid, &record, &frame)
            })
            .await
            .unwrap()
            .unwrap();
            step(&db, &mut cursor, 1, payload).await;
            step(&db,&mut cursor,2,json!({"type":"assistant","user_message_uuid":input.native,"message":{"id":"tool-reply","content":[{"type":"text","text":"Working"}]}})).await;
            let mut old = db
                .call(|a| a.store.load_chat("history-chat"))
                .await
                .unwrap()
                .unwrap();
            old["external_stop"] = json!({"turn":"stopped-turn","request":"unique-stop-request","acknowledged":false,"result_seen":false});
            db.call(move |a| a.store.save_chat(&old, None))
                .await
                .unwrap()
                .unwrap();
            let event = Envelope::receipts(vec![crate::claude_remote::DeliveryReceipt {
                input: input.native.clone(),
                processed: true,
            }]);
            if !late {
                apply_delivery(&db, &mut cursor, &event).await;
            }
            step(&db,&mut cursor,3,json!({"type":"control_response","response":{"request_id":"old-stop-request","subtype":"success"}})).await;
            let before = step(
                &db,
                &mut cursor,
                4,
                json!({"type":"result","is_error":false,"num_turns":0}),
            )
            .await;
            assert_eq!(before["thread"]["latestTurn"]["state"], "running");
            step(&db,&mut cursor,5,json!({"type":"control_response","response":{"request_id":"unique-stop-request","subtype":"success"}})).await;
            if late {
                // The worker's marker is not a new task or a human prompt.
                step(&db,&mut cursor,6,json!({"type":"user","uuid":uuid::Uuid::new_v4().to_string(),"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}})).await;
                assert_eq!(
                    control_state(&cursor.projection)
                        .unwrap()
                        .0
                        .pending()
                        .count(),
                    1
                );
                apply_delivery(&db, &mut cursor, &event).await;
            }
            let saved = db
                .call(|a| a.store.load_chat("history-chat"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved["thread"]["latestTurn"]["state"], "interrupted");
            assert_eq!(saved["thread"]["messages"].as_array().unwrap().len(), 2);
            assert!(control_state(&cursor.projection)
                .unwrap()
                .0
                .active()
                .is_none());
            assert_eq!(
                control_state(&cursor.projection)
                    .unwrap()
                    .0
                    .pending()
                    .count(),
                0
            );
        }
    }
    async fn step(db: &AuthService, cursor: &mut Cursor, seq: u64, payload: Value) -> Value {
        step_at(db, cursor, seq, payload, "2026-10-22T12:00:00Z").await
    }
    async fn step_at(
        db: &AuthService,
        cursor: &mut Cursor,
        seq: u64,
        payload: Value,
        at: &str,
    ) -> Value {
        let event = Envelope {
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
            sequence: Some(seq),
            event_id: Some(format!("event-{seq}")),
            source: "worker".into(),
            created_at: Some(at.into()),
            payload: Some(payload),
        };
        commit_event(db, cursor, event).await
    }
    async fn commit_event(db: &AuthService, cursor: &mut Cursor, event: Envelope) -> Value {
        let old = db
            .call(|a| a.store.load_chat("history-chat"))
            .await
            .unwrap()
            .unwrap();
        let (next, reduced) = project(db, old, cursor, &event, true).await.unwrap();
        let after = cursor.advance(&event, reduced).unwrap();
        let a = after.clone();
        let b = cursor.clone();
        let value = next.clone();
        db.call(move |auth| auth.store.save_claude_remote(&value, &b, &a))
            .await
            .unwrap()
            .unwrap();
        *cursor = after;
        next
    }
    #[tokio::test]
    async fn remote_stop_needs_worker_ack_result_and_exact_native_processing_receipts() {
        let (_dir, db, mut cursor, _native) = setup().await;
        let base = uuid::Uuid::new_v4().to_string();
        commit_event(&db,&mut cursor,Envelope {sequence:Some(1),event_id:Some(base.clone()),created_at:None,source:"client".into(),payload:Some(json!({"type":"user","uuid":base,"message":{"role":"user","content":"Desktop task"}})),deliveries:vec![crate::claude_remote::DeliveryReceipt {input:base.clone(),processed:true}],receipt_checkpoint:false,recovery:None}).await;
        step(&db,&mut cursor,2,json!({"type":"assistant","user_message_uuid":base,"message":{"id":"work","content":[{"type":"text","text":"Working"}]}})).await;
        commit_event(&db,&mut cursor,Envelope {sequence:Some(3),event_id:Some("remote-stop-event".into()),created_at:None,source:"client".into(),payload:Some(json!({"type":"control_request","request_id":"desktop-stop","request":{"subtype":"interrupt"}})),deliveries:vec![],receipt_checkpoint:false,recovery:None}).await;
        assert!(control_state(&cursor.projection)
            .unwrap()
            .0
            .active()
            .is_some());
        step(&db,&mut cursor,4,json!({"type":"control_response","response":{"request_id":"desktop-stop","subtype":"success"}})).await;
        step(&db,&mut cursor,5,json!({"type":"user","uuid":uuid::Uuid::new_v4().to_string(),"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}})).await;
        let done = step(
            &db,
            &mut cursor,
            6,
            json!({"type":"result","is_error":false,"num_turns":0}),
        )
        .await;
        assert_eq!(done["thread"]["latestTurn"]["state"], "interrupted");
        assert_eq!(done["thread"]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            control_state(&cursor.projection)
                .unwrap()
                .0
                .pending()
                .count(),
            0
        );
        assert!(done["paused"].as_bool().unwrap());
    }
    #[tokio::test]
    async fn catchup_preserves_prompts_and_tool_details_without_private_context_or_raw_output() {
        let (_dir, db, mut cursor, native) = setup().await;
        let input = uuid::Uuid::new_v4().to_string();
        step(&db,&mut cursor,1,json!({"type":"user","session_id":native,"uuid":input,"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>PRIVATE_SETUP</system-reminder>"},{"type":"text","text":"Hii"}]}})).await;
        step(&db,&mut cursor,2,json!({"type":"assistant","session_id":native,"user_message_uuid":input,"message":{"id":"assistant-reply","model":"unknown-private-model","content":[{"type":"thinking","thinking":"PRIVATE_THOUGHTS"},{"type":"text","text":"Hello"},{"type":"tool_use","id":"read-one","name":"Read","input":{"file_path":"README.md"}}]}})).await;
        step(&db,&mut cursor,3,json!({"type":"user","session_id":native,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-one","content":"PRIVATE_RAW_OUTPUT"}]}})).await;
        let completed = step(
            &db,
            &mut cursor,
            4,
            json!({"type":"result","session_id":native,"user_message_uuid":input,"is_error":false}),
        )
        .await;
        assert_eq!(completed["thread"]["latestTurn"]["state"], "completed");
        let saved = db
            .call(|a| a.store.load_chat("history-chat"))
            .await
            .unwrap()
            .unwrap();
        let text = saved.to_string();
        for private in ["PRIVATE_SETUP", "PRIVATE_THOUGHTS", "PRIVATE_RAW_OUTPUT"] {
            assert!(!text.contains(private));
        }
        assert_eq!(saved["thread"]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(saved["thread"]["messages"][0]["text"], "Hii");
        assert!(text.contains("README.md"));
        assert_eq!(
            saved["thread"]["activities"][0]["payload"]["status"],
            "completed"
        );
        // A resumed observer uses exactly the saved sequence and reducer, not
        // a replay of user input or a newly invented native session.
        let identity = cursor.identity.clone();
        let reloaded = db
            .call(move |a| a.store.claude_remote_cursor("history-chat", identity))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor, reloaded);
    }
    #[tokio::test]
    async fn historical_turn_keeps_native_times_across_reopen() {
        let (_dir, db, mut cursor, native) = setup().await;
        let input = uuid::Uuid::new_v4().to_string();
        step_at(&db,&mut cursor,1,json!({"type":"user","session_id":native,"uuid":input,"message":{"role":"user","content":"Measure frames"}}),"2026-10-21T01:00:00Z").await;
        step_at(&db,&mut cursor,2,json!({"type":"assistant","session_id":native,"user_message_uuid":input,"message":{"id":"old-reply","content":[{"type":"text","text":"Measured"}]}}),"2026-10-21T01:00:02Z").await;
        let done = step_at(
            &db,
            &mut cursor,
            3,
            json!({"type":"result","session_id":native,"user_message_uuid":input,"is_error":false}),
            "2026-10-21T01:02:00Z",
        )
        .await;
        assert_eq!(
            done["thread"]["latestTurn"]["requestedAt"],
            "2026-10-21T01:00:00Z"
        );
        assert_eq!(
            done["thread"]["latestTurn"]["startedAt"],
            "2026-10-21T01:00:02Z"
        );
        assert_eq!(
            done["thread"]["latestTurn"]["completedAt"],
            "2026-10-21T01:02:00Z"
        );
        assert_eq!(
            done["thread"]["messages"][1]["createdAt"],
            "2026-10-21T01:00:02Z"
        );
        let reconnected = step_at(
            &db,
            &mut cursor,
            4,
            json!({"type":"system","subtype":"init","session_id":native,"model":"test"}),
            "2026-10-22T03:00:00Z",
        )
        .await;
        assert_eq!(
            done["thread"]["latestTurn"],
            reconnected["thread"]["latestTurn"]
        );
    }
    #[tokio::test]
    async fn a_local_prompt_echo_keeps_its_android_message_id() {
        let (_dir, db, mut cursor, native) = setup().await;
        let input = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: "android-message".into(),
            public_turn: "android-turn".into(),
            local: true,
            steer: false,
        };
        let payload = json!({"type":"user","session_id":native,"uuid":input.native,"message":{"role":"user","content":"Original phone prompt"}});
        let record = input.clone();
        let frame = payload.clone();
        let session = native.clone();
        db.call(move |a| {
            a.store
                .offer_claude_input("history-chat", &session, &record, &frame)
        })
        .await
        .unwrap()
        .unwrap();
        step(&db, &mut cursor, 1, payload.clone()).await;
        step(&db, &mut cursor, 2, payload).await;
        let saved = db
            .call(|a| a.store.load_chat("history-chat"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved["thread"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(saved["thread"]["messages"][0]["id"], "android-message");
        assert_eq!(saved["thread"]["messages"][0]["turnId"], "android-turn");
    }
}
