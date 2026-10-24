//! Explicit reconnect, or a durable pinned fork for a Remodex-owned expired
//! link. Neither path sends a prompt, resumes a queue, or changes a Desktop owner.
use super::*;

pub(super) struct HistoryCheck {
    native: String,
    recovering: bool,
    pub(super) cancelled: bool,
    pub(super) task: tokio::task::JoinHandle<Result<(), String>>,
}

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
        let check = self.recovery_checks.get(id);
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
        } else if check.is_some_and(|c| c.cancelled) {
            "Connection attempt cancelled. Saved history is intact."
        } else if check.is_some() {
            "Preparing saved Claude history. No prompt has been sent."
        } else {
            chat.thread["session"]["lastError"].as_str().filter(|s| !s.is_empty()).unwrap_or("Claude is disconnected. Check its sign-in and Remote Control setup, then retry.")
        };
        Ok(
            json!({"threadId":id,"status":if connected {"connected"} else if worker.is_some() || check.is_some_and(|c| !c.cancelled) {"connecting"} else {"disconnected"},"detail":detail}),
        )
    }
    pub(super) async fn request_sync_retry(
        &mut self,
        agent: AgentKind,
        client: &str,
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
        let replace = match s(&p["mode"]) {
            "retry" => false,
            "replace" => true,
            _ => return Err("Choose retry or create a replacement Claude link".into()),
        };
        if !self.chats.contains_key(id) {
            self.connect_external_claude(agent, p).await?;
        }
        let chat = self.checked(agent, id)?;
        if !chat.thread["archivedAt"].is_null() {
            return Err("Open an active saved Claude chat first".into());
        }
        if replace {
            self.prepare_sync_replacement(id, client, p).await?;
        }
        // Existing observers and hosted handshakes are joined, including a
        // repeated Retry. No disconnect, re-key, queue resume or prompt replay.
        if self.active.contains_key(id) || self.recovery_checks.contains_key(id) {
            return self.sync_retry_status(agent, id);
        }
        if !claude_recoverable(self.checked(agent, id)?) {
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

    pub(super) fn start_recovery_check(
        &mut self,
        id: &str,
        recovering: bool,
    ) -> Result<bool, String> {
        let chat = self.chats.get(id).ok_or("Claude chat is unavailable")?;
        let Some(intent) = chat.sync_replacement.clone() else {
            return Ok(false);
        };
        if self.active.contains_key(id)
            || self.prepared_recoveries.remove(id).as_deref() == Some(&chat.native_id)
        {
            return Ok(false);
        }
        if self.recovery_checks.contains_key(id) {
            return Ok(true);
        }
        if chat.agent != AgentKind::Claude
            || chat.external_claude
            || chat
                .fork_from
                .as_ref()
                .is_some_and(|f| f.session != intent.session || f.point != intent.point)
        {
            return Err("Claude recovery ownership is inconsistent; history is intact".into());
        }
        if self.recovery_checks.len() >= 2 || self.tool_controls.contains(id) {
            return Err("Claude history recovery is waiting for an available worker".into());
        }
        let profile = self.config.claude.profile.clone();
        let target = chat.native_id.clone();
        let native = target.clone();
        let pending = chat.fork_from.is_some();
        let task = tokio::task::spawn_blocking(move || {
            if pending {
                crate::claude_fork_resume::materialize(&profile, &target, &intent)
            } else if crate::claude_fork_resume::ready(&profile, &target, &intent)? {
                Ok(())
            } else {
                Err("Claude replacement history is missing; no empty session was started".into())
            }
        });
        self.tool_controls.insert(id.into());
        self.recovery_checks.insert(
            id.into(),
            HistoryCheck {
                native,
                recovering,
                cancelled: false,
                task,
            },
        );
        Ok(true)
    }

    pub(super) async fn finish_recovery_checks(&mut self) {
        let done: Vec<_> = self
            .recovery_checks
            .iter()
            .filter(|(_, check)| check.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            let check = self.recovery_checks.remove(&id).unwrap();
            self.tool_controls.remove(&id);
            let result = check
                .task
                .await
                .map_err(|_| "Claude recovery history check stopped".to_owned())
                .and_then(|r| r);
            if check.cancelled {
                continue;
            }
            if self.chats.get(&id).is_none_or(|c| {
                c.native_id != check.native || c.agent != AgentKind::Claude || c.external_claude
            }) {
                continue;
            }
            let result = match result {
                Ok(()) => {
                    let mut chat = self.chats[&id].clone();
                    chat.resumed = true;
                    chat.fork_from = None;
                    let state = serde_json::to_value(&chat).unwrap();
                    match self.db.call(move |a| a.store.save_chat(&state, None)).await {
                        Ok(Ok(())) => {
                            self.chats.insert(id.clone(), chat);
                            self.prepared_recoveries.insert(id.clone(), check.native);
                            let result = self.launch_worker(&id, check.recovering).await;
                            self.prepared_recoveries.remove(&id);
                            result
                        }
                        _ => Err("Cannot save Claude's recovery checkpoint".into()),
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                self.claude_recovery.failed(&id, Instant::now());
                let chat = self.chats.get_mut(&id).unwrap();
                if !check.recovering {
                    finish(chat, "error", Some(&e));
                }
                chat.thread["session"]["lastError"] = json!(e);
                projection::activity(
                    chat,
                    "claude-native-sync",
                    "claude.desktop-sync.updated",
                    "Claude Desktop sync",
                    "info",
                    json!({"status":"disconnected","detail":e}),
                );
                let _ = self.save(&id, None).await;
                self.dirty = true;
            }
        }
    }

    async fn prepare_sync_replacement(
        &mut self,
        id: &str,
        client: &str,
        params: &Value,
    ) -> Result<(), String> {
        let chat = self.checked(AgentKind::Claude, id)?;
        if chat.external_claude {
            return Err("Desktop-owned chats keep their original identity. Retry its existing connection; Remodex will not create a competing session.".into());
        }
        let key = format!("claude-recovery:{client}:{}", s(&params["commandId"]));
        let digest = format!("{:x}", Sha256::digest(params.to_string().as_bytes()));
        let lookup = key.clone();
        if let Some((previous, value)) = self
            .db
            .call(move |a| a.store.tool_action(&lookup))
            .await?
            .map_err(|_| "Claude recovery receipt is unavailable")?
        {
            if previous != digest || value["nativeSessionId"] != self.chats[id].native_id {
                return Err("This recovery command was already used for a different session; nothing was replaced".into());
            }
            return Ok(());
        }
        // A repeated tap with a fresh command ID also joins the same pending
        // replacement; never fork a fork which has not accepted its first turn.
        if self.chats[id].sync_replacement.is_some() && self.chats[id].remote_identity.is_none() {
            let state = serde_json::to_value(&self.chats[id]).unwrap();
            let result = json!({"nativeSessionId":self.chats[id].native_id});
            self.db
                .call(move |a| a.store.save_tool_action(&state, &key, &digest, &result))
                .await?
                .map_err(|_| "Cannot save Claude recovery receipt")?;
            return Ok(());
        }
        let mut chat = self.chats[id].clone();
        if self.sync_retry_status(AgentKind::Claude, id)?["status"] == "connected" {
            let state = serde_json::to_value(&chat).unwrap();
            let result = json!({"nativeSessionId":chat.native_id});
            self.db
                .call(move |a| a.store.save_tool_action(&state, &key, &digest, &result))
                .await?
                .map_err(|_| "Cannot save Claude recovery receipt")?;
            return Ok(());
        }
        if !chat.resumed
            || chat.remote_identity.is_none()
            || chat.fork_from.is_some()
            || chat.thread["latestTurn"]["state"] == "running"
            || chat.thread["session"]["status"] == "running"
            || chat.thread["hasPendingApprovals"] == true
            || chat.thread["hasPendingUserInput"] == true
            || chat.external_stop.is_some()
            || chat
                .native_control
                .as_ref()
                .is_some_and(|c| c.result.is_none())
            || self.tool_controls.contains(id)
        {
            return Err("Finish or stop pending Claude work before replacing its link. History and queued prompts were kept.".into());
        }
        if self.active.contains_key(id) {
            // Until its handshake ends, another client may be sending work
            // which this bridge has not observed. Never kill that worker.
            return Err("Wait for the current Claude connection attempt to finish before replacing its link".into());
        }
        let profile = self.config.claude.profile.clone();
        let source = chat.native_id.clone();
        let native = source.clone();
        let point = tokio::task::spawn_blocking(move || crate::branches::point(&profile, &native))
            .await
            .map_err(|_| "Claude recovery history reader stopped")??;
        let fork = crate::branches::Fork {
            session: source,
            point,
        };
        chat.native_id = uuid::Uuid::new_v4().to_string();
        chat.remote_identity = None;
        chat.resumed = false;
        chat.fork_from = Some(fork.clone());
        chat.sync_replacement = Some(fork);
        chat.pending_message = Value::Null;
        chat.stream_id.clear();
        chat.stream_model.clear();
        chat.paused = true;
        chat.thread["nativeSessionId"] = json!(chat.native_id);
        chat.thread["session"]["lastError"] =
            json!("Connecting a replacement Claude link; saved history is retained");
        chat.thread["session"]["statusConfidence"] = json!("unknown");
        let state = serde_json::to_value(&chat).unwrap();
        let result = json!({"nativeSessionId":chat.native_id});
        self.db
            .call(move |a| a.store.save_tool_action(&state, &key, &digest, &result))
            .await?
            .map_err(|_| "Could not save the recovery identity; the original session is intact")?;
        self.chats.insert(id.into(), chat);
        self.claude_recovery.remember(id, Instant::now());
        self.dirty = true;
        Ok(())
    }
}
