//! Wait for a shared Codex control connection without replaying a prompt or
//! starting a competing turn. Bind every wait to the observed native turn.
use super::*;

fn bounded(value: &Value) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("pending control budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(2 * 1024 * 1024), value).is_ok()
}

pub(super) fn candidate(r: &Request) -> bool {
    r.agent == AgentKind::Codex
        && r.method == "orchestration.dispatchCommand"
        && ((r.params["type"] == "thread.turn.start"
            && r.params.get("bootstrap").is_none()
            && r.params.get("promptEdit").is_none())
            || (r.params["type"] == "thread.user-input.respond"
                && s(&r.params["requestId"]).starts_with("desktop-question:")))
}

struct Waiting {
    at: Instant,
    native: String,
    turn: String,
    request: Request,
}
#[derive(Default)]
pub(super) struct Pending(HashMap<String, Waiting>);
impl Pending {
    pub fn hold(&mut self, hub: &mut Hub, r: Request) {
        let id = s(&r.params["threadId"]).to_owned();
        // The HTTP gateway already bounds input, but retain no more than four
        // requests and two MiB each while the native handshake is pending.
        if self.0.contains_key(&id) || self.0.len() >= 4 || !bounded(&r.params) {
            let _ = r.reply.send(Err(
                "A Codex connection check is already pending. Nothing was sent; retry shortly."
                    .into(),
            ));
            return;
        }
        let Some(w) = hub.active.get(&id) else {
            let _ = r.reply.send(Err(
                "Codex disconnected before delivery; nothing was sent.".into()
            ));
            return;
        };
        hub.tool_controls.insert(id.clone());
        self.0.insert(
            id.clone(),
            Waiting {
                at: Instant::now(),
                native: hub.chats[&id].native_id.clone(),
                turn: w.native_turn.clone(),
                request: r,
            },
        );
    }
    pub fn ready(&mut self, hub: &Hub, restarting: bool) -> Vec<(Request, Result<(), String>)> {
        let mut finished = Vec::new();
        for (id, p) in &self.0 {
            let error = if restarting || p.request.reply.is_closed() {
                Some("Connection wait cancelled; nothing was sent.")
            } else if p.at.elapsed() >= Duration::from_secs(20) {
                Some("Codex is still reconnecting; nothing was sent. Try again shortly.")
            } else if hub.chats.get(id).is_none_or(|c| {
                c.native_id != p.native
                    || !c.thread["archivedAt"].is_null()
                    || !c.thread["deletedAt"].is_null()
            }) || hub
                .active
                .get(id)
                .is_none_or(|w| w.native_turn != p.turn || w.stopping)
            {
                Some("The original Codex task changed while connecting; nothing was sent.")
            } else {
                None
            };
            if let Some(error) = error {
                finished.push((id.clone(), Err(error.into())));
            } else if hub.active.get(id).is_some_and(|w| w.stage == "running") {
                finished.push((id.clone(), Ok(())));
            }
        }
        finished
            .into_iter()
            .map(|(id, result)| (self.0.remove(&id).unwrap().request, result))
            .collect()
    }
    pub fn cancel_for(&mut self, hub: &mut Hub, r: &Request) {
        if r.agent == AgentKind::Codex
            && r.method == "orchestration.dispatchCommand"
            && !s(&r.params["commandId"]).is_empty()
            && s(&r.params["commandId"]).len() <= 128
            && ["thread.turn.interrupt", "thread.session.stop"].contains(&s(&r.params["type"]))
        {
            let id = s(&r.params["threadId"]);
            if let Some(p) = self.0.remove(id) {
                hub.tool_controls.remove(id);
                let _ = p.request.reply.send(Err(
                    "Connection wait cancelled by Stop; nothing was sent.".into(),
                ));
            }
        }
    }
    pub fn shutdown(self) {
        for (_, p) in self.0 {
            let _ = p.request.reply.send(Err(
                "Bridge stopped before delivery; nothing was sent.".into()
            ));
        }
    }
}
