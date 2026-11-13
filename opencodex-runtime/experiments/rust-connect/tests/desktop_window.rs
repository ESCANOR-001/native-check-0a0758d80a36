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
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const NATIVE: &str = "12345678-1234-4234-8234-123456789abc";
async fn detail(hub: &Chats) -> Value {
    // The network subscription renews this watch before reading its snapshot.
    // A direct Chats call must do the same; snapshots alone are read-only.
    hub.call(
        AgentKind::Codex,
        "phone",
        "native.watchShared",
        json!({"threadId":"window"}),
    )
    .await
    .unwrap();
    hub.call(
        AgentKind::Codex,
        "phone",
        "orchestration.subscribeThread",
        json!({"threadId":"window"}),
    )
    .await
    .unwrap()["snapshot"]["thread"]
        .clone()
}
#[tokio::test]
async fn latest_window_and_status_are_independent_of_older_scroll_pages() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let transcript = profile.join("sessions/rollout.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"latest"}})
        ),
    )
    .unwrap();
    let mut turns = vec![];
    for (n, id) in ["oldest", "old", "recent-oldest", "recent-old", "latest"]
        .into_iter()
        .enumerate()
    {
        let at = 1_700_000_000 + n * 100;
        let mut rows = vec![
            json!({"turnId":id,"item":{"type":"userMessage","id":format!("{id}-prompt"),"content":[{"type":"text","text":format!("{id} PROMPT")}]},"startedAtMs":at*1000,"completedAtMs":at*1000}),
        ];
        for i in 0..40 {
            if i == 10 {
                rows.push(json!({"turnId":id,"item":{"type":"commandExecution","id":format!("{id}-work-{i}"),"command":"cat README.md","commandActions":[{"type":"read","name":"README.md","path":"/tmp/README.md"}]},"startedAtMs":at*1000+i+1,"completedAtMs":at*1000+i+2}));
            } else {
                rows.push(json!({"turnId":id,"item":{"type":"reasoning","id":format!("{id}-work-{i}"),"summary":[format!("{id} THINKING {i}")],"content":[]},"startedAtMs":at*1000+i+1,"completedAtMs":at*1000+i+2}));
            }
        }
        rows.push(json!({"turnId":id,"item":{"type":"agentMessage","id":format!("{id}-answer"),"text":format!("{id} ANSWER"),"phase":if n==4{"commentary"}else{"final_answer"}},"startedAtMs":at*1000+999,"completedAtMs":at*1000+1000}));
        turns.push(json!({"id":id,"status":if n==4{"inProgress"}else{"completed"},"startedAt":at,"completedAt":at+1,"items":rows}));
    }
    let state = profile.join("desktop-state.json");
    std::fs::write(&state, json!({"turns":turns}).to_string()).unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"window","path":transcript}).to_string(),
    )
    .unwrap();
    let socket = profile.join("ipc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let status = std::sync::Arc::new(std::sync::Mutex::new("inProgress"));
    let observed = status.clone();
    let stalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ipc_stalled = stalled.clone();
    let server = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let status = observed.clone();
            let stalled = ipc_stalled.clone();
            tasks.spawn(async move {
            while let Ok(size)=stream.read_u32_le().await {let mut bytes=vec![0;size as usize];if stream.read_exact(&mut bytes).await.is_err(){break;}let v:Value=serde_json::from_slice(&bytes).unwrap();
                while stalled.load(std::sync::atomic::Ordering::Relaxed) { tokio::time::sleep(Duration::from_millis(10)).await; }
                let reply=if v["method"]=="thread-stream-following-changed" {
                    json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{"conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":1,"conversationState":{"id":NATIVE,"hostId":"local","turns":[{"turnId":"latest","status":*status.lock().unwrap(),"items":[]}],"requests":[]}}}})
                }else {
                    let result=match v["method"].as_str().unwrap(){"initialize"=>json!({"clientId":"reader"}),"thread-owner-discovery"=>json!({"handledByClientId":"original"}),other=>panic!("Unexpected mutation {other}")};
                    json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","handledByClientId":"original","result":result})
                };let bytes=reply.to_string().into_bytes();if stream.write_u32_le(bytes.len() as u32).await.is_err() || stream.write_all(&bytes).await.is_err(){break;}
            }
        });
        }
    });
    let agent = AgentConfig {
        executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
        profile: profile.clone(),
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let mut config = Config {
        storage: Default::default(),
        native_installation: None,
        native_profile_handoff: None,
        connection: None,
        max_active_chats: 1,
        workspace: dir.path().into(),
        claude: None,
        codex: Some(agent),
    };
    let db = AuthService::new(Auth::new(Store::open(&dir.path().join("store")).unwrap())).unwrap();
    let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
    hub.call(AgentKind::Codex,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":"window","projectId":"native-test-project"})).await.unwrap();
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
    db.call(|a| {
        let mut c = a.store.load_chat("window")?;
        c["native_id"] = json!(NATIVE);
        c["external_codex"] = json!(true);
        c["thread"]["messages"] = json!([{"id":"latest-answer","role":"assistant","text":"latest ANSWER","phase":"final_answer","turnId":"desktop-latest","streaming":false,"createdAt":"2023-11-14T22:20:01Z","updatedAt":"2023-11-14T22:20:01Z"}]);
        // A prior live receipt used arrival time and generic-command metadata.
        // A full Work read must correct both, even though the ID is already saved.
        c["thread"]["activities"] = json!([{"id":"latest-work-10","turnId":"desktop-latest","kind":"tool.completed","summary":"Command Execution","tone":"tool","createdAt":"2026-10-26T00:00:00Z","payload":{"itemType":"command_execution","status":"completed"}}]);
        a.store.save_chat(&c, None)
    })
    .await
    .unwrap()
    .unwrap();
    config.codex.as_mut().unwrap().shared_server = Some(SharedServer::Desktop {
        desktop_endpoint: Endpoint { path: socket },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: dir.path().into(),
        activation: None,
    });
    let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
    let current = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let t = detail(&hub).await;
            if t["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["id"] == "latest-answer" && v["phase"] == "commentary")
            {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "Desktop window did not refresh: {error}; reader requests: {}",
            std::fs::read_to_string(profile.join("requests.jsonl")).unwrap_or_default()
        )
    });
    assert_eq!(current["messages"].as_array().unwrap().len(), 6);
    assert_eq!(
        current["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "latest-answer")
            .unwrap()["phase"],
        "commentary",
        "refresh must correct a cached final-answer phase even when text is unchanged"
    );
    assert_eq!(current["messages"][0]["id"], "recent-oldest-prompt");
    assert_eq!(current["messages"][2]["id"], "recent-old-prompt");
    assert_eq!(current["messages"][4]["id"], "latest-prompt");
    assert_eq!(current["session"]["executionState"], "running");
    let shell = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.subscribeShell",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(
        shell["snapshot"]["threads"][0]["session"]["executionState"],
        "running"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let count = rusqlite::Connection::open(dir.path().join("store/native-connect.sqlite"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM chat_rows WHERE chat='window' AND kind='messages'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(count, 6, "No older messages should backfill automatically");
    let token = current["historyPage"]["olderCursor"].clone();
    assert!(token.as_str().unwrap().starts_with("d1:"));
    let older = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.getThreadPage",
            json!({"threadId":"window","cursor":token}),
        )
        .await
        .unwrap();
    // An already-open phone may still hold the old item-based cursor.
    use base64::Engine;
    let legacy = format!("d1:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!(["window",NATIVE,{"projection_version":6,"turns":null,"turn":"latest","items":"8","complete":false,"final_pass":true}]).to_string()));
    let legacy_page = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.getThreadPage",
            json!({"threadId":"window","cursor":legacy}),
        )
        .await
        .unwrap();
    assert_eq!(legacy_page["messages"][0]["id"], "recent-old-prompt");
    assert_eq!(older["activities"][0]["payload"]["turnDurationMs"], 1000);
    assert_eq!(older["messages"].as_array().unwrap().len(), 2);
    assert_eq!(older["messages"][0]["id"], "old-prompt");
    assert_eq!(older["messages"][1]["id"], "old-answer");
    assert!(
        older["messages"][1]["sequence"].as_i64().unwrap()
            < current["messages"][0]["sequence"].as_i64().unwrap()
    );
    assert_eq!(
        detail(&hub).await["messages"],
        current["messages"],
        "Older conversation must not replace the live window"
    );
    let oldest = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.getThreadPage",
            json!({"threadId":"window","cursor":older["pageInfo"]["olderCursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(oldest["messages"][0]["id"], "oldest-prompt");
    assert_eq!(oldest["messages"][1]["id"], "oldest-answer");
    assert_eq!(oldest["pageInfo"]["hasOlder"], false);
    assert!(oldest["pageInfo"]["olderCursor"].is_null());
    // Forty tool items must never be prerequisites for the earlier prompt.
    let marker = current["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "work.deferred" && a["turnId"] == "desktop-latest")
        .unwrap();
    let mut next = marker["payload"]["cursor"].clone();
    assert!(next.as_str().unwrap().starts_with("i1:"));
    let mismatched = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.getTurnWork",
            json!({"threadId":"window","turnId":"desktop-old","cursor":next}),
        )
        .await;
    assert!(mismatched.unwrap_err().contains("another response"));
    // A new turn changes the descending turn-list offsets, but must not
    // invalidate an already displayed response's Work cursor.
    let saved_state = std::fs::read(&state).unwrap();
    let mut shifted: Value = serde_json::from_slice(&saved_state).unwrap();
    let mut added = shifted["turns"][4].clone();
    added["id"] = json!("arrived-later");
    added["status"] = json!("completed");
    shifted["turns"].as_array_mut().unwrap().push(added);
    std::fs::write(&state, shifted.to_string()).unwrap();
    // Saved Work must be readable while Desktop IPC is unresponsive. No
    // discovery/status/compaction checks belong in an individual Work page.
    stalled.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut ids = std::collections::HashSet::new();
    let mut work_reads = 0;
    while next.is_string() {
        work_reads += 1;
        let work = tokio::time::timeout(
            Duration::from_secs(3),
            hub.call(
                AgentKind::Codex,
                "phone",
                "orchestration.getTurnWork",
                json!({"threadId":"window","turnId":"desktop-latest","cursor":next}),
            ),
        )
        .await
        .expect("saved Work should not wait for Desktop IPC")
        .unwrap();
        assert_eq!(work["turnId"], "desktop-latest");
        assert!(work["activities"].as_array().unwrap().len() <= 65);
        for row in work["activities"].as_array().unwrap() {
            if row["id"].as_str().unwrap().starts_with("latest-work-") {
                if row["id"] == "latest-work-10" {
                    assert_eq!(row["payload"]["itemType"], "file-read");
                    assert_eq!(
                        row["createdAt"],
                        remodex_connect_native::auth::timestamp(1700000400011)
                    );
                }
                ids.insert(row["id"].as_str().unwrap().to_owned());
            }
        }
        next = work["nextCursor"].clone();
    }
    stalled.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ids.len(), 40);
    assert_eq!(
        work_reads, 1,
        "ordinary Work fits one bounded reader session"
    );
    std::fs::write(&state, saved_state).unwrap();
    // The background reader may have observed the temporary added turn.
    // Wait for its next normal refresh before inspecting the original window.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let refreshed = detail(&hub).await;
            if refreshed["messages"] == current["messages"]
                && refreshed["session"]["executionState"] == "running"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    // Streaming can add more activity than the visible tail allows. The
    // originating prompt must still be included alongside the latest answer.
    rusqlite::Connection::open(dir.path().join("store/native-connect.sqlite")).unwrap().execute("INSERT OR IGNORE INTO native_recent_rows(chat,kind,id) SELECT chat,kind,id FROM chat_rows WHERE chat='window' AND turn='desktop-latest'",[]).unwrap();
    let full_tail = detail(&hub).await;
    assert_eq!(
        full_tail["messages"], current["messages"],
        "All three exchanges survive a long live activity tail"
    );
    assert_eq!(
        full_tail["activities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["id"].as_str().unwrap().starts_with("latest-work-"))
            .count(),
        40,
        "opening a running chat includes all cached work, not just the live tail"
    );

    assert_eq!(
        detail(&hub).await["historyPage"]["olderCursor"],
        token,
        "Work paging must not move the conversation boundary"
    );
    assert_eq!(detail(&hub).await["messages"], current["messages"]);
    assert_eq!(detail(&hub).await["session"]["executionState"], "running");
    *status.lock().unwrap() = "interrupted";
    {
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap(),
            "{}",
            json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"latest"}})
        )
        .unwrap();
    }
    hub.call(
        AgentKind::Codex,
        "phone",
        "native.watchShared",
        json!({"threadId":"window"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if detail(&hub).await["session"]["executionState"] == "paused" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
    let before = rusqlite::Connection::open(dir.path().join("store/native-connect.sqlite"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM chat_rows WHERE chat='window' AND kind='messages'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(before, 10);
    let hub = Chats::start(db.clone(), config).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let t = detail(&hub).await;
    assert_eq!(t["messages"], current["messages"]);
    assert_eq!(t["session"]["executionState"], "paused");
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
    server.abort();
    assert!(!std::fs::read_to_string(profile.join("methods.log"))
        .unwrap()
        .contains("thread/resume"));
}
