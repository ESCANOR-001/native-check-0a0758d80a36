//! Explicit same-identity reconnect. Never sends a user prompt or native control.
use super::*;

impl Chats {
    pub(super) async fn retry_claude_sync(
        &self,
        agent: AgentKind,
        client: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut changes = self.subscribe();
        let id = s(&params["threadId"]).to_owned();
        let initial = self
            .send_request(agent, client, "native.retryClaudeSync", params)
            .await?;
        if initial["status"] == "connected" {
            return Ok(initial);
        }
        // The UI's RPC deadline is90s. Bound the whole acknowledgement wait and
        // leave a slow handshake running; a timeout is never a successful repair.
        tokio::time::timeout(Duration::from_secs(70), async {
            loop {
                let state = self.send_request(agent, client, "native.claudeSyncStatus", json!({"threadId":id})).await?;
                match s(&state["status"]) {
                    "connected" => return Ok(state),
                    "disconnected" => return Err(s(&state["detail"]).to_owned()),
                    _ => {},
                }
                tokio::select! {
                    changed = changes.changed() => { if changed.is_err() { return Err("Bridge stopped before Claude confirmed the connection".into()); } },
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {},
                }
            }
        }).await.map_err(|_| "Claude has not confirmed the connection yet. History and the original link were kept; retry after checking Status.".to_owned())?
    }
}
impl Hub {
    pub(super) fn sync_retry_status(&self, agent: AgentKind, id: &str) -> Result<Value, String> {
        if agent != AgentKind::Claude {
            return Err("Select a Claude chat".into());
        }
        let chat = self.checked(agent, id)?;
        let worker = self.active.get(id);
        // publish() spills idle activity pages to disk. Readiness must come
        // from the worker's verified handshake, never a retained history row.
        let connected = worker.is_some_and(|w| {
            w.metadata_ready() && (w.observer.is_some() || w.claude_router.is_some())
        }) && chat
            .remote_identity
            .as_ref()
            .is_some_and(|i| i.native_session == chat.native_id);
        let detail = if connected {
            "Live sync connected"
        } else {
            chat.thread["session"]["lastError"].as_str().filter(|s| !s.is_empty()).unwrap_or("Claude is disconnected. Check its sign-in and Remote Control setup, then retry.")
        };
        Ok(
            json!({"threadId":id,"status":if connected {"connected"} else if worker.is_some() {"connecting"} else {"disconnected"},"detail":detail}),
        )
    }
    pub(super) async fn request_sync_retry(
        &mut self,
        agent: AgentKind,
        p: &Value,
    ) -> Result<Value, String> {
        if agent != AgentKind::Claude {
            return Err("Select a Claude chat".into());
        }
        let id = s(&p["threadId"]);
        let command = s(&p["commandId"]);
        if id.is_empty()
            || id.len() > 128
            || command.is_empty()
            || command.len() > 128
            || !p.is_object()
            || p.as_object()
                .unwrap()
                .keys()
                .any(|k| !["threadId", "mode", "commandId"].contains(&k.as_str()))
        {
            return Err("Provide the Claude chat, recovery mode and command ID".into());
        }
        if !self.claude_enabled || !self.config.claude.remote_control {
            return Err("Enable Claude Code and Desktop sharing in this bridge first".into());
        }
        if s(&p["mode"]) != "retry" {
            return Err("This native candidate can retry the original connection. Creating a replacement Desktop link is not migrated yet.".into());
        }
        if !self.chats.contains_key(id) {
            self.connect_external_claude(agent, p).await?;
        }
        let chat = self.checked(agent, id)?;
        if !chat.thread["archivedAt"].is_null() {
            return Err("Open an active saved Claude chat first".into());
        }
        // Existing observers and hosted handshakes are joined, including a
        // repeated Retry. No disconnect, re-key, queue resume or prompt replay.
        if self.active.contains_key(id) {
            return self.sync_retry_status(agent, id);
        }
        if !claude_recoverable(chat) {
            return Err("The original Claude Remote Control link is unavailable. Complete its one-time setup on the PC first.".into());
        }
        if self.tool_controls.contains(id)
            || self.busy_count() >= self.config.max_active_chats
            || self
                .active
                .values()
                .filter(|w| w.claude_router.is_some() || w.observer.is_some())
                .count()
                >= crate::claude_reconnect::HOST_LIMIT
        {
            return Err(
                "Claude reconnect is waiting for an available worker; try again shortly".into(),
            );
        }
        self.db.call(|a| a.store.check_history(0)).await??;
        self.launch_worker(id, true).await?;
        self.sync_retry_status(agent, id)
    }
}
