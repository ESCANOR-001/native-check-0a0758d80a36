//! Direct native CLI transport. No JavaScript SDK or base64 relay is involved.
//! Agent-specific policy and Android projection are separate migration steps.
use serde_json::Value;
use std::{collections::HashMap, io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
};

pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Input may contain Android’s bounded 40 MiB of base64 images.
pub const MAX_INPUT_FRAME: usize = 56 * 1024 * 1024;
#[derive(Clone, Copy)]
pub enum Agent {
    Codex,
    Claude,
}
pub struct Launch {
    pub agent: Agent,
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub extra_args: Vec<String>,
}
#[derive(Clone, serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SharedServer {
    Capability {
        address: std::net::SocketAddr,
        token_file: PathBuf,
    },
    Unix {
        socket_path: PathBuf,
    },
}
impl SharedServer {
    pub fn validate(&self, profile: &std::path::Path) -> Result<(), String> {
        match self {
            Self::Capability { .. } => self.open_token(profile).map(|_| ()),
            Self::Unix { socket_path } => validate_socket(profile, socket_path).map(|_| ()),
        }
    }
    fn open_token(&self, profile: &std::path::Path) -> Result<std::fs::File, String> {
        let Self::Capability {
            address,
            token_file,
        } = self
        else {
            return Err("A token connection is required".into());
        };
        if !address.ip().is_loopback() || address.port() == 0 || !token_file.is_absolute() {
            return Err(
                "Shared Codex requires an explicit loopback address and private token file".into(),
            );
        }
        let parent = profile
            .canonicalize()
            .map_err(|_| "Shared Codex test profile unavailable")?;
        let file = token_file
            .canonicalize()
            .map_err(|_| "Shared Codex token unavailable")?;
        let meta =
            std::fs::symlink_metadata(token_file).map_err(|_| "Shared Codex token unavailable")?;
        if !file.starts_with(parent) || !meta.is_file() || meta.len() > 4096 {
            return Err("Shared Codex token must be a small file in its isolated profile".into());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        // Open the resolved path and validate the actual handle too. A swapped
        // symlink, special file or permissions change must not expose a token
        // from elsewhere or block the runtime waiting on a pipe.
        let handle = options
            .open(&file)
            .map_err(|_| "Shared Codex token unavailable")?;
        let opened = handle
            .metadata()
            .map_err(|_| "Shared Codex token unavailable")?;
        if !opened.is_file() || opened.len() > 4096 {
            return Err("Shared Codex token must be a small regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.mode() & 0o077 != 0
                || opened.nlink() != 1
                || opened.uid() != unsafe { libc::geteuid() }
                || opened.dev() != meta.dev()
                || opened.ino() != meta.ino()
            {
                return Err(
                    "Shared Codex token permissions must restrict access to this user".into(),
                );
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if opened.file_attributes() & 0x400 != 0 {
                // FILE_ATTRIBUTE_REPARSE_POINT
                return Err("Shared Codex token cannot be a reparse point".into());
            }
        }
        Ok(handle)
    }
    pub async fn connect(&self, profile: &std::path::Path) -> Result<Connection, String> {
        if let Self::Unix { socket_path } = self {
            let resolved = validate_socket(profile, socket_path)?;
            return Connection::connect_unix(&resolved)
                .await
                .map_err(|_| "Could not connect to the isolated shared Codex socket".into());
        }
        let Self::Capability { address, .. } = self else {
            unreachable!()
        };
        use std::io::Read;
        let file = self.open_token(profile)?;
        let mut token = String::new();
        file.take(4097)
            .read_to_string(&mut token)
            .map_err(|_| "Shared Codex token unavailable")?;
        Connection::connect_loopback(*address, token.trim())
            .await
            .map_err(|_| "Could not connect to the isolated shared Codex server".into())
    }
}
fn validate_socket(profile: &std::path::Path, socket: &std::path::Path) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if !socket.is_absolute() {
            return Err("An absolute isolated socket is required".into());
        }
        let owner = unsafe { libc::geteuid() };
        let profile = profile
            .canonicalize()
            .map_err(|_| "Shared Codex profile unavailable")?;
        let parent = socket
            .parent()
            .ok_or("Shared Codex socket needs a parent")?
            .canonicalize()
            .map_err(|_| "Shared Codex socket unavailable")?;
        if !parent.starts_with(&profile) {
            return Err("Shared Codex socket must belong to its isolated profile".into());
        }
        let resolved = socket
            .canonicalize()
            .map_err(|_| "Shared Codex socket unavailable")?;
        let link =
            std::fs::symlink_metadata(socket).map_err(|_| "Shared Codex socket unavailable")?;
        if link.file_type().is_symlink() {
            // Codex shortens Unix paths using its fixed /tmp rendezvous root,
            // canonicalized for /tmp -> /private/tmp on macOS. The CLI does not
            // use TMPDIR here, which can be both different and too long. The
            // SHA256 name is bound to the requested absolute socket path.
            // Accept that exact owned alias, never an arbitrary redirect.
            let directory = std::path::Path::new("/tmp")
                .canonicalize()
                .map_err(|_| "Private Codex socket directory unavailable")?
                .join(format!("codex-daemon-{owner}"));
            let meta = std::fs::symlink_metadata(&directory)
                .map_err(|_| "Private Codex socket directory unavailable")?;
            let digest = format!(
                "{:x}",
                Sha256::digest(socket.as_os_str().as_encoded_bytes())
            );
            if !meta.is_dir()
                || meta.uid() != owner
                || meta.mode() & 0o077 != 0
                || link.uid() != owner
                || resolved != directory.join(digest)
            {
                return Err(
                    "Shared Codex socket alias is not a private, path-bound CLI socket".into(),
                );
            }
        } else if !resolved.starts_with(&profile) {
            return Err("Shared Codex socket escaped its isolated profile".into());
        }
        let meta =
            std::fs::symlink_metadata(&resolved).map_err(|_| "Shared Codex socket unavailable")?;
        if !meta.file_type().is_socket() || meta.uid() != owner || meta.mode() & 0o077 != 0 {
            return Err("Shared Codex requires a private socket in its isolated profile".into());
        }
        Ok(resolved)
    }
    #[cfg(not(unix))]
    {
        let _ = (profile, socket);
        Err("Private Unix sockets are unavailable on this platform".into())
    }
}
#[derive(Debug)]
pub enum Event {
    Message(Value),
    ClaudeRemote(crate::claude_remote::Envelope),
    Closed { code: Option<i32>, forced: bool },
}
type Write = (Vec<u8>, oneshot::Sender<io::Result<()>>);

pub struct Connection {
    writes: mpsc::Sender<Write>,
    pub events: mpsc::Receiver<Event>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Connection {
    /// Existing-session observer. Internal commit packets never go to Claude.
    pub fn observe_claude(
        profile: PathBuf,
        workspace: PathBuf,
        native: String,
        cursor: crate::claude_cursor::Cursor,
        db: crate::auth_service::AuthService,
        chat: String,
    ) -> Self {
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        #[cfg(test)]
        if let Some(fixture) = observer_fixture::registered(&profile) {
            let _ = fixture.send(observer_fixture::Fake {
                input,
                output,
                cursor,
                db,
            });
            return Self {
                writes,
                events,
                stop,
                task: None,
            };
        }
        let task = tokio::spawn(async move {
            let session = native.clone();
            let pending = db
                .call(move |a| a.store.unresolved_claude_inputs(&chat, &session))
                .await;
            let connection = tokio::select! {_=stopping.changed()=>return,result=crate::claude_observer::Observer::connect(profile,workspace,native,cursor)=>result};
            let mut observer = match connection {
                Ok(observer) => observer,
                Err(error) => {
                    let _ = output
                        .send(Event::Message(
                            serde_json::json!({"type":"remodex_observer_error","detail":error}),
                        ))
                        .await;
                    return;
                }
            };
            let recovered = pending
                .map_err(str::to_owned)
                .and_then(|r| r.map_err(|_| "Claude delivery state unavailable".to_owned()))
                .and_then(|ids| observer.recover_deliveries(ids));
            if let Err(error) = recovered {
                let _ = output
                    .send(Event::Message(
                        serde_json::json!({"type":"remodex_observer_error","detail":error}),
                    ))
                    .await;
                return;
            }
            use futures_util::{stream::FuturesUnordered, StreamExt};
            let mut deliveries = FuturesUnordered::new();
            let mut sending_bytes = 0usize;
            let mut delivery_error: Option<String> = None;
            let mut announced = false;
            let mut outbound: Option<Event> = None;
            let mut terminal = false;
            loop {
                if outbound.is_none() && !observer.pending() {
                    if let Some(error) = delivery_error.take() {
                        terminal = true;
                        outbound = Some(Event::Message(
                            serde_json::json!({"type":"remodex_observer_error","detail":error}),
                        ));
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>break,
                    permit=output.reserve(),if outbound.is_some()=>{
                        match permit {Ok(permit)=>permit.send(outbound.take().unwrap()),Err(_)=>break};
                        if terminal {break;}
                    },
                    result=deliveries.next(),if !deliveries.is_empty()=>{
                        if let Some((bytes, result)) = result {
                            sending_bytes = sending_bytes.saturating_sub(bytes);
                            if let Err(error) = result { delivery_error = Some(error); }
                        }
                    },
                    request=input.recv()=>{
                        let Some((bytes,reply))=request else{break};
                        let value:Value=match serde_json::from_slice(&bytes) {Ok(value)=>value,Err(_)=>{let _=reply.send(Err(io::ErrorKind::InvalidInput.into()));continue}};
                        let result = if value["type"] == "remodex_observer_commit" {
                            observer.commit(&db,value["state"].clone(),value["projection"].clone()).await
                        } else if value["type"] == "remodex_observer_forget_read" {
                            observer.forget_read(value["request_id"].as_str().unwrap_or(""))
                        } else if delivery_error.is_some() || terminal {
                            Err("Claude delivery is uncertain; reconnect before sending".into())
                        } else if deliveries.len() >= 8 || sending_bytes.saturating_add(bytes.len()).saturating_add(2048) > MAX_INPUT_FRAME {
                            Err("Claude delivery is busy; no additional command was sent".into())
                        } else {
                            let prepared = if value["type"] == "remodex_observer_prompt" {
                                match serde_json::from_value(value["input"].clone()) {
                                    Ok(input)=>observer.prepare_prompt(&db,value["chat"].as_str().unwrap_or("").into(),input,value["payload"].clone()).await,
                                    Err(_)=>Err("Invalid durable prompt identity".into()),
                                }
                            } else { observer.prepare_control(value) };
                            match prepared {
                                Ok(delivery) => {
                                    let size = delivery.bytes();
                                    sending_bytes += size;
                                    deliveries.push(async move { (size, delivery.deliver().await) });
                                    // This acknowledges local durable reservation, not
                                    // remote consumption. Only native receipts confirm it.
                                    Ok(())
                                },
                                Err(error) => Err(error),
                            }
                        };
                        let committed=result.is_ok();
                        let _=reply.send(result.map_err(io::Error::other));
                        if committed && !announced && observer.ready() {
                            announced=true;
                            outbound=Some(Event::Message(serde_json::json!({"type":"remodex_observer_ready"})));
                        }
                    },
                    result=observer.next(),if !observer.pending() && outbound.is_none() && delivery_error.is_none()=>{
                        outbound=Some(match result {Ok(event)=>{
                            if event.recovery == Some(true) { announced=false; }
                            Event::ClaudeRemote(event)
                        },Err(error)=>{
                            terminal=true;
                            Event::Message(serde_json::json!({"type":"remodex_observer_error","detail":error}))
                        }});
                    }
                }
            }
            // Dropping observer closes only this HTTP client. No interrupt,
            // remote_control=false, CLI termination or owner settings change.
        });
        Self {
            writes,
            events,
            stop,
            task: Some(task),
        }
    }
    /// Owned Claude host behind the same bounded transport interface. The
    /// identity-commit packet is internal and never forwarded to the CLI.
    pub fn host_claude(
        connection: Self,
        native: String,
        prior: Option<crate::claude_host::Identity>,
        title: String,
    ) -> Self {
        Self::host_claude_configured(connection, native, prior, title, vec![])
    }
    pub fn host_claude_configured(
        connection: Self,
        native: String,
        prior: Option<crate::claude_host::Identity>,
        title: String,
        disabled: Vec<String>,
    ) -> Self {
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let first = tokio::select! {
                _=stopping.changed()=>return,
                first=input.recv()=>first,
            };
            let Some((bytes, reply)) = first else { return };
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if value["type"] != "control_request" || value["request"]["subtype"] != "initialize" {
                let _ = reply.send(Err(io::ErrorKind::InvalidInput.into()));
                return;
            }
            let _ = reply.send(Ok(()));
            let attached = tokio::select! {
                _=stopping.changed()=>return,
                result=crate::claude_host::HostedSession::attach_configured(connection,native.clone(),prior,&title,Duration::from_secs(30),&disabled)=>result,
            };
            let mut host = match attached {
                Ok(host) => host,
                Err(error) => {
                    let _ = tokio::time::timeout(Duration::from_secs(5),output.send(Event::Message(serde_json::json!({"type":"control_response","response":{"request_id":value["request_id"],"subtype":"error","error":error}})))).await;
                    return;
                }
            };
            let mut init = host.initialize.clone();
            if !init.is_object() {
                init = serde_json::json!({});
            }
            init["remodexRemoteIdentity"] = serde_json::to_value(host.identity()).unwrap();
            let initialized = serde_json::json!({"type":"control_response","response":{"request_id":value["request_id"],"subtype":"success","response":init}});
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    output.send(Event::Message(initialized))
                )
                .await,
                Ok(Ok(()))
            ) {
                host.close().await;
                return;
            }
            let mut pending = std::collections::VecDeque::new();
            let mut source_closed = false;
            let mut committed = false;
            loop {
                if source_closed && pending.is_empty() {
                    break;
                }
                tokio::select! {
                    _=stopping.changed()=>break,
                    permit=output.reserve(),if !pending.is_empty()=>{
                        let Ok(permit)=permit else {break};
                        permit.send(Event::Message(pending.pop_front().unwrap()));
                    },
                    packet=input.recv()=>{
                        let Some((bytes,reply))=packet else {break};
                        let value:Value=serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                        let commit = value["type"]=="remodex_identity_commit";
                        let result = if source_closed {
                            Err("Claude connection is closed".into())
                        } else if commit && committed {
                            Err("Claude identity has already been committed".into())
                        } else if commit {
                            match serde_json::from_value::<crate::claude_host::Identity>(value["identity"].clone()) {
                                Ok(identity)=>host.confirm_persisted(&identity),
                                Err(_)=>Err("Invalid persisted Claude identity".into()),
                            }
                        } else if value["type"]=="user" {
                            if value["session_id"]!=native || !value["parent_tool_use_id"].is_null()
                                || value["priority"].as_str().is_some_and(|s|s!="now") || value["message"]["role"]!="user" {
                                Err("Invalid Claude prompt envelope".into())
                            } else {
                                host.send_prompt(value["uuid"].as_str().unwrap_or(""),value["message"]["content"].clone(),value["priority"]=="now").await
                            }
                        } else { host.send_control(&value).await };
                        let success=result.is_ok();
                        let _=reply.send(result.map_err(io::Error::other));
                        if !success {source_closed=true;continue;}
                        if commit {
                            committed=true;
                            let event=serde_json::json!({"type":"control_response","response":{"request_id":"remote-commit","subtype":"success","response":{}}});
                            // At most two native frames plus this one-time ack.
                            // Never wait on output while the caller is waiting
                            // for its input write acknowledgment.
                            pending.push_back(event);
                        }
                    },
                    // Deliver the durable identity acknowledgment before any
                    // buffered remote turn. Otherwise a fast remote result can
                    // overwrite the worker's connection-handshake stage.
                    frame=host.next(),if committed && !source_closed && pending.len()<2=>{
                        match frame {
                            Ok(Some(frame))=>pending.push_back(frame.value),
                            Err(error)=>{
                                pending.push_back(serde_json::json!({"type":"control_response","response":{"request_id":"remote-stream","subtype":"error","error":error}}));
                                source_closed=true;
                            },
                            Ok(None)=>source_closed=true,
                        }
                    }
                }
            }
            host.close().await;
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                output.send(Event::Closed {
                    code: None,
                    forced: true,
                }),
            )
            .await;
        });
        Self {
            writes,
            events,
            stop,
            task: Some(task),
        }
    }
    /// Attach to an explicitly configured, capability-protected local Codex
    /// app-server. This connection never owns or terminates that server process.
    pub async fn connect_loopback(address: std::net::SocketAddr, token: &str) -> io::Result<Self> {
        use tokio_tungstenite::tungstenite::{
            client::IntoClientRequest, protocol::WebSocketConfig,
        };
        if !address.ip().is_loopback()
            || address.port() == 0
            || token.len() < 16
            || token.len() > 4096
            || !token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut request = format!("ws://{address}")
            .into_client_request()
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| io::ErrorKind::InvalidInput)?,
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async_with_config(request, Some(config), true),
        )
        .await
        .map_err(|_| io::ErrorKind::TimedOut)?
        .map_err(|_| io::ErrorKind::ConnectionRefused)?;
        Ok(Self::from_socket(socket))
    }
    #[cfg(unix)]
    async fn connect_unix(path: &std::path::Path) -> io::Result<Self> {
        use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), async {
            let stream = tokio::net::UnixStream::connect(path).await?;
            if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            tokio_tungstenite::client_async_with_config(
                "ws://localhost/rpc",
                stream,
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(MAX_FRAME))
                        .max_frame_size(Some(MAX_FRAME)),
                ),
            )
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::ConnectionRefused))
        })
        .await
        .map_err(|_| io::ErrorKind::TimedOut)??;
        Ok(Self::from_socket(socket))
    }
    #[cfg(not(unix))]
    async fn connect_unix(_path: &std::path::Path) -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn from_socket<S>(socket: tokio_tungstenite::WebSocketStream<S>) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let (mut sink, mut stream) = socket.split();
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut forced = false;
            let mut pending = None;
            let mut pending_deadline = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _=stopping.changed()=>{forced=true;break;},
                    command=input.recv()=>{
                        let Some((bytes,reply))=command else {forced=true;break;};
                        let text=match String::from_utf8(bytes) {Ok(v)=>v,Err(_)=>{let _=reply.send(Err(io::ErrorKind::InvalidInput.into()));forced=true;break;}};
                        let result=tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Text(text.into()))).await;
                        let ok=matches!(result,Ok(Ok(())));
                        let _=reply.send(if ok {Ok(())} else {Err(io::ErrorKind::BrokenPipe.into())});
                        if !ok {forced=true;break;}
                    },
                    permit=output.reserve(), if pending.is_some()=>{
                        match permit {
                            Ok(permit)=>permit.send(pending.take().unwrap()),
                            Err(_)=>{forced=true;break;}
                        }
                    },
                    _=tokio::time::sleep_until(pending_deadline), if pending.is_some()=>{
                        forced=true;break;
                    },
                    message=stream.next(), if pending.is_none()=>{
                        match message {
                            Some(Ok(Message::Text(bytes)))=>{
                                let value=match serde_json::from_slice(bytes.as_bytes()) {Ok(v)=>v,Err(_)=>{forced=true;break;}};
                                // Keep one pending event, but continue servicing writes.
                                // The consumer can be awaiting a write acknowledgement
                                // while native startup notifications fill its queue.
                                pending=Some(Event::Message(value));
                                pending_deadline=tokio::time::Instant::now()+Duration::from_secs(5);
                            },
                            Some(Ok(Message::Ping(bytes)))=>{
                                if !matches!(tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Pong(bytes))).await,Ok(Ok(()))) {forced=true;break;}
                            },
                            Some(Ok(Message::Pong(_)))=>{},
                            Some(Ok(Message::Close(_)))|None=>break,
                            _=>{forced=true;break;},
                        }
                    },
                }
            }
            // Closing this client's socket must never signal the shared CLI or
            // another client. No reconnect/replay is performed by the transport.
            let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                output.send(Event::Closed { code: None, forced }),
            )
            .await;
        });
        Self {
            writes,
            events,
            stop,
            task: Some(task),
        }
    }
    pub fn start(launch: Launch) -> io::Result<Self> {
        if !launch.executable.is_absolute() || !launch.cwd.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut cmd = Command::new(launch.executable);
        match launch.agent {
            Agent::Codex => {
                cmd.args(["app-server", "--listen", "stdio://"]);
            }
            Agent::Claude => {
                cmd.args([
                    "--print",
                    "--output-format",
                    "stream-json",
                    "--input-format",
                    "stream-json",
                    "--verbose",
                    "--permission-prompt-tool",
                    "stdio",
                    "--include-partial-messages",
                ]);
            }
        }
        cmd.args(launch.extra_args)
            .current_dir(launch.cwd)
            .env_clear()
            .envs(launch.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let mut child = cmd.spawn()?;
        let mut process_tree = crate::process_tree::ProcessTree::attach(&child)?;
        // Retain the process group identity: Child::id becomes None after wait,
        // but a tool descendant may still hold the CLI's inherited pipe open.
        let owned_pid = child.id();
        let stdin = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let mut stderr = child.stderr.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut writer = tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some((bytes, reply)) = input.recv().await {
                    let result =
                        tokio::time::timeout(Duration::from_secs(5), stdin.write_all(&bytes))
                            .await
                            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()));
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    if failed {
                        break;
                    }
                }
            });
            let read_output = output.clone();
            let mut reader = tokio::spawn(async move {
                let mut stdout = BufReader::new(stdout);
                while let Some(bytes) = frame(&mut stdout).await? {
                    let value =
                        serde_json::from_slice(&bytes).map_err(|_| io::ErrorKind::InvalidData)?;
                    // A slow UI cannot grow an unbounded transcript queue.
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        read_output.send(Event::Message(value)),
                    )
                    .await
                    .map_err(|_| io::ErrorKind::TimedOut)?
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                }
                Ok::<(), io::Error>(())
            });
            let errors = tokio::spawn(async move {
                let mut buffer = [0; 8192];
                while matches!(stderr.read(&mut buffer).await,Ok(n) if n>0) {}
            });
            let mut code = None;
            let forced = tokio::select! {
                status=child.wait()=>{code=status.ok().and_then(|s|s.code());false},
                _=stopping.changed()=>true,
                _=&mut writer=>true,
                _=&mut reader=>true,
            };
            // Even an EOF/error path must reap its owned CLI. Do not search for
            // processes by executable name or interfere with another instance.
            terminate(&mut child, owned_pid).await;
            process_tree.reaped();
            if !reader.is_finished()
                && tokio::time::timeout(Duration::from_secs(1), &mut reader)
                    .await
                    .is_err()
            {
                reader.abort();
            }
            writer.abort();
            errors.abort();
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                output.send(Event::Closed { code, forced }),
            )
            .await;
        });
        Ok(Self {
            writes,
            events,
            stop,
            task: Some(task),
        })
    }
    pub async fn send(&self, value: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        if bytes.len() >= MAX_INPUT_FRAME {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        bytes.push(b'\n');
        let (reply, receive) = oneshot::channel();
        self.writes
            .try_send((bytes, reply))
            .map_err(|_| io::ErrorKind::WouldBlock)?;
        receive.await.map_err(|_| io::ErrorKind::BrokenPipe)?
    }
    pub async fn close(mut self) {
        let _ = self.stop.send(true);
        // Discard pending output on an explicit close so shutdown never waits on
        // an application which has already stopped consuming it.
        self.events.close();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
async fn terminate(child: &mut Child, owned_pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = owned_pid {
        if unsafe { libc::kill(-(pid as i32), libc::SIGTERM) } == 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = owned_pid;
    let _ = child.kill().await;
    let _ = child.wait().await;
}
pub async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let end = chunk.iter().position(|b| *b == b'\n');
        let count = end.map_or(chunk.len(), |n| n + 1);
        if bytes.len() + count > MAX_FRAME {
            return Err(io::ErrorKind::InvalidData.into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(bytes));
        }
    }
}

/// Unit-test stand-in for the Remote Control relay of one isolated profile.
/// It receives exactly the observer's writes and supplies its events.
#[cfg(test)]
pub(crate) mod observer_fixture {
    use super::*;
    use std::{path::Path, sync::Mutex};
    pub(crate) struct Fake {
        pub input: mpsc::Receiver<Write>,
        pub output: mpsc::Sender<Event>,
        pub cursor: crate::claude_cursor::Cursor,
        pub db: crate::auth_service::AuthService,
    }
    type Registry = Mutex<Vec<(PathBuf, mpsc::UnboundedSender<Fake>)>>;
    static FIXTURES: Registry = Mutex::new(Vec::new());
    pub(crate) fn register(profile: &Path) -> mpsc::UnboundedReceiver<Fake> {
        let (send, receive) = mpsc::unbounded_channel();
        FIXTURES.lock().unwrap().push((profile.into(), send));
        receive
    }
    pub(super) fn registered(profile: &Path) -> Option<mpsc::UnboundedSender<Fake>> {
        let fixtures = FIXTURES.lock().unwrap();
        fixtures
            .iter()
            .find(|(path, _)| path == profile)
            .map(|(_, send)| send.clone())
    }
    /// An emitted worker event awaiting the hub's commit.
    pub(crate) type Pending = Option<(crate::claude_cursor::Cursor, crate::claude_host::Receipt)>;
    /// Offers one durable worker event, as the real observer does.
    pub(crate) async fn emit(
        fake: &mut Fake,
        pending: &mut Pending,
        payload: serde_json::Value,
    ) -> bool {
        let sequence = fake.cursor.sequence + 1;
        let event = crate::claude_remote::Envelope {
            sequence: Some(sequence),
            event_id: Some(format!("fixture-{sequence}")),
            created_at: None,
            source: "worker".into(),
            payload: Some(payload),
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let receipt = crate::claude_host::Ledger::default()
            .observe(event.payload.as_ref().unwrap())
            .unwrap();
        *pending = Some((
            fake.cursor
                .advance(&event, serde_json::Value::Null)
                .unwrap(),
            receipt,
        ));
        fake.output.send(Event::ClaudeRemote(event)).await.is_ok()
    }
    /// The checkpoint the real observer offers after catch-up, before ready.
    pub(crate) async fn emit_owner(fake: &mut Fake, pending: &mut Pending, pid: u64) -> bool {
        let previous = fake.cursor.projection["owner_pid"].as_u64();
        let receipt = crate::claude_host::Receipt {
            owner_replaced: previous.is_some_and(|p| p != pid),
            ..Default::default()
        };
        *pending = Some((fake.cursor.clone(), receipt));
        fake.output
            .send(Event::ClaudeRemote(crate::claude_remote::Envelope::owner(
                Some(pid),
            )))
            .await
            .is_ok()
    }
    /// Answers one write; a commit saves the pending event with its receipt.
    pub(crate) async fn answer(
        fake: &mut Fake,
        pending: &mut Pending,
        (bytes, reply): Write,
    ) -> serde_json::Value {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if value["type"] == "remodex_observer_commit" {
            let (mut after, receipt) = pending.take().expect("commit without a pending event");
            after.projection = value["projection"].clone();
            let (before, saved, state) =
                (fake.cursor.clone(), after.clone(), value["state"].clone());
            fake.db
                .call(move |a| {
                    a.store
                        .save_claude_remote_receipt(&state, &before, &saved, Some(&receipt))
                })
                .await
                .unwrap()
                .unwrap();
            fake.cursor = after;
        }
        let _ = reply.send(Ok(()));
        value
    }
}
