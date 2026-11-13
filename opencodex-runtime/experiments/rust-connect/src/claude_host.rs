//! Persistent, owned Claude Remote Control transport. A result ends a turn,
//! never the process. The caller must durably commit the verified identities
//! before sending a prompt, and must project/commit each received frame.
//!
//! This is for Remodex-owned CLI sessions. It must not launch a competing CLI
//! for a Desktop-owned session; those require the separate remote observer.
use crate::agent::{Connection, Event};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const MAX_PENDING: usize = 64;
const RECENT_RECEIPTS: usize = 256;
const HANDSHAKE_FRAMES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub native_session: String,
    pub bridge_session: String,
}
impl Identity {
    pub fn validate(&self) -> Result<(), String> {
        native_id(&self.native_session)?;
        if !self.bridge_session.starts_with("cse_")
            || !(8..=128).contains(&self.bridge_session.len())
            || !self
                .bridge_session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Claude returned an invalid Remote Control identity".into());
        }
        Ok(())
    }
}
fn native_id(id: &str) -> Result<(), String> {
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err("Claude returned an invalid native message or session identity".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Offered,
    Consumed,
    Uncertain,
}
/// Factual native acknowledgments. A replayed user message is not proof that
/// the model consumed it. An old result never acknowledges a newer send.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub consumed: Vec<String>,
    pub completed: Vec<String>,
    pub cancelled: Vec<String>,
    pub owner_stopped: bool,
    /// A different worker process now owns the session: inputs the previous
    /// process held can never finish and are never resent.
    pub owner_replaced: bool,
}
#[derive(Default)]
pub struct Ledger {
    pending: HashMap<String, Delivery>,
    recent: VecDeque<String>,
}
impl Ledger {
    pub fn offer(&mut self, id: &str) -> Result<(), String> {
        native_id(id)?;
        if self.pending.contains_key(id) || self.recent.iter().any(|v| v == id) {
            return Err("This Claude prompt was already offered; it was not sent again".into());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err("Wait for pending Claude messages before sending more".into());
        }
        self.pending.insert(id.into(), Delivery::Offered);
        Ok(())
    }
    pub fn delivery(&self, id: &str) -> Option<Delivery> {
        self.pending.get(id).copied()
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    fn uncertain(&mut self) {
        for state in self.pending.values_mut() {
            *state = Delivery::Uncertain;
        }
    }
    fn retire(&mut self, id: &str) {
        self.pending.remove(id);
        if !self.recent.iter().any(|v| v == id) {
            self.recent.push_back(id.into());
        }
        while self.recent.len() > RECENT_RECEIPTS {
            self.recent.pop_front();
        }
    }
    pub fn observe(&mut self, frame: &Value) -> Result<Receipt, String> {
        let mut receipt = Receipt::default();
        if !frame["parent_tool_use_id"].is_null() {
            return Ok(receipt);
        }
        if frame["type"] == "system" && frame["subtype"] == "worker_shutting_down" {
            self.uncertain();
            receipt.owner_stopped = true;
            return Ok(receipt);
        }
        if frame["type"] == "command_lifecycle" && frame["state"] == "cancelled" {
            let id = frame["command_uuid"]
                .as_str()
                .ok_or("Missing cancelled message ID")?;
            native_id(id)?;
            self.retire(id);
            receipt.cancelled.push(id.into());
            return Ok(receipt);
        }
        if !["assistant", "stream_event", "result"].contains(&frame["type"].as_str().unwrap_or(""))
        {
            return Ok(receipt);
        }
        let mut ids = Vec::new();
        if let Some(list) = frame.get("user_message_uuids") {
            let list = list
                .as_array()
                .ok_or("Invalid Claude consumption receipt")?;
            if list.len() > MAX_PENDING {
                return Err("Claude consumption receipt exceeds its message budget".into());
            }
            for id in list {
                let id = id.as_str().ok_or("Invalid Claude consumed message ID")?;
                native_id(id)?;
                if !ids.iter().any(|v| v == id) {
                    ids.push(id.to_owned());
                }
            }
        }
        if let Some(id) = frame.get("user_message_uuid") {
            let id = id.as_str().ok_or("Invalid Claude consumed message ID")?;
            native_id(id)?;
            if !ids.is_empty() && !ids.iter().any(|v| v == id) {
                return Err("Claude consumption receipt contains conflicting identities".into());
            }
            if ids.is_empty() {
                ids.push(id.to_owned());
            }
        }
        // Validate the entire receipt before changing any delivery state.
        for id in &ids {
            if let Some(state) = self.pending.get_mut(id) {
                *state = Delivery::Consumed;
            }
        }
        receipt.consumed = ids.clone();
        if frame["type"] == "result" {
            for id in &ids {
                self.retire(id);
            }
            receipt.completed = ids;
        }
        Ok(receipt)
    }
}

pub struct Frame {
    pub value: Value,
    pub receipt: Receipt,
}
async fn setup_control(
    connection: &mut Connection,
    native: &str,
    buffered: &mut VecDeque<Value>,
    bytes: &mut usize,
    request: Value,
) -> Result<Value, String> {
    let id = format!("rm-setup-{}", uuid::Uuid::new_v4());
    connection
        .send(&json!({"type":"control_request","request_id":id,"request":request}))
        .await
        .map_err(|_| "Claude tool settings could not be sent")?;
    while let Some(Event::Message(value)) = connection.events.recv().await {
        validate_session(&value, native)?;
        if value["type"] == "control_response" && value["response"]["request_id"] == id {
            return if value["response"]["subtype"] == "success" {
                Ok(value["response"]["response"].clone())
            } else {
                Err("Claude could not restore this chat's tool settings; Remote Control was not enabled".into())
            };
        }
        *bytes = bytes.saturating_add(value.to_string().len());
        if buffered.len() >= HANDSHAKE_FRAMES || *bytes > crate::agent::MAX_FRAME {
            return Err("Claude tool setup exceeded its history budget".into());
        }
        buffered.push_back(value);
    }
    Err("Claude disconnected before restoring tool settings".into())
}

pub struct HostedSession {
    connection: Connection,
    identity: Identity,
    committed: bool,
    closed: bool,
    buffered: VecDeque<Value>,
    pub ledger: Ledger,
    /// Model/capability metadata from initialize, retained only for the caller.
    pub initialize: Value,
}
impl HostedSession {
    /// The supplied connection must be an isolated, Remodex-owned CLI.
    /// No prompt is sent and no identity is silently replaced on failure.
    pub async fn attach(
        connection: Connection,
        native_session: String,
        prior: Option<Identity>,
        name: &str,
    ) -> Result<Self, String> {
        Self::attach_with_timeout(
            connection,
            native_session,
            prior,
            name,
            Duration::from_secs(30),
        )
        .await
    }
    pub async fn attach_with_timeout(
        connection: Connection,
        native_session: String,
        prior: Option<Identity>,
        name: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::attach_configured(connection, native_session, prior, name, timeout, &[]).await
    }
    pub async fn attach_configured(
        mut connection: Connection,
        native_session: String,
        prior: Option<Identity>,
        name: &str,
        timeout: Duration,
        disabled: &[String],
    ) -> Result<Self, String> {
        let result = async {
            native_id(&native_session)?;
            if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
                return Err("Choose a short Remote Control chat title".into());
            }
            if let Some(prior) = &prior {
                prior.validate()?;
                if prior.native_session != native_session {
                    return Err("Cannot attach a shared link to a different native session".into());
                }
            }
            let init_id = format!("rm-init-{}", uuid::Uuid::new_v4());
            let remote_id = format!("rm-remote-{}", uuid::Uuid::new_v4());
            let mut buffered = VecDeque::new();
            let mut bytes = 0usize;
            connection.send(&json!({"type":"control_request","request_id":init_id,"request":{"subtype":"initialize"}})).await.map_err(|_|"Claude initialization could not be sent")?;
            let mut initialize = None;
            loop {
                let value = match connection.events.recv().await {
                    Some(Event::Message(v)) => v,
                    _ => return Err("Claude disconnected during Remote Control setup".into()),
                };
                validate_session(&value, &native_session)?;
                let response = &value["response"];
                if value["type"] == "control_response" && response["request_id"] == init_id {
                    if initialize.is_some() || response["subtype"] != "success" {
                        return Err("Claude initialization was not confirmed".into());
                    }
                    initialize = Some(response["response"].clone());
                    // Restore per-chat policy before exposing this process to
                    // remote clients, including idle worker recovery.
                    if !disabled.is_empty() {
                        let servers = setup_control(
                            &mut connection,
                            &native_session,
                            &mut buffered,
                            &mut bytes,
                            json!({"subtype":"mcp_status"}),
                        )
                        .await?;
                        let servers = servers["mcpServers"]
                            .as_array()
                            .ok_or("Claude did not report its configured tools")?;
                        for server in disabled {
                            if servers.iter().any(|v| v["name"] == *server) {
                                setup_control(&mut connection, &native_session, &mut buffered, &mut bytes,
                                    json!({"subtype":"mcp_toggle","serverName":server,"enabled":false})).await?;
                            }
                        }
                    }
                    let mut request = json!({"subtype":"remote_control","enabled":true,"name":name,"keep_session_on_exit":true});
                    if let Some(prior) = &prior {
                        request["reattach_session_id"] = json!(prior.bridge_session);
                    }
                    connection.send(&json!({"type":"control_request","request_id":remote_id,"request":request})).await.map_err(|_|"Claude Remote Control setup could not be sent")?;
                } else if value["type"] == "control_response" && response["request_id"] == remote_id
                {
                    if initialize.is_none() || response["subtype"] != "success" {
                        // Do not copy raw errors: these can include auth URLs,
                        // work secrets or account details into a phone notice.
                        return Err("Claude could not connect Remote Control. Check the isolated profile's sign-in and Remote Control access.".into());
                    }
                    let identity = Identity {
                        native_session: native_session.clone(),
                        bridge_session: response["response"]["bridge_session_id"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                    };
                    identity.validate()?;
                    if prior.as_ref().is_some_and(|prior| prior != &identity) {
                        // Only the owned process's newly returned wrong link
                        // is disabled. Never send a prompt to another history.
                        let _ = connection.send(&json!({"type":"control_request","request_id":"rm-disable-wrong-link","request":{"subtype":"remote_control","enabled":false,"keep_session_on_exit":true}})).await;
                        let _ = tokio::time::timeout(Duration::from_secs(2), async {
                            while let Some(Event::Message(v)) = connection.events.recv().await {
                                if v["type"] == "control_response"
                                    && v["response"]["request_id"] == "rm-disable-wrong-link"
                                {
                                    break;
                                }
                            }
                        })
                        .await;
                        return Err("Claude changed the shared-session identity. History was retained and no prompt was sent.".into());
                    }
                    return Ok((identity, initialize.unwrap(), buffered));
                } else {
                    bytes = bytes.saturating_add(value.to_string().len());
                    if buffered.len() >= HANDSHAKE_FRAMES || bytes > crate::agent::MAX_FRAME {
                        return Err("Claude sent too much history during connection setup; no prompt was sent".into());
                    }
                    buffered.push_back(value);
                }
            }
        };
        match tokio::time::timeout(timeout, result).await {
            Ok(Ok((identity, initialize, buffered))) => Ok(Self {
                connection,
                identity,
                initialize,
                buffered,
                committed: false,
                closed: false,
                ledger: Ledger::default(),
            }),
            result => {
                connection.close().await;
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "Claude Remote Control connection timed out; no prompt was sent".into(),
                })
            }
        }
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    /// Call only after the caller's durable identity transaction succeeds.
    pub fn confirm_persisted(&mut self, identity: &Identity) -> Result<(), String> {
        if identity != &self.identity {
            return Err("The persisted Claude session does not match its live connection".into());
        }
        self.committed = true;
        Ok(())
    }
    pub async fn send_prompt(
        &mut self,
        id: &str,
        content: Value,
        steer: bool,
    ) -> Result<(), String> {
        if self.closed || !self.committed {
            return Err("Claude's shared-session identity must be saved before sending".into());
        }
        if !content.is_string() && !content.is_array() {
            return Err("Invalid Claude prompt content".into());
        }
        let mut value = json!({"type":"user","uuid":id,"session_id":self.identity.native_session,"parent_tool_use_id":null,"origin":{"kind":"human"},"message":{"role":"user","content":content}});
        if steer {
            value["priority"] = json!("now");
        }
        if value.to_string().len() > crate::agent::MAX_INPUT_FRAME {
            return Err("Claude prompt exceeds the transport limit".into());
        }
        self.ledger.offer(id)?;
        if self.connection.send(&value).await.is_err() {
            self.closed = true;
            self.ledger.uncertain();
            return Err("Claude prompt delivery is uncertain. It was not retried.".into());
        }
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Option<Frame>, String> {
        if self.closed {
            return Ok(None);
        }
        let value = if let Some(value) = self.buffered.pop_front() {
            value
        } else {
            match self.connection.events.recv().await {
                Some(Event::Message(value)) => value,
                _ => {
                    self.closed = true;
                    self.ledger.uncertain();
                    return Ok(None);
                }
            }
        };
        let receipt = match validate_session(&value, &self.identity.native_session)
            .and_then(|()| self.ledger.observe(&value))
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.closed = true;
                self.ledger.uncertain();
                return Err(error);
            }
        };
        Ok(Some(Frame { value, receipt }))
    }
    /// Replies/controls originate in the existing policy/approval layer. This
    /// method cannot be used as a second path for untracked prompt delivery.
    pub async fn send_control(&self, value: &Value) -> Result<(), String> {
        if self.closed
            || !self.committed
            || ![
                "control_request",
                "control_response",
                "control_cancel_request",
            ]
            .contains(&value["type"].as_str().unwrap_or(""))
            || value["request"]["subtype"] == "remote_control"
        {
            return Err("Invalid control for this Claude connection".into());
        }
        self.connection
            .send(value)
            .await
            .map_err(|_| "Claude control delivery is uncertain; it was not retried".into())
    }
    /// Explicit owner shutdown. Completing a turn never calls this method.
    pub async fn close(self) {
        self.connection.close().await;
    }
}
fn validate_session(frame: &Value, expected: &str) -> Result<(), String> {
    if frame.get("session_id").is_some_and(|id| id != expected) {
        return Err("Claude emitted a frame for a different native session".into());
    }
    Ok(())
}
