//! Local history/resume and existing-process delivery, independent of RC.
use super::*;
use crate::{
    claude_local::{self, Owner},
    claude_local_history::{Batch, Cursor, Entry},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Local {
    /// Exact native control rows to remove from the derived display cache in
    /// the same transaction as the cursor. Never changes Claude's JSONL file.
    #[serde(skip)]
    pub removed_messages: Vec<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub cursor: Cursor,
    #[serde(default)]
    pub offers: Vec<Offer>,
    #[serde(skip)]
    pub owner: Option<Owner>,
    #[serde(default)]
    pub caught_up: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Offer {
    pub uuid: String,
    pub message: String,
    pub turn: String,
    pub text: String,
    pub at: String,
    pub confirmed: bool,
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub held: bool,
    #[serde(default)]
    pub receipt: Option<claude_local::Receipt>,
    #[serde(skip)]
    pub receipt_started: bool,
}

impl Hub {
    pub(super) async fn local_claude_error(&mut self, id: &str, error: &str) {
        if self.active.contains_key(id) {
            return;
        }
        let Some(chat) = self.chats.get_mut(id) else {
            return;
        };
        if let Some(local) = chat.local_claude.as_mut() {
            local.caught_up = false;
        }
        chat.thread["session"]["statusConfidence"] = json!("unknown");
        chat.thread["session"]["lastError"] = json!(projection::bounded(error, 1024));
        chat.thread["session"]["connectionState"] = json!("disconnected");
        chat.thread["session"]["connectionDetail"] = json!(projection::bounded(error, 1024));
        projection::activity(
            chat,
            "claude-native-sync",
            "claude.desktop-sync.updated",
            "Local Claude session",
            "error",
            json!({"status":"disconnected","local":true,"existingSession":true,"readOnly":true,"detail":projection::bounded(error,1024)}),
        );
        let _ = self.save(id, None).await;
        self.dirty = true;
    }
    pub(super) async fn register_local_claude(
        &mut self,
        entries: Vec<Entry>,
        owners: Vec<Owner>,
    ) -> Result<(), String> {
        for entry in entries {
            let existing = self
                .chats
                .iter()
                .find(|(_, c)| c.agent == AgentKind::Claude && c.native_id == entry.native)
                .map(|(id, _)| id.clone());
            let id = existing
                .clone()
                .unwrap_or_else(|| format!("claude-history-{}", entry.native));
            if self.active.contains_key(&id)
                || existing.is_some()
                    && (!self.chats[&id].thread["archivedAt"].is_null()
                        || !self.chats[&id].thread["deletedAt"].is_null())
            {
                continue;
            }
            // A saved Remote Control attachment already has its own delivery
            // protocol and recovery intent. Discovery must not silently replace
            // that transport or discard its queued input.
            if existing.is_some()
                && self.chats[&id].local_claude.is_none()
                && (!self.chats[&id].external_claude || self.chats[&id].remote_identity.is_some())
            {
                // An expired sharing link must not hide readable local outcomes.
                // Retain the original transport, cursor, queue and turn ownership;
                // this is display evidence only, not permission to resume a writer.
                if self.chats[&id].external_claude {
                    let saved = if owners
                        .iter()
                        .any(|o| o.session_id == entry.native && o.busy())
                    {
                        Value::Null
                    } else {
                        entry.saved_turn.clone().unwrap_or(Value::Null)
                    };
                    let chat = self.chats.get_mut(&id).unwrap();
                    if chat.thread["session"].get("claudeHistoryTurn") != Some(&saved)
                        || chat.thread["session"]["statusConfidence"] != "unknown"
                    {
                        chat.thread["session"]["claudeHistoryTurn"] = saved;
                        chat.thread["session"]["statusConfidence"] = json!("unknown");
                        chat.thread["session"]["updatedAt"] = json!(projection::now());
                        self.save(&id, None).await?;
                        self.dirty = true;
                    }
                }
                continue;
            }
            if existing.is_none() {
                if self.chats.len() >= 10000 {
                    break;
                }
                let timestamp = entry.created.clone();
                let model = self.config.claude()?.model.clone();
                let project = self
                    .projects
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|p| entry.cwd.starts_with(s(&p["workspaceRoot"])))
                    .max_by_key(|p| s(&p["workspaceRoot"]).len());
                let thread = json!({"id":id,"projectId":project.map(|p|p["id"].clone()).unwrap_or(json!("codex-project-chats")),"workspaceKind":if project.is_some(){Value::Null}else{json!("projectless")},
                    "title":entry.title,"modelSelection":{"instanceId":"claude-code","model":model,"options":[]},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":entry.cwd,
                    "latestTurn":null,"createdAt":timestamp,"updatedAt":timestamp,"archivedAt":null,"deletedAt":null,"latestUserMessageAt":null,"hasPendingApprovals":false,"hasPendingUserInput":false,"hasActionableProposedPlan":false,
                    "session":{"threadId":id,"providerName":"claude","providerInstanceId":"claude-code","status":"idle","statusConfidence":"unknown","runtimeMode":"approval-required","activeTurnId":null,"lastError":null,"updatedAt":timestamp},
                    "messages":[],"activities":[],"checkpoints":[],"proposedPlans":[]});
                let chat: Chat = serde_json::from_value(json!({"agent":"claude","thread":thread,"native_id":entry.native,"resumed":true})).map_err(|_|"Cannot import local Claude session")?;
                self.chats.insert(id.clone(), chat);
            }
            let chat = self.chats.get_mut(&id).unwrap();
            let previous = serde_json::to_value(&*chat).map_err(|_| "Cannot retain local chat")?;
            if !chat.thread["deletedAt"].is_null() || !chat.thread["archivedAt"].is_null() {
                continue;
            }
            let matches: Vec<_> = owners
                .iter()
                .filter(|o| o.session_id == chat.native_id)
                .collect();
            if matches.len() > 1 {
                self.local_claude_error(&id,"More than one Claude process owns this session; close the extra process before sending").await;
                continue;
            }
            let local = chat.local_claude.get_or_insert(Local {
                removed_messages: vec![],
                path: entry.path.clone(),
                cursor: Cursor::default(),
                offers: vec![],
                owner: None,
                caught_up: false,
            });
            local.path = entry.path;
            local.owner = matches.first().map(|o| (*o).clone());
            chat.external_claude = false;
            chat.remote_identity = None;
            chat.resumed = true;
            self.claude_recovery.forget(&id);
            local_status(chat);
            if existing.is_none()
                || previous
                    != serde_json::to_value(&*chat).map_err(|_| "Cannot retain local chat")?
            {
                self.save(&id, None).await?;
                self.dirty = true;
            }
        }
        Ok(())
    }

    pub(super) async fn apply_local_claude(
        &mut self,
        id: &str,
        before: Cursor,
        batch: Batch,
        owner: Option<Owner>,
    ) -> Result<(), String> {
        if self.active.contains_key(id) {
            return Ok(());
        }
        let Some(chat) = self.chats.get(id) else {
            return Ok(());
        };
        let Some(local) = &chat.local_claude else {
            return Ok(());
        };
        if local.cursor.offset != before.offset || local.cursor.identity != before.identity {
            return Ok(());
        }
        let previous = chat.clone();
        // Restore tool starts that have already paged out before their result.
        let tool_ids: Vec<String> = batch
            .rows
            .iter()
            .flat_map(|r| r["message"]["content"].as_array().into_iter().flatten())
            .filter_map(|b| b["tool_use_id"].as_str().map(str::to_owned))
            .collect();
        let cid = id.to_owned();
        let saved = self
            .db
            .call(move |a| {
                tool_ids
                    .iter()
                    .map(|tid| a.store.history_row(&cid, "activities", tid))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?
            .map_err(|_| "Cannot read saved Claude tools")?;
        let keys: Vec<_> = batch
            .rows
            .iter()
            .map(|r| (s(&r["uuid"]).to_owned(), s(&r["message"]["id"]).to_owned()))
            .collect();
        let cid = id.to_owned();
        let sid = chat.native_id.clone();
        let mappings = self
            .db
            .call(move |a| {
                keys.into_iter()
                    .map(|(uuid, aid)| {
                        let input = a.store.claude_input(&cid, &sid, &uuid)?;
                        let mid = input
                            .as_ref()
                            .map(|r| r.input.message.as_str())
                            .unwrap_or(&uuid);
                        let existing = a.store.history_row(&cid, "messages", mid)?;
                        let assistant = if existing.is_some() {
                            existing.clone()
                        } else if !aid.is_empty() {
                            a.store.history_row(&cid, "messages", &aid)?
                        } else {
                            None
                        };
                        Ok((uuid, (input, existing, assistant)))
                    })
                    .collect::<rusqlite::Result<HashMap<_, _>>>()
            })
            .await?
            .map_err(|_| "Cannot reconcile local Claude message identities")?;
        let images: Vec<_> = batch
            .rows
            .iter()
            .flat_map(|r| r["message"]["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .map(|b| (s(&b["tool_use_id"]).to_owned(), b["content"].clone()))
            .collect();
        let cid = id.to_owned();
        let captured = self
            .db
            .call(move |a| {
                images
                    .into_iter()
                    .map(|(tid, content)| {
                        a.store
                            .capture_claude_tool_images(&cid, &content)
                            .map(|paths| (tid, paths))
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .await?
            .map_err(|_| "Cannot retain images from local Claude history")?;
        let chat = self.chats.get_mut(id).unwrap();
        if before.projection_version != batch.next.projection_version
            || !before.identity.is_empty() && before.identity != batch.next.identity
            || before.offset > batch.next.offset
        {
            let local = chat.local_claude.as_mut().unwrap();
            local.cursor.turn.clear();
            local.cursor.complete = false;
        }
        for row in saved.into_iter().flatten() {
            if !chat.thread["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == row["id"])
            {
                chat.thread["activities"].as_array_mut().unwrap().push(row);
            }
        }
        for row in batch.rows {
            let mapping = mappings.get(s(&row["uuid"]));
            project_row(chat, row, mapping);
        }
        for (tid, media) in captured {
            media.project_warning(chat, &tid);
            if !media.paths.is_empty() {
                if let Some(row) = chat.thread["activities"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|r| r["id"] == tid)
                {
                    row["payload"]["data"]["toolImages"] = json!(media.paths);
                }
            }
        }
        update_pending_confirmations(chat, batch.caught_up, owner.is_some());
        let local = chat.local_claude.as_mut().unwrap();
        local.cursor.offset = batch.next.offset;
        local.cursor.identity = batch.next.identity;
        local.cursor.unreadable = batch.next.unreadable;
        local.cursor.projection_version = batch.next.projection_version;
        local.cursor.prompt_id = batch.next.prompt_id;
        local.caught_up = batch.caught_up;
        local.owner = owner;
        if local.cursor.unreadable > 0 {
            let unreadable = local.cursor.unreadable;
            projection::activity(
                chat,
                "claude-local-history-warning",
                "session.notice",
                "Some saved history records could not be read",
                "info",
                json!({"detail":format!("{unreadable} unreadable records in the original Claude history. Existing saved messages are retained and newer history continues loading.")}),
            );
        }
        local_status(chat);
        if serde_json::to_value(&*chat).map_err(|_| "Cannot retain local history")?
            != serde_json::to_value(&previous).map_err(|_| "Cannot retain local history")?
        {
            if let Err(e) = self.save(id, None).await {
                self.chats.insert(id.into(), previous);
                return Err(e);
            }
            self.dirty = true;
        }
        Ok(())
    }

    pub(super) async fn local_claude_offer(
        &mut self,
        id: &str,
        c: Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        let profile = self.config.claude()?.profile.clone();
        let native = self.chats[id].native_id.clone();
        let p = profile.clone();
        let owner = tokio::task::spawn_blocking(move || claude_local::owner(&p, &native))
            .await
            .map_err(|_| "Claude owner check stopped")??
            .ok_or("The original Claude process just exited; retry to resume this session")?;
        if c["message"]["attachments"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            return Err("Claude's live local inbox accepts text. Send file paths, or attach files after this session closes.".into());
        }
        let chat = &self.chats[id];
        if c["deliveryMode"] != "steer" && owner.busy() {
            return Err("Claude started another task; this prompt remains unsent. Queue it or choose Steer.".into());
        }
        if c["modelSelection"].is_object() && c["modelSelection"] != chat.thread["modelSelection"]
            || c["runtimeMode"]
                .as_str()
                .is_some_and(|m| chat.thread["runtimeMode"] != m)
            || c["interactionMode"]
                .as_str()
                .is_some_and(|m| chat.thread["interactionMode"] != m)
        {
            return Err("Change model or permissions in the original Claude session while it is open. No prompt was sent.".into());
        }
        if !chat.local_claude.as_ref().is_some_and(|l| l.caught_up) {
            return Err("This local chat's history is still loading; no prompt was sent".into());
        }
        if chat
            .local_claude
            .as_ref()
            .unwrap()
            .offers
            .iter()
            .filter(|o| !o.confirmed && !o.settled)
            .count()
            >= 8
        {
            return Err("Wait for Claude to confirm pending messages before sending more".into());
        }
        let old = chat.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let chat = self.chats.get_mut(id).unwrap();
        let steer = c["deliveryMode"] == "steer";
        let turn = if steer {
            s(&chat.thread["latestTurn"]["turnId"]).to_owned()
        } else {
            format!("history-{uuid}")
        };
        let offer = Offer {
            uuid: uuid.clone(),
            message: s(&c["message"]["messageId"]).into(),
            text: s(&c["message"]["text"]).into(),
            turn,
            at: now(),
            confirmed: false,
            settled: false,
            held: false,
            receipt: claude_local::Receipt::prepare(&profile, &owner),
            receipt_started: true,
        };
        let offers = &mut chat.local_claude.as_mut().unwrap().offers;
        if offers.len() >= 128 {
            if let Some(index) = offers.iter().position(|o| o.confirmed || o.settled) {
                offers.remove(index);
            }
        }
        chat.local_claude
            .as_mut()
            .unwrap()
            .offers
            .push(offer.clone());
        if !steer {
            chat.thread["latestTurn"] = json!({"turnId":offer.turn,"state":"running","requestedAt":offer.at,"startedAt":null,"completedAt":null,"assistantMessageId":null});
        }
        projection::message(chat, &offer.message, &offer.text, "user", false, false);
        if steer {
            if let Some(m) = chat.thread["messages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|m| m["id"] == offer.message)
            {
                m["phase"] = json!("steer");
            }
        }
        projection::activity(
            chat,
            &uuid,
            "steer.updated",
            "Sending to Claude",
            "info",
            json!({"status":"pending","detail":"Sent through Claude’s local peer inbox. The original session’s inbound rules and existing instructions apply; waiting for it to record delivery."}),
        );
        // Commit command identity before any network write: retries never offer
        // a second copy, even when the first acknowledgement is lost.
        if let Err(e) = self.save(id, receipt).await {
            self.chats.insert(id.into(), old);
            return Err(e);
        }
        let cid = id.to_owned();
        let events = self.local_receipt_events.0.clone();
        let key = uuid.clone();
        let handle = self.local_deliveries.spawn(async move {
            let (updates, mut receiver) = tokio::sync::mpsc::channel(8);
            let delivery = claude_local::offer_watched(profile, owner, &uuid, &offer.text, steer, Some(updates), offer.receipt);
            tokio::pin!(delivery);
            let result = loop {
                tokio::select! {
                    result = &mut delivery => break result,
                    Some(status) = receiver.recv() => {
                        if events.send((cid.clone(),uuid.clone(),status)).await.is_err() {break Ok(claude_local::Delivery::Offered);}
                    }
                }
            };
            (cid, uuid.clone(), result)
        });
        self.local_receipts.insert(key, handle);
        local_status(self.chats.get_mut(id).unwrap());
        self.dirty = true;
        Ok(())
    }
    /// Restore listeners after persisted history has reconciled confirmations.
    /// A restart never replays an offer whose delivery may already have happened.
    pub(super) fn recover_local_receipts(&mut self) {
        for (id, chat) in &mut self.chats {
            let Some(local) = chat.local_claude.as_mut().filter(|l| l.caught_up) else {
                continue;
            };
            for offer in &mut local.offers {
                if offer.confirmed || offer.settled || offer.receipt_started {
                    continue;
                }
                offer.receipt_started = true;
                let Some(receipt) = offer.receipt.clone() else {
                    continue;
                };
                let (cid, uuid) = (id.clone(), offer.uuid.clone());
                let key = uuid.clone();
                let events = self.local_receipt_events.0.clone();
                let handle = self.local_deliveries.spawn(async move {
                    let (updates, mut receiver) = tokio::sync::mpsc::channel(8);
                    let delivery = claude_local::recover_receipt(receipt, &uuid, updates);
                    tokio::pin!(delivery);
                    let result = loop {
                        tokio::select! {
                            result = &mut delivery => break result,
                            Some(status) = receiver.recv() => {
                                if events.send((cid.clone(), uuid.clone(), status)).await.is_err() { break Ok(claude_local::Delivery::Offered); }
                            }
                        }
                    };
                    (cid, uuid.clone(), result)
                });
                self.local_receipts.insert(key, handle);
            }
        }
    }

    pub(super) async fn local_claude_delivery(
        &mut self,
        id: &str,
        uuid: &str,
        result: Result<claude_local::Delivery, String>,
    ) {
        let Some(chat) = self.chats.get_mut(id) else {
            return;
        };
        let Some(offer) = chat
            .local_claude
            .as_mut()
            .and_then(|l| l.offers.iter_mut().find(|o| o.uuid == uuid))
        else {
            return;
        };
        if offer.confirmed || offer.settled {
            return;
        }
        match result {
            Ok(claude_local::Delivery::Offered) => {}
            Ok(claude_local::Delivery::Held) => {
                // Held is pending: keep listening for accept/refusal/expiry.
                offer.held = true;
                projection::activity(
                    chat,
                    uuid,
                    "steer.updated",
                    "Claude is holding this message",
                    "info",
                    json!({"status":"pending","detail":"Accept the incoming message in the original Claude session. Its inbound rules apply; Remote Control is not required."}),
                );
            }
            Err(e) | Ok(claude_local::Delivery::Rejected(e)) => {
                offer.settled = true;
                projection::activity(
                    chat,
                    uuid,
                    "steer.updated",
                    "Message delivery not confirmed",
                    "error",
                    json!({"status":"failed","detail":e}),
                );
            }
        }
        local_status(chat);
        let _ = self.save(id, None).await;
        self.dirty = true;
    }
}

type Mapping = (
    Option<crate::claude_outbox::Record>,
    Option<Value>,
    Option<Value>,
);
fn project_row(chat: &mut Chat, mut row: Value, mapping: Option<&Mapping>) {
    let stamp = row["timestamp"]
        .as_str()
        .unwrap_or(s(&chat.thread["updatedAt"]))
        .to_owned();
    let native = s(&row["uuid"]).to_owned();
    let offer = chat
        .local_claude
        .as_ref()
        .unwrap()
        .offers
        .iter()
        .find(|o| o.uuid == native)
        .cloned();
    let saved = mapping.and_then(|m| m.1.as_ref());
    let submitted = offer.is_some() || mapping.is_some_and(|m| m.0.is_some());
    if submitted && crate::claude_turns::interruption_notice(&row, None) {
        row["isSynthetic"] = json!(false);
        row["shouldQuery"] = json!(true);
    }
    let interrupted = !submitted && crate::claude_turns::interruption_notice(&row, None);
    if interrupted || crate::claude_turns::no_response_placeholder(&row) {
        let ids: Vec<_> = [Some(native.as_str()), row["message"]["id"].as_str()]
            .into_iter()
            .flatten()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        chat.thread["messages"]
            .as_array_mut()
            .unwrap()
            .retain(|m| !ids.iter().any(|id| m["id"] == *id));
        let local = chat.local_claude.as_mut().unwrap();
        local.removed_messages.extend(ids);
        if interrupted
            && !local.cursor.turn.is_empty()
            && chat.thread["latestTurn"]["state"] != "interrupted"
        {
            local.cursor.complete = false;
            chat.thread["latestTurn"]["state"] = json!("interrupted");
            chat.thread["latestTurn"]["completedAt"] = json!(stamp);
        }
        return;
    }
    let input = offer
        .as_ref()
        .map(|o| (o.message.clone(), o.text.clone()))
        .or_else(|| crate::claude_turns::user_input(&row).ok().flatten());
    if let Some((mut mid, text)) = input {
        let outbox = mapping.and_then(|m| m.0.as_ref());
        if let Some(record) = outbox {
            mid = record.input.message.clone();
        }
        let turn = offer
            .as_ref()
            .map(|o| o.turn.clone())
            .or_else(|| outbox.map(|r| r.input.public_turn.clone()))
            .or_else(|| saved.and_then(|r| r["turnId"].as_str().map(str::to_owned)))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("history-{native}"));
        let local = chat.local_claude.as_mut().unwrap();
        local.cursor.turn = turn.clone();
        local.cursor.complete = false;
        if chat.thread["latestTurn"]["turnId"] != turn {
            chat.thread["latestTurn"] = json!({"turnId":turn,"state":"running","requestedAt":stamp,"startedAt":stamp,"completedAt":null,"assistantMessageId":null});
        }
        if let Some(saved) = saved {
            if !chat.thread["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == mid)
            {
                chat.thread["messages"]
                    .as_array_mut()
                    .unwrap()
                    .push(saved.clone());
            }
        } else {
            projection::message(chat, &mid, &text, "user", false, false);
            if let Some(m) = chat.thread["messages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == mid)
            {
                m["createdAt"] = json!(stamp);
                m["updatedAt"] = json!(stamp);
            }
        }
        chat.thread["latestUserMessageAt"] = json!(stamp);
        if let Some(o) = offer {
            chat.local_claude
                .as_mut()
                .unwrap()
                .offers
                .iter_mut()
                .find(|v| v.uuid == o.uuid)
                .unwrap()
                .confirmed = true;
            projection::activity(
                chat,
                &o.uuid,
                "steer.updated",
                "Message delivered",
                "info",
                json!({"status":"completed"}),
            );
        }
    }
    let terminal = row["type"] == "assistant"
        && matches!(
            row["message"]["stop_reason"].as_str(),
            Some("end_turn" | "stop_sequence")
        );
    let assistant = mapping.and_then(|m| m.2.as_ref());
    let aid = assistant
        .and_then(|m| m["id"].as_str())
        .unwrap_or(&native)
        .to_owned();
    if row["type"] == "assistant" && !native.is_empty() {
        row["message"]["id"] = json!(aid);
    }
    let known: HashSet<String> = chat.thread["activities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| s(&r["id"]).to_owned())
        .collect();
    if row["type"] != "assistant" || assistant.is_none() {
        projection::project(chat, &row);
    }
    for m in chat.thread["messages"].as_array_mut().unwrap() {
        if m["id"] == aid && m["role"] == "assistant" && assistant.is_none() {
            m["createdAt"] = json!(stamp);
            m["updatedAt"] = json!(stamp);
            m["phase"] = json!(if terminal {
                "final_answer"
            } else {
                "commentary"
            });
        }
    }
    for a in chat.thread["activities"].as_array_mut().unwrap() {
        if !known.contains(s(&a["id"])) {
            a["createdAt"] = json!(stamp);
        }
    }
    if terminal || row["type"] == "result" {
        chat.local_claude.as_mut().unwrap().cursor.complete = true;
        if !chat.thread["latestTurn"].is_null() {
            chat.thread["latestTurn"]["state"] = json!(if row["is_error"] == true
                || row["isApiErrorMessage"] == true
            {
                "error"
            } else {
                "completed"
            });
            chat.thread["latestTurn"]["completedAt"] = json!(stamp);
            if terminal {
                chat.thread["latestTurn"]["assistantMessageId"] = json!(aid);
            }
        }
    }
    if !stamp.is_empty() && stamp.as_str() > s(&chat.thread["updatedAt"]) {
        chat.thread["updatedAt"] = json!(stamp);
    }
}

// A busy native process may record an inbox message only at its next input
// boundary. Time alone is not evidence of failure and must not release the
// pending-message guard (which could allow a second copy to be submitted).
fn update_pending_confirmations(chat: &mut Chat, caught_up: bool, owner_present: bool) {
    if !caught_up {
        return;
    }
    let unresolved: Vec<_> = chat
        .local_claude
        .as_ref()
        .unwrap()
        .offers
        .iter()
        .filter(|o| {
            !o.confirmed
                && !o.settled
                && (!o.held || !owner_present)
                && time::OffsetDateTime::parse(
                    &o.at,
                    &time::format_description::well_known::Rfc3339,
                )
                .is_ok_and(|at| time::OffsetDateTime::now_utc() - at > time::Duration::seconds(30))
        })
        .map(|o| o.uuid.clone())
        .collect();
    for uuid in unresolved {
        chat.local_claude
            .as_mut()
            .unwrap()
            .offers
            .iter_mut()
            .find(|o| o.uuid == uuid)
            .unwrap()
            .settled = !owner_present;
        projection::activity(
            chat,
            &uuid,
            "steer.updated",
            if owner_present {
                "Waiting for Claude confirmation"
            } else {
                "Claude exited before confirming delivery"
            },
            "info",
            json!({"status":if owner_present {"pending"} else {"failed"},"detail":if owner_present {
                "Claude has not recorded this message yet. It may still be working or holding the message under its inbound rules. It has not been resent."
            } else {
                "The original Claude process exited without recording this message. Delivery is unconfirmed; inspect its history before resending."
            }}),
        );
    }
}

fn local_status(chat: &mut Chat) {
    let local = chat.local_claude.as_ref().unwrap();
    let caught_up = local.caught_up;
    let busy = local.owner.as_ref().is_some_and(Owner::busy);
    let pending = local.offers.iter().any(|o| !o.confirmed && !o.settled);
    let live_owner = local.owner.is_some();
    let waiting = local.owner.as_ref().is_some_and(|o| o.status == "waiting")
        || local
            .offers
            .iter()
            .any(|o| o.held && !o.confirmed && !o.settled);
    let status = if !caught_up || pending && !busy {
        "starting"
    } else if busy {
        "running"
    } else {
        "ready"
    };
    chat.thread["session"]["status"] = json!(status);
    chat.thread["session"]["statusConfidence"] =
        json!(if caught_up { "confirmed" } else { "unknown" });
    chat.thread["session"]["activeTurnId"] = if busy {
        chat.thread["latestTurn"]["turnId"].clone()
    } else {
        Value::Null
    };
    if caught_up && !busy && !pending && chat.thread["latestTurn"]["state"] == "running" {
        chat.thread["latestTurn"]["state"] = json!(if local.cursor.complete {
            "completed"
        } else {
            "interrupted"
        });
    }
    if caught_up {
        chat.thread["session"]["lastError"] = Value::Null;
    }
    chat.thread["session"]["connectionState"] =
        json!(if caught_up { "connected" } else { "connecting" });
    chat.thread["session"]["connectionIssue"] = Value::Null;
    chat.thread["session"]["connectionDetail"] = Value::Null;
    chat.thread["session"]["localClaude"] = json!(true);
    chat.thread["session"]["localClaudeOwner"] = json!(local.owner.is_some());
    chat.thread["session"]["localClaudePending"] = json!(pending);
    chat.thread["session"]["localClaudeWaiting"] = json!(waiting);
    projection::activity(
        chat,
        "claude-native-sync",
        "claude.desktop-sync.updated",
        "Local Claude session",
        "info",
        json!({"status":if caught_up {"connected"}else{"connecting"},"existingSession":true,"readOnly":!caught_up,"local":true,"liveOwner":live_owner,"waiting":waiting,
        "detail":if !caught_up {"Loading saved Claude history"}else if live_owner {"Queue and Steer use Claude’s local peer inbox while this session is open on the PC. Claude may hold or refuse incoming messages; they do not override direct instructions in that session."}else{"Local Claude history connected; sending resumes the same session"}}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chat() -> Chat {
        let mut c:Chat=serde_json::from_value(json!({"agent":"claude","native_id":"native","resumed":true,"thread":{"id":"chat","latestTurn":null,"session":{"status":"idle"},"messages":[],"activities":[],"updatedAt":"2026-11-01T00:00:00Z"}})).unwrap();
        c.local_claude = Some(Local {
            removed_messages: vec![],
            path: "/fixture".into(),
            cursor: Default::default(),
            offers: vec![],
            owner: None,
            caught_up: true,
        });
        c
    }
    #[test]
    fn interruption_and_resume_placeholder_do_not_create_prompts_or_completed_answers() {
        let mut c = chat();
        let input = "11111111-1111-4111-8111-111111111111";
        project_row(
            &mut c,
            json!({"type":"user","uuid":input,"timestamp":"2026-11-01T00:00:01Z","message":{"role":"user","content":"Analyze the clip"}}),
            None,
        );
        let turn = c.thread["latestTurn"]["turnId"].clone();
        let notice = json!({"type":"user","uuid":"22222222-2222-4222-8222-222222222222","isSynthetic":true,"shouldQuery":false,"timestamp":"2026-11-01T00:00:02Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        project_row(&mut c, notice.clone(), None);
        let mut repeat = notice;
        repeat["timestamp"] = json!("2026-11-01T00:00:03Z");
        project_row(&mut c, repeat, None);
        let placeholder = json!({"type":"assistant","uuid":"placeholder","timestamp":"2026-11-01T00:00:04Z","message":{"id":"synthetic","model":"<synthetic>","stop_reason":"stop_sequence","content":[{"type":"text","text":"No response requested."}]}});
        project_row(&mut c, placeholder, None);
        local_status(&mut c);
        assert_eq!(c.thread["messages"].as_array().unwrap().len(), 1);
        assert_eq!(c.thread["latestTurn"]["turnId"], turn);
        assert_eq!(c.thread["latestTurn"]["state"], "interrupted");
        assert_eq!(
            c.thread["latestTurn"]["completedAt"],
            "2026-11-01T00:00:02Z"
        );
        assert_eq!(c.thread["latestUserMessageAt"], "2026-11-01T00:00:01Z");
        assert!(c.thread["latestTurn"]["assistantMessageId"].is_null());
    }
    #[test]
    fn local_history_preserves_native_timestamps_and_terminal_outcomes() {
        let mut c = chat();
        project_row(
            &mut c,
            json!({"type":"user","uuid":"11111111-1111-4111-8111-111111111111","timestamp":"2026-11-01T00:00:01Z","message":{"role":"user","content":"hello"}}),
            None,
        );
        project_row(
            &mut c,
            json!({"type":"assistant","uuid":"answer-1","timestamp":"2026-11-01T00:00:02Z","message":{"id":"wire-1","stop_reason":"end_turn","content":[{"type":"text","text":"answer"}]}}),
            None,
        );
        local_status(&mut c);
        assert_eq!(c.thread["messages"][0]["createdAt"], "2026-11-01T00:00:01Z");
        assert_eq!(c.thread["latestTurn"]["state"], "completed");
        assert_eq!(c.thread["session"]["status"], "ready");
        project_row(
            &mut c,
            json!({"type":"user","uuid":"22222222-2222-4222-8222-222222222222","timestamp":"2026-11-01T00:00:03Z","message":{"role":"user","content":"again"}}),
            None,
        );
        local_status(&mut c);
        assert_eq!(
            c.thread["latestTurn"]["state"], "interrupted",
            "A stopped process with no answer must not be Completed"
        );
    }
    #[test]
    fn local_history_reuses_managed_message_and_turn_ids() {
        let mut c = chat();
        let input = crate::claude_outbox::Record {
            state: "consumed".into(),
            input: crate::claude_turns::Input {
                native: "33333333-3333-4333-8333-333333333333".into(),
                message: "android-user".into(),
                public_turn: "same-turn".into(),
                local: true,
                steer: false,
            },
        };
        let saved = json!({"id":"android-user","role":"user","text":"original","turnId":"same-turn","createdAt":"2026-11-01T00:00:01Z"});
        project_row(
            &mut c,
            json!({"type":"user","uuid":"33333333-3333-4333-8333-333333333333","timestamp":"2026-11-01T00:00:01Z","message":{"role":"user","content":"original"}}),
            Some(&(Some(input), Some(saved), None)),
        );
        assert_eq!(c.thread["messages"].as_array().unwrap().len(), 1);
        assert_eq!(c.thread["messages"][0]["id"], "android-user");
        assert_eq!(c.thread["latestTurn"]["turnId"], "same-turn");
        let assistant = json!({"id":"wire-answer","role":"assistant","text":"complete response","turnId":"same-turn"});
        c.thread["messages"]
            .as_array_mut()
            .unwrap()
            .push(assistant.clone());
        project_row(
            &mut c,
            json!({"type":"assistant","uuid":"native-answer","message":{"id":"wire-answer","stop_reason":"end_turn","content":[{"type":"text","text":"partial"}]}}),
            Some(&(None, None, Some(assistant))),
        );
        assert_eq!(c.thread["messages"].as_array().unwrap().len(), 2);
        assert_eq!(c.thread["messages"][1]["text"], "complete response");
        assert_eq!(c.thread["latestTurn"]["assistantMessageId"], "wire-answer");
    }
    #[test]
    fn awaiting_inbox_confirmation_does_not_look_completed_or_stopped() {
        let mut c = chat();
        c.thread["latestTurn"] = json!({"turnId":"pending-turn","state":"running"});
        c.local_claude.as_mut().unwrap().offers.push(Offer {
            uuid: "native-input".into(),
            message: "android-input".into(),
            turn: "pending-turn".into(),
            text: "hello".into(),
            at: now(),
            confirmed: false,
            settled: false,
            held: false,
            receipt: None,
            receipt_started: false,
        });
        local_status(&mut c);
        assert_eq!(c.thread["latestTurn"]["state"], "running");
        assert_eq!(c.thread["session"]["status"], "starting");
        assert_eq!(c.thread["session"]["localClaudePending"], true);
    }

    fn delayed_offer() -> Chat {
        let mut c = chat();
        c.thread["latestTurn"] = json!({"turnId":"active-review","state":"running"});
        c.local_claude.as_mut().unwrap().offers.push(Offer {
            uuid: "11111111-1111-4111-8111-111111111111".into(),
            message: "android-steer".into(),
            turn: "active-review".into(),
            text: "Focus on completion".into(),
            at: "2020-01-01T00:00:00Z".into(),
            confirmed: false,
            settled: false,
            held: false,
            receipt: None,
            receipt_started: false,
        });
        c
    }

    #[test]
    fn slow_live_steer_stays_pending_until_native_history_confirms_once() {
        let mut c = delayed_offer();
        update_pending_confirmations(&mut c, true, true);
        assert!(!c.local_claude.as_ref().unwrap().offers[0].settled);
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "pending");
        assert_eq!(c.thread["activities"][0]["tone"], "info");
        let previous = c.thread.clone();
        update_pending_confirmations(&mut c, true, true);
        assert_eq!(
            c.thread, previous,
            "Polling must not churn the waiting activity"
        );

        let row = json!({"type":"user","uuid":"11111111-1111-4111-8111-111111111111",
            "timestamp":"2026-11-06T06:00:00Z","message":{"role":"user","content":"Focus on completion"}});
        project_row(&mut c, row.clone(), None);
        project_row(&mut c, row, None);
        update_pending_confirmations(&mut c, true, true);
        assert!(c.local_claude.as_ref().unwrap().offers[0].confirmed);
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "completed");
        assert_eq!(c.thread["messages"].as_array().unwrap().len(), 1);
        assert_eq!(c.thread["messages"][0]["id"], "android-steer");
        assert_eq!(c.thread["latestTurn"]["turnId"], "active-review");
    }

    #[test]
    fn held_receipt_survives_slow_confirmation_until_the_owner_exits() {
        let mut c = delayed_offer();
        c.local_claude.as_mut().unwrap().offers[0].held = true;
        projection::activity(
            &mut c,
            "11111111-1111-4111-8111-111111111111",
            "steer.updated",
            "Claude is holding this message",
            "info",
            json!({"status":"pending"}),
        );
        let previous = c.thread.clone();
        update_pending_confirmations(&mut c, true, true);
        assert_eq!(c.thread, previous);
        assert!(!c.local_claude.as_ref().unwrap().offers[0].settled);
        update_pending_confirmations(&mut c, true, false);
        assert!(c.local_claude.as_ref().unwrap().offers[0].settled);
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "failed");
    }

    #[test]
    fn owner_exit_is_unconfirmed_only_after_history_is_caught_up() {
        let mut c = delayed_offer();
        update_pending_confirmations(&mut c, false, false);
        assert!(!c.local_claude.as_ref().unwrap().offers[0].settled);
        assert!(c.thread["activities"].as_array().unwrap().is_empty());
        update_pending_confirmations(&mut c, true, false);
        assert!(c.local_claude.as_ref().unwrap().offers[0].settled);
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "failed");
        assert!(s(&c.thread["activities"][0]["payload"]["detail"]).contains("unconfirmed"));
    }
}
