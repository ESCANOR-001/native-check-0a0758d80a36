use super::*;
use crate::capabilities::Control;
use std::collections::VecDeque;

pub(super) struct Waiter {
    /// None for an automatically started queued prompt: its outcome is saved.
    reply: Option<oneshot::Sender<Result<Value, String>>>,
    deadline: Instant,
    dispatch: bool,
    settings: Option<Settings>,
}

/// Native settings that must be confirmed before one prompt can be sent.
/// Each step is reserved durably before delivery and none is ever replayed.
pub(super) struct Settings {
    then: Then,
    steps: VecDeque<Step>,
    confirmed: Vec<&'static str>,
}
pub(super) enum Then {
    /// Run the client's original command through the ordinary prompt path.
    Dispatch {
        agent: AgentKind,
        client: String,
        params: Value,
    },
    /// Start the queued prompt, provided it is still first in the queue.
    Queue { message: String },
}
pub(super) struct Step {
    key: String,
    digest: String,
    action: Control,
}
const SETTINGS_BUSY: &str = "Wait for the original Claude session to reconnect and finish its task before changing its model or effort.";
const NOT_SENT: &str = "Your message was not sent.";
const QUEUE_NOT_SENT: &str =
    "The queued message was not sent; remove it and send it again when ready.";

fn label(action: &Control) -> &'static str {
    match action {
        Control::Model(_) => "model",
        _ => "effort",
    }
}

pub(super) fn supported(action: &Value) -> bool {
    ["mcpToggle", "mcpReconnect", "reloadSkills", "reloadPlugins"].contains(&s(action))
}
impl Hub {
    pub(super) async fn cached_native_dispatch(
        &self,
        r: &Request,
    ) -> Result<Option<Value>, String> {
        let key = format!(
            "{}:{}:{}",
            r.agent.instance(),
            r.client,
            s(&r.params["commandId"])
        );
        let receipt = self
            .db
            .call(move |a| a.store.tool_action(&key))
            .await?
            .map_err(|_| "Control store unavailable")?;
        let Some((previous, value)) = receipt else {
            // Ordinary prompts may contain attachments. Do not serialize or
            // duplicate their buffers just to check an absent control receipt.
            return Ok(None);
        };
        let digest = format!("{:x}", Sha256::digest(r.params.to_string().as_bytes()));
        if previous == digest {
            crate::native_control::response(value).map(|_| Some(json!({"sequence":self.seq})))
        } else {
            Err("Command ID was already used for different content".into())
        }
    }

    /// Shared settings go through the existing worker, never a new CLI or an
    /// optimistic local-only update. Use the dispatch namespace for receipts so
    /// a command ID cannot be reused as a prompt after a lost acknowledgement.
    pub(super) async fn prepare_access_control(&self, r: &Request) -> Result<PreparedTool, String> {
        let id = s(&r.params["threadId"]);
        let chat = self.checked(r.agent, id)?;
        let command = s(&r.params["commandId"]);
        if r.agent != AgentKind::Claude
            || command.is_empty()
            || command.len() > 128
            || id.len() > 128
        {
            return Err("Command and Claude thread IDs required".into());
        }
        let key = format!("{}:{}:{command}", r.agent.instance(), r.client);
        let digest = format!("{:x}", Sha256::digest(r.params.to_string().as_bytes()));
        let lookup = key.clone();
        let (command_receipt, control_receipt) = self
            .db
            .call(move |a| {
                Ok::<_, rusqlite::Error>((
                    a.store.command_digest(&lookup)?,
                    a.store.tool_action(&lookup)?,
                ))
            })
            .await?
            .map_err(|_| "Control store unavailable")?;
        if let Some(previous) = command_receipt {
            return if previous == digest {
                Ok(PreparedTool::Cached(json!({"updated":true})))
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        if let Some((previous, value)) = control_receipt {
            return if previous == digest {
                Ok(PreparedTool::Cached(value))
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        if self.tool_controls.contains(id)
            || !chat.queue.is_empty()
            || self
                .active
                .get(id)
                .is_none_or(|w| w.stage != "idle" || !w.metadata_ready())
        {
            return Err("Wait for the original Claude session to reconnect and finish its task; clear queued messages before changing settings".into());
        }
        if !chat.thread["deletedAt"].is_null() || !chat.thread["archivedAt"].is_null() {
            return Err("Select an active chat".into());
        }
        let mut runtime_mode = s(&chat.thread["runtimeMode"]).to_owned();
        let mut interaction_mode = s(&chat.thread["interactionMode"]).to_owned();
        match s(&r.params["type"]) {
            "thread.runtime-mode.set" => runtime_mode = s(&r.params["runtimeMode"]).to_owned(),
            "thread.interaction-mode.set" => {
                interaction_mode = s(&r.params["interactionMode"]).to_owned()
            }
            _ => return Err("Unsupported setting".into()),
        }
        if !["approval-required", "full-access", "auto-accept-edits"]
            .contains(&runtime_mode.as_str())
            || !["default", "plan"].contains(&interaction_mode.as_str())
        {
            return Err("Unsupported access or interaction mode".into());
        }
        Ok(PreparedTool::Run(
            key,
            digest,
            crate::capabilities::Control::Access {
                runtime_mode,
                interaction_mode,
            },
        ))
    }

    /// Model and effort for a prompt on an idle, externally owned Claude
    /// session. Ok(None) leaves the prompt to the ordinary path: nothing needs
    /// changing, the command was already accepted, or the chat is busy (queued
    /// prompts keep their selection; steering never changes settings).
    pub(super) async fn prepare_shared_settings(
        &self,
        r: &Request,
    ) -> Result<Option<VecDeque<Step>>, String> {
        let id = s(&r.params["threadId"]);
        let command = s(&r.params["commandId"]);
        let Some(chat) = self.chats.get(id).filter(|c| {
            c.agent == AgentKind::Claude && c.external_claude && c.thread["deletedAt"].is_null()
        }) else {
            return Ok(None);
        };
        if r.agent != AgentKind::Claude
            || command.is_empty()
            || command.len() > 128
            || id.len() > 128
            || self.busy(id)
        {
            return Ok(None);
        }
        let requested = match s(&r.params["type"]) {
            "thread.turn.start" => {
                if r.params.get("promptEdit").is_some()
                    || r.params.get("bootstrap").is_some()
                    || r.params["deliveryMode"] == "steer"
                    || !chat.queue.is_empty()
                {
                    return Ok(None);
                }
                r.params["modelSelection"].clone()
            }
            // Queued prompts are started by the queue runner below. An idle
            // shared chat rejects "send now", so no setting may change for it.
            _ => return Ok(None),
        };
        if !requested.is_object() {
            return Ok(None);
        }
        let actions = self.shared_actions(id, &requested)?;
        if actions.is_empty() {
            return Ok(None);
        }
        let key = format!("{}:{}:{command}", r.agent.instance(), r.client);
        if self
            .db
            .call(move |a| a.store.command_digest(&key))
            .await?
            .map_err(|_| "Command store unavailable")?
            .is_some()
        {
            return Ok(None);
        }
        if r.params["type"] == "thread.turn.start" {
            // Never change a session's settings for a prompt the ordinary
            // path is certain to reject afterwards.
            let text = s(&r.params["message"]["text"]);
            let mid = s(&r.params["message"]["messageId"]);
            if (text.trim().is_empty()
                && r.params["message"]["attachments"]
                    .as_array()
                    .is_none_or(|a| a.is_empty()))
                || text.len() > 32000
                || mid.is_empty()
                || mid.len() > 128
                || !["normal", "queue", ""].contains(&s(&r.params["deliveryMode"]))
            {
                return Err("A prompt of up to 32,000 bytes and a message ID are required".into());
            }
            if chat.thread["messages"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m["id"] == mid))
            {
                return Err("Message ID already accepted".into());
            }
            if r.params["runtimeMode"]
                .as_str()
                .is_some_and(|v| chat.thread["runtimeMode"] != v)
                || r.params["interactionMode"]
                    .as_str()
                    .is_some_and(|v| chat.thread["interactionMode"] != v)
            {
                return Err("Change access with its own control before sending; nothing was changed or sent".into());
            }
        }
        self.shared_settings_ready(id)?;
        self.shared_steps(id, command, actions).await.map(Some)
    }

    fn shared_settings_ready(&self, id: &str) -> Result<(), String> {
        let chat = &self.chats[id];
        if self.tool_controls.contains(id)
            || !chat.thread["archivedAt"].is_null()
            || self.active.get(id).is_none_or(|w| {
                w.stage != "idle"
                    || !w.metadata_ready()
                    || w.control_reply.is_some()
                    || !w.observer.as_ref().is_some_and(|o| o.ready)
            })
        {
            return Err(SETTINGS_BUSY.into());
        }
        Ok(())
    }

    /// Validates the selection against the CLI's catalog; returns the native
    /// controls, in model-then-effort order, that differ from confirmed state.
    fn shared_actions(&self, id: &str, requested: &Value) -> Result<Vec<Control>, String> {
        let catalog = self
            .models
            .get(AgentKind::Claude.instance())
            .cloned()
            .unwrap_or_else(|| crate::models::fallback(&self.config.claude.model));
        let current = &self.chats[id].thread["modelSelection"];
        let target = crate::native_control::shared_selection(&catalog, requested, current)?;
        Ok(crate::native_control::settings_steps(&target, current))
    }

    /// Receipts are scoped to the command: a confirmed step is skipped, while
    /// an uncertain or rejected one returns its saved outcome and is never
    /// sent again.
    async fn shared_steps(
        &self,
        id: &str,
        command: &str,
        actions: Vec<Control>,
    ) -> Result<VecDeque<Step>, String> {
        let mut steps = VecDeque::new();
        for action in actions {
            let key = format!(
                "{}:settings:{id}:{command}#{}",
                AgentKind::Claude.instance(),
                label(&action)
            );
            let digest = format!(
                "{:x}",
                Sha256::digest(json!({"threadId":id,"request":action.request()}).to_string())
            );
            let lookup = key.clone();
            let receipt = self
                .db
                .call(move |a| a.store.tool_action(&lookup))
                .await?
                .map_err(|_| "Control store unavailable")?;
            if let Some((previous, value)) = receipt {
                if previous != digest {
                    return Err("Command ID was already used for different content".into());
                }
                crate::native_control::response(value)
                    .map_err(|error| format!("{error} {NOT_SENT}"))?;
                continue;
            }
            steps.push_back(Step {
                key,
                digest,
                action,
            });
        }
        Ok(steps)
    }

    pub(super) async fn start_shared_settings(&mut self, r: Request, steps: VecDeque<Step>) {
        let id = s(&r.params["threadId"]).to_owned();
        let settings = Settings {
            then: Then::Dispatch {
                agent: r.agent,
                client: r.client,
                params: r.params,
            },
            steps,
            confirmed: vec![],
        };
        self.run_shared_settings(&id, settings, Some(r.reply)).await;
    }

    /// The automatic queue starts its first prompt only after that prompt's
    /// own settings are confirmed. Returns true when settings are in progress
    /// or failed (the queue is then paused with the reason).
    pub(super) async fn queue_shared_settings(&mut self, id: &str) -> bool {
        let Some(head) = self.chats[id].queue.first() else {
            return false;
        };
        let message = s(&head["message"]["messageId"]).to_owned();
        let command = s(&head["commandId"]).to_owned();
        let requested = head["modelSelection"].clone();
        if !requested.is_object() || command.is_empty() {
            return false;
        }
        let planned = match self.shared_actions(id, &requested) {
            Ok(actions) if actions.is_empty() => {
                // Equivalent (e.g. an alias of the confirmed wire model).
                let chat = self.chats.get_mut(id).unwrap();
                chat.queue[0]["modelSelection"] = chat.thread["modelSelection"].clone();
                None
            }
            Ok(actions) => Some(match self.shared_steps(id, &command, actions).await {
                Ok(steps) => (steps, self.shared_settings_ready(id)),
                Err(error) => (VecDeque::new(), Err(error)),
            }),
            Err(error) => Some((VecDeque::new(), Err(error))),
        };
        match planned {
            None => false,
            Some((steps, Ok(()))) => {
                let settings = Settings {
                    then: Then::Queue { message },
                    steps,
                    confirmed: vec![],
                };
                self.run_shared_settings(id, settings, None).await;
                true
            }
            Some((_, Err(error))) => {
                self.pause_queue(id, format!("{error} {QUEUE_NOT_SENT}"))
                    .await;
                true
            }
        }
    }

    async fn run_shared_settings(
        &mut self,
        id: &str,
        mut settings: Settings,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    ) {
        let Some(step) = settings.steps.pop_front() else {
            return self.finish_shared_settings(id, settings.then, reply).await;
        };
        let result = self
            .reserve_native_control(id, step.key, step.digest, step.action)
            .await;
        let request = match result {
            Ok(request) => request,
            Err(error) => return self.fail_shared_settings(settings, reply, error, id).await,
        };
        self.tool_controls.insert(id.to_owned());
        let w = self.active.get_mut(id).unwrap();
        if w.connection.send(&request).await.is_ok() {
            w.control_reply = Some(Waiter {
                reply,
                deadline: Instant::now() + Duration::from_secs(15),
                dispatch: true,
                settings: Some(settings),
            });
        } else {
            self.tool_controls.remove(id);
            let error = crate::native_control::UNCERTAIN.to_owned();
            self.fail_shared_settings(settings, reply, error, id).await;
        }
    }

    async fn finish_shared_settings(
        &mut self,
        id: &str,
        then: Then,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    ) {
        match then {
            Then::Dispatch {
                agent,
                client,
                params,
            } => {
                // The ordinary path validates and delivers the prompt once,
                // using the settings the worker has just confirmed.
                let result = match self.shared_settings_confirmed(id, &params["modelSelection"]) {
                    Ok(()) => {
                        let command = s(&params["commandId"]).to_owned();
                        self.confirmed_settings = Some((id.to_owned(), command));
                        let result = self.dispatch(agent, &client, params).await;
                        self.confirmed_settings = None;
                        result
                    }
                    Err(error) => Err(error),
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            Then::Queue { message } => {
                let chat = &self.chats[id];
                if chat.paused
                    || chat
                        .queue
                        .first()
                        .is_none_or(|q| q["message"]["messageId"] != message.as_str())
                {
                    return;
                }
                let mut next = self.chats.get_mut(id).unwrap().queue.remove(0);
                if let Err(error) = self.shared_settings_confirmed(id, &next["modelSelection"]) {
                    self.chats.get_mut(id).unwrap().queue.insert(0, next);
                    return self
                        .pause_queue(id, format!("{error} {QUEUE_NOT_SENT}"))
                        .await;
                }
                next["modelSelection"] = self.chats[id].thread["modelSelection"].clone();
                if let Err(error) = self.begin(id, next.clone(), None).await {
                    self.chats.get_mut(id).unwrap().queue.insert(0, next);
                    self.pause_queue(id, error).await;
                }
            }
        }
    }

    /// After every acknowledgement, the state the worker reports must still
    /// satisfy the selection. Only an alias whose wire ID the catalog does not
    /// map is accepted on the strength of its matching acknowledgement.
    fn shared_settings_confirmed(&self, id: &str, requested: &Value) -> Result<(), String> {
        let catalog = self.models.get(AgentKind::Claude.instance());
        match self.shared_actions(id, requested)?.as_slice() {
            [] => Ok(()),
            [Control::Model(model)]
                if catalog
                    .and_then(|c| crate::models::row(c, model))
                    .is_none_or(|r| r["resolvedModel"].is_null()) =>
            {
                Ok(())
            }
            _ => Err(format!("Claude reported different settings than this message selected. Check the model and effort, then try again. {NOT_SENT}")),
        }
    }

    async fn fail_shared_settings(
        &mut self,
        settings: Settings,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
        error: String,
        id: &str,
    ) {
        // Confirmed steps are already saved with their acknowledgements.
        let partial = if settings.confirmed.contains(&"model") {
            "Claude switched to the selected model, but the effort was not changed. "
        } else {
            ""
        };
        match settings.then {
            Then::Dispatch { .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(format!("{partial}{error} {NOT_SENT}")));
                }
            }
            Then::Queue { .. } => {
                self.pause_queue(id, format!("{partial}{error} {QUEUE_NOT_SENT}"))
                    .await
            }
        }
    }

    async fn pause_queue(&mut self, id: &str, error: String) {
        let Some(chat) = self.chats.get_mut(id) else {
            return;
        };
        chat.paused = true;
        chat.thread["session"]["lastError"] = json!(error);
        let _ = self.save(id, None).await;
        self.dirty = true;
    }

    pub(super) async fn start_native_control(
        &mut self,
        r: Request,
        key: String,
        digest: String,
        action: crate::capabilities::Control,
    ) {
        let id = s(&r.params["threadId"]).to_owned();
        let result = self.reserve_native_control(&id, key, digest, action).await;
        match result {
            Ok(request) => {
                self.tool_controls.insert(id.clone());
                let w = self.active.get_mut(&id).unwrap();
                w.metadata.capability_cache = None;
                if w.connection.send(&request).await.is_ok() {
                    w.control_reply = Some(Waiter {
                        reply: Some(r.reply),
                        deadline: Instant::now() + Duration::from_secs(15),
                        dispatch: r.method == "orchestration.dispatchCommand",
                        settings: None,
                    });
                } else {
                    self.tool_controls.remove(&id);
                    let _ = r.reply.send(Err(crate::native_control::UNCERTAIN.into()));
                }
            }
            Err(error) => {
                let _ = r.reply.send(Err(error));
            }
        }
    }
    async fn reserve_native_control(
        &mut self,
        id: &str,
        key: String,
        digest: String,
        action: crate::capabilities::Control,
    ) -> Result<Value, String> {
        if self
            .active
            .get(id)
            .is_none_or(|w| w.stage != "idle" || !w.metadata_ready() || w.control_reply.is_some())
        {
            return Err(
                "Wait for the original Claude session to reconnect and finish its task".into(),
            );
        }
        if let Some(intent) = self.chats[id]
            .native_control
            .as_ref()
            .filter(|i| i.result.is_none())
        {
            let now = crate::auth::now();
            // Wall-clock changes must not strand a restored receipt indefinitely.
            if intent.deadline > now && intent.deadline <= now.saturating_add(15_000) {
                return Err("Wait for the current Claude control to finish".into());
            }
            // A new explicit request may supersede an uncertain operation. The
            // old ID remains durably failed and can never silently resend it.
            let previous = self.chats[id].clone();
            self.chats
                .get_mut(id)
                .unwrap()
                .native_control
                .as_mut()
                .unwrap()
                .result = Some(json!({"error":crate::native_control::UNCERTAIN}));
            if let Err(error) = self.save(id, None).await {
                self.chats.insert(id.into(), previous);
                return Err(error);
            }
        }
        let old = self.chats[id].clone();
        let mut chat = old.clone();
        let request_id = format!("tool-control-{}", uuid::Uuid::new_v4());
        let request =
            json!({"type":"control_request","request_id":request_id,"request":action.request()});
        chat.native_control = Some(crate::native_control::Intent {
            key: key.clone(),
            digest: digest.clone(),
            request: request_id.clone(),
            action,
            deadline: crate::auth::now() + 15_000,
            result: None,
        });
        let saved = serde_json::to_value(&chat).map_err(|_| "Cannot save Claude control")?;
        self.db
            .call(move |a| {
                a.store.save_tool_action(
                    &saved,
                    &key,
                    &digest,
                    &json!({"nativePending":true,"requestId":request_id}),
                )
            })
            .await?
            .map_err(|_| "Cannot reserve Claude control; nothing was sent")?;
        self.chats.insert(id.into(), chat);
        Ok(request)
    }
    /// Call only after saving the acknowledgement with the chat/cursor.
    pub(super) async fn complete_native_control(&mut self, id: &str) {
        if self
            .active
            .get(id)
            .is_none_or(|w| w.control_reply.is_none())
        {
            return;
        }
        let Some(intent) = self.chats[id]
            .native_control
            .as_ref()
            .filter(|i| i.result.is_some())
        else {
            return;
        };
        let step = label(&intent.action);
        let result = crate::native_control::response(intent.result.clone().unwrap());
        self.tool_controls.remove(id);
        let Some(w) = self.active.get_mut(id) else {
            return;
        };
        w.metadata.capability_cache = None;
        let Some(waiter) = w.control_reply.take() else {
            return;
        };
        match waiter.settings {
            Some(mut settings) => match result {
                Ok(_) => {
                    settings.confirmed.push(step);
                    self.run_shared_settings(id, settings, waiter.reply).await;
                }
                Err(error) => {
                    self.fail_shared_settings(settings, waiter.reply, error, id)
                        .await
                }
            },
            None => {
                let result = result.map(|v| {
                    if waiter.dispatch {
                        json!({"sequence":self.seq})
                    } else {
                        v
                    }
                });
                if let Some(reply) = waiter.reply {
                    let _ = reply.send(result);
                }
            }
        }
    }
    pub(super) async fn expire_native_controls(&mut self) {
        let expired: Vec<_> = self
            .active
            .iter()
            .filter(|(_, w)| {
                w.control_reply
                    .as_ref()
                    .is_some_and(|waiter| Instant::now() >= waiter.deadline)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let waiter = self
                .active
                .get_mut(&id)
                .and_then(|w| w.control_reply.take())
                .unwrap();
            self.tool_controls.remove(&id);
            let error = crate::native_control::UNCERTAIN.to_owned();
            match waiter.settings {
                Some(settings) => {
                    self.fail_shared_settings(settings, waiter.reply, error, &id)
                        .await
                }
                None => {
                    if let Some(reply) = waiter.reply {
                        let _ = reply.send(Err(error));
                    }
                }
            }
        }
    }
}
/// The caller pauses the chat; an unconfirmed setting never sends its prompt.
/// Returns the reason to save when a queued prompt's settings were cut off.
pub(super) fn disconnect(w: &mut Worker) -> Option<String> {
    let waiter = w.control_reply.take()?;
    let uncertain = crate::native_control::UNCERTAIN;
    match waiter.settings.map(|s| s.then) {
        Some(Then::Queue { .. }) => Some(format!("{uncertain} {QUEUE_NOT_SENT}")),
        settings => {
            if let Some(reply) = waiter.reply {
                let error = if settings.is_some() {
                    format!("{uncertain} {NOT_SENT}")
                } else {
                    uncertain.into()
                };
                let _ = reply.send(Err(error));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::observer_fixture::{self, Fake};
    use crate::{auth::Auth, store::Store};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    const NATIVE: &str = "00000000-0000-4000-8000-0000000000c1";
    const CHAT: &str = "shared-settings";
    #[derive(Clone, Copy)]
    enum Reply {
        Accept,
        Reject,
        Pending,
        /// The relay drops while the control is outstanding.
        Drop,
    }
    #[derive(Clone)]
    struct Worker {
        log: Arc<Mutex<Vec<Value>>>,
        script: Arc<Mutex<VecDeque<Reply>>>,
        /// Wire model the worker reports after switching to an alias.
        wire: Arc<Mutex<HashMap<String, String>>>,
        /// Holds each scripted acknowledgement and prompt turn until released.
        gate: Arc<tokio::sync::Semaphore>,
        connections: Arc<AtomicUsize>,
    }
    impl Default for Worker {
        fn default() -> Self {
            Self {
                log: Default::default(),
                script: Default::default(),
                wire: Arc::new(Mutex::new(HashMap::from([
                    ("opus".into(), "claude-wire-opus".into()),
                    ("default".into(), "claude-wire-sonnet".into()),
                ]))),
                gate: Arc::new(tokio::sync::Semaphore::new(0)),
                connections: Default::default(),
            }
        }
    }
    struct Fixture {
        _dir: tempfile::TempDir,
        db: AuthService,
        config: Config,
        hub: Chats,
        worker: Worker,
    }
    fn controls(log: &[Value]) -> Vec<Value> {
        log.iter()
            .filter(|v| v["type"] == "control_request")
            .map(|v| v["request"].clone())
            .collect()
    }
    fn prompts(log: &[Value]) -> usize {
        log.iter()
            .filter(|v| v["type"] == "remodex_observer_prompt")
            .count()
    }
    /// One original worker behind the relay: it acknowledges settings by
    /// script and records prompts, then finishes their turns. Like the real
    /// relay, writes are answered while earlier events await their commit.
    async fn serve(mut fake: Fake, worker: Worker) {
        let (done, mut replies) = mpsc::unbounded_channel::<Option<Vec<Value>>>();
        let mut outgoing = VecDeque::new();
        let mut pending: observer_fixture::Pending = None;
        if fake
            .output
            .send(Event::Message(json!({"type":"remodex_observer_ready"})))
            .await
            .is_err()
        {
            return;
        }
        loop {
            // Each durable event is committed before the next is offered.
            if pending.is_none() {
                if let Some(payload) = outgoing.pop_front() {
                    if !observer_fixture::emit(&mut fake, &mut pending, payload).await {
                        return;
                    }
                    continue;
                }
            }
            let write = tokio::select! {
                write = fake.input.recv() => match write {Some(write) => write, None => return},
                batch = replies.recv() => match batch {
                    Some(Some(batch)) => {outgoing.extend(batch); continue},
                    _ => return,
                },
            };
            let value = observer_fixture::answer(&mut fake, &mut pending, write).await;
            if value["type"] == "remodex_observer_commit" {
                continue;
            }
            worker.log.lock().unwrap().push(value.clone());
            let (worker, done, db) = (worker.clone(), done.clone(), fake.db.clone());
            tokio::spawn(async move {
                let _ = done.send(respond(value, worker, db).await);
            });
        }
    }
    /// The worker's events for one write; None drops the relay connection.
    async fn respond(value: Value, worker: Worker, db: AuthService) -> Option<Vec<Value>> {
        if value["type"] == "remodex_observer_prompt" {
            let input: crate::claude_turns::Input =
                serde_json::from_value(value["input"].clone()).unwrap();
            let (chat, saved, offered) = (
                s(&value["chat"]).to_owned(),
                value["payload"].clone(),
                input.clone(),
            );
            assert!(db
                .call(move |a| a.store.offer_claude_input(&chat, NATIVE, &offered, &saved))
                .await
                .unwrap()
                .unwrap());
            worker.gate.acquire().await.unwrap().forget();
            return Some(vec![
                json!({"type":"user","session_id":NATIVE,"uuid":input.native,"message":value["payload"]["message"]}),
                json!({"type":"assistant","session_id":NATIVE,"user_message_uuid":input.native,"message":{"id":format!("reply-{}",input.native),"content":[{"type":"text","text":"FIXTURE_REPLY"}]}}),
                json!({"type":"result","session_id":NATIVE,"user_message_uuid":input.native,"is_error":false}),
            ]);
        }
        if value["type"] != "control_request" {
            return Some(vec![]);
        }
        let scripted =
            ["set_model", "apply_flag_settings"].contains(&s(&value["request"]["subtype"]));
        let reply = if scripted {
            worker.gate.acquire().await.unwrap().forget();
            worker.script.lock().unwrap().pop_front()
        } else {
            None
        };
        let (subtype, error) = match reply.unwrap_or(Reply::Accept) {
            Reply::Accept => ("success", ""),
            Reply::Reject => ("error", "PRIVATE_METADATA refused"),
            Reply::Pending => ("error", "This model switch is still pending (an earlier model request or a PreModelSwitch hook has not finished); it will apply when that completes unless it is refused"),
            Reply::Drop => return None,
        };
        let mut events = vec![
            json!({"type":"control_response","response":{"request_id":value["request_id"],"subtype":subtype,"error":error,"response":{}}}),
        ];
        // Like the CLI, a switched worker re-announces itself with the wire
        // model ID, not the alias that was requested.
        if subtype == "success" && value["request"]["subtype"] == "set_model" {
            let alias = s(&value["request"]["model"]);
            let model = worker.wire.lock().unwrap().get(alias).cloned();
            events.push(json!({"type":"system","subtype":"init","session_id":NATIVE,"permissionMode":"default","model":model.as_deref().unwrap_or(alias)}));
        }
        Some(events)
    }
    impl Fixture {
        async fn start() -> Self {
            Self::with_catalog(true).await
        }
        /// `resolved`: whether the CLI catalog reports alias-to-wire IDs.
        async fn with_catalog(resolved: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let profile = dir.path().join("claude-profile");
            std::fs::create_dir(&profile).unwrap();
            // A directory cannot be launched: model discovery fails at once
            // and the saved catalog below remains the CLI's reported list.
            let unused = dir.path().to_path_buf();
            let agent = |profile: PathBuf, remote_control| AgentConfig {
                executable: unused.clone(),
                profile,
                model: "default".into(),
                tools: Default::default(),
                shared_server: None,
                remote_control,
            };
            let config = Config {
                storage: Default::default(),
                max_active_chats: 1,
                workspace: dir.path().into(),
                codex: agent(dir.path().into(), false),
                claude: agent(profile.clone(), false),
            };
            let mut rows = json!([
                {"value":"default","displayName":"Default","resolvedModel":"claude-wire-sonnet","supportedEffortLevels":["low","high"]},
                {"value":"opus","displayName":"Opus","resolvedModel":"claude-wire-opus","supportedEffortLevels":["low","high","max"]}
            ]);
            if !resolved {
                for row in rows.as_array_mut().unwrap() {
                    row.as_object_mut().unwrap().remove("resolvedModel");
                }
            }
            let catalog = crate::models::catalog(&rows, false, "default");
            let store = Store::open(dir.path()).unwrap();
            store
                .db
                .execute(
                    "INSERT INTO metadata(key,value) VALUES('native.models',?1)",
                    [json!({"claude-code":catalog}).to_string()],
                )
                .unwrap();
            let db = AuthService::new(Auth::new(store)).unwrap();
            let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
            hub.call(AgentKind::Claude,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":CHAT,"projectId":"native-test-project"})).await.unwrap();
            hub.call(AgentKind::Codex, "local", "native.shutdown", json!({}))
                .await
                .unwrap();
            db.call(|a| {
                let mut chat = a.store.load_chat(CHAT).unwrap();
                chat["native_id"] = json!(NATIVE);
                chat["external_claude"] = json!(true);
                chat["resumed"] = json!(true);
                chat["remote_identity"] =
                    json!({"native_session":NATIVE,"bridge_session":"cse_settings_fixture"});
                chat["thread"]["modelSelection"] =
                    json!({"instanceId":"claude-code","model":"claude-wire-sonnet","options":[]});
                a.store.save_chat(&chat, None).unwrap();
            })
            .await
            .unwrap();
            let worker = Worker::default();
            let mut fakes = observer_fixture::register(&profile);
            let served = worker.clone();
            tokio::spawn(async move {
                while let Some(fake) = fakes.recv().await {
                    served.connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve(fake, served.clone()));
                }
            });
            let mut config = config;
            config.claude.remote_control = true;
            let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
            let fixture = Self {
                _dir: dir,
                db,
                config,
                hub,
                worker,
            };
            fixture.connected(1).await;
            fixture
        }
        async fn restart(&mut self) {
            self.stop().await;
            let before = self.worker.connections.load(Ordering::SeqCst);
            self.hub = Chats::start(self.db.clone(), self.config.clone())
                .await
                .unwrap();
            self.connected(before + 1).await;
        }
        async fn stop(&self) {
            self.hub
                .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
                .await
                .unwrap();
        }
        /// Wait for the original-session handshake of the given connection.
        async fn connected(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if self.worker.connections.load(Ordering::SeqCst) >= count
                        && self.thread().await["session"]["statusConfidence"] == "confirmed"
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the shared session did not reconnect");
        }
        async fn thread(&self) -> Value {
            self.hub
                .call(
                    AgentKind::Claude,
                    "phone",
                    "orchestration.subscribeThread",
                    json!({"threadId":CHAT}),
                )
                .await
                .unwrap()["snapshot"]["thread"]
                .clone()
        }
        async fn send(&self, command: Value) -> Result<Value, String> {
            self.hub
                .call(
                    AgentKind::Claude,
                    "phone",
                    "orchestration.dispatchCommand",
                    command,
                )
                .await
        }
        fn log(&self) -> Vec<Value> {
            self.worker.log.lock().unwrap().clone()
        }
        fn script(&self, replies: &[Reply]) {
            self.worker.script.lock().unwrap().extend(replies);
        }
        async fn wait_log(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(8), async {
                while self.log().len() < count {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the worker did not receive the expected writes");
        }
        async fn idle(&self) -> Value {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let thread = self.thread().await;
                    if thread["latestTurn"]["state"] == "completed"
                        && thread["session"]["status"] != "running"
                    {
                        return thread;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the shared turn did not complete")
        }
    }
    fn prompt(command: &str, message: &str, selection: Value, delivery: &str) -> Value {
        json!({"type":"thread.turn.start","threadId":CHAT,"commandId":command,
            "message":{"messageId":message,"role":"user","text":format!("PROMPT_{message}"),"attachments":[]},
            "modelSelection":selection,"runtimeMode":"approval-required","interactionMode":"default","deliveryMode":delivery})
    }
    fn opus(effort: &str) -> Value {
        json!({"instanceId":"claude-code","model":"opus","options":[{"id":"effort","value":effort}]})
    }

    #[tokio::test]
    async fn shared_model_and_effort_are_confirmed_by_the_original_worker_before_one_prompt() {
        let mut f = Fixture::start().await;
        let before = f.thread().await;
        let command = prompt("settings", "first", opus("high"), "normal");
        let hub = f.hub.clone();
        let sent = command.clone();
        let send = tokio::spawn(async move {
            hub.call(
                AgentKind::Claude,
                "phone",
                "orchestration.dispatchCommand",
                sent,
            )
            .await
        });
        f.wait_log(1).await;
        assert_eq!(
            controls(&f.log()),
            vec![json!({"subtype":"set_model","model":"opus"})]
        );
        assert_eq!(
            f.thread().await["modelSelection"],
            before["modelSelection"],
            "a sent control is not confirmation"
        );
        assert!(f
            .send(prompt("other", "other", Value::Null, "normal"))
            .await
            .unwrap_err()
            .contains("Wait"));
        f.worker.gate.add_permits(1);
        f.wait_log(2).await;
        assert_eq!(
            controls(&f.log())[1],
            json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"high"}}),
            "effort waits for the model acknowledgement"
        );
        assert_eq!(prompts(&f.log()), 0);
        assert_eq!(
            f.thread().await["modelSelection"]["model"],
            "claude-wire-opus",
            "the worker's own report is the confirmed model"
        );
        f.worker.gate.add_permits(2);
        assert!(send.await.unwrap().unwrap()["sequence"].is_number());
        assert_eq!(f.log()[2]["type"], "remodex_observer_prompt");
        assert_eq!(
            f.log()[2]["payload"]["message"]["content"][0]["text"],
            "PROMPT_first"
        );
        let done = f.idle().await;
        assert_eq!(
            done["modelSelection"],
            json!({"instanceId":"claude-code","model":"claude-wire-opus","options":[{"id":"effort","value":"high"}]})
        );
        let users: Vec<_> = done["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .collect();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["text"], "PROMPT_first");
        // A retry or a restart answers from receipts; nothing is resent.
        f.send(command.clone()).await.unwrap();
        f.restart().await;
        f.send(command).await.unwrap();
        assert_eq!(f.log().len(), 3);
        let after = f.thread().await;
        assert_eq!(after["messages"], done["messages"]);
        // The alias of the confirmed wire model is not a change.
        f.worker.gate.add_permits(1);
        f.send(prompt("unchanged", "second", opus("high"), "normal"))
            .await
            .unwrap();
        f.idle().await;
        assert_eq!(controls(&f.log()).len(), 2);
        assert_eq!(prompts(&f.log()), 2);
        f.stop().await;
    }

    #[tokio::test]
    async fn shared_settings_failures_keep_confirmed_state_and_never_send_or_replay() {
        for (replies, expected, model) in [
            (
                vec![Reply::Reject],
                "previous model is retained",
                "claude-wire-sonnet",
            ),
            (
                vec![Reply::Pending],
                "may still apply",
                "claude-wire-sonnet",
            ),
            (
                vec![Reply::Accept, Reply::Reject],
                "previous effort is retained",
                "claude-wire-opus",
            ),
        ] {
            let f = Fixture::start().await;
            f.worker.gate.add_permits(16);
            f.script(&replies);
            let command = prompt("failing", "failing", opus("high"), "normal");
            let error = f.send(command.clone()).await.unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(error.contains("not sent"), "{error}");
            assert!(!error.contains("PRIVATE_METADATA"));
            if model == "claude-wire-opus" {
                assert!(error.contains("switched to the selected model"), "{error}");
            }
            let thread = f.thread().await;
            assert_eq!(thread["modelSelection"]["model"], model);
            assert_eq!(thread["modelSelection"]["options"], json!([]));
            assert!(thread["messages"].as_array().unwrap().is_empty());
            let sent = f.log().len();
            assert_eq!(sent, replies.len());
            assert_eq!(prompts(&f.log()), 0);
            // The same command repeats its outcome; nothing is sent again.
            assert!(f.send(command).await.unwrap_err().contains(expected));
            assert_eq!(f.log().len(), sent);
            if model == "claude-wire-opus" {
                // A new explicit send applies only the unconfirmed effort.
                f.send(prompt("retry", "retry", opus("high"), "normal"))
                    .await
                    .unwrap();
                assert_eq!(
                    controls(&f.log())[2],
                    json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"high"}})
                );
                assert_eq!(controls(&f.log()).len(), 3);
                assert_eq!(prompts(&f.log()), 1);
                assert_eq!(
                    f.idle().await["modelSelection"],
                    json!({"instanceId":"claude-code","model":"claude-wire-opus","options":[{"id":"effort","value":"high"}]})
                );
            }
            f.stop().await;
        }
    }

    #[tokio::test]
    async fn lost_shared_setting_acknowledgement_sends_nothing_and_is_never_replayed() {
        let mut f = Fixture::start().await;
        f.worker.gate.add_permits(16);
        f.script(&[Reply::Drop]);
        let command = prompt("lost", "lost", opus("max"), "normal");
        let error = f.send(command.clone()).await.unwrap_err();
        assert!(error.contains("not confirmed"), "{error}");
        assert!(error.contains("not sent"), "{error}");
        f.connected(2).await;
        for _ in 0..2 {
            let error = f.send(command.clone()).await.unwrap_err();
            assert!(error.contains("not replayed"), "{error}");
            f.restart().await;
        }
        assert_eq!(controls(&f.log()).len(), 1);
        assert_eq!(prompts(&f.log()), 0);
        let thread = f.thread().await;
        assert_eq!(thread["modelSelection"]["model"], "claude-wire-sonnet");
        assert!(thread["messages"].as_array().unwrap().is_empty());
        f.stop().await;
    }

    #[tokio::test]
    async fn queued_prompt_applies_its_own_settings_in_order_and_steer_cannot_change_them() {
        let f = Fixture::start().await;
        f.send(prompt("running", "running", Value::Null, "normal"))
            .await
            .unwrap();
        f.wait_log(1).await;
        assert!(f
            .send(prompt("steer", "steer", opus("high"), "steer"))
            .await
            .unwrap_err()
            .contains("next turn"));
        f.send(prompt("queued-1", "queued-1", opus("low"), "queue"))
            .await
            .unwrap();
        f.send(prompt("queued-2", "queued-2", opus("low"), "queue"))
            .await
            .unwrap();
        // No selection keeps the settings confirmed when it was queued.
        f.send(prompt("queued-3", "queued-3", Value::Null, "queue"))
            .await
            .unwrap();
        assert!(controls(&f.log()).is_empty(), "queueing changes nothing");
        // Each queued prompt starts only after its own settings are confirmed;
        // one that matches the confirmed settings sends no control.
        f.worker.gate.add_permits(32);
        tokio::time::timeout(Duration::from_secs(8), async {
            while prompts(&f.log()) < 4 || f.thread().await["latestTurn"]["state"] != "completed" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the queue did not drain: {:?}",
                f.log()
                    .iter()
                    .map(|v| (v["type"].clone(), v["request"]["subtype"].clone()))
                    .collect::<Vec<_>>()
            )
        });
        let log = f.log();
        let order: Vec<_> = log
            .iter()
            .map(|v| {
                if v["type"] == "control_request" {
                    format!("{}", v["request"])
                } else {
                    s(&v["payload"]["message"]["content"][0]["text"]).to_owned()
                }
            })
            .collect();
        assert_eq!(
            order,
            [
                "PROMPT_running".to_owned(),
                json!({"subtype":"set_model","model":"opus"}).to_string(),
                json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"low"}})
                    .to_string(),
                "PROMPT_queued-1".to_owned(),
                "PROMPT_queued-2".to_owned(),
                json!({"subtype":"set_model","model":"claude-wire-sonnet"}).to_string(),
                json!({"subtype":"apply_flag_settings","settings":{"effortLevel":null}})
                    .to_string(),
                "PROMPT_queued-3".to_owned(),
            ]
        );
        let thread = f.thread().await;
        assert_eq!(
            thread["modelSelection"],
            json!({"instanceId":"claude-code","model":"claude-wire-sonnet","options":[]})
        );
        let users: Vec<_> = thread["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| m["text"].clone())
            .collect();
        assert_eq!(
            users,
            [
                json!("PROMPT_running"),
                json!("PROMPT_queued-1"),
                json!("PROMPT_queued-2"),
                json!("PROMPT_queued-3")
            ]
        );
        f.stop().await;
    }

    #[tokio::test]
    async fn rejected_queued_settings_pause_the_queue_without_sending() {
        let f = Fixture::start().await;
        f.send(prompt("running", "running", Value::Null, "normal"))
            .await
            .unwrap();
        f.wait_log(1).await;
        f.script(&[Reply::Reject]);
        f.send(prompt("queued", "queued", opus("high"), "queue"))
            .await
            .unwrap();
        f.worker.gate.add_permits(16);
        let thread = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let thread = f.thread().await;
                if thread["session"]["lastError"].is_string() {
                    return thread;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the queue did not report the rejected setting");
        let error = s(&thread["session"]["lastError"]);
        assert!(error.contains("previous model is retained"), "{error}");
        assert!(error.contains("queued message was not sent"), "{error}");
        assert_eq!(controls(&f.log()).len(), 1);
        assert_eq!(prompts(&f.log()), 1);
        assert_eq!(thread["modelSelection"]["model"], "claude-wire-sonnet");
        let saved =
            f.db.call(|a| a.store.load_chat(CHAT))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(saved["queue"][0]["message"]["messageId"], "queued");
        assert_eq!(saved["paused"], true);
        f.stop().await;
    }

    #[tokio::test]
    async fn acknowledged_alias_sends_once_without_a_catalog_mapping_but_not_on_a_contradiction() {
        // Without the CLI's mapping, the matching acknowledgement is proof.
        let f = Fixture::with_catalog(false).await;
        f.worker.gate.add_permits(16);
        f.send(prompt("unmapped", "unmapped", opus("high"), "normal"))
            .await
            .unwrap();
        assert_eq!(controls(&f.log()).len(), 2);
        assert_eq!(prompts(&f.log()), 1);
        assert_eq!(
            f.idle().await["modelSelection"]["model"],
            "claude-wire-opus"
        );
        f.stop().await;
        // With the mapping, a worker reporting another model never runs the
        // prompt under settings the user did not select.
        let f = Fixture::start().await;
        f.worker.gate.add_permits(16);
        f.worker
            .wire
            .lock()
            .unwrap()
            .insert("opus".into(), "claude-wire-other".into());
        let error = f
            .send(prompt(
                "contradicted",
                "contradicted",
                opus("high"),
                "normal",
            ))
            .await
            .unwrap_err();
        assert!(error.contains("reported different settings"), "{error}");
        assert!(error.contains("not sent"), "{error}");
        assert_eq!(prompts(&f.log()), 0);
        assert!(f.thread().await["messages"].as_array().unwrap().is_empty());
        f.stop().await;
    }
}
