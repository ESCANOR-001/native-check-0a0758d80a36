#![cfg(unix)]
use remodex_connect_native::{
    agent::SharedServer,
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    desktop_activation::Activation,
    desktop_ipc::Endpoint,
    store::Store,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NATIVE: &str = "12345678-1234-4234-8234-123456789abc";

async fn call(hub: &Chats, method: &str, params: Value) -> Value {
    tokio::time::timeout(
        Duration::from_secs(35),
        hub.call(AgentKind::Codex, "phone", method, params),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn browsing_reconnect_and_work_reads_never_open_desktop_but_sending_can() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let transcript = profile.join("sessions/saved.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"saved"}})
        ),
    )
    .unwrap();
    let state = profile.join("desktop-state.json");
    let saved = json!({"id":"saved","status":"completed","startedAt":1700000000,"completedAt":1700000010,"items":[
        {"turnId":"saved","item":{"id":"prompt","type":"userMessage","content":[{"type":"text","text":"SAVED_PROMPT"}]},"startedAtMs":1700000000000_u64,"completedAtMs":1700000000001_u64},
        {"turnId":"saved","item":{"id":"work","type":"commandExecution","command":"cat README.md","status":"completed"},"startedAtMs":1700000000002_u64,"completedAtMs":1700000000003_u64},
        {"turnId":"saved","item":{"id":"reply","type":"agentMessage","text":"SAVED_REPLY"},"startedAtMs":1700000009000_u64,"completedAtMs":1700000010000_u64}
    ]});
    std::fs::write(&state, json!({"turns":[saved.clone()]}).to_string()).unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"window","path":transcript}).to_string(),
    )
    .unwrap();
    let socket = profile.join("ipc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let marker = dir.path().join("activation.log");
    let log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = log.clone();
    let activated = marker.clone();
    let native_file = transcript.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let log = seen.clone();
            let marker = activated.clone();
            let state = state.clone();
            let transcript = native_file.clone();
            connections.spawn(async move {
                while let Ok(size) = stream.read_u32_le().await {
                    assert!(size < 65536);
                    let mut bytes = vec![0; size as usize];
                    if stream.read_exact(&mut bytes).await.is_err() { break; }
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let method = request["method"].as_str().unwrap();
                    let mut reply = json!({"type":"response","requestId":request["requestId"],"method":method,"resultType":"success","handledByClientId":"original","result":{}});
                    match method {
                        "initialize" => reply["result"] = json!({"clientId":"reader"}),
                        "thread-owner-discovery" if !marker.exists() => {
                            reply["resultType"] = json!("error");
                            reply["error"] = json!("no-client-found");
                        }
                        "thread-owner-discovery" => {},
                        "thread-stream-following-changed" => {
                            reply = json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{"conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":1,"conversationState":{"id":NATIVE,"hostId":"local","turns":[],"requests":[]}}}});
                        }
                        "thread-follower-start-turn" => {
                            assert!(marker.exists());
                            assert_eq!(request["targetClientId"], "original");
                            let mut history: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
                            history["turns"].as_array_mut().unwrap().push(json!({"id":"current","status":"inProgress","startedAt":1700000020,"items":[]}));
                            std::fs::write(&state, history.to_string()).unwrap();
                            use std::io::Write;
                            writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(), "{}", json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"current"}})).unwrap();
                            reply["result"] = json!({"result":{"turn":{"id":"current","status":"inProgress"}}});
                        }
                        "thread-follower-compact-thread" => {
                            assert!(marker.exists());
                            assert_eq!(request["targetClientId"], "original");
                            reply["result"] = json!({"result":{}});
                        }
                        other => panic!("Unexpected Desktop action: {other}"),
                    }
                    let bytes = serde_json::to_vec(&reply).unwrap();
                    if stream.write_u32_le(bytes.len() as u32).await.is_err() || stream.write_all(&bytes).await.is_err() { break; }
                }
            });
        }
    });
    let mut config = Config {
        storage: Default::default(),
        native_installation: None,
        native_profile_handoff: None,
        connection: None,
        max_active_chats: 1,
        workspace: dir.path().into(),
        claude: None,
        codex: Some(AgentConfig {
            executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
            profile: profile.clone(),
            model: "test".into(),
            tools: Default::default(),
            remote_control: false,
            shared_server: None,
        }),
    };
    let db = AuthService::new(Auth::new(Store::open(&dir.path().join("store")).unwrap())).unwrap();
    let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
    call(&hub, "orchestration.dispatchCommand", json!({"type":"thread.create","commandId":"create","threadId":"browse","projectId":"native-test-project"})).await;
    call(&hub, "native.shutdown", json!({})).await;
    db.call(|a| {
        let mut chat = a.store.load_chat("browse")?;
        chat["native_id"] = json!(NATIVE);
        chat["external_codex"] = json!(true);
        a.store.save_chat(&chat, None)
    })
    .await
    .unwrap()
    .unwrap();
    config.codex.as_mut().unwrap().shared_server = Some(SharedServer::Desktop {
        desktop_endpoint: Endpoint { path: socket },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: dir.path().into(),
        activation: Some(Activation::Command {
            executable: env!("CARGO_BIN_EXE_history-fixture").into(),
            arguments: vec!["--activate".into(), marker.to_str().unwrap().into()],
        }),
    });
    // The old subscription flag must not turn a history watch into user consent
    // to navigate Desktop. Check both first open and reconnect to saved state.
    for _ in 0..2 {
        let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
        call(
            &hub,
            "native.watchShared",
            json!({"threadId":"browse","activateDesktop":true}),
        )
        .await;
        let thread = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let detail = call(
                    &hub,
                    "orchestration.subscribeThread",
                    json!({"threadId":"browse"}),
                )
                .await;
                let t = &detail["snapshot"]["thread"];
                if t["messages"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|r| r["text"] == "SAVED_REPLY"))
                {
                    break t.clone();
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        for _ in 0..3 {
            call(
                &hub,
                "native.watchShared",
                json!({"threadId":"browse","activateDesktop":true}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let deferred = thread["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "work.deferred")
            .expect("Saved turn must expose its unloaded work");
        let work = call(&hub, "orchestration.getTurnWork", json!({"threadId":"browse","turnId":deferred["turnId"],"cursor":deferred["payload"]["cursor"]})).await;
        assert!(work["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "work"));
        assert!(
            !marker.exists(),
            "Selecting/reopening a phone chat must not open Desktop"
        );
        assert!(!log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "thread-follower-start-turn"));
        call(&hub, "native.shutdown", json!({})).await;
    }
    let hub = Chats::start(db.clone(), config).await.unwrap();
    // An explicit /compact is allowed to reopen the original route. Its
    // preflight must not fail solely because passive browsing left it closed.
    let saved_transcript = std::fs::read(&transcript).unwrap();
    let compacted = call(
        &hub,
        "codex.compact",
        json!({"threadId":"browse","commandId":"compact-dormant"}),
    )
    .await;
    assert_eq!(compacted["accepted"], true);
    assert_eq!(std::fs::read(&transcript).unwrap(), saved_transcript);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    {
        let requests = log.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread-follower-compact-thread")
                .count(),
            1
        );
        assert!(!requests
            .iter()
            .any(|r| r["method"] == "thread-follower-start-turn"));
    }
    std::fs::remove_file(&marker).unwrap();
    call(&hub, "orchestration.dispatchCommand", json!({"type":"thread.turn.start","threadId":"browse","commandId":"send-once","message":{"messageId":"phone-send","text":"EXPLICIT_SEND","attachments":[]},"deliveryMode":"queue"})).await;
    let started = tokio::time::timeout(Duration::from_secs(10), async {
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "thread-follower-start-turn")
        {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    assert!(
        started.is_ok(),
        "Send did not reach Desktop: {}; reader: {}; IPC: {:?}",
        call(
            &hub,
            "orchestration.subscribeThread",
            json!({"threadId":"browse"})
        )
        .await,
        std::fs::read_to_string(profile.join("methods.log")).unwrap_or_default(),
        log.lock().unwrap()
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "thread-follower-start-turn")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(profile.join("methods.log"))
            .unwrap()
            .lines()
            .filter(|m| *m == "thread/resume" || *m == "turn/start")
            .count(),
        0,
        "Private reader must never own the task"
    );
    call(&hub, "native.shutdown", json!({})).await;
    server.abort();
    let _ = server.await;
}
