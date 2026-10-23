//! Claude sessions owned by another process. A persisted ownership flag keeps
//! every reconnect on the observer path; there is no CLI-launch fallback.
use super::*;
use crate::{claude_cursor::Cursor, claude_remote::Envelope, claude_turns::Input};

pub(super) struct State {
    cursor: Cursor,
    pub(super) ready: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StopIntent {
    pub turn: String,
    pub request: String,
    pub acknowledged: bool,
    pub result_seen: bool,
    #[serde(default)]
    pub background: Vec<crate::claude_background::StopTarget>,
}
pub(super) fn record_stop(
    chat: &mut Chat,
    targets: Vec<crate::claude_background::StopTarget>,
) -> String {
    if chat.agent != AgentKind::Claude {
        return "stop".into();
    }
    let request = format!("remodex-stop-{}", uuid::Uuid::new_v4());
    chat.external_stop = Some(StopIntent {
        turn: s(&chat.thread["session"]["activeTurnId"]).into(),
        request: request.clone(),
        acknowledged: false,
        result_seen: false,
        background: targets,
    });
    request
}

impl Hub {
    pub(super) fn background_tasks(
        &self,
        id: &str,
    ) -> Result<crate::claude_background::Tasks, String> {
        let worker = self.active.get(id).ok_or("Claude worker disconnected")?;
        if let Some(observer) = &worker.observer {
            let (router, _) = crate::claude_history::control_state(&observer.cursor.projection)?;
            Ok(router.background().clone())
        } else if let Some(router) = &worker.claude_router {
            Ok(router.background().clone())
        } else {
            Ok(worker.claude_background.clone())
        }
    }
    /// Called only for original worker replies, after their state is durable.
    pub(super) async fn advance_task_stop(
        &mut self,
        id: &str,
        frame: &Value,
    ) -> Result<(), String> {
        if !self
            .active
            .get(id)
            .is_some_and(|w| w.stopping && w.claude_stop.is_some())
        {
            return Ok(());
        }
        let tasks = self.background_tasks(id)?;
        let w = self.active.get_mut(id).unwrap();
        if let Some(request) = w.claude_stop.as_mut().unwrap().reply(frame, &tasks)? {
            w.deadline = Some(Instant::now() + Duration::from_secs(10));
            w.connection
                .send(&request)
                .await
                .map_err(|_| "Background Stop delivery is uncertain; it was not replayed")?;
        }
        Ok(())
    }
    pub(super) async fn connect_external_claude(
        &mut self,
        agent: AgentKind,
        p: &Value,
    ) -> Result<Value, String> {
        if agent != AgentKind::Claude || !self.claude_enabled || !self.config.claude.remote_control
        {
            return Err("Enable Claude synchronization in this isolated profile first".into());
        }
        let requested = s(&p["threadId"]);
        if let Some(chat) = self.chats.get(requested) {
            if chat.agent != agent
                || !chat.thread["deletedAt"].is_null()
                || !chat.thread["archivedAt"].is_null()
            {
                return Err("Select an available Claude chat".into());
            }
            if !chat.external_claude {
                return Ok(json!({"status":"managed","threadId":requested}));
            }
            if !self.active.contains_key(requested) {
                self.launch_external_claude(requested).await?;
            }
            return Ok(json!({"status":"connecting","threadId":requested}));
        }
        let native = requested
            .strip_prefix("claude-history-")
            .ok_or("Select an existing Claude history chat")?;
        if native.len() != 36 || uuid::Uuid::parse_str(native).is_err() {
            return Err("Invalid Claude session identity".into());
        }
        if self
            .chats
            .values()
            .any(|c| c.agent == agent && c.native_id == native)
        {
            return Err("This Claude session already has a Remodex chat".into());
        }
        let profile = self.config.claude.profile.clone();
        let root = self.config.workspace.clone();
        let native = native.to_owned();
        let link =
            tokio::task::spawn_blocking(move || crate::claude_link::read(&profile, &root, &native))
                .await
                .map_err(|_| "Claude history reader stopped")??;
        let project = self
            .projects
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| link.cwd.starts_with(s(&p["workspaceRoot"])))
            .max_by_key(|p| s(&p["workspaceRoot"]).len())
            .ok_or("Register this chat's project folder first")?;
        let mut chat = self
            .new_chat(
                agent,
                requested,
                &json!({"projectId":project["id"],"title":link.title}),
            )
            .await?;
        chat.native_id = link.identity.native_session.clone();
        chat.remote_identity = Some(link.identity);
        chat.external_claude = true;
        chat.resumed = true;
        chat.paused = true;
        chat.thread["worktreePath"] = json!(link.cwd);
        chat.thread["session"]["statusConfidence"] = json!("unavailable");
        self.chats.insert(requested.into(), chat);
        if let Err(error) = self.save(requested, None).await {
            self.chats.remove(requested);
            return Err(error);
        }
        self.claude_recovery.remember(requested, Instant::now());
        self.launch_external_claude(requested).await?;
        self.publish();
        Ok(json!({"status":"connecting","threadId":requested}))
    }

    pub(super) async fn launch_external_claude(&mut self, id: &str) -> Result<(), String> {
        if !self.claude_enabled || !self.config.claude.remote_control {
            return Err("Enable Claude Code sharing in this bridge first".into());
        }
        if self.active.contains_key(id) {
            return Ok(());
        }
        if self
            .active
            .values()
            .filter(|w| w.observer.is_some() || w.claude_router.is_some())
            .count()
            >= crate::claude_reconnect::HOST_LIMIT
        {
            return Err(
                "The shared connection limit is reached; reconnect will wait for a free slot"
                    .into(),
            );
        }
        let chat = &self.chats[id];
        if !chat.external_claude || !claude_recoverable(chat) {
            return Err("No existing Claude link is available".into());
        }
        let identity = chat.remote_identity.clone().unwrap();
        let key = id.to_owned();
        let cursor = self
            .db
            .call(move |a| a.store.claude_remote_cursor(&key, identity))
            .await?
            .map_err(|_| "Saved Claude cursor is unavailable")?;
        let (_, requests) = crate::claude_history::control_state(&cursor.projection)?;
        let connection = Connection::observe_claude(
            self.config.claude.profile.clone(),
            self.config.workspace.clone(),
            chat.native_id.clone(),
            cursor.clone(),
            self.db.clone(),
            id.into(),
        );
        self.active.insert(
            id.into(),
            Worker {
                connection,
                chat_id: id.into(),
                stage: "observer-init",
                native_turn: String::new(),
                owns_turn: false,
                pre_turn_events: Default::default(),
                pre_turn_bytes: 0,
                requests,
                steers: HashMap::new(),
                last_prompt: String::new(),
                first_prompt: String::new(),
                deadline: Some(Instant::now() + Duration::from_secs(60)),
                stopping: false,
                metadata: metadata::Reads::default(),
                control_reply: None,
                startup_tools: vec![],
                checkpoint_labels: HashMap::new(),
                unsupported_controls: 0,
                claude_router: None,
                claude_background: Default::default(),
                claude_deferred_result: None,
                claude_stop: None,
                local_turn: None,
                recovering: true,
                observer: Some(State {
                    cursor,
                    ready: false,
                }),
            },
        );
        Ok(())
    }

    pub(super) fn external_send_ready(
        &self,
        id: &str,
        c: &Value,
        steer: bool,
    ) -> Result<(), String> {
        let w = self
            .active
            .get(id)
            .ok_or("Claude is reconnecting; the prompt has not been sent")?;
        if !w.observer.as_ref().is_some_and(|o| o.ready)
            || w.stopping
            || (steer && w.stage != "running")
            || (!steer && w.stage != "idle")
        {
            return Err("Wait for the existing Claude session to finish connecting".into());
        }
        let chat = &self.chats[id];
        if (c["modelSelection"].is_object() && c["modelSelection"] != chat.thread["modelSelection"])
            || c["runtimeMode"]
                .as_str()
                .is_some_and(|v| chat.thread["runtimeMode"] != v)
            || c["interactionMode"]
                .as_str()
                .is_some_and(|v| chat.thread["interactionMode"] != v)
        {
            // Settings are confirmed by the worker first; never send a prompt
            // whose model, effort or access differs from the confirmed state.
            return Err("Claude has not confirmed this message's model, effort or access settings. Nothing was sent; try again.".into());
        }
        Ok(())
    }

    pub(super) async fn send_external_claude(&mut self, id: &str) -> Result<(), String> {
        let chat = &self.chats[id];
        let input = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: s(&chat.pending_message["messageId"]).into(),
            public_turn: s(&chat.thread["latestTurn"]["turnId"]).into(),
            local: true,
            steer: false,
        };
        let content = crate::attachments::input(&chat.pending_message, false)?;
        self.deliver_external_claude(id, input, content).await
    }

    async fn deliver_external_claude(
        &mut self,
        id: &str,
        input: Input,
        content: Value,
    ) -> Result<(), String> {
        let mut payload = json!({"type":"user","uuid":input.native,"session_id":self.chats[id].native_id,"origin":{"kind":"human"},"parent_tool_use_id":null,"message":{"role":"user","content":content}});
        if input.steer {
            payload["priority"] = json!("now");
        }
        let w = self
            .active
            .get_mut(id)
            .ok_or("The observer disconnected before delivery; no retry was made")?;
        let o = w
            .observer
            .as_ref()
            .ok_or("This is not an existing-session observer")?;
        if !o.ready {
            return Err("Claude is catching up; the prompt has not been sent".into());
        }
        w.last_prompt = input.native.clone();
        // A user echo is not acceptance. Stay busy until a native receipt arrives.
        w.stage = "running";
        w.connection.send(&json!({"type":"remodex_observer_prompt","chat":id,"input":input,"payload":payload})).await.map_err(|_|"Claude delivery is uncertain. No prompt was retried.".to_owned())
    }

    pub(super) async fn steer_external_claude(
        &mut self,
        id: &str,
        c: Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        self.external_send_ready(id, &c, true)?;
        let w = &self.active[id];
        if w.steers.len() >= 8 {
            return Err("Wait for pending steering messages".into());
        }
        let (mut router, _) =
            crate::claude_history::control_state(&w.observer.as_ref().unwrap().cursor.projection)?;
        let input = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: s(&c["message"]["messageId"]).into(),
            public_turn: s(&self.chats[id].thread["session"]["activeTurnId"]).into(),
            local: true,
            steer: true,
        };
        router.register(input.clone())?;
        let content = crate::attachments::input(&c["message"], false)?;
        let previous = self.chats[id].clone();
        let chat = self.chats.get_mut(id).unwrap();
        projection::message(
            chat,
            &input.message,
            &crate::attachments::display_message(&c["message"]),
            "user",
            false,
            false,
        );
        projection::activity(
            chat,
            &input.native,
            "steer.updated",
            "Steering message sent",
            "info",
            json!({"status":"pending","detail":"Waiting for Claude confirmation"}),
        );
        if let Err(error) = self.save(id, receipt).await {
            self.chats.insert(id.into(), previous);
            return Err(error);
        }
        self.active
            .get_mut(id)
            .unwrap()
            .steers
            .insert(input.native.clone(), false);
        if self
            .deliver_external_claude(id, input, content)
            .await
            .is_err()
        {
            self.detach_external_claude(
                id,
                Some("Steer delivery is uncertain. No message was retried."),
            )
            .await;
        }
        Ok(())
    }

    pub(super) async fn event_external_claude(
        &mut self,
        id: &str,
        event: Option<Event>,
    ) -> Result<(), String> {
        match event {
            Some(Event::ClaudeRemote(event)) => self.project_external_claude(id, event).await,
            Some(Event::Message(v)) if v["type"] == "remodex_observer_ready" => {
                let cid = id.to_owned();
                let native = self.chats[id].native_id.clone();
                let pending = self
                    .db
                    .call(move |a| a.store.unresolved_claude_inputs(&cid, &native))
                    .await?
                    .map_err(|_| "Claude delivery state unavailable")?;
                let w = self.active.get_mut(id).ok_or("Observer is unavailable")?;
                w.observer.as_mut().unwrap().ready = true;
                w.recovering = false;
                w.deadline = None;
                let (router, requests) = crate::claude_history::control_state(
                    &w.observer.as_ref().unwrap().cursor.projection,
                )?;
                w.requests = requests;
                w.stage = if router.active().is_some()
                    || router.pending().next().is_some()
                    || !pending.is_empty()
                {
                    "running"
                } else {
                    "idle"
                };
                self.claude_recovery.connected(id, Instant::now());
                let chat = self.chats.get_mut(id).unwrap();
                chat.thread["session"]["statusConfidence"] = json!("confirmed");
                projection::activity(
                    chat,
                    "claude-native-sync",
                    "claude.desktop-sync.updated",
                    "Claude Desktop sync",
                    "info",
                    json!({"status":"connected","existingSession":true,"readOnly":false,"detail":"Connected to the original Claude session"}),
                );
                self.save(id, None).await?;
                self.dirty = true;
                self.run_external_queue(id).await?;
                Ok(())
            }
            Some(Event::Message(v)) if v["type"] == "remodex_observer_error" => {
                self.detach_external_claude(id, v["detail"].as_str()).await;
                Ok(())
            }
            _ => {
                self.detach_external_claude(
                    id,
                    Some("Claude connection lost; reconnecting with saved history"),
                )
                .await;
                Ok(())
            }
        }
    }

    async fn project_external_claude(&mut self, id: &str, event: Envelope) -> Result<(), String> {
        if event.recovery == Some(true) {
            let w = self.active.get_mut(id).ok_or("Observer is unavailable")?;
            w.observer.as_mut().unwrap().ready = false;
            w.recovering = true;
            w.stage = "observer-init";
            // Each history page has its own timeout. A fixed startup deadline
            // would repeatedly kill valid recovery of a large conversation.
            w.deadline = None;
            let chat = self.chats.get_mut(id).unwrap();
            chat.thread["session"]["statusConfidence"] = json!("unavailable");
            projection::activity(
                chat,
                "claude-native-sync",
                "claude.desktop-sync.updated",
                "Claude Desktop sync",
                "info",
                json!({"status":"connecting","existingSession":true,
                    "readOnly":true,"detail":"Catching up saved Claude messages"}),
            );
        }
        let o = self.active[id].observer.as_ref().unwrap();
        let cursor = o.cursor.clone();
        let catchup = !o.ready;
        let state = serde_json::to_value(&self.chats[id]).map_err(|_| "Invalid observer chat")?;
        let (state, projection) =
            crate::claude_history::project(&self.db, state, &cursor, &event, catchup).await?;
        let (router, requests) = crate::claude_history::control_state(&projection)?;
        let previous_state = self.chats[id].thread["latestTurn"]["state"].clone();
        let previous_stage = self.active[id].stage;
        let mut next: Chat =
            serde_json::from_value(state.clone()).map_err(|_| "Invalid projected chat")?;
        if event.needs_commit() {
            self.active[id].connection.send(&json!({"type":"remodex_observer_commit","state":state,"projection":projection})).await.map_err(|_|"Claude history commit failed; the saved cursor was retained")?;
            let key = id.to_owned();
            let saved = self
                .db
                .call(move |a| a.store.load_chat(&key))
                .await?
                .map_err(|_| "Saved observer history unavailable")?;
            next.thread = saved["thread"].clone();
        } else {
            let saved = state.clone();
            self.db
                .call(move |a| a.store.save_chat(&saved, None))
                .await?
                .map_err(|_| "Cannot save streamed Claude output")?;
        }
        let w = self.active.get_mut(id).unwrap();
        let o = w.observer.as_mut().unwrap();
        if event.sequence.is_some() {
            o.cursor = cursor.advance(&event, projection.clone())?;
        } else {
            o.cursor.projection = projection;
        }
        w.requests = requests;
        if o.ready {
            // An offered local prompt with no echo yet is still pending.
            let pending = if w.last_prompt.is_empty() {
                false
            } else {
                let cid = id.to_owned();
                let native = next.native_id.clone();
                let prompt = w.last_prompt.clone();
                self.db
                    .call(move |a| a.store.claude_input(&cid, &native, &prompt))
                    .await?
                    .map_err(|_| "Delivery state unavailable")?
                    .is_some_and(|r| ["offered", "consumed"].contains(&r.state.as_str()))
            };
            w.stage = if router.active().is_some() || router.pending().next().is_some() || pending {
                "running"
            } else {
                "idle"
            };
            if w.stage == "idle" {
                w.deadline = None;
                w.stopping = false;
                w.claude_stop = None;
                w.steers.clear();
            }
        }
        // Only the original worker may answer reads. Apply replies after the
        // corresponding event/cursor commit, never from another client's echo.
        let metadata = if event.source == "worker" {
            event.payload.as_ref().map(|v| w.metadata.event(v))
        } else {
            None
        };
        self.chats.insert(id.into(), next);
        if event.source == "worker" && event.sequence.is_some() {
            if let Some(payload) = &event.payload {
                self.advance_task_stop(id, payload).await?;
            }
        }
        if let (Some(intent), Some(payload)) =
            (&self.chats[id].native_control, event.payload.as_ref())
        {
            if event.source == "worker"
                && event.sequence.is_some()
                && intent.result.is_some()
                && payload["response"]["request_id"] == intent.request
            {
                crate::capabilities::control_seed(
                    &mut self.active.get_mut(id).unwrap().metadata.capability_seed,
                    &intent.action,
                    payload,
                );
            }
        }
        self.complete_native_control(id).await;
        if let Some(metadata::Reply::Context(value)) = metadata {
            self.report_usage(id, true, value).await?;
        }
        self.dirty = true;
        if !catchup && (previous_state != "completed" || previous_stage != "idle") {
            self.run_external_queue(id).await?;
        }
        Ok(())
    }

    async fn run_external_queue(&mut self, id: &str) -> Result<(), String> {
        if self.active[id].stage == "idle"
            && self.active[id].observer.as_ref().is_some_and(|o| o.ready)
            && self.chats[id].thread["latestTurn"]["state"] == "completed"
            && !self.chats[id].paused
            && !self.chats[id].queue.is_empty()
        {
            if self.queue_shared_settings(id).await {
                return Ok(());
            }
            let next = self.chats.get_mut(id).unwrap().queue.remove(0);
            if let Err(error) = self.begin(id, next.clone(), None).await {
                let chat = self.chats.get_mut(id).unwrap();
                chat.queue.insert(0, next);
                chat.paused = true;
                chat.thread["session"]["lastError"] = json!(error);
                self.save(id, None).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn detach_external_claude(&mut self, id: &str, error: Option<&str>) {
        let Some(mut w) = self.active.remove(id) else {
            return;
        };
        let queued = native_control::disconnect(&mut w);
        self.tool_controls.remove(id);
        w.metadata.disconnect();
        w.connection.close().await; // Only this observer socket; never the owner.
        self.claude_recovery.failed(id, Instant::now());
        let cid = id.to_owned();
        let native = self.chats[id].native_id.clone();
        self.db
            .call(move |a| a.store.mark_claude_delivery_uncertain(&cid, &native))
            .await
            .ok();
        let chat = self.chats.get_mut(id).unwrap();
        chat.paused = true;
        if let Some(error) = queued {
            chat.thread["session"]["lastError"] = json!(error);
        }
        chat.thread["session"]["statusConfidence"] = json!("unavailable");
        projection::activity(
            chat,
            "claude-native-sync",
            "claude.desktop-sync.updated",
            "Claude Desktop sync",
            "info",
            json!({"status":"disconnected","existingSession":true,"readOnly":true,"detail":error.unwrap_or("Reconnecting to the original Claude session")}),
        );
        let _ = self.save(id, None).await;
        self.dirty = true;
    }
}
