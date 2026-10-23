//! Hold a new, unaccepted prompt briefly while its existing Claude worker
//! reconnects. No receipt or prompt is written until the normal dispatcher
//! accepts it; cancellation, Stop and identity changes discard the wait.
use super::*;
use std::collections::VecDeque;
const WAIT: Duration = Duration::from_secs(30);
fn bounded(value: &Value) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("pending prompt budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(2 * 1024 * 1024), value).is_ok()
}
pub(super) struct Token {
    started: Instant,
    native: String,
    link: Option<crate::claude_host::Identity>,
}
struct Pending {
    token: Token,
    request: Request,
}
#[derive(Default)]
pub(super) struct Sends {
    waiting: HashMap<String, Pending>,
    ready: VecDeque<Pending>,
}
impl Sends {
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty() && self.ready.is_empty()
    }
    pub fn pop(&mut self) -> Option<(Request, Option<Token>)> {
        self.ready.pop_front().map(|p| (p.request, Some(p.token)))
    }
    pub fn valid(hub: &Hub, r: &Request, token: &Token) -> bool {
        !r.reply.is_closed()
            && token.started.elapsed() < WAIT
            && hub.claude_enabled
            && hub.config.claude.remote_control
            && hub.chats.get(s(&r.params["threadId"])).is_some_and(|c| {
                c.native_id == token.native
                    && c.remote_identity == token.link
                    && c.thread["deletedAt"].is_null()
                    && c.thread["archivedAt"].is_null()
            })
    }
    pub fn cancel_for(&mut self, request: &Request) {
        if request.agent == AgentKind::Claude
            && request.method == "orchestration.dispatchCommand"
            && !s(&request.params["commandId"]).is_empty()
            && s(&request.params["commandId"]).len() <= 128
            && ["thread.turn.interrupt", "thread.session.stop"]
                .contains(&s(&request.params["type"]))
        {
            if let Some(p) = self.waiting.remove(s(&request.params["threadId"])) {
                let _ = p.request.reply.send(Err(
                    "Connection wait cancelled by Stop. Your new prompt was not sent.".into(),
                ));
            }
        }
    }
    pub fn needs_wait(hub: &Hub, r: &Request) -> bool {
        r.agent == AgentKind::Claude
            && r.method == "orchestration.dispatchCommand"
            && r.params["type"] == "thread.turn.start"
            && !["queue", "steer"].contains(&s(&r.params["deliveryMode"]))
            && r.params.get("bootstrap").is_none()
            && r.params.get("promptEdit").is_none()
            && hub
                .chats
                .get(s(&r.params["threadId"]))
                .is_some_and(|c| c.agent == AgentKind::Claude && !c.native_id.is_empty())
            && (hub.recovery_checks.contains_key(s(&r.params["threadId"]))
                || hub
                    .active
                    .get(s(&r.params["threadId"]))
                    .is_some_and(|w| w.recovering && !w.metadata_ready() && !w.stopping))
    }
    pub fn hold(&mut self, hub: &Hub, r: Request, token: Option<Token>) {
        let id = s(&r.params["threadId"]).to_owned();
        let error = if self.waiting.contains_key(&id) {
            Some("This chat already has a prompt waiting for its connection")
        } else if self.waiting.len() >= 4 || !bounded(&r.params) {
            Some("Claude is reconnecting. Wait for Live sync connected before sending this prompt; nothing was sent.")
        } else {
            None
        };
        if let Some(error) = error {
            let _ = r.reply.send(Err(error.into()));
            return;
        }
        let c = &hub.chats[&id];
        self.waiting.insert(
            id,
            Pending {
                token: token.unwrap_or_else(|| Token {
                    started: Instant::now(),
                    native: c.native_id.clone(),
                    link: c.remote_identity.clone(),
                }),
                request: r,
            },
        );
    }
    pub fn refresh(&mut self, hub: &Hub) {
        if !self.ready.is_empty() {
            return;
        }
        let mut ids: Vec<_> = self
            .waiting
            .iter()
            .map(|(id, p)| (id.clone(), p.token.started))
            .collect();
        ids.sort_by_key(|(_, started)| *started);
        for (id, _) in ids {
            let p = &self.waiting[&id];
            let error = if p.request.reply.is_closed() {
                Some("Caller disconnected; prompt was not sent")
            } else if p.token.started.elapsed() >= WAIT {
                Some("Claude is still reconnecting. Your prompt was not sent; wait for Live sync connected and try again.")
            } else if !hub.claude_enabled || !hub.config.claude.remote_control {
                Some("Claude sharing was disabled; prompt was not sent")
            } else if hub.chats.get(&id).is_none_or(|c| {
                c.native_id != p.token.native
                    || c.remote_identity != p.token.link
                    || !c.thread["deletedAt"].is_null()
                    || !c.thread["archivedAt"].is_null()
            }) {
                Some("Claude session changed while reconnecting; prompt was not sent")
            } else {
                None
            };
            if let Some(error) = error {
                let p = self.waiting.remove(&id).unwrap();
                let _ = p.request.reply.send(Err(error.into()));
                continue;
            }
            if hub
                .active
                .get(&id)
                .is_some_and(|w| w.metadata_ready() && !w.stopping)
            {
                self.ready.push_back(self.waiting.remove(&id).unwrap());
                break;
            }
        }
    }
    pub fn shutdown(self) {
        for request in self
            .waiting
            .into_values()
            .chain(self.ready)
            .map(|p| p.request)
        {
            let _ = request.reply.send(Err(
                "Bridge stopped while reconnecting; your prompt was not sent".into(),
            ));
        }
    }
}
