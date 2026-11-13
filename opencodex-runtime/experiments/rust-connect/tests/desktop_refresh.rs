#![cfg(unix)]
use remodex_connect_native::{
    agent::SharedServer,
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    desktop_ipc::Endpoint,
    store::Store,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const NATIVE: &str = "12345678-1234-4234-8234-123456789abc";
const TURN: &str = "12345678-1234-4234-8234-123456789abd";
async fn call(h: &Chats, m: &str, p: Value) -> Value {
    tokio::time::timeout(
        Duration::from_secs(20),
        h.call(AgentKind::Codex, "phone", m, p),
    )
    .await
    .unwrap()
    .unwrap()
}
fn update(profile: &Path, status: &str, revision: i64) {
    let turn = if revision == 1 { "old" } else { TURN };
    let turns = if revision == 1 {
        json!([])
    } else {
        json!([{"id":TURN,"turnId":TURN,"status":status,"startedAt":1800000000_i64,"completedAt":if status=="inProgress"{Value::Null}else{json!(1800000010_i64)},"items":[
            {"turnId":TURN,"item":{"id":"desktop-prompt","type":"userMessage","content":[{"type":"text","text":"DESKTOP_PROMPT"}]},"startedAtMs":1800000000000_i64,"completedAtMs":1800000000001_i64},
            {"turnId":TURN,"item":{"id":"desktop-reply","type":"agentMessage","text":"DESKTOP_REPLY"},"startedAtMs":1800000000002_i64,"completedAtMs":1800000000003_i64}
        ]}])
    };
    let state = json!({"revision":revision,"turns":turns,"inventory":[{"id":NATIVE,"name":"Original title","source":"cli","cwd":profile,"createdAt":1700000000,"updatedAt":1800000000_i64+revision,"status":{"type":"notLoaded"}}]});
    std::fs::write(profile.join("desktop-state.next"), state.to_string()).unwrap();
    std::fs::rename(
        profile.join("desktop-state.next"),
        profile.join("desktop-state.json"),
    )
    .unwrap();
    writeln!(std::fs::OpenOptions::new().append(true).open(profile.join("sessions/chat.jsonl")).unwrap(),"{}",json!({"type":"event_msg","payload":{"type":if status=="inProgress"{"task_started"}else{"task_complete"},"turn_id":turn}})).unwrap();
}
async fn wait_status(h: &Chats, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let v = call(h, "orchestration.subscribeShell", json!({})).await;
            let t = &v["snapshot"]["threads"][0];
            if t["session"]["executionState"] == expected {
                return t.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("List status did not refresh without opening the chat")
}
#[tokio::test]
async fn android_origin_adopts_verified_desktop_owner_and_idle_list_detects_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let transcript = profile.join("sessions/chat.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}})
        ),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"window","path":transcript}).to_string(),
    )
    .unwrap();
    update(&profile, "completed", 1);
    let socket = profile.join("ipc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = log.clone();
    let fixture = profile.clone();
    let server = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let log = seen.clone();
            let root = fixture.clone();
            peers.spawn(async move {
                while let Ok(n)=stream.read_u32_le().await {
                    assert!(n<65536);let mut bytes=vec![0;n as usize];if stream.read_exact(&mut bytes).await.is_err(){break;}
                    let r:Value=serde_json::from_slice(&bytes).unwrap();let m=r["method"].as_str().unwrap();
                    log.lock().unwrap().push(m.into());
                    let state:Value=serde_json::from_slice(&std::fs::read(root.join("desktop-state.json")).unwrap()).unwrap();
                    let mut reply=json!({"type":"response","requestId":r["requestId"],"method":m,"resultType":"success","handledByClientId":"desktop-owner","result":{}});
                    match m {
                        "initialize"=>reply["result"]=json!({"clientId":"observer"}),
                        "query-cache-invalidate"=>continue,
                        "thread-owner-discovery" if !root.join("owned").exists()=>{reply["resultType"]=json!("error");reply["error"]=json!("no-client-found");},
                        "thread-owner-discovery"=>{},
                        "thread-stream-following-changed"=>reply=json!({"type":"broadcast","sourceClientId":"desktop-owner","method":"thread-stream-state-changed","params":{"conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":state["revision"],"conversationState":{"id":NATIVE,"hostId":"local","turns":state["turns"],"requests":[]}}}}),
                        "thread-follower-start-turn"=>{
                            assert_eq!(r["targetClientId"],"desktop-owner");
                            reply["result"]=json!({"result":{"turn":{"id":TURN,"status":"inProgress"}}});
                        }
                        other=>panic!("Unexpected Desktop mutation: {other}"),
                    }
                    let bytes=serde_json::to_vec(&reply).unwrap();
                    if stream.write_u32_le(bytes.len() as u32).await.is_err() || stream.write_all(&bytes).await.is_err(){break;}
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
    call(&hub,"orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":"android-alias","projectId":"native-test-project"})).await;
    call(&hub, "native.shutdown", json!({})).await;
    db.call(|a|{
        let mut c=a.store.load_chat("android-alias")?;c["native_id"]=json!(NATIVE);c["resumed"]=json!(true);
        c["thread"]["title"]=json!("Keep my title");
        c["thread"]["messages"]=json!([{"id":"saved-message","role":"user","text":"KEEP_HISTORY","createdAt":"2023-11-14T22:13:20Z","streaming":false}]);
        a.store.save_chat(&c,None)
    }).await.unwrap().unwrap();
    config.codex.as_mut().unwrap().shared_server = Some(SharedServer::Desktop {
        desktop_endpoint: Endpoint { path: socket },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: dir.path().into(),
        activation: None,
    });
    let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        db.call(|a| a.store.load_chat("android-alias"))
            .await
            .unwrap()
            .unwrap()["external_codex"],
        false,
        "No owner is not permission to promote or start one"
    );
    std::fs::write(profile.join("owned"), b"owner").unwrap();
    // Reads fail beyond the old ten-second idle watch. Recovery must retry
    // this same native revision without a phone-open or another Desktop turn.
    std::fs::write(profile.join("read-failures"), b"3").unwrap();
    update(&profile, "inProgress", 2);
    let running = wait_status(&hub, "running").await;
    assert_eq!(running["id"], "android-alias");
    assert_eq!(running["nativeSessionId"], NATIVE);
    assert_eq!(running["title"], "Keep my title");
    assert_eq!(
        std::fs::read_to_string(profile.join("read-failures")).unwrap(),
        "0"
    );
    assert_eq!(
        db.call(|a| a.store.load_chat("android-alias"))
            .await
            .unwrap()
            .unwrap()["external_codex"],
        true
    );
    update(&profile, "completed", 3);
    wait_status(&hub, "idle").await;
    // Let the idle watch expire, then start again without opening/subscribing to the chat.
    tokio::time::sleep(Duration::from_secs(11)).await;
    update(&profile, "inProgress", 4);
    wait_status(&hub, "running").await;
    update(&profile, "completed", 5);
    wait_status(&hub, "idle").await;
    // The display window now follows Desktop; the original saved row must
    // remain intact even when outside that bounded window.
    let saved = db
        .call(|a| {
            a.store
                .history_row("android-alias", "messages", "saved-message")
        })
        .await
        .unwrap()
        .unwrap()
        .expect("Original history was removed during ownership handoff");
    assert_eq!(saved["text"], "KEEP_HISTORY");
    assert_eq!(saved["role"], "user");
    assert!(!log
        .lock()
        .unwrap()
        .iter()
        .any(|m| m == "thread-follower-start-turn"));
    call(&hub, "native.shutdown", json!({})).await;
    // An explicit send must check ownership even before the next inventory
    // scan has adopted an Android-created chat.
    db.call(|a| {
        let mut c = a.store.load_chat("android-alias")?;
        c["external_codex"] = json!(false);
        a.store.save_chat(&c, None)
    })
    .await
    .unwrap()
    .unwrap();
    let hub = Chats::start(db.clone(), config).await.unwrap();
    call(&hub,"orchestration.dispatchCommand",json!({"type":"thread.turn.start","threadId":"android-alias","commandId":"send-once","message":{"messageId":"phone-send","text":"EXPLICIT_SEND","attachments":[]},"deliveryMode":"queue"})).await;
    tokio::time::timeout(Duration::from_secs(8), async {
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "thread-follower-start-turn")
        {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "thread-follower-start-turn")
            .count(),
        1
    );
    assert!(!std::fs::read_to_string(profile.join("methods.log"))
        .unwrap()
        .lines()
        .any(|m| matches!(m, "thread/resume" | "turn/start")));
    call(&hub, "native.shutdown", json!({})).await;
    server.abort();
    let _ = server.await;
}
