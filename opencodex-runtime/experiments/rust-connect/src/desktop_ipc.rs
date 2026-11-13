//! Codex Desktop's original-owner transport. This client never launches a CLI,
//! opens a Desktop window, claims a chat, or retries a possibly delivered write.
use serde_json::{json, Value};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[path = "desktop_frame.rs"]
mod desktop_frame;
#[path = "desktop_phase.rs"]
mod desktop_phase;
#[path = "desktop_requests.rs"]
mod desktop_requests;
#[cfg(windows)]
#[path = "desktop_windows.rs"]
mod desktop_windows;

const MAX_FRAME: usize = 8 * 1024 * 1024;
// Large image-heavy snapshots are spooled, never allocated as one heap buffer.
const MAX_SNAPSHOT_FRAME: usize = 256 * 1024 * 1024;
static SNAPSHOT_READER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
const TIMEOUT: Duration = Duration::from_secs(12);
const UNAVAILABLE: &str = "The original Codex Desktop connection is unavailable; history is intact and no replacement CLI was started";
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub path: PathBuf,
}
impl Endpoint {
    /// Experiments bind a caller-selected endpoint inside the isolated profile.
    /// Normal Desktop endpoint discovery is deliberately separate from transport.
    pub fn validate(&self, profile: &Path) -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if !self.path.is_absolute()
                || self.path.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(UNAVAILABLE.into());
            }
            let root = profile.canonicalize().map_err(|_| UNAVAILABLE)?;
            let mut ancestor = self.path.parent().ok_or(UNAVAILABLE)?;
            let parent = loop {
                match ancestor.canonicalize() {
                    Ok(path) => break path,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        ancestor = ancestor.parent().ok_or(UNAVAILABLE)?;
                    }
                    Err(_) => return Err(UNAVAILABLE.into()),
                }
            };
            if !parent.starts_with(root) {
                return Err(UNAVAILABLE.into());
            }
            // A saved connection may be offline. Configuration validation must
            // still allow the bridge to show history; only connect() establishes
            // live authority, without starting or resuming a replacement owner.
            let meta = match std::fs::symlink_metadata(&self.path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(_) => return Err(UNAVAILABLE.into()),
            };
            if !meta.file_type().is_socket()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o077 != 0
            {
                return Err(UNAVAILABLE.into());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            use sha2::{Digest, Sha256};
            let root = profile.canonicalize().map_err(|_| UNAVAILABLE)?;
            let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
            let expected = format!(r"\\.\pipe\remodex-desktop-test-{hash}");
            if self.path.as_path() != Path::new(&expected)
                && !desktop_windows::standard_endpoint(&self.path, &root)
            {
                return Err(UNAVAILABLE.into());
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = profile;
            Err(UNAVAILABLE.into())
        }
    }
    pub async fn connect(&self, profile: &Path) -> Result<Client, String> {
        self.validate(profile)?;
        #[cfg(unix)]
        std::fs::symlink_metadata(&self.path).map_err(|_| UNAVAILABLE)?;
        #[cfg(unix)]
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::UnixStream::connect(&self.path),
        )
        .await
        .map_err(|_| UNAVAILABLE)?
        .map_err(|_| UNAVAILABLE)?;
        #[cfg(unix)]
        {
            // Check the connected peer as well as the socket pathname. A
            // pathname check alone is insufficient if it changes at connect.
            self.validate(profile)?;
            std::fs::symlink_metadata(&self.path).map_err(|_| UNAVAILABLE)?;
            if stream.peer_cred().map_err(|_| UNAVAILABLE)?.uid() != unsafe { libc::geteuid() } {
                return Err(UNAVAILABLE.into());
            }
        }
        #[cfg(windows)]
        let stream = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&self.path)
            .map_err(|_| UNAVAILABLE)?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // Authenticate the connected pipe before sending any IPC frame.
            // The global pipe name alone does not identify its process owner.
            desktop_windows::verify_peer(stream.as_raw_handle()).map_err(|_| UNAVAILABLE)?;
        }
        #[cfg(any(unix, windows))]
        {
            Client::initialize(Box::new(stream), TIMEOUT).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(UNAVAILABLE.into())
        }
    }
}

// Desktop broadcasts ownership discovery to every connected client and waits
// for all negative replies before declaring a dormant chat ownerless. Followers
// must decline even while idle or awaiting CLI/disk work; ignoring discovery
// stalls other clients' sends until the router's ten-second timeout.
type SharedWriter =
    std::sync::Arc<tokio::sync::Mutex<Option<tokio::io::WriteHalf<Box<dyn Stream>>>>>;
struct Transport {
    writer: SharedWriter,
    events: tokio::sync::mpsc::Receiver<(Value, tokio::sync::OwnedSemaphorePermit)>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Transport {
    fn new(stream: Box<dyn Stream>) -> Self {
        let (mut reader, writer) = tokio::io::split(stream);
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(Some(writer)));
        let replies = writer.clone();
        let (output, events) = tokio::sync::mpsc::channel(64);
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(16 * 1024 * 1024));
        let task = tokio::spawn(async move {
            while let Ok(value) = read_frame(&mut reader).await {
                if value["type"] == "client-discovery-request" {
                    let Ok(id) = text(&value["requestId"], 256) else {
                        break;
                    };
                    let response = json!({"type":"client-discovery-response","requestId":id,"response":{"canHandle":false}});
                    if !matches!(
                        tokio::time::timeout(Duration::from_secs(2), async {
                            Self::send(&replies, &response).await
                        })
                        .await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                    continue;
                }
                // Bound both event count and retained JSON bytes. A slow consumer
                // disconnects safely instead of blocking discovery indefinitely.
                // Account for JSON without copying large message/image strings.
                struct Size(usize);
                impl io::Write for Size {
                    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                        self.0 = self.0.saturating_add(bytes.len());
                        Ok(bytes.len())
                    }
                    fn flush(&mut self) -> io::Result<()> {
                        Ok(())
                    }
                }
                let mut size = Size(0);
                if serde_json::to_writer(&mut size, &value).is_err() {
                    break;
                }
                let Ok(size) = u32::try_from(size.0.max(1)) else {
                    break;
                };
                let Ok(permit) = budget.clone().try_acquire_many_owned(size) else {
                    break;
                };
                if output.try_send((value, permit)).is_err() {
                    break;
                }
            }
            // Also close the write side: otherwise the router could keep a dead
            // follower registered and wait for its discovery replies again.
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                let writer = replies.lock().await.take();
                if let Some(mut writer) = writer {
                    let _ = writer.shutdown().await;
                }
            })
            .await;
        });
        Self {
            writer,
            events,
            task,
        }
    }
    async fn send(writer: &SharedWriter, value: &Value) -> io::Result<()> {
        let mut slot = writer.lock().await;
        let stream = slot.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Desktop connection closed")
        })?;
        write_frame(stream, value).await
    }
    async fn write(&mut self, value: &Value) -> io::Result<()> {
        Self::send(&self.writer, value).await
    }
    async fn read(&mut self) -> io::Result<Value> {
        self.events
            .recv()
            .await
            .map(|(value, _permit)| value)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "Desktop connection closed")
            })
    }
}

pub struct Client {
    transport: Transport,
    client: String,
    generation: String,
    timeout: Duration,
    poisoned: bool,
    followed: Option<Owner>,
    phase: desktop_phase::Phase,
    requests: desktop_requests::Requests,
}
#[derive(Clone)]
pub struct Owner {
    thread: String,
    client: String,
    connection: String,
}

#[cfg(test)]
#[path = "desktop_snapshot_tests.rs"]
mod snapshot_tests;
fn text(value: &Value, limit: usize) -> Result<&str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .ok_or_else(|| UNAVAILABLE.into())
}
async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Value> {
    let size = stream.read_u32_le().await? as usize;
    if size == 0 || size > MAX_SNAPSHOT_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid IPC frame",
        ));
    }
    if size > MAX_FRAME {
        let _permit = SNAPSHOT_READER.acquire().await.map_err(io::Error::other)?;
        let temporary = tempfile::tempfile()?;
        let mut file = tokio::fs::File::from_std(temporary);
        let copied = tokio::io::copy(&mut (&mut *stream).take(size as u64), &mut file).await?;
        if copied != size as u64 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete Desktop snapshot",
            ));
        }
        file.flush().await?;
        let file = file.into_std().await;
        return tokio::task::spawn_blocking(move || {
            let _permit = _permit;
            // SAFETY: this anonymous private file has no other writer or path.
            // Its complete, bounded contents stay immutable until the map drops.
            let mapped = unsafe { memmap2::MmapOptions::new().map(&file)? };
            desktop_frame::reduce(&mapped)
        })
        .await
        .map_err(|_| io::Error::other("Desktop snapshot decoding interrupted"))?;
    }
    let mut frame = vec![0; size];
    stream.read_exact(&mut frame).await?;
    desktop_frame::decode(&frame)
}
async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC input exceeds the safe limit",
        ));
    }
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}
impl Client {
    /// Invalidate only Desktop's task-list queries. This is a cache notification,
    /// not an archive mutation, route activation, or claim of session ownership.
    pub(crate) async fn invalidate_tasks(&mut self) -> Result<(), String> {
        if self.poisoned {
            return Err(UNAVAILABLE.into());
        }
        let frame = json!({"type":"broadcast","sourceClientId":self.client,
            "method":"query-cache-invalidate","version":0,
            "params":{"queryKey":["tasks"],"reset":false}});
        let result =
            tokio::time::timeout(Duration::from_secs(2), self.transport.write(&frame)).await;
        if !matches!(result, Ok(Ok(()))) {
            self.poisoned = true;
            return Err(UNAVAILABLE.into());
        }
        Ok(())
    }

    async fn initialize(stream: Box<dyn Stream>, timeout: Duration) -> Result<Self, String> {
        let mut client = Self {
            transport: Transport::new(stream),
            client: "initializing-client".into(),
            generation: uuid::Uuid::new_v4().to_string(),
            timeout,
            poisoned: false,
            followed: None,
            phase: Default::default(),
            requests: Default::default(),
        };
        let result = client
            .exchange(
                "initialize",
                1,
                json!({"clientType":"opencodex-android-bridge"}),
                None,
                false,
            )
            .await?;
        client.client = text(&result["clientId"], 256)?.into();
        Ok(client)
    }
    async fn exchange(
        &mut self,
        method: &str,
        version: u32,
        params: Value,
        target: Option<&str>,
        discovery: bool,
    ) -> Result<Value, String> {
        if self.poisoned {
            return Err(UNAVAILABLE.into());
        }
        let request = uuid::Uuid::new_v4().to_string();
        let mut envelope = json!({"type":"request","requestId":request,"sourceClientId":self.client,"method":method,"version":version,"params":params});
        if let Some(target) = target {
            envelope["targetClientId"] = json!(target);
        }
        // A timeout includes the write itself. Once it expires the connection is
        // unusable, even if a late response subsequently arrives.
        let result = tokio::time::timeout(self.timeout, async {
            self.transport
                .write(&envelope)
                .await
                .map_err(|_| UNAVAILABLE)?;
            for _ in 0..256 {
                let response = self.transport.read().await.map_err(|_| UNAVAILABLE)?;
                self.observe_phase(&response)?;
                if response["requestId"] != request {
                    // The transport declines discovery independently. Unrelated
                    // responses never grant control of another chat.
                    continue;
                }
                if response.get("method").is_some_and(|v| v != method) {
                    return Err(UNAVAILABLE.to_owned());
                }
                if discovery && response["type"] == "client-discovery-response" {
                    if response["response"]["canHandle"] != true {
                        return Err(UNAVAILABLE.to_owned());
                    }
                    let owner = response["response"]
                        .get("handledByClientId")
                        .unwrap_or(&response["handledByClientId"]);
                    return Ok(json!({"owner":text(owner,256)?}));
                }
                if discovery
                    && response["type"] == "response"
                    && response["resultType"] == "error"
                    && response["error"] == "no-client-found"
                {
                    return Ok(Value::Null);
                }
                if response["type"] != "response" || response["resultType"] != "success" {
                    // Errors are deliberately not interpreted as permission to
                    // replay or to take ownership. Reconnect is a fresh read.
                    return Err(UNAVAILABLE.to_owned());
                }
                if discovery {
                    let owner = response
                        .get("handledByClientId")
                        .unwrap_or(&response["result"]["handledByClientId"]);
                    return Ok(json!({"owner":text(owner,256)?}));
                }
                if let Some(target) = target {
                    if response["handledByClientId"] != target {
                        return Err(UNAVAILABLE.to_owned());
                    }
                }
                return response
                    .get("result")
                    .cloned()
                    .ok_or_else(|| UNAVAILABLE.to_owned());
            }
            Err(UNAVAILABLE.to_owned())
        })
        .await
        .unwrap_or_else(|_| Err(UNAVAILABLE.into()));
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    pub async fn owner(&mut self, thread: &str) -> Result<Owner, String> {
        self.find_owner(thread)
            .await?
            .ok_or_else(|| UNAVAILABLE.into())
    }
    /// Only an explicit router no-client-found is absent. Timeout, malformed
    /// replies and connection loss remain errors and cannot authorize activation.
    pub async fn find_owner(&mut self, thread: &str) -> Result<Option<Owner>, String> {
        if uuid::Uuid::parse_str(thread).is_err() {
            return Err(UNAVAILABLE.into());
        }
        let result = self
            .exchange(
                "thread-owner-discovery",
                1,
                json!({"hostId":"local","conversationId":thread}),
                None,
                true,
            )
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        let owner = text(&result["owner"], 256)?;
        if owner == self.client {
            return Err(UNAVAILABLE.into());
        }
        Ok(Some(Owner {
            thread: thread.into(),
            client: owner.into(),
            connection: self.generation.clone(),
        }))
    }
    fn observe_phase(&mut self, value: &Value) -> Result<(), String> {
        let Some(owner) = &self.followed else {
            return Ok(());
        };
        let p = &value["params"];
        if value["type"] == "broadcast"
            && value["method"] == "thread-stream-state-changed"
            && value["sourceClientId"] == owner.client
            && p["conversationId"] == owner.thread
            && p["hostId"] == "local"
        {
            self.phase.apply(&p["change"], &owner.thread)?;
            self.requests.apply(&p["change"], &owner.thread)?;
        }
        Ok(())
    }
    /// Observe only the discovered owner's live phase. Frames are bounded before
    /// decoding; the thin projection drops all message and tool content.
    pub async fn compacting(
        &mut self,
        owner: &Owner,
        current_turn: &str,
    ) -> Result<Option<(String, String)>, String> {
        self.follow(owner).await?;
        Ok(self.phase.compacting(current_turn))
    }
    pub async fn question_replies(&mut self, owner: &Owner) -> Result<Vec<Value>, String> {
        self.follow(owner).await?;
        Ok(self.phase.question_replies())
    }
    pub async fn questions(&mut self, owner: &Owner, turn: &str) -> Result<Vec<Value>, String> {
        self.follow(owner).await?;
        Ok(self.phase.questions(turn))
    }
    pub async fn task_status(
        &mut self,
        owner: &Owner,
        current_turn: &str,
    ) -> Result<Option<(String, Option<Value>)>, String> {
        self.follow(owner).await?;
        Ok(self
            .phase
            .task_status(current_turn)
            .map(|status| (status, self.phase.task_error(current_turn))))
    }
    async fn follow(&mut self, owner: &Owner) -> Result<(), String> {
        if self.poisoned || owner.connection != self.generation {
            return Err(UNAVAILABLE.into());
        }
        if self
            .followed
            .as_ref()
            .is_some_and(|old| old.thread != owner.thread || old.client != owner.client)
        {
            self.poisoned = true;
            return Err(UNAVAILABLE.into());
        }
        if self.followed.is_none() {
            self.followed = Some(owner.clone());
            let result = tokio::time::timeout(self.timeout, async {
                self.transport
                    .write(&json!({"type":"broadcast","sourceClientId":self.client,
                    "method":"thread-stream-following-changed","version":1,
                    "params":{"conversationId":owner.thread,"hostId":"local","following":true}}))
                    .await
                    .map_err(|_| UNAVAILABLE)?;
                for _ in 0..256 {
                    let value = self.transport.read().await.map_err(|_| UNAVAILABLE)?;
                    self.observe_phase(&value)?;
                    if self.phase.ready() {
                        return Ok(());
                    }
                }
                Err(UNAVAILABLE.to_owned())
            })
            .await
            .unwrap_or_else(|_| Err(UNAVAILABLE.into()));
            if result.is_err() {
                self.poisoned = true;
            }
            result?;
        } else {
            // A harmless owner probe drains preceding status broadcasts and
            // verifies that cached state still belongs to this connection.
            let current = self.owner(&owner.thread).await?;
            if current.client != owner.client {
                return Err(UNAVAILABLE.into());
            }
        }
        Ok(())
    }
    pub async fn pending_requests(&mut self, owner: &Owner) -> Result<Vec<Value>, String> {
        self.follow(owner).await?;
        Ok(self.requests.rows().to_vec())
    }
    pub async fn respond(&mut self, owner: &Owner, response: &Value) -> Result<(), String> {
        // Refresh before a decision: Desktop may have answered or cancelled it.
        self.follow(owner).await?;
        let (method, params) = self.requests.response(response)?;
        let result = self.action(owner, method, 1, params).await?;
        if result["ok"] != true {
            self.poisoned = true;
            return Err(
                "Desktop did not acknowledge the control response; it was not retried".into(),
            );
        }
        Ok(())
    }
    async fn action(
        &mut self,
        owner: &Owner,
        method: &str,
        version: u32,
        mut params: Value,
    ) -> Result<Value, String> {
        if owner.connection != self.generation || !params.is_object() {
            return Err(UNAVAILABLE.into());
        }
        params["hostId"] = json!("local");
        params["conversationId"] = json!(owner.thread);
        self.exchange(method, version, params, Some(&owner.client), false)
            .await
    }
    pub async fn history(
        &mut self,
        owner: &Owner,
        direction: &str,
        token: Option<&str>,
    ) -> Result<Value, String> {
        if !matches!(direction, "recent" | "older" | "newer")
            || token.is_some_and(|s| s.is_empty() || s.len() > 8192)
        {
            return Err(UNAVAILABLE.into());
        }
        let value = self
            .action(
                owner,
                "thread-follower-load-history-page",
                1,
                json!({"direction":direction,"pageToken":token}),
            )
            .await?;
        let state = value
            .get("conversationState")
            .or_else(|| value.get("conversation_state"))
            .or_else(|| value.get("state"))
            .ok_or(UNAVAILABLE)?;
        if state["id"] != owner.thread
            || state.get("hostId").is_some_and(|v| v != "local")
            || !state["turns"].as_array().is_some_and(|a| a.len() <= 512)
        {
            self.poisoned = true;
            return Err(UNAVAILABLE.into());
        }
        Ok(value)
    }
    pub async fn content(
        &mut self,
        owner: &Owner,
        handle: &str,
        revision: &str,
        offset: u64,
    ) -> Result<Value, String> {
        text(&json!(handle), 256)?;
        text(&json!(revision), 256)?;
        self.action(
            owner,
            "thread-follower-read-content-chunk",
            1,
            json!({"handle":handle,"sourceRevision":revision,"offset":offset}),
        )
        .await
    }
    pub async fn start(
        &mut self,
        owner: &Owner,
        mut request: Value,
        command: &str,
    ) -> Result<Value, String> {
        if !request.is_object()
            || uuid::Uuid::parse_str(command).is_err()
            || !request["input"]
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.len() <= 128)
        {
            return Err(UNAVAILABLE.into());
        }
        normalize_text_input(&mut request)?;
        request["threadId"] = json!(owner.thread);
        let result=self.action(owner,"thread-follower-start-turn",2,json!({"commandId":command,"turnStart":{"request":request,"context":{"inheritThreadSettings":true}}})).await?;
        let turn = result.get("result").unwrap_or(&result).get("turn");
        if !turn.is_some_and(|turn| {
            text(&turn["id"], 256).is_ok()
                && matches!(
                    turn["status"].as_str(),
                    Some("inProgress" | "completed" | "failed" | "interrupted")
                )
                && turn.get("threadId").is_none_or(|id| id == &owner.thread)
        }) {
            self.poisoned = true;
            return Err("Desktop did not confirm the new turn; the prompt was not replayed".into());
        }
        Ok(result)
    }
    pub async fn steer(
        &mut self,
        owner: &Owner,
        mut request: Value,
        command: &str,
    ) -> Result<Value, String> {
        if !request.is_object()
            || uuid::Uuid::parse_str(command).is_err()
            || text(&request["expectedTurnId"], 256).is_err()
            || !request["input"]
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.len() <= 128)
        {
            return Err(UNAVAILABLE.into());
        }
        normalize_text_input(&mut request)?;
        let expected = request["expectedTurnId"].clone();
        // Desktop consumes these fields directly before forwarding turn/steer.
        // Its optimistic bubble requires restoreMessage.context even when the
        // input has already been uploaded. An empty workspaceRoots array lets
        // the original owner resolve its current cwd without changing it.
        let message_id = request
            .get("clientUserMessageId")
            .filter(|value| text(value, 256).is_ok())
            .cloned()
            .unwrap_or_else(|| json!(command));
        let prompt = request["input"]
            .as_array()
            .ok_or(UNAVAILABLE)?
            .iter()
            .filter(|item| item["type"] == "text")
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut params = json!({
            "commandId": command,
            "expectedTurnId": expected,
            "input": request["input"],
            "clientUserMessageId": message_id,
            "attachments": [],
            "restoreMessage": {
                "id": message_id,
                "text": prompt,
                "createdAt": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| UNAVAILABLE)?.as_millis() as u64,
                "context": {
                    "prompt": prompt, "addedFiles": [], "fileAttachments": [],
                    "ideContext": null, "imageAttachments": [],
                    "workspaceRoots": [], "collaborationMode": null
                }
            }
        });
        for key in ["serviceTier", "additionalContext"] {
            if let Some(value) = request.get(key) {
                params[key] = value.clone();
            }
        }
        let result = self
            .action(owner, "thread-follower-steer-turn", 1, params)
            .await?;
        let result = result.get("result").unwrap_or(&result).clone();
        if result["turnId"] != expected {
            self.poisoned = true;
            return Err(
                "Desktop has not confirmed this steering message; it was not replayed".into(),
            );
        }
        Ok(result)
    }
    pub async fn compact(&mut self, owner: &Owner) -> Result<Value, String> {
        // Same follower operation as the native app; never start a competing CLI.
        let result = self
            .action(owner, "thread-follower-compact-thread", 1, json!({}))
            .await?;
        let result = result.get("result").unwrap_or(&result);
        if !result.is_object() {
            return Err(
                "Codex did not confirm compaction. Check this chat before retrying.".into(),
            );
        }
        Ok(json!({}))
    }
    pub async fn stop(&mut self, owner: &Owner, turn: &str) -> Result<Value, String> {
        text(&json!(turn), 256)?;
        let result = self
            .action(
                owner,
                "thread-follower-interrupt-turn",
                4,
                json!({"turnId":turn,"expectedTurnId":turn}),
            )
            .await?;
        if result["interruptedTurnId"] != turn {
            self.poisoned = true;
            return Err("Desktop has not confirmed that this turn stopped".into());
        }
        Ok(result)
    }
}

// Desktop renders the pending input before app-server normalizes defaults.
// Plain text must carry the same explicit element array as its own composer;
// otherwise its continuation/mention renderer can throw while the turn runs.
fn normalize_text_input(request: &mut Value) -> Result<(), String> {
    let input = request["input"].as_array_mut().ok_or(UNAVAILABLE)?;
    for item in input {
        if item["type"] == "text" {
            if item.get("text_elements").is_none() {
                item["text_elements"] = json!([]);
            } else if !item["text_elements"].is_array() {
                return Err(UNAVAILABLE.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    const THREAD: &str = "12345678-1234-4234-8234-123456789abc";
    type Log = Arc<Mutex<Vec<Value>>>;
    pub(super) async fn fixture(change: fn(&Value, Value) -> Value) -> (Client, Log) {
        let (a, b) = tokio::io::duplex(65536);
        let log = Arc::new(Mutex::new(vec![]));
        let received = log.clone();
        tokio::spawn(async move {
            let mut stream: Box<dyn Stream> = Box::new(b);
            while let Ok(v) = read_frame(&mut stream).await {
                received.lock().unwrap().push(v.clone());
                let result = match v["method"].as_str().unwrap_or_default() {
                    "initialize" => json!({"clientId":"follower"}),
                    "thread-owner-discovery" => json!({"handledByClientId":"original-owner"}),
                    "thread-follower-load-history-page" => {
                        json!({"revision":7,"conversationState":{"id":THREAD,"hostId":"local","turns":[{"id":"t1","turnId":"t1","status":"completed","items":[]}]},"historyPage":{"olderCursor":null,"newerCursor":null}})
                    }
                    "thread-follower-start-turn" => {
                        json!({"result":{"turn":{"id":"t2","status":"inProgress"}}})
                    }
                    "thread-follower-steer-turn" => {
                        let params = &v["params"];
                        assert!(params.get("turnSteerParams").is_none());
                        assert!(params["input"].is_array());
                        assert_eq!(
                            params["restoreMessage"]["id"],
                            params["clientUserMessageId"]
                        );
                        assert!(params["restoreMessage"]["context"]["workspaceRoots"].is_array());
                        json!({"result":{"turnId":"t2"}})
                    }
                    "thread-follower-interrupt-turn" => json!({"ok":true,"interruptedTurnId":"t2"}),
                    "thread-follower-compact-thread" => json!({"result":{}}),
                    _ => panic!("unexpected request"),
                };
                let response = json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","handledByClientId":"original-owner","result":result});
                if write_frame(&mut stream, &change(&v, response))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        (
            Client::initialize(Box::new(a), Duration::from_millis(300))
                .await
                .unwrap(),
            log,
        )
    }
    #[tokio::test]
    async fn compact_targets_original_owner_once_without_a_new_turn() {
        let (mut client, log) = fixture(|_, v| v).await;
        let owner = client.owner(THREAD).await.unwrap();
        assert_eq!(client.compact(&owner).await.unwrap(), json!({}));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[2]["method"], "thread-follower-compact-thread");
        assert_eq!(log[2]["version"], 1);
        assert_eq!(log[2]["targetClientId"], "original-owner");
        assert_eq!(
            log[2]["params"],
            json!({"conversationId":THREAD,"hostId":"local"})
        );
    }

    #[tokio::test]
    async fn idle_follower_declines_discovery_without_claiming_any_chat() {
        let (a, b) = tokio::io::duplex(65536);
        let router = tokio::spawn(async move {
            let mut stream: Box<dyn Stream> = Box::new(b);
            let init = read_frame(&mut stream).await.unwrap();
            write_frame(&mut stream, &json!({"type":"response","requestId":init["requestId"],"resultType":"success","result":{"clientId":"follower"}})).await.unwrap();
            // No RPC is outstanding and the follower has not subscribed to a
            // turn. Desktop still requires negative discovery responses.
            for method in [
                "thread-owner-discovery",
                "thread-follower-start-turn",
                "unknown-method",
            ] {
                let id = uuid::Uuid::new_v4().to_string();
                write_frame(&mut stream, &json!({"type":"client-discovery-request","requestId":id,"request":{"method":method,"params":{"conversationId":THREAD}}})).await.unwrap();
                let reply = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut stream))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    reply,
                    json!({"type":"client-discovery-response","requestId":id,"response":{"canHandle":false}})
                );
            }
        });
        let client = Client::initialize(Box::new(a), Duration::from_secs(2))
            .await
            .unwrap();
        router.await.unwrap();
        drop(client);
    }

    #[tokio::test]
    async fn discovery_is_declined_while_an_owner_rpc_is_waiting() {
        let (a, b) = tokio::io::duplex(65536);
        let router = tokio::spawn(async move {
            let mut stream: Box<dyn Stream> = Box::new(b);
            let init = read_frame(&mut stream).await.unwrap();
            write_frame(&mut stream, &json!({"type":"response","requestId":init["requestId"],"resultType":"success","result":{"clientId":"follower"}})).await.unwrap();
            let pending = read_frame(&mut stream).await.unwrap();
            write_frame(&mut stream, &json!({"type":"client-discovery-request","requestId":"other-phone-discovery","request":{"method":"thread-owner-discovery"}})).await.unwrap();
            let decline = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut stream))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(decline["response"]["canHandle"], false);
            assert_eq!(decline["requestId"], "other-phone-discovery");
            // The separate RPC's correlation and absence semantics survive.
            write_frame(&mut stream, &json!({"type":"response","requestId":pending["requestId"],"resultType":"error","error":"no-client-found"})).await.unwrap();
        });
        let mut client = Client::initialize(Box::new(a), Duration::from_secs(2))
            .await
            .unwrap();
        assert!(client.find_owner(THREAD).await.unwrap().is_none());
        router.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_follower_closes_on_count_or_byte_budget_instead_of_blocking_discovery() {
        for body in [String::new(), "x".repeat(2 * 1024 * 1024)] {
            let (a, b) = tokio::io::duplex(65536);
            let transport = Transport::new(Box::new(a));
            let router = tokio::spawn(async move {
                let mut stream: Box<dyn Stream> = Box::new(b);
                for i in 0..65 {
                    if write_frame(
                        &mut stream,
                        &json!({"type":"broadcast","index":i,"body":body}),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                // Even though the caller keeps the client alive, its dead
                // transport must unregister so Desktop does not wait for it.
                assert!(read_frame(&mut stream).await.is_err());
            });
            tokio::time::timeout(Duration::from_secs(5), router)
                .await
                .unwrap()
                .unwrap();
            drop(transport);
        }
    }

    #[tokio::test]
    async fn follower_controls_bind_to_original_owner_and_preserve_wire_versions() {
        let (mut c, log) = fixture(|_, v| v).await;
        let owner = c.owner(THREAD).await.unwrap();
        let h = c.history(&owner, "recent", None).await.unwrap();
        assert_eq!(h["revision"], 7);
        let command = uuid::Uuid::new_v4().to_string();
        c.start(&owner,json!({"threadId":"wrong","input":[{"type":"text","text":"hello"}],"clientUserMessageId":"phone-message"}),&command).await.unwrap();
        c.steer(
            &owner,
            json!({"input":[{"type":"text","text":"change"},{"type":"localImage","path":"/workspace/screenshot.png"}],"expectedTurnId":"t2","clientUserMessageId":"steer-message","serviceTier":"fast","additionalContext":{"test":"context"}}),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        c.stop(&owner, "t2").await.unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 6);
        for r in &log[2..] {
            assert_eq!(r["targetClientId"], "original-owner");
            assert_eq!(r["params"]["conversationId"], THREAD);
            assert_eq!(r["params"]["hostId"], "local");
        }
        assert_eq!(log[3]["version"], 2);
        assert_eq!(log[3]["params"]["turnStart"]["request"]["threadId"], THREAD);
        assert_eq!(
            log[3]["params"]["turnStart"]["request"]["clientUserMessageId"],
            "phone-message"
        );
        assert_eq!(log[3]["params"]["commandId"], command);
        assert_eq!(
            log[3]["params"]["turnStart"]["request"]["input"][0]["text_elements"],
            json!([])
        );
        assert_eq!(log[4]["version"], 1);
        assert_eq!(log[4]["params"]["input"][0]["text_elements"], json!([]));
        let steer = &log[4]["params"];
        assert_eq!(steer["expectedTurnId"], "t2");
        assert_eq!(
            steer["input"][1],
            json!({"type":"localImage","path":"/workspace/screenshot.png"})
        );
        assert_eq!(steer["restoreMessage"]["id"], "steer-message");
        assert_eq!(steer["restoreMessage"]["text"], "change");
        assert_eq!(steer["restoreMessage"]["context"]["prompt"], "change");
        assert_eq!(
            steer["restoreMessage"]["context"]["workspaceRoots"],
            json!([])
        );
        assert!(steer["restoreMessage"].get("cwd").is_none());
        assert!(steer["restoreMessage"]["createdAt"].as_u64().unwrap() > 0);
        assert_eq!(steer["serviceTier"], "fast");
        assert_eq!(steer["additionalContext"], json!({"test":"context"}));
        assert_eq!(steer["attachments"], json!([]));
        assert_eq!(log[5]["version"], 4);
        assert_eq!(log[5]["params"]["expectedTurnId"], "t2");
    }
    #[test]
    fn desktop_input_retains_existing_elements_and_refuses_malformed_values() {
        let mut input = json!({"input":[{"type":"text","text":"hello","text_elements":[{"placeholder":"mention","byteRange":{"start":0,"end":5}}]},{"type":"localImage","path":"image.png"}]});
        let original = input.clone();
        normalize_text_input(&mut input).unwrap();
        assert_eq!(input, original);
        input["input"][0]["text_elements"] = Value::Null;
        assert!(normalize_text_input(&mut input).is_err());
    }
    #[tokio::test]
    async fn flat_steer_reply_and_image_only_restore_keep_the_original_turn() {
        let (mut client, log) = fixture(|request, mut response| {
            if request["method"] == "thread-follower-steer-turn" {
                response["result"] = json!({"turnId":"t2"});
            }
            response
        })
        .await;
        let owner = client.owner(THREAD).await.unwrap();
        let command = uuid::Uuid::new_v4().to_string();
        let result = client.steer(&owner, json!({"expectedTurnId":"t2","input":[{"type":"localImage","path":"/workspace/image.png"}]}), &command).await.unwrap();
        assert_eq!(result, json!({"turnId":"t2"}));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3);
        let params = &log[2]["params"];
        assert_eq!(params["clientUserMessageId"], command);
        assert_eq!(params["restoreMessage"]["text"], "");
        assert_eq!(params["restoreMessage"]["context"]["prompt"], "");
        assert_eq!(params["input"][0]["path"], "/workspace/image.png");
    }
    #[tokio::test]
    async fn rejected_steer_is_not_replayed_and_does_not_interrupt_the_owner() {
        let (mut client, log) = fixture(|request, mut response| {
            if request["method"] == "thread-follower-steer-turn" {
                response["resultType"] = json!("error");
                response["error"] = json!({"message":"native steer rejected"});
                response.as_object_mut().unwrap().remove("result");
            }
            response
        })
        .await;
        let owner = client.owner(THREAD).await.unwrap();
        let command = uuid::Uuid::new_v4().to_string();
        let request = json!({"expectedTurnId":"t2","input":[{"type":"text","text":"change"}]});
        assert!(client
            .steer(&owner, request.clone(), &command)
            .await
            .is_err());
        assert!(client.steer(&owner, request, &command).await.is_err());
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[2]["method"], "thread-follower-steer-turn");
    }
    #[tokio::test]
    async fn wrong_owner_or_method_cannot_confirm_a_mutation_or_trigger_replay() {
        for changed in [0, 1] {
            let edit: fn(&Value, Value) -> Value = if changed == 0 {
                |request, mut v| {
                    if request["method"] == "thread-follower-start-turn" {
                        v["handledByClientId"] = json!("replacement");
                    }
                    v
                }
            } else {
                |request, mut v| {
                    if request["method"] == "thread-follower-start-turn" {
                        v["method"] = json!("thread/read");
                    }
                    v
                }
            };
            let (mut c, log) = fixture(edit).await;
            let owner = c.owner(THREAD).await.unwrap();
            let request = json!({"input":[{"type":"text","text":"once"}]});
            let command = uuid::Uuid::new_v4().to_string();
            assert!(c.start(&owner, request.clone(), &command).await.is_err());
            assert!(c.start(&owner, request, &command).await.is_err());
            assert_eq!(log.lock().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn empty_start_and_foreign_steer_acknowledgements_remain_uncertain() {
        let (mut client, log) = fixture(|request, mut response| {
            if request["method"] == "thread-follower-start-turn" {
                response["result"] = json!({"ok":true});
            }
            response
        })
        .await;
        let owner = client.owner(THREAD).await.unwrap();
        let command = uuid::Uuid::new_v4().to_string();
        assert!(client
            .start(
                &owner,
                json!({"input":[{"type":"text","text":"hi"}]}),
                &command
            )
            .await
            .is_err());
        assert!(client
            .start(
                &owner,
                json!({"input":[{"type":"text","text":"hi"}]}),
                &command
            )
            .await
            .is_err());
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "thread-follower-start-turn")
                .count(),
            1
        );

        let (mut client, log) = fixture(|request, mut response| {
            if request["method"] == "thread-follower-steer-turn" {
                response["result"] = json!({"result":{"turnId":"different-turn"}});
            }
            response
        })
        .await;
        let owner = client.owner(THREAD).await.unwrap();
        let input = json!({"expectedTurnId":"t2","input":[{"type":"text","text":"change"}]});
        assert!(client.steer(&owner, input.clone(), &command).await.is_err());
        assert!(client.steer(&owner, input, &command).await.is_err());
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "thread-follower-steer-turn")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn another_chat_history_and_unconfirmed_stop_are_rejected() {
        let (mut c, _) = fixture(|r, mut v| {
            if r["method"] == "thread-follower-load-history-page" {
                v["result"]["conversationState"]["id"] = json!("another-chat");
            }
            v
        })
        .await;
        let owner = c.owner(THREAD).await.unwrap();
        assert!(c.history(&owner, "recent", None).await.is_err());
        let (mut c, log) = fixture(|r, mut v| {
            if r["method"] == "thread-follower-interrupt-turn" {
                v["result"] = json!({"ok":true});
            }
            v
        })
        .await;
        let owner = c.owner(THREAD).await.unwrap();
        assert!(c.stop(&owner, "t2").await.is_err());
        assert!(c.stop(&owner, "t2").await.is_err());
        assert_eq!(log.lock().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn dropped_mutation_response_is_uncertain_and_never_retried() {
        let (mut c, log) = fixture(|r, mut v| {
            if r["method"] == "thread-follower-start-turn" {
                v["requestId"] = json!("unrelated");
            }
            v
        })
        .await;
        let owner = c.owner(THREAD).await.unwrap();
        let request = json!({"input":[{"type":"text","text":"once"}]});
        assert!(c
            .start(&owner, request.clone(), &uuid::Uuid::new_v4().to_string())
            .await
            .is_err());
        assert!(c
            .start(&owner, request, &uuid::Uuid::new_v4().to_string())
            .await
            .is_err());
        assert_eq!(log.lock().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn frame_limit_is_checked_before_payload_allocation() {
        let (a, mut b) = tokio::io::duplex(32);
        let mut a: Box<dyn Stream> = Box::new(a);
        b.write_u32_le((MAX_SNAPSHOT_FRAME + 1) as u32)
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut a).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[tokio::test]
    async fn split_frames_are_reassembled_without_changing_protocol() {
        let (a, mut b) = tokio::io::duplex(32);
        let mut a: Box<dyn Stream> = Box::new(a);
        tokio::spawn(async move {
            let data = br#"{"type":"response","result":{"ok":true}}"#;
            for x in (data.len() as u32)
                .to_le_bytes()
                .into_iter()
                .chain(data.iter().copied())
            {
                b.write_all(&[x]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        assert_eq!(read_frame(&mut a).await.unwrap()["result"]["ok"], true);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn endpoint_refuses_foreign_or_public_sockets() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = root.path().join("ipc");
        let _server = tokio::net::UnixListener::bind(&p).unwrap();
        let endpoint = Endpoint { path: p.clone() };
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        endpoint.validate(root.path()).unwrap();
        assert!(endpoint.validate(other.path()).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(endpoint.validate(root.path()).is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn offline_endpoint_keeps_configuration_readable_without_authorizing_control() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let endpoint = Endpoint {
            path: root.path().join("ipc/ipc.sock"),
        };
        endpoint.validate(root.path()).unwrap();
        assert!(endpoint.connect(root.path()).await.is_err());
        assert!(endpoint.validate(other.path()).is_err());
        std::fs::create_dir(root.path().join("ipc")).unwrap();
        endpoint.validate(root.path()).unwrap();
        std::fs::write(&endpoint.path, b"not a socket").unwrap();
        assert!(endpoint.validate(root.path()).is_err());
        std::fs::remove_file(&endpoint.path).unwrap();
        std::fs::remove_dir(root.path().join("ipc")).unwrap();
        std::os::unix::fs::symlink(other.path(), root.path().join("ipc")).unwrap();
        assert!(endpoint.validate(root.path()).is_err());
        let traversal = Endpoint {
            path: root.path().join("uncreated/../../elsewhere/ipc.sock"),
        };
        assert!(traversal.validate(root.path()).is_err());
    }
}

#[cfg(test)]
mod connection_tests {
    use super::tests::fixture;
    #[tokio::test]
    async fn owner_binding_cannot_cross_reconnections_even_if_client_names_repeat() {
        let (mut a, _) = fixture(|_, v| v).await;
        let owner = a
            .owner("12345678-1234-4234-8234-123456789abc")
            .await
            .unwrap();
        let (mut b, log) = fixture(|_, v| v).await;
        assert!(b.history(&owner, "recent", None).await.is_err());
        assert_eq!(log.lock().unwrap().len(), 1);
    }
}
