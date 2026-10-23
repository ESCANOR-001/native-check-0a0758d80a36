//! One existing-session observer. No process is spawned, no sharing preference
//! changes, and dropping it never interrupts the original session owner.
use crate::{
    auth_service::AuthService,
    claude_cursor::Cursor,
    claude_link::Link,
    claude_remote::{Delivery, Envelope, Remote},
    claude_turns::Input,
};
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct Observer {
    remote: Remote,
    link: Link,
    cursor: Cursor,
    pending: Option<Cursor>,
    incoming: Option<Envelope>,
    pending_receipt: Option<crate::claude_host::Receipt>,
    init_request: String,
    init_response: Option<u64>,
    verified_at: Option<u64>,
    owner_stopped: bool,
    read_receipts: bool,
    receipt_inputs: Vec<String>,
    receipts_checked: bool,
    receipt_recovery_pending: bool,
    recovering_history: bool,
    recovery_commit: Option<bool>,
    link_source: Option<(PathBuf, PathBuf)>,
    /// Process ID the verified original worker reported for this connection.
    worker_pid: Option<u64>,
    /// The owner checkpoint for this connection has been committed.
    owner_checked: bool,
    owner_pending: bool,
    /// At most one in-stream metadata check. Lost replies expire locally;
    /// a later quiet turn must still be checked on this same connection.
    owner_recheck: Option<(String, std::time::Instant)>,
    /// A newer worker process reported by that check, awaiting its checkpoint.
    owner_replacement: Option<u64>,
    last_event: std::time::Instant,
}

/// The relay says the worker is idle while a consumed turn has produced
/// nothing for a while: ask the worker which process it is. Only a different
/// process proves the turn can never finish; the answer alone changes nothing.
pub(crate) fn owner_recheck_due(
    status: &str,
    projection: &Value,
    quiet: std::time::Duration,
) -> bool {
    status == "idle"
        && quiet >= std::time::Duration::from_secs(10)
        && crate::claude_history::control_state(projection)
            .is_ok_and(|(router, _)| router.active().is_some())
}

impl Observer {
    pub async fn connect(
        profile: PathBuf,
        workspace: PathBuf,
        native: String,
        cursor: Cursor,
    ) -> Result<Self, String> {
        cursor.validate()?;
        let pending = crate::claude_history::control_state(&cursor.projection)?
            .0
            .pending()
            .filter(|i| !i.local)
            .map(|i| i.native.clone())
            .collect::<Vec<_>>();
        let read_profile = profile.clone();
        let read_workspace = workspace.clone();
        let link = tokio::task::spawn_blocking(move || {
            crate::claude_link::read_for_inputs(&read_profile, &read_workspace, &native, &pending)
        })
        .await
        .map_err(|_| "Native history reader stopped")??;
        if cursor.identity != link.identity {
            return Err("Claude's shared link changed; saved history must be reconciled before reconnecting".into());
        }
        let remote = Remote::open(&profile, link.identity.clone(), cursor.sequence).await?;
        let mut observer = Self::from_remote(remote, link, cursor).await?;
        observer.link_source = Some((profile, workspace));
        Ok(observer)
    }
    async fn from_remote(mut remote: Remote, link: Link, cursor: Cursor) -> Result<Self, String> {
        // Always reconcile the saved cursor against the durable REST range.
        // This also covers missing/truncated initial SSE replay on reconnect.
        remote.recover_history();
        let init_request = format!("remodex-observe-{}", uuid::Uuid::new_v4());
        remote.send(json!({"type":"control_request","request_id":init_request,"request":{"subtype":"initialize"}})).await?;
        Ok(Self {
            remote,
            link,
            cursor,
            pending: None,
            incoming: None,
            pending_receipt: None,
            init_request,
            init_response: None,
            verified_at: None,
            owner_stopped: false,
            read_receipts: false,
            receipt_inputs: vec![],
            receipts_checked: true,
            receipt_recovery_pending: false,
            recovering_history: false,
            recovery_commit: None,
            link_source: None,
            worker_pid: None,
            owner_checked: false,
            owner_pending: false,
            owner_recheck: None,
            owner_replacement: None,
            last_event: std::time::Instant::now(),
        })
    }
    pub fn forget_read(&mut self, id: &str) -> Result<(), String> {
        self.remote.forget_read(id)
    }
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }
    pub fn ready(&self) -> bool {
        !self.owner_stopped
            && !self.recovering_history
            && self.receipts_checked
            && self.owner_checked
            && self
                .verified_at
                .is_some_and(|seq| self.cursor.sequence >= seq)
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn link(&self) -> &Link {
        &self.link
    }
    pub fn recover_deliveries(&mut self, inputs: Vec<String>) -> Result<(), String> {
        if inputs.len() > 64 {
            return Err("Too many unresolved Claude deliveries".into());
        }
        self.receipts_checked = inputs.is_empty();
        self.receipt_inputs = inputs;
        Ok(())
    }
    /// Only one uncommitted durable event can leave the observer at a time.
    /// Backpressure therefore bounds memory even if the PC disk is slow.
    pub async fn next(&mut self) -> Result<Envelope, String> {
        if self.pending.is_some() {
            return Err("Commit the previous Claude event before receiving another".into());
        }
        if self.owner_stopped {
            return Err("The original Claude worker stopped; saved history is retained".into());
        }
        // Keep the acquired frame across cancellation by the actor's select.
        // Revalidating a redacted cwd may await disk I/O; losing that frame
        // would leave a cursor gap and a permanently backpressured observer.
        if self.incoming.is_none() {
            if !self.receipts_checked
                && self
                    .verified_at
                    .is_some_and(|seq| self.cursor.sequence >= seq)
            {
                let receipts = if self.read_receipts {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        self.remote.delivery_snapshot(&self.receipt_inputs),
                    )
                    .await
                    .map_err(|_| "Claude receipt recovery timed out; input was not resent")??
                } else {
                    vec![]
                };
                self.incoming = Some(Envelope::receipts(receipts));
                self.receipt_recovery_pending = true;
            } else if let Some(pid) = self.owner_replacement.filter(|_| self.ready()) {
                self.worker_pid = Some(pid);
                self.incoming = Some(Envelope::owner(Some(pid)));
                self.pending_receipt = Some(crate::claude_host::Receipt {
                    owner_replaced: true,
                    ..Default::default()
                });
                self.owner_pending = true;
            } else if self.receipts_checked
                && !self.owner_checked
                && !self.recovering_history
                && self
                    .verified_at
                    .is_some_and(|seq| self.cursor.sequence >= seq)
            {
                // After catch-up, before any send: if another process now owns
                // the session, the previous process's turn can never finish.
                let previous = self.cursor.projection["owner_pid"].as_u64();
                let replaced = matches!((previous, self.worker_pid), (Some(a), Some(b)) if a != b);
                let mut checkpoint = Envelope::owner(self.worker_pid);
                checkpoint.payload.as_mut().unwrap()["internal_inputs"] =
                    json!(self.link.internal_inputs);
                self.incoming = Some(checkpoint);
                self.pending_receipt = Some(crate::claude_host::Receipt {
                    owner_replaced: replaced,
                    ..Default::default()
                });
                self.owner_pending = true;
            } else {
                let envelope = loop {
                    let envelope = self.remote.next().await?;
                    if !envelope.is_worker_status() {
                        break envelope;
                    }
                    if self.owner_recheck.as_ref().is_some_and(|(_, sent)| {
                        sent.elapsed() >= std::time::Duration::from_secs(30)
                    }) {
                        let (id, _) = self.owner_recheck.take().unwrap();
                        self.remote.forget_read(&id)?;
                    }
                    if self.ready()
                        && self.owner_recheck.is_none()
                        && owner_recheck_due(
                            &self.remote.worker_status,
                            &self.cursor.projection,
                            self.last_event.elapsed(),
                        )
                    {
                        let id = format!("remodex-owner-check-{}", uuid::Uuid::new_v4());
                        self.owner_recheck = Some((id.clone(), std::time::Instant::now()));
                        self.remote.send(json!({"type":"control_request","request_id":id,"request":{"subtype":"initialize"}})).await?;
                    }
                };
                self.incoming = Some(envelope);
            }
        }
        let envelope = self.incoming.as_ref().unwrap();
        if let Some(active) = envelope.recovery {
            // Enter the barrier immediately. Leave it only after the end
            // checkpoint has been committed, including across cancellation.
            if active {
                self.recovering_history = true;
            }
            self.recovery_commit = Some(active);
        }
        if let Some(sequence) = envelope.sequence {
            if envelope.source == "worker" {
                self.last_event = std::time::Instant::now();
                if let Some(frame) = &envelope.payload {
                    if frame["type"] == "control_response"
                        && self
                            .owner_recheck
                            .as_ref()
                            .is_some_and(|(id, _)| frame["response"]["request_id"] == id.as_str())
                    {
                        self.owner_recheck = None;
                        if frame["response"]["subtype"] == "success" {
                            let previous = self.cursor.projection["owner_pid"].as_u64();
                            let current = frame["response"]["response"]["pid"].as_u64();
                            if matches!((previous, current), (Some(a), Some(b)) if a != b) {
                                self.owner_replacement = current;
                            }
                        }
                    }
                    let receipt = crate::claude_host::Ledger::default().observe(frame)?;
                    // Catch-up can contain a prior worker's shutdown. A fresh
                    // initialize response plus init is still required before
                    // readiness. Only a shutdown after that handshake belongs
                    // to the connection we have actually verified.
                    self.owner_stopped = receipt.owner_stopped && self.verified_at.is_some();
                    self.pending_receipt = Some(receipt);
                    if frame["type"] == "control_response"
                        && frame["response"]["request_id"] == self.init_request
                    {
                        if frame["response"]["subtype"] != "success" {
                            return Err(
                                "The original Claude worker did not confirm attachment".into()
                            );
                        }
                        if self.init_response.is_none() {
                            self.init_response = Some(sequence);
                            self.read_receipts =
                                frame["response"]["response"]["inbound_read_receipts"] == true;
                            self.worker_pid = frame["response"]["response"]["pid"].as_u64();
                        }
                    }
                    if frame["type"] == "system"
                        && frame["subtype"] == "init"
                        && self.init_response.is_some_and(|seq| sequence > seq)
                    {
                        let cwd = frame["cwd"]
                            .as_str()
                            .ok_or("Claude did not confirm its project metadata")?;
                        if cwd.is_empty() || self.link_source.is_some() {
                            // Remote Control redacts cwd in some CLI versions.
                            // Revalidate the owner's local binding after the fresh
                            // worker handshake; never guess from a chat title.
                            let (profile, workspace) = self
                                .link_source
                                .clone()
                                .ok_or("Claude did not confirm the original project folder")?;
                            let native = self.link.identity.native_session.clone();
                            let current = tokio::task::spawn_blocking(move || {
                                crate::claude_link::read(&profile, &workspace, &native)
                            })
                            .await
                            .map_err(|_| "Claude local binding check stopped")??;
                            if current.identity != self.link.identity
                                || current.cwd != self.link.cwd
                            {
                                return Err(
                                    "Claude's local session binding changed during connection"
                                        .into(),
                                );
                            }
                        }
                        if !cwd.is_empty() {
                            let path = PathBuf::from(cwd);
                            if cwd.len() > 16384
                                || !path.is_absolute()
                                || path
                                    .canonicalize()
                                    .map_err(|_| "Claude project folder is unavailable")?
                                    != self.link.cwd
                            {
                                return Err("Claude's live folder differs from its saved history; no prompt was sent".into());
                            }
                        }
                        self.verified_at = Some(sequence);
                    }
                }
            }
        }
        let mut envelope = self.incoming.take().unwrap();
        if !self.read_receipts {
            envelope.deliveries.clear();
        }
        if !envelope.deliveries.is_empty() {
            let receipt = self.pending_receipt.get_or_insert_with(Default::default);
            for delivered in &envelope.deliveries {
                receipt.consumed.push(delivered.input.clone());
                if delivered.processed {
                    receipt.completed.push(delivered.input.clone());
                }
            }
        }
        if envelope.sequence.is_some() {
            self.pending = Some(self.cursor.advance(&envelope, Value::Null)?);
        } else if envelope.receipt_checkpoint {
            self.pending = Some(self.cursor.clone());
            self.pending_receipt.get_or_insert_with(Default::default);
        }
        Ok(envelope)
    }
    /// Commit public projection and resume point atomically, then acknowledge
    /// in memory. A failed database write leaves the stream backpressured.
    pub async fn commit(
        &mut self,
        db: &AuthService,
        state: Value,
        projection: Value,
    ) -> Result<(), String> {
        let mut after = self
            .pending
            .clone()
            .ok_or("No durable Claude event is pending")?;
        after.projection = projection;
        after.validate()?;
        let before = self.cursor.clone();
        let saved = after.clone();
        let receipt = self
            .pending_receipt
            .as_ref()
            .map(|r| crate::claude_host::Receipt {
                consumed: r.consumed.clone(),
                completed: r.completed.clone(),
                cancelled: r.cancelled.clone(),
                owner_stopped: r.owner_stopped,
                owner_replaced: r.owner_replaced,
            });
        db.call(move |a| {
            a.store
                .save_claude_remote_receipt(&state, &before, &saved, receipt.as_ref())
        })
        .await?
        .map_err(|_| "Could not commit Claude history; the event was not acknowledged")?;
        self.cursor = after;
        self.pending = None;
        self.pending_receipt = None;
        if let Some(active) = self.recovery_commit.take() {
            self.recovering_history = active;
            if !active {
                // Historical processing metadata may precede the fresh worker
                // handshake. Recheck surviving remote as well as local inputs
                // once support is known; an old stopped turn must not remain
                // active merely because its result omitted a UUID.
                let (router, _) = crate::claude_history::control_state(&self.cursor.projection)?;
                let mut inputs = self.receipt_inputs.clone();
                inputs.extend(router.pending().map(|i| i.native.clone()));
                inputs.sort();
                inputs.dedup();
                self.recover_deliveries(inputs)?;
            }
        }
        if self.receipt_recovery_pending {
            self.receipts_checked = true;
            self.receipt_recovery_pending = false;
        }
        if self.owner_pending {
            self.owner_checked = true;
            self.owner_pending = false;
            self.owner_replacement = None;
        }
        Ok(())
    }
    /// Caller persists the displayed input before this call. An already
    /// reserved UUID is not sent again, including after timeout or restart.
    pub async fn send_prompt(
        &mut self,
        db: &AuthService,
        chat: String,
        input: Input,
        payload: Value,
    ) -> Result<(), String> {
        self.prepare_prompt(db, chat, input, payload)
            .await?
            .deliver()
            .await
    }
    pub async fn prepare_prompt(
        &mut self,
        db: &AuthService,
        chat: String,
        input: Input,
        mut payload: Value,
    ) -> Result<Delivery, String> {
        if !self.ready() {
            return Err("Claude history is still connecting; no prompt was sent".into());
        }
        if payload["session_id"] != self.link.identity.native_session {
            return Err("Claude prompt targets another session".into());
        }
        payload["origin"] = json!({"kind":"human"});
        let session = self.link.identity.native_session.clone();
        let saved = payload.clone();
        if !db
            .call(move |a| a.store.offer_claude_input(&chat, &session, &input, &saved))
            .await??
        {
            return Err(
                "Claude delivery was already offered. Review the chat before resending.".into(),
            );
        }
        self.remote.prepare_send(payload)
    }
    /// User-requested controls only. The attachment handshake never applies
    /// model, effort, permission, MCP, or sharing settings on its own.
    pub async fn control(&mut self, payload: Value) -> Result<(), String> {
        self.prepare_control(payload)?.deliver().await
    }
    pub fn prepare_control(&mut self, payload: Value) -> Result<Delivery, String> {
        if !self.ready() {
            return Err("Claude is still connecting; no control was sent".into());
        }
        if payload["type"] == "user"
            || (payload["type"] == "control_request"
                && payload["request"]["subtype"] == "initialize")
        {
            return Err("Use the durable prompt path for messages".into());
        }
        self.remote.prepare_send(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Auth, claude_host::Identity, store::Store};
    use axum::{
        body::Body,
        extract::State,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    #[test]
    fn owner_recheck_only_for_a_quiet_active_turn_on_an_idle_worker() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut router = crate::claude_turns::Router::default();
        router
            .register(Input {
                native: id.clone(),
                message: "message".into(),
                public_turn: "turn".into(),
                local: true,
                steer: false,
            })
            .unwrap();
        router
            .observe(&json!({"type":"assistant","user_message_uuids":[id]}))
            .unwrap();
        let active = json!({"router":router,"history_turn":null,"owner_pid":1});
        let idle = json!({"router":crate::claude_turns::Router::default(),"history_turn":null});
        let quiet = std::time::Duration::from_secs(11);
        assert!(owner_recheck_due("idle", &active, quiet));
        for status in ["running", "requires_action", ""] {
            assert!(!owner_recheck_due(status, &active, quiet));
        }
        assert!(!owner_recheck_due(
            "idle",
            &active,
            std::time::Duration::from_secs(5)
        ));
        assert!(!owner_recheck_due("idle", &idle, quiet));
        assert!(!owner_recheck_due("idle", &Value::Null, quiet));
    }
    #[derive(Clone)]
    struct Server {
        identity: Identity,
        cwd: PathBuf,
        send: mpsc::Sender<String>,
        receive: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
        posts: Arc<Mutex<Vec<Value>>>,
        post_started: Arc<tokio::sync::Notify>,
        release_post: Arc<tokio::sync::Notify>,
        connected: Arc<std::sync::atomic::AtomicBool>,
        history: Arc<Mutex<Vec<Value>>>,
        pid: Arc<std::sync::atomic::AtomicU64>,
        lose_init: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Server {
        async fn emit(&self, seq: u64, source: &str, payload: Value) {
            let value = json!({"event_id":format!("event-{seq}"),"sequence_num":seq.to_string(),"source":source,"payload":payload});
            self.history.lock().unwrap().push(value.clone());
            self.send
                .send(format!("event: client_event\nid: {seq}\ndata: {value}\n\n"))
                .await
                .unwrap();
        }
    }
    async fn metadata(State(s): State<Server>) -> Json<Value> {
        Json(
            json!({"response_shape":{"id":s.identity.bridge_session,"worker_status":"idle","connection_status":if s.connected.load(std::sync::atomic::Ordering::SeqCst) {"connected"} else {"disconnected"}}}),
        )
    }
    async fn stream(State(s): State<Server>) -> impl IntoResponse {
        let rx = s.receive.lock().unwrap().take().unwrap();
        let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|text| (Ok::<_, std::io::Error>(text), rx))
        }));
        ([("content-type", "text/event-stream")], body)
    }
    async fn send(State(s): State<Server>, Json(v): Json<Value>) -> impl IntoResponse {
        s.posts.lock().unwrap().push(v.clone());
        let p = &v["events"][0]["payload"];
        if p["type"] == "control_request" && p["request"]["subtype"] == "initialize" {
            if s.lose_init.load(std::sync::atomic::Ordering::SeqCst) {
                return axum::http::StatusCode::OK;
            }
            let seq = s.history.lock().unwrap().last().unwrap()["sequence_num"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                + 1;
            let reply = json!({"type":"control_response","response":{"request_id":p["request_id"],"subtype":"success","response":{"pid":s.pid.load(std::sync::atomic::Ordering::SeqCst)}}});
            s.emit(seq, "client", reply.clone()).await;
            s.emit(seq + 1, "worker", reply).await;
            s.emit(seq + 2,"worker",json!({"type":"system","subtype":"init","session_id":s.identity.bridge_session,"cwd":s.cwd})).await;
            axum::http::StatusCode::OK
        } else if p["type"] == "control_request" && p["request"]["subtype"] == "interrupt" {
            s.post_started.notify_one();
            s.release_post.notified().await;
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        }
    }
    async fn receipts(
        State(s): State<Server>,
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> Json<Value> {
        if q.get("limit").is_some_and(|n| n == "1") {
            return Json(
                json!({"data":s.history.lock().unwrap().last().into_iter().cloned().collect::<Vec<_>>()}),
            );
        }
        if q.get("sort_order").is_some_and(|s| s == "asc") {
            let after: u64 = q["cursor"].parse().unwrap();
            return Json(
                json!({"data":s.history.lock().unwrap().iter().filter(|r| r["sequence_num"].as_str().unwrap().parse::<u64>().unwrap() > after).cloned().collect::<Vec<_>>()}),
            );
        }
        let input = s.posts.lock().unwrap()[2]["events"][0]["payload"]["uuid"].clone();
        if !q.contains_key("cursor") {
            Json(
                json!({"data":[{"event_id":"unrelated","source":"worker","payload":{"type":"assistant"}}],"resume_cursor":"20"}),
            )
        } else {
            assert_eq!(q["cursor"], "20");
            Json(
                json!({"data":[{"event_id":input,"source":"client","payload":{"type":"user"},"processed_at":"2026-10-22T01:00:00Z"}],"resume_cursor":"19"}),
            )
        }
    }
    #[tokio::test]
    async fn in_stream_owner_checks_survive_same_owner_lost_reply_and_repeated_replacement() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity {
            native_session: uuid::Uuid::new_v4().to_string(),
            bridge_session: "cse_observer_test".into(),
        };
        let (send, receive) = mpsc::channel(8);
        let server = Server {
            identity: identity.clone(),
            cwd: dir.path().into(),
            send,
            receive: Arc::new(Mutex::new(Some(receive))),
            posts: Default::default(),
            post_started: Default::default(),
            release_post: Default::default(),
            connected: Arc::new(AtomicBool::new(true)),
            history: Default::default(),
            pid: Arc::new(AtomicU64::new(777)),
            lose_init: Default::default(),
        };
        server
            .emit(
                1,
                "worker",
                json!({"type":"system","subtype":"worker_shutting_down"}),
            )
            .await;
        let app = Router::new()
            .route("/cse_observer_test", get(metadata))
            .route("/cse_observer_test/events/stream", get(stream))
            .route("/cse_observer_test/events", post(self::send).get(receipts))
            .with_state(server.clone());
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", socket.local_addr().unwrap());
        let serving = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let remote = Remote::fixture(&base, identity.clone(), 0).await.unwrap();
        let mut observer = Observer::from_remote(
            remote,
            Link {
                identity: identity.clone(),
                cwd: dir.path().canonicalize().unwrap(),
                title: "Test".into(),
                internal_inputs: vec![],
            },
            Cursor::new(identity.clone()).unwrap(),
        )
        .await
        .unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let chat = json!({"agent":"claude","native_id":identity.native_session,"remote_identity":identity,"thread":{"id":"owner-check-chat","messages":[],"activities":[],"proposedPlans":[]}});
        store.save_chat(&chat, None).unwrap();
        let db = AuthService::new(Auth::new(store)).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut router = crate::claude_turns::Router::default();
        router
            .register(Input {
                native: id.clone(),
                message: "message".into(),
                public_turn: "turn".into(),
                local: true,
                steer: false,
            })
            .unwrap();
        router
            .observe(&json!({"type":"assistant","user_message_uuids":[id]}))
            .unwrap();
        let mut projection = json!({"router":router,"owner_pid":777});
        async fn pump(o: &mut Observer, db: &AuthService, chat: &Value, p: &mut Value) -> bool {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), o.next())
                .await
                .unwrap()
                .unwrap();
            let replaced = o.pending_receipt.as_ref().is_some_and(|r| r.owner_replaced);
            if event.source == "local" {
                p["owner_pid"] = event.payload.unwrap()["pid"].clone();
            }
            o.commit(db, chat.clone(), p.clone()).await.unwrap();
            replaced
        }
        while !observer.ready() {
            assert!(!pump(&mut observer, &db, &chat, &mut projection).await);
        }
        // A healthy owner's answer must not disable checks for future crashes.
        for pid in [777, 888, 999] {
            server.pid.store(pid, Ordering::SeqCst);
            observer.last_event = std::time::Instant::now() - std::time::Duration::from_secs(11);
            observer.remote.check_health_now();
            let mut replacements = 0;
            for _ in 0..if pid == 777 { 3 } else { 4 } {
                replacements += usize::from(pump(&mut observer, &db, &chat, &mut projection).await);
            }
            assert_eq!(replacements, usize::from(pid != 777));
            assert_eq!(projection["owner_pid"], pid);
            assert!(observer.ready());
            assert!(observer.owner_recheck.is_none());
        }
        // A lost metadata response cannot permanently suppress checks or fill
        // the pending-control budget. It never permits replaying a mutation.
        server.lose_init.store(true, Ordering::SeqCst);
        observer.last_event = std::time::Instant::now() - std::time::Duration::from_secs(11);
        observer.remote.check_health_now();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), observer.next())
                .await
                .is_err()
        );
        let (lost, sent) = observer.owner_recheck.as_mut().unwrap();
        let lost = lost.clone();
        *sent -= std::time::Duration::from_secs(31);
        server.lose_init.store(false, Ordering::SeqCst);
        observer.remote.check_health_now();
        for _ in 0..3 {
            assert!(!pump(&mut observer, &db, &chat, &mut projection).await);
        }
        assert!(observer.owner_recheck.is_none());
        assert!(observer.remote.prepare_send(json!({"type":"control_request","request_id":lost,"request":{"subtype":"initialize"}})).is_ok(), "expired read bookkeeping is released");
        assert_eq!(server.posts.lock().unwrap().len(), 6);
        assert!(server
            .posts
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["events"][0]["payload"]["request"]["subtype"] == "initialize"));
        assert!(observer.remote.forget_read("mutation-1").is_err());
        // A shutdown in old catch-up did not block the fresh handshake, but
        // an actual shutdown after readiness must immediately revoke control.
        server
            .emit(
                observer.cursor.sequence + 1,
                "worker",
                json!({"type":"system","subtype":"worker_shutting_down"}),
            )
            .await;
        assert!(!pump(&mut observer, &db, &chat, &mut projection).await);
        assert!(!observer.ready());
        assert!(observer
            .next()
            .await
            .unwrap_err()
            .contains("worker stopped"));
        serving.abort();
    }

    #[tokio::test]
    async fn attachment_requires_fresh_worker_init_and_durable_commit_without_automatic_resend() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity {
            native_session: uuid::Uuid::new_v4().to_string(),
            bridge_session: "cse_observer_test".into(),
        };
        let (send_tx, rx) = mpsc::channel(8);
        let server = Server {
            identity: identity.clone(),
            cwd: dir.path().into(),
            send: send_tx,
            receive: Arc::new(Mutex::new(Some(rx))),
            posts: Default::default(),
            post_started: Default::default(),
            release_post: Default::default(),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            history: Default::default(),
            pid: Arc::new(std::sync::atomic::AtomicU64::new(777)),
            lose_init: Default::default(),
        };
        server.emit(1,"worker",json!({"type":"system","subtype":"init","session_id":identity.bridge_session,"cwd":dir.path()})).await;
        let app = Router::new()
            .route("/cse_observer_test", get(metadata))
            .route("/cse_observer_test/events/stream", get(stream))
            .route("/cse_observer_test/events", post(send).get(receipts))
            .with_state(server.clone());
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", socket.local_addr().unwrap());
        let serving = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let remote = Remote::fixture(&base, identity.clone(), 0).await.unwrap();
        let cursor = Cursor::new(identity.clone()).unwrap();
        let mut observer = Observer::from_remote(
            remote,
            Link {
                identity: identity.clone(),
                // Real links are canonicalized by claude_link::read. On
                // macOS temp paths are symlinks; Windows adds a verbatim
                // prefix. Keep the fixture faithful to that invariant.
                cwd: dir.path().canonicalize().unwrap(),
                title: "Test".into(),
                internal_inputs: vec![],
            },
            cursor,
        )
        .await
        .unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let chat = json!({"agent":"claude","native_id":identity.native_session,"remote_identity":identity,"thread":{"id":"observer-chat","messages":[],"activities":[],"proposedPlans":[]}});
        store.save_chat(&chat, None).unwrap();
        let db = AuthService::new(Auth::new(store)).unwrap();
        assert_eq!(observer.next().await.unwrap().recovery, Some(true));
        observer
            .commit(&db, chat.clone(), Value::Null)
            .await
            .unwrap();
        for seq in 1..=4 {
            if seq == 4 {
                let profile = dir.path().join("profile");
                let transcripts = profile.join("projects").join("fixture");
                std::fs::create_dir_all(&transcripts).unwrap();
                let path = transcripts.join(format!("{}.jsonl", identity.native_session));
                let mut row = json!({"type":"bridge-session","sessionId":identity.native_session,
                    "bridgeSessionId":"cse_changed_during_handshake","cwd":dir.path()});
                std::fs::write(&path, format!("{row}\n")).unwrap();
                observer.link_source = Some((profile, dir.path().into()));
                assert!(observer
                    .next()
                    .await
                    .unwrap_err()
                    .contains("binding changed"));
                assert!(!observer.ready());
                // The already received frame survives cancellation/validation
                // failure; readiness still requires the original binding.
                row["bridgeSessionId"] = json!(identity.bridge_session);
                std::fs::write(&path, format!("{row}\n")).unwrap();
            }
            let e = observer.next().await.unwrap();
            assert_eq!(e.sequence, Some(seq));
            assert!(!observer.ready());
            assert!(observer.control(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}})).await.is_err());
            if seq == 2 {
                assert!(
                    e.payload.is_none(),
                    "another client's response cannot authenticate the worker"
                );
            }
            assert!(
                observer.next().await.is_err(),
                "uncommitted history must apply backpressure"
            );
            if seq == 4 {
                let mut invalid = chat.clone();
                invalid["thread"]["messages"] = json!([{"text":"invalid row"}]);
                assert!(observer.commit(&db, invalid, Value::Null).await.is_err());
                assert!(!observer.ready());
                assert_eq!(observer.cursor().sequence, 3);
            }
            observer
                .commit(&db, chat.clone(), Value::Null)
                .await
                .unwrap();
        }
        assert!(
            !observer.ready(),
            "even a fresh handshake waits for the recovery boundary"
        );
        assert_eq!(observer.next().await.unwrap().recovery, Some(false));
        assert!(!observer.ready());
        // History last committed under another worker process.
        let projection = json!({"router":crate::claude_turns::Router::default(),"history_turn":null,"owner_pid":1});
        observer
            .commit(&db, chat.clone(), projection)
            .await
            .unwrap();
        assert!(
            !observer.ready(),
            "the verified owner process is checkpointed before any send"
        );
        // A different process than the last committed owner means any turn
        // the previous process held can no longer finish.
        let owner = observer.next().await.unwrap();
        assert_eq!(owner.source, "local");
        assert_eq!(owner.sequence, None);
        assert_eq!(owner.payload.as_ref().unwrap()["pid"], 777);
        assert!(owner.needs_commit());
        assert!(observer.pending_receipt.as_ref().unwrap().owner_replaced);
        let sequence = observer.cursor.sequence;
        observer
            .commit(&db, chat.clone(), json!({"owner_pid":777}))
            .await
            .unwrap();
        assert_eq!(
            observer.cursor.sequence, sequence,
            "a local checkpoint is not history"
        );
        assert!(observer.ready());
        let delivery = observer.prepare_control(json!({"type":"control_request","request_id":"slow-stop","request":{"subtype":"interrupt"}})).unwrap();
        let posting = tokio::spawn(delivery.deliver());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.post_started.notified(),
        )
        .await
        .unwrap();
        server.emit(5,"worker",json!({"type":"system","subtype":"status","status":null,"session_id":identity.bridge_session})).await;
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), observer.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.sequence,
            Some(5),
            "slow POST must not stall stream reads"
        );
        assert!(
            observer.ready(),
            "an ordinary pending commit does not undo a verified handshake"
        );
        observer
            .commit(&db, chat.clone(), Value::Null)
            .await
            .unwrap();
        server.release_post.notify_one();
        posting.await.unwrap().unwrap();
        let input = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: "phone-input".into(),
            public_turn: "turn-input".into(),
            local: true,
            steer: false,
        };
        let payload = json!({"type":"user","uuid":input.native,"session_id":identity.native_session,"message":{"role":"user","content":"A test prompt"}});
        assert!(observer
            .send_prompt(&db, "observer-chat".into(), input.clone(), payload.clone())
            .await
            .is_err());
        assert!(observer
            .send_prompt(&db, "observer-chat".into(), input.clone(), payload)
            .await
            .is_err());
        assert_eq!(
            server.posts.lock().unwrap().len(),
            3,
            "only initialize, slow control and one uncertain prompt POST"
        );
        assert_eq!(
            server.posts.lock().unwrap()[2]["events"][0]["payload"]["origin"]["kind"],
            "human"
        );
        // Older workers can report processed without read-receipt semantics.
        // Until a fresh initialize advertises support, it proves nothing.
        let receipt = json!({"event_id":input.native,"status":"DELIVERY_STATUS_PROCESSED","timestamp":"2026-10-22T01:00:00Z"});
        server
            .send
            .send(format!("event: delivery_update\ndata: {receipt}\n\n"))
            .await
            .unwrap();
        let event = observer.next().await.unwrap();
        assert!(event.deliveries.is_empty());
        observer
            .commit(&db, chat.clone(), Value::Null)
            .await
            .unwrap();
        let sid = identity.native_session.clone();
        let iid = input.native.clone();
        assert_eq!(
            db.call(move |a| a.store.claude_input("observer-chat", &sid, &iid))
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state,
            "offered"
        );
        // A fresh supported worker can recover a lost ephemeral receipt via
        // two bounded pages. The cursor and public history do not advance.
        observer.read_receipts = true;
        observer
            .recover_deliveries(vec![input.native.clone()])
            .unwrap();
        let seq = observer.cursor.sequence;
        let event = observer.next().await.unwrap();
        assert!(!observer.ready());
        assert!(event.needs_commit());
        assert!(event.deliveries[0].processed);
        assert_eq!(event.deliveries[0].input, input.native);
        observer
            .commit(&db, chat.clone(), Value::Null)
            .await
            .unwrap();
        assert!(observer.ready());
        assert_eq!(observer.cursor.sequence, seq);
        let sid = identity.native_session.clone();
        let iid = input.native.clone();
        assert_eq!(
            db.call(move |a| a.store.claude_input("observer-chat", &sid, &iid))
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        server
            .connected
            .store(false, std::sync::atomic::Ordering::SeqCst);
        observer.remote.check_health_now();
        assert!(observer.next().await.unwrap_err().contains("disconnected"));
        drop(observer);
        assert_eq!(
            server.posts.lock().unwrap().len(),
            3,
            "dropping an observer cannot control the owner"
        );
        serving.abort();
    }
}
