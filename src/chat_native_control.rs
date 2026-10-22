use super::*;

pub(super) struct Waiter {
    reply: oneshot::Sender<Result<Value, String>>,
    deadline: Instant,
}

pub(super) fn supported(action: &Value) -> bool {
    ["mcpToggle", "mcpReconnect", "reloadSkills", "reloadPlugins"].contains(&s(action))
}
impl Hub {
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
                        reply: r.reply,
                        deadline: Instant::now() + Duration::from_secs(15),
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
    pub(super) fn complete_native_control(&mut self, id: &str) {
        if self
            .active
            .get(id)
            .is_none_or(|w| w.control_reply.is_none())
        {
            return;
        }
        let Some(result) = self.chats[id]
            .native_control
            .as_ref()
            .and_then(|i| i.result.clone())
        else {
            return;
        };
        self.tool_controls.remove(id);
        if let Some(w) = self.active.get_mut(id) {
            w.metadata.capability_cache = None;
            if let Some(reply) = w.control_reply.take() {
                let _ = reply.reply.send(crate::native_control::response(result));
            }
        }
    }
    pub(super) fn expire_native_controls(&mut self) {
        for (id, w) in &mut self.active {
            if w.control_reply
                .as_ref()
                .is_some_and(|waiter| Instant::now() >= waiter.deadline)
            {
                let _ = w
                    .control_reply
                    .take()
                    .unwrap()
                    .reply
                    .send(Err(crate::native_control::UNCERTAIN.into()));
                self.tool_controls.remove(id);
            }
        }
    }
}
pub(super) fn disconnect(w: &mut Worker) {
    if let Some(reply) = w.control_reply.take() {
        let _ = reply
            .reply
            .send(Err(crate::native_control::UNCERTAIN.into()));
    }
}
