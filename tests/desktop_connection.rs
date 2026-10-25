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
    let state = profile.join("desktop-state.json");
    std::fs::write(
        &state,
        json!({"turn":"previous","status":"completed"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":NATIVE,"mode":"desktop","path":transcript}).to_string(),
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
                    "conversationState":{"id":NATIVE,"hostId":"local","turns":[],"requests":[{
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
                "thread-follower-steer-turn" => json!({"turnId":"current"}),
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
    let resume = rpc(&mut c, "thread/resume", json!({"threadId":NATIVE})).await;
    assert_eq!(resume["result"]["thread"]["status"]["type"], "idle");
    let start = rpc(
        &mut c,
        "turn/start",
        json!({"threadId":NATIVE,"input":[{"type":"text","text":"synthetic prompt"}]}),
    )
    .await;
    assert_eq!(start["result"]["turn"]["id"], "current");
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
    assert_eq!(rpc(&mut c,"turn/steer",json!({"threadId":NATIVE,"expectedTurnId":"current","input":[{"type":"text","text":"synthetic steer"}]})).await["result"]["turnId"],"current");
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
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"current"}})
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
