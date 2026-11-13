use futures_util::{SinkExt, StreamExt};
use remodex_connect_native::{
    agent::SharedServer,
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct Fixture {
    _dir: tempfile::TempDir,
    hub: Chats,
    log: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(mode: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "fixture-private-shared-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(vec![]));
        let recorded = log.clone();
        let server = tokio::spawn(async move {
            let mut submitted = false;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let v: Value = serde_json::from_str(&text).unwrap();
                    recorded.lock().unwrap().push(v.clone());
                    let method = v["method"].as_str().unwrap_or("");
                    if mode == "delayed-admission" && submitted && method == "thread/read" {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    let result = match method {
                        "initialize" => json!({}),
                        "initialized" => continue,
                        "model/list" => {
                            json!({"data":[{"model":"test","displayName":"Test","supportedReasoningEfforts":[]}]})
                        }
                        "skills/list" => json!({"data":[]}),
                        "plugin/installed" => json!({"marketplaces":[]}),
                        "config/read" => json!({"config":{"mcp_servers":{}}}),
                        "account/rateLimits/read" => json!({}),
                        "thread/start" | "thread/resume" | "thread/read" => {
                            if method != "thread/read" {
                                assert_eq!(v["params"]["excludeTurns"], true);
                            }
                            if method == "thread/read" {
                                assert_eq!(v["params"]["includeTurns"], false);
                            }
                            let status = if mode == "busy" && submitted {
                                "active"
                            } else {
                                "idle"
                            };
                            json!({"thread":{"id":"shared-fixture","status":{"type":status}}})
                        }
                        "thread/turns/list" => {
                            json!({"data":[{"id":"owned-turn","status":if mode == "provider-error" {"failed"}else{"completed"},"error":if mode=="provider-error" {json!({"message":"Error running remote compact task: {\"detail\":\"Bad Request\"}","codexErrorInfo":"badRequest"})}else{Value::Null},"startedAt":1700000000,"completedAt":1700000001}],"nextCursor":null})
                        }
                        "thread/items/list" => {
                            json!({"data":[{"turnId":"owned-turn","item":{"id":"owned-message","type":"agentMessage","text":"OWNED_OUTPUT"}}],"nextCursor":null})
                        }
                        "turn/start" => {
                            submitted = true;
                            json!({"turn":{"id":"owned-turn"}})
                        }
                        "thread/compact/start" => {
                            assert_eq!(v["params"], json!({"threadId":"shared-fixture"}));
                            json!({})
                        }
                        "turn/interrupt" => {
                            assert_eq!(v["params"]["threadId"], "shared-fixture");
                            assert_eq!(v["params"]["turnId"], "owned-turn");
                            json!({})
                        }
                        _ => panic!("Unexpected shared request: {method}"),
                    };
                    if method == "turn/start" {
                        for event in [
                            json!({"method":"turn/started","params":{"threadId":"shared-fixture","turn":{"id":"foreign-before-ack"}}}),
                            json!({"id":801,"method":"item/commandExecution/requestApproval","params":{"threadId":"shared-fixture","turnId":"foreign-before-ack","command":"foreign before ack"}}),
                            json!({"method":"item/completed","params":{"threadId":"shared-fixture","turnId":"owned-turn","item":{"id":"early-message","type":"agentMessage","text":"PRE_ACK_OWN"}}}),
                        ] {
                            ws.send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                    if ws
                        .send(Message::Text(
                            json!({"id":v["id"],"result":result}).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if method == "turn/start" {
                        for event in [
                            json!({"method":"turn/started","params":{"threadId":"shared-fixture","turn":{"id":"foreign-turn"}}}),
                            json!({"id":800,"method":"item/commandExecution/requestApproval","params":{"threadId":"shared-fixture","turnId":"foreign-turn","command":"foreign command"}}),
                            json!({"method":"turn/completed","params":{"threadId":"shared-fixture","turn":{"id":"foreign-turn","status":"completed"}}}),
                            json!({"method":"item/completed","params":{"threadId":"shared-fixture","turnId":"owned-turn","item":{"id":"owned-message","type":"agentMessage","text":"OWNED_OUTPUT"}}}),
                        ] {
                            ws.send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        if mode != "shutdown" {
                            let error = json!({"message":"Error running remote compact task: {\"detail\":\"Bad Request\"}","codexErrorInfo":"badRequest"});
                            if mode == "provider-error" || mode == "retry-success" {
                                ws.send(Message::Text(json!({"method":"error","params":{"threadId":"shared-fixture","turnId":"owned-turn","error":error,"willRetry":mode=="retry-success"}}).to_string().into())).await.unwrap();
                            }
                            ws.send(Message::Text(json!({"method":"turn/completed","params":{"threadId":"shared-fixture","turn":{"id":"owned-turn","status":if mode=="provider-error" {"failed"}else{"completed"},"error":if mode=="provider-error" {error}else{Value::Null}}}}).to_string().into())).await.unwrap();
                        }
                    }
                }
            }
        });
        let agent = AgentConfig {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture")),
            profile: dir.path().into(),
            model: "test".into(),
            tools: Default::default(),
            shared_server: None,
            remote_control: false,
        };
        let config = Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            claude: Some(agent.clone()),
            codex: Some(AgentConfig {
                shared_server: Some(SharedServer::Capability {
                    address,
                    token_file: token,
                }),
                ..agent
            }),
        };
        let db = AuthService::new(Auth::new(Store::open(dir.path()).unwrap())).unwrap();
        let hub = Chats::start(db, config).await.unwrap();
        Self {
            _dir: dir,
            hub,
            log,
            server,
        }
    }
    async fn call(&self, c: Value) {
        self.hub
            .call(
                AgentKind::Codex,
                "phone",
                "orchestration.dispatchCommand",
                c,
            )
            .await
            .unwrap();
    }
    async fn create(&self) {
        self.call(json!({"type":"thread.create","commandId":"create","threadId":"shared","projectId":"native-test-project"})).await;
    }
    async fn send(&self, n: u32) {
        self.call(json!({"type":"thread.turn.start","commandId":format!("send-{n}"),"threadId":"shared","deliveryMode":"normal","message":{"messageId":format!("message-{n}"),"text":"test","attachments":[]}})).await;
    }
    async fn wait(&self, status: &str) -> Value {
        let mut last = Value::Null;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = self
                    .hub
                    .call(
                        AgentKind::Codex,
                        "phone",
                        "orchestration.subscribeThread",
                        json!({"threadId":"shared"}),
                    )
                    .await
                    .unwrap()["snapshot"]["thread"]
                    .clone();
                if d["session"]["status"] == status && d.to_string().contains("OWNED_OUTPUT") {
                    return d;
                }
                last = d;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        result.unwrap_or_else(|_| panic!("Wanted {status}, got {last}"))
    }
    async fn stop(&self) {
        self.hub
            .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn provider_failure_reaches_android_snapshot_without_becoming_success_or_losing_history() {
    let f = Fixture::new("provider-error").await;
    f.create().await;
    f.send(1).await;
    let snapshot = f.wait("error").await;
    let message = "Error running remote compact task: {\"detail\":\"Bad Request\"}";
    assert_eq!(snapshot["session"]["lastError"], message);
    assert_eq!(snapshot["latestTurn"]["state"], "error");
    let errors: Vec<_> = snapshot["activities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "provider.error")
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["payload"]["detail"], message);
    assert_eq!(errors[0]["payload"]["willRetry"], false);
    assert!(snapshot["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "OWNED_OUTPUT"));
    // Reopening uses persisted rows, not only a transient event on a socket.
    let reopened = f.wait("error").await;
    assert_eq!(reopened["session"]["lastError"], message);
    f.stop().await;
}

#[tokio::test]
async fn provider_retry_does_not_turn_a_successful_completion_into_failure() {
    let f = Fixture::new("retry-success").await;
    f.create().await;
    f.send(1).await;
    let snapshot = f.wait("ready").await;
    assert_eq!(snapshot["latestTurn"]["state"], "completed");
    assert!(snapshot["session"]["lastError"].is_null());
    assert!(snapshot["activities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["kind"] == "provider.error"
            && r["payload"]["status"] == "recovered"
            && r["payload"]["willRetry"] == false));
    f.stop().await;
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn stop_cancels_pending_shared_admission_without_late_delivery() {
    let f = Fixture::new("delayed-admission").await;
    f.create().await;
    f.send(1).await;
    f.wait("ready").await;
    let hub = f.hub.clone();
    let pending = tokio::spawn(async move {
        hub.call(AgentKind::Codex, "phone", "orchestration.dispatchCommand",
            json!({"type":"thread.turn.start","commandId":"cancelled-send","threadId":"shared","message":{"messageId":"cancelled-message","text":"must not run","attachments":[]}})).await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.log
                .lock()
                .unwrap()
                .iter()
                .any(|v| v["method"] == "thread/read")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_millis(500), f.call(
        json!({"type":"thread.turn.interrupt","commandId":"cancel-admission","threadId":"shared"})
    )).await.expect("Stop must not wait for the history check");
    let error = pending.await.unwrap().unwrap_err();
    assert!(error.contains("cancelled before delivery"), "{error}");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let after = f.wait("ready").await;
    assert!(!after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "cancelled-message"));
    f.stop().await;
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|v| v["method"] == "turn/start").count(),
        1
    );
    assert!(!log.iter().any(|v| v["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn shared_busy_thread_does_not_resume_send_or_interrupt_another_clients_turn() {
    let f = Fixture::new("busy").await;
    f.create().await;
    f.send(1).await;
    let first = f.wait("ready").await;
    assert!(!first.to_string().contains("foreign command"));
    assert!(first.to_string().contains("PRE_ACK_OWN"));
    assert!(!first.to_string().contains("foreign before ack"));
    let error=f.hub.call(AgentKind::Codex,"phone","orchestration.dispatchCommand",json!({"type":"thread.turn.start","commandId":"second","threadId":"shared","message":{"messageId":"second","text":"test","attachments":[]}})).await.unwrap_err();
    // Active status without an identifiable in-progress turn cannot authorize
    // any send. It remains retryable instead of claiming every Desktop task
    // categorically prevents sending from Android.
    assert!(
        error.contains("reconnecting") && error.contains("nothing was sent"),
        "{error}"
    );
    let after = f.wait("ready").await;
    assert!(!after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "second"));
    f.stop().await;
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|v| v["method"] == "turn/start").count(),
        1
    );
    assert_eq!(
        log.iter().filter(|v| v["method"] == "thread/read").count(),
        1
    );
    assert!(!log
        .iter()
        .any(|v| v["method"] == "thread/resume" || v["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn shared_shutdown_interrupts_only_the_verified_owned_turn() {
    let f = Fixture::new("shutdown").await;
    f.create().await;
    f.send(1).await;
    f.wait("running").await;
    f.stop().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.log
                .lock()
                .unwrap()
                .iter()
                .any(|v| v["method"] == "turn/interrupt")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|v| v["method"] == "turn/interrupt")
            .count(),
        1
    );
    assert!(!log.iter().any(|v| v["id"] == 800 || v["id"] == 801));
}

#[tokio::test]
async fn manual_compact_keeps_native_identity_history_and_rejects_duplicate_requests() {
    let f = Fixture::new("ready").await;
    f.create().await;
    f.send(1).await;
    let before = f.wait("ready").await;
    let params = json!({"threadId":"shared","commandId":"compact-once"});
    let result = f
        .hub
        .call(AgentKind::Codex, "phone", "codex.compact", params.clone())
        .await
        .unwrap();
    assert_eq!(result, json!({"accepted":true}));
    let error = f
        .hub
        .call(AgentKind::Codex, "phone", "codex.compact", params)
        .await
        .unwrap_err();
    assert!(error.contains("already sent"), "{error}");
    let after = f.wait("ready").await;
    for message in before["messages"].as_array().unwrap() {
        assert!(after["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == message["id"] && m["text"] == message["text"]));
    }
    f.stop().await;
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|v| v["method"] == "thread/compact/start")
            .count(),
        1
    );
    assert_eq!(
        log.iter().filter(|v| v["method"] == "turn/start").count(),
        1
    );
    assert_eq!(
        log.iter().filter(|v| v["method"] == "thread/start").count(),
        1
    );
}

#[tokio::test]
async fn manual_compact_refuses_foreign_running_task_without_resuming_or_interrupting() {
    let f = Fixture::new("busy").await;
    f.create().await;
    f.send(1).await;
    f.wait("ready").await;
    let error = f
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "codex.compact",
            json!({"threadId":"shared","commandId":"compact-busy"}),
        )
        .await
        .unwrap_err();
    assert!(error.contains("current task"), "{error}");
    f.stop().await;
    let log = f.log.lock().unwrap();
    assert!(!log.iter().any(|v| matches!(
        v["method"].as_str(),
        Some("thread/compact/start" | "turn/interrupt" | "thread/resume")
    )));
}
