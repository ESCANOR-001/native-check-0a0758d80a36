#![cfg(unix)]
use remodex_connect_native::{
    agent::{Connection, Event, SharedServer},
    desktop_ipc::Endpoint,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const NATIVE: &str = "12345678-1234-4234-8234-123456789abc";
async fn rpc(c: &mut Connection, method: &str, params: Value) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Event::Message(v)) = c.events.recv().await {
                if v["id"] == id {
                    return v;
                }
            } else {
                panic!("connection closed")
            }
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn controls_stay_with_desktop_and_history_reader_never_resumes_the_chat() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let path = profile.join("ipc.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir(profile.join("sessions")).unwrap();
    let transcript = profile.join("sessions/saved.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"previous"}})
        ),
    )
    .unwrap();
    {
        use std::io::Write;
        writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(),"{}",
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":42000},"total_token_usage":{"total_tokens":999999999},"model_context_window":258400},"rate_limits":{"primary":{"used_percent":99,"window_minutes":10080}}}})).unwrap();
    }
    let state = profile.join("desktop-state.json");
    std::fs::write(
        &state,
        json!({"turn":"previous","status":"completed"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"desktop","path":transcript,"rateLimits":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":10080}}}}).to_string(),
    )
    .unwrap();
    let log = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = log.clone();
    let saved = state.clone();
    let native_file = transcript.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        while let Ok(size) = stream.read_u32_le().await {
            assert!(size < 65536);
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            seen.lock().unwrap().push(v.clone());
            if v["method"] == "thread-stream-following-changed" {
                assert_eq!(v["params"]["conversationId"], NATIVE);
                let b = json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{
                    "conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":1,
                    "conversationState":{"id":NATIVE,"hostId":"local","turns":[{"id":"current","status":"inProgress","items":[{"id":"desktop-question-item","type":"agentMessage","delivery":"async","questions":[{"title":"Pick a color","options":["Blue","Green"]}]}]}],"requests":[{
                        "id":42,"method":"item/commandExecution/requestApproval",
                        "params":{"threadId":NATIVE,"turnId":"current","itemId":"cmd","command":"echo approved"}
                    }]}}
                }}).to_string().into_bytes();
                stream.write_u32_le(b.len() as u32).await.unwrap();
                stream.write_all(&b).await.unwrap();
                continue;
            }
            let result = match v["method"].as_str().unwrap() {
                "initialize" => json!({"clientId":"reader"}),
                "thread-owner-discovery" => json!({"handledByClientId":"original"}),
                "thread-follower-start-turn" => {
                    assert_eq!(v["targetClientId"], "original");
                    assert_eq!(v["params"]["conversationId"], NATIVE);
                    std::fs::write(
                        &saved,
                        json!({"turn":"current","status":"inProgress"}).to_string(),
                    )
                    .unwrap();
                    use std::io::Write;
                    writeln!(std::fs::OpenOptions::new().append(true).open(&native_file).unwrap(),"{}",json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"current"}})).unwrap();
                    json!({"result":{"turn":{"id":"current","status":"inProgress"}}})
                }
                "thread-follower-steer-turn" => {
                    let params = &v["params"];
                    assert_eq!(params["input"][0]["text"], "synthetic steer");
                    assert_eq!(params["input"][1]["type"], "localImage");
                    assert_eq!(params["restoreMessage"]["text"], "synthetic steer");
                    assert_eq!(params["restoreMessage"]["id"], "phone-steer");
                    assert!(params["restoreMessage"]["context"]["workspaceRoots"].is_array());
                    assert!(params.get("turnSteerParams").is_none());
                    json!({"result":{"turnId":"current"}})
                }
                "thread-follower-command-approval-decision" => {
                    assert_eq!(v["targetClientId"], "original");
                    assert_eq!(v["params"]["conversationId"], NATIVE);
                    assert_eq!(v["params"]["requestId"], 42);
                    assert_eq!(v["params"]["decision"], "decline");
                    let change=json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{
                        "hostId":"local","conversationId":NATIVE,"change":{"type":"patches","baseRevision":1,"revision":2,
                            "patches":[{"op":"remove","path":["requests",0]}]}
                    }}).to_string().into_bytes();
                    stream.write_u32_le(change.len() as u32).await.unwrap();
                    stream.write_all(&change).await.unwrap();
                    json!({"ok":true})
                }
                "thread-follower-interrupt-turn" => {
                    json!({"ok":true,"interruptedTurnId":"current"})
                }
                other => panic!("Forbidden Desktop action: {other}"),
            };
            let b=json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","handledByClientId":"original","result":result}).to_string().into_bytes();
            stream.write_u32_le(b.len() as u32).await.unwrap();
            stream.write_all(&b).await.unwrap();
        }
    });
    let config = SharedServer::Desktop {
        desktop_endpoint: Endpoint { path },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: root.path().into(),
        activation: None,
    };
    let mut c = config.connect(&profile).await.unwrap();
    assert!(rpc(
        &mut c,
        "thread/read",
        json!({"threadId":NATIVE,"includeTurns":true})
    )
    .await
    .get("error")
    .is_some());
    let metadata = rpc(
        &mut c,
        "thread/read",
        json!({"threadId":NATIVE,"includeTurns":false}),
    )
    .await;
    assert_eq!(
        metadata["result"]["thread"]["remodexTokenUsage"]["last"]["totalTokens"],
        42000
    );
    assert_eq!(
        metadata["result"]["thread"]["remodexTokenUsage"]["modelContextWindow"],
        258400
    );
    assert_eq!(
        metadata["result"]["thread"]["remodexRateLimits"]["rateLimits"]["primary"]["usedPercent"],
        25
    );
    let resume = rpc(&mut c, "thread/resume", json!({"threadId":NATIVE})).await;
    assert_eq!(resume["result"]["thread"]["status"]["type"], "idle");
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "thread-stream-following-changed"),
        "Idle send admission must not fetch the full Desktop snapshot"
    );
    let start = rpc(
        &mut c,
        "turn/start",
        json!({"threadId":NATIVE,"input":[{"type":"text","text":"synthetic prompt"}]}),
    )
    .await;
    assert_eq!(start["result"]["turn"]["id"], "current");
    // Let live notifications fill the bounded receive channel before sending.
    // A writable transport must not wait for us to consume those notifications.
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        c.send(&json!({"id":"duplex-probe","method":"initialize","params":{}})),
    )
    .await
    .expect("live output must not block a control write")
    .unwrap();
    // Same body must still emit completion after an earlier running snapshot.
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        let mut approval = false;
        loop {
            if let Some(Event::Message(v)) = c.events.recv().await {
                if v["method"] == "item/commandExecution/requestApproval" {
                    assert_eq!(v["id"], 42);
                    assert_eq!(v["params"]["threadId"], NATIVE);
                    approval = true;
                }
                if v["method"] == "item/started" {
                    assert!(approval);
                    return v;
                }
            } else {
                panic!("closed")
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(first["params"]["item"]["text"], "OWNER_REPLY");
    let question = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::Message(v)) = c.events.recv().await {
                if v["params"]["item"]["id"] == "desktop-question-item" {
                    break v;
                }
            } else {
                panic!("question connection closed");
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        question["params"]["item"]["questions"][0]["title"],
        "Pick a color"
    );
    let summary = rpc(
        &mut c,
        "thread/turns/list",
        json!({"threadId":NATIVE,"itemsView":"summary","limit":1}),
    )
    .await;
    assert!(summary["result"]["data"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["id"] == "desktop-question-item"));

    c.send(&json!({"id":42,"result":{"decision":"decline"}}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match c.events.recv().await {
                Some(Event::Message(v)) if v["method"] == "serverRequest/resolved" => {
                    assert_eq!(v["params"]["requestId"], 42);
                    break;
                }
                Some(Event::Message(_)) => {}
                _ => panic!("closed before Desktop resolved its request"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(rpc(&mut c,"turn/steer",json!({"threadId":NATIVE,"expectedTurnId":"current","input":[{"type":"text","text":"synthetic steer"},{"type":"localImage","path":"/test/screenshot.png"}],"clientUserMessageId":"phone-steer"})).await["result"]["turnId"],"current");
    assert_eq!(
        rpc(
            &mut c,
            "turn/interrupt",
            json!({"threadId":NATIVE,"turnId":"current"})
        )
        .await["result"]["interruptedTurnId"],
        "current"
    );
    std::fs::write(
        &state,
        json!({"turn":"current","status":"completed"}).to_string(),
    )
    .unwrap();
    {
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap(),
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"current","error":{"message":"Error running remote compact task: {\"detail\":\"Bad Request\"}","codex_error_info":"other"}}})
        )
        .unwrap();
    }
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut item = false;
        loop {
            if let Some(Event::Message(v)) = c.events.recv().await {
                if v["method"] == "item/completed" {
                    item = true;
                }
                if v["method"] == "turn/completed" {
                    assert_eq!(v["params"]["turn"]["status"], "failed");
                    assert_eq!(
                        v["params"]["turn"]["error"]["message"],
                        "Error running remote compact task: {\"detail\":\"Bad Request\"}"
                    );
                    return item;
                }
            } else {
                panic!("closed")
            }
        }
    })
    .await
    .unwrap();
    assert!(completed);
    assert!(rpc(&mut c,"turn/start",json!({"threadId":uuid::Uuid::new_v4().to_string(),"input":[{"type":"text","text":"foreign"}]})).await.get("error").is_some());
    c.close().await;
    server.await.unwrap();
    let methods = std::fs::read_to_string(profile.join("methods.log")).unwrap();
    assert_eq!(
        methods
            .lines()
            .filter(|m| *m == "account/rateLimits/read")
            .count(),
        1,
        "Repeated polls reuse the current account reading"
    );
    assert!(!methods.contains("thread/resume"));
    assert!(!methods.contains("turn/start"));
    assert!(!methods.contains("turn/interrupt"));
    assert!(!methods.contains("approval"));
    let log = log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|v| v["method"] == "thread-follower-start-turn")
            .count(),
        1
    );
    assert!(!log
        .iter()
        .any(|v| v["method"] == "thread-follower-load-complete-history"));
    assert_eq!(
        log.iter()
            .filter(|v| v["method"] == "thread-follower-command-approval-decision")
            .count(),
        1
    );
}

#[tokio::test]
async fn dormant_owner_loads_only_for_explicit_send_and_never_for_uncertain_discovery() {
    for uncertain in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let path = profile.join("ipc.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir(profile.join("sessions")).unwrap();
        let transcript = profile.join("sessions/saved.jsonl");
        let history = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"saved"}})
        );
        std::fs::write(&transcript, &history).unwrap();
        std::fs::write(
            profile.join("desktop-state.json"),
            json!({"turn":"saved","status":"completed"}).to_string(),
        )
        .unwrap();
        std::fs::write(
            profile.join("fixture.json"),
            json!({"native":NATIVE,"mode":"desktop","path":transcript}).to_string(),
        )
        .unwrap();
        let marker = root.path().join("activation.log");
        let activated = marker.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(size) = stream.read_u32_le().await {
                assert!(size < 65536);
                let mut bytes = vec![0; size as usize];
                stream.read_exact(&mut bytes).await.unwrap();
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                if v["method"] == "thread-stream-following-changed" {
                    let frame=json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{"conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":1,"conversationState":{"id":NATIVE,"hostId":"local","turns":[],"requests":[]}}}}).to_string();
                    stream.write_u32_le(frame.len() as u32).await.unwrap();
                    stream.write_all(frame.as_bytes()).await.unwrap();
                    continue;
                }
                let mut response = json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","handledByClientId":"original","result":{}});
                match v["method"].as_str().unwrap() {
                    "initialize" => response["result"] = json!({"clientId":"reader"}),
                    "thread-owner-discovery" => {
                        assert_eq!(v["params"]["conversationId"], NATIVE);
                        if !activated.exists() {
                            response["resultType"] = json!("error");
                            response["error"] = json!(if uncertain {
                                "temporarily-unavailable"
                            } else {
                                "no-client-found"
                            });
                        }
                    }
                    other => panic!("Unexpected owner mutation: {other}"),
                }
                let bytes = serde_json::to_vec(&response).unwrap();
                stream.write_u32_le(bytes.len() as u32).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            }
        });
        let config = SharedServer::Desktop {
            desktop_endpoint: Endpoint { path },
            reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
            reader_workspace: root.path().into(),
            activation: Some(
                remodex_connect_native::desktop_activation::Activation::Command {
                    executable: env!("CARGO_BIN_EXE_history-fixture").into(),
                    arguments: vec!["--activate".into(), marker.to_str().unwrap().into()],
                },
            ),
        };
        let mut c = config.connect(&profile).await.unwrap();
        assert!(rpc(
            &mut c,
            "thread/read",
            json!({"threadId":NATIVE,"includeTurns":false})
        )
        .await
        .get("error")
        .is_some());
        assert!(
            !marker.exists(),
            "background history must not activate Desktop"
        );
        let result = rpc(
            &mut c,
            "thread/read",
            json!({"threadId":NATIVE,"includeTurns":false,"remodexAllowActivation":true}),
        )
        .await;
        if uncertain {
            assert!(result.get("error").is_some());
            assert!(
                !marker.exists(),
                "uncertain ownership must not authorize activation"
            );
        } else {
            assert_eq!(result["result"]["thread"]["id"], NATIVE);
            assert_eq!(result["result"]["thread"]["status"]["type"], "idle");
            assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
            let result = rpc(
                &mut c,
                "thread/read",
                json!({"threadId":NATIVE,"includeTurns":false,"remodexAllowActivation":true}),
            )
            .await;
            assert!(result.get("error").is_none());
            assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        }
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), history);
        let methods = std::fs::read_to_string(profile.join("methods.log")).unwrap();
        assert!(!methods.contains("thread/resume") && !methods.contains("turn/start"));
        c.close().await;
        server.await.unwrap();
    }
}

#[tokio::test]
async fn offline_history_is_readable_without_granting_control_authority() {
    history_without_owner(false).await;
}
#[tokio::test]
async fn silent_owner_does_not_hold_saved_history_or_gain_control_authority() {
    history_without_owner(true).await;
}
async fn history_without_owner(silent: bool) {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
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
    std::fs::write(
        profile.join("desktop-state.json"),
        json!({"turn":"saved","status":"completed"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"desktop","path":transcript}).to_string(),
    )
    .unwrap();
    let endpoint = profile.join("offline.sock");
    let server = if silent {
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        Some(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read_u32_le().await.unwrap();
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            let response = serde_json::to_vec(&json!({"type":"response","requestId":request["requestId"],"method":"initialize","resultType":"success","result":{"clientId":"reader"}})).unwrap();
            stream.write_u32_le(response.len() as u32).await.unwrap();
            stream.write_all(&response).await.unwrap();
            let size = stream.read_u32_le().await.unwrap();
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["method"], "thread-owner-discovery");
            // Keep the socket open without answering discovery.
            assert!(stream.read_u8().await.is_err());
        }))
    } else {
        None
    };
    let config = SharedServer::Desktop {
        desktop_endpoint: Endpoint { path: endpoint },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: root.path().into(),
        activation: None,
    };
    if !silent {
        assert!(config.connect(&profile).await.is_err());
    }
    let mut c = config.connect_history(&profile).await.unwrap();
    let started = std::time::Instant::now();
    let result = rpc(
        &mut c,
        "thread/read",
        json!({"threadId":NATIVE,"includeTurns":false}),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(4));
    if let Some(server) = server {
        server.await.unwrap();
    }
    assert_eq!(result["result"]["thread"]["id"], NATIVE);
    assert_eq!(result["result"]["thread"]["remodexLiveStatusKnown"], false);
    assert_eq!(result["result"]["thread"]["remodexTaskStatusKnown"], true);
    let turns = rpc(
        &mut c,
        "thread/turns/list",
        json!({"threadId":NATIVE,"limit":1,"sortDirection":"asc","itemsView":"notLoaded"}),
    )
    .await;
    assert_eq!(turns["result"]["data"][0]["id"], "saved");
    let items = rpc(
        &mut c,
        "thread/items/list",
        json!({"threadId":NATIVE,"turnId":"saved","limit":8,"sortDirection":"asc"}),
    )
    .await;
    assert!(items["result"]["data"]
        .as_array()
        .is_some_and(|v| !v.is_empty()));
    for method in [
        "thread/resume",
        "turn/start",
        "turn/steer",
        "turn/interrupt",
    ] {
        assert!(
            rpc(&mut c, method, json!({"threadId":NATIVE,"turnId":"saved"}))
                .await
                .get("error")
                .is_some()
        );
    }
    c.close().await;
    assert_eq!(
        std::fs::read_to_string(profile.join("methods.log"))
            .unwrap()
            .lines()
            .filter(|v| [
                "thread/resume",
                "turn/start",
                "turn/steer",
                "turn/interrupt"
            ]
            .contains(v))
            .count(),
        0
    );
}

#[tokio::test]
async fn reloaded_owner_settles_an_old_unfinished_native_turn_without_restarting_it() {
    owner_snapshot_history(false).await;
}

#[tokio::test]
async fn unreadable_live_snapshot_does_not_block_saved_history_or_authorize_controls() {
    owner_snapshot_history(true).await;
}

async fn owner_snapshot_history(reject_snapshot: bool) {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let transcript = profile.join("sessions/saved.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":NATIVE}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}})
        ),
    )
    .unwrap();
    std::fs::write(
        profile.join("desktop-state.json"),
        json!({"turn":"old","status":"inProgress"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"desktop","path":transcript}).to_string(),
    )
    .unwrap();
    let path = profile.join("ipc.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        while let Ok(size) = stream.read_u32_le().await {
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            let response = match v["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","result":{"clientId":"reader"}})
                }
                "thread-owner-discovery" => {
                    json!({"type":"response","requestId":v["requestId"],"method":v["method"],"resultType":"success","handledByClientId":"original","result":{}})
                }
                "thread-stream-following-changed" => {
                    if reject_snapshot {
                        stream.write_u32_le(65 * 1024 * 1024).await.unwrap();
                        break;
                    }
                    json!({"type":"broadcast","sourceClientId":"original","method":"thread-stream-state-changed","params":{"conversationId":NATIVE,"hostId":"local","change":{"type":"snapshot","revision":1,"conversationState":{"id":NATIVE,"hostId":"local","turns":[{"id":"old","status":"interrupted","items":[]}],"requests":[]}}}})
                }
                other => panic!("Unexpected mutation: {other}"),
            };
            let b = response.to_string().into_bytes();
            stream.write_u32_le(b.len() as u32).await.unwrap();
            stream.write_all(&b).await.unwrap();
        }
    });
    let config = SharedServer::Desktop {
        desktop_endpoint: Endpoint { path },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: root.path().into(),
        activation: None,
    };
    let mut c = config.connect_history(&profile).await.unwrap();
    let result = rpc(
        &mut c,
        "thread/read",
        json!({"threadId":NATIVE,"includeTurns":false}),
    )
    .await;
    assert_eq!(
        result["result"]["thread"]["status"]["type"],
        if reject_snapshot { "active" } else { "idle" }
    );
    assert_eq!(
        result["result"]["thread"]["remodexTaskStatusKnown"],
        !reject_snapshot
    );
    let turns = rpc(
        &mut c,
        "thread/turns/list",
        json!({"threadId":NATIVE,"limit":1,"sortDirection":"asc","itemsView":"notLoaded"}),
    )
    .await;
    assert_eq!(
        turns["result"]["data"][0]["status"],
        if reject_snapshot {
            "inProgress"
        } else {
            "interrupted"
        }
    );
    let items = rpc(
        &mut c,
        "thread/items/list",
        json!({"threadId":NATIVE,"turnId":"old","limit":8,"sortDirection":"asc"}),
    )
    .await;
    assert!(items["result"]["data"]
        .as_array()
        .is_some_and(|v| !v.is_empty()));
    assert!(rpc(&mut c, "turn/start", json!({"threadId":NATIVE}))
        .await
        .get("error")
        .is_some());
    c.close().await;
    server.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(transcript).unwrap().lines().count(),
        2
    );
}
