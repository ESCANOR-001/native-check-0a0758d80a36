use remodex_connect_native::agent::{self, Agent, Connection, Event, Launch};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Duration};
#[cfg(target_os = "linux")]
#[test]
fn cancelling_the_runtime_cleans_up_the_owned_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (pid, connection) = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut c = Connection::start(Launch {
                agent: Agent::Codex,
                executable: PathBuf::from(env!("CARGO_BIN_EXE_protocol-fixture")),
                cwd: root,
                env: HashMap::from([("REMODEX_FIXTURE_BEHAVIOR".into(), "tree-hold".into())]),
                extra_args: vec![],
            })
            .unwrap();
            let Some(Event::Message(v)) =
                tokio::time::timeout(Duration::from_secs(3), c.events.recv())
                    .await
                    .unwrap()
            else {
                panic!("Missing child pid")
            };
            (v["childPid"].as_u64().unwrap(), c)
        })
        // Drop the runtime while its transport is still owned by the caller.
    })
    .join()
    .unwrap();
    drop(connection);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let running = status
            .lines()
            .find(|s| s.starts_with("State:"))
            .is_some_and(|s| !s.contains("Z (zombie)"));
        if !running {
            break;
        }
        if std::time::Instant::now() > deadline {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            panic!("An owned descendant survived runtime cancellation");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn natural_cli_exit_also_cleans_up_its_owned_child_processes() {
    let dir = tempfile::tempdir().unwrap();
    let mut connection = Connection::start(Launch {
        agent: Agent::Codex,
        executable: PathBuf::from(env!("CARGO_BIN_EXE_protocol-fixture")),
        cwd: dir.path().to_path_buf(),
        env: HashMap::from([("REMODEX_FIXTURE_BEHAVIOR".into(), "tree-exit".into())]),
        extra_args: vec![],
    })
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(3), connection.events.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Message(value) = event else {
        panic!("Missing process identity")
    };
    let pid = value["childPid"].as_u64().unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), connection.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(closed, Event::Closed { .. }));
    connection.close().await;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let running = status
        .lines()
        .find(|s| s.starts_with("State:"))
        .is_some_and(|s| !s.contains("Z (zombie)"));
    // Clean up the disposable fixture even on the deliberately red run.
    if running {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    assert!(
        !running,
        "An owned subprocess survived the CLI leader exiting"
    );
}
#[tokio::test]
async fn direct_native_transport_preserves_both_agent_protocols_and_approval_identity() {
    for agent in [Agent::Codex, Agent::Claude] {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = Connection::start(Launch {
            agent,
            executable: PathBuf::from(env!("CARGO_BIN_EXE_protocol-fixture")),
            cwd: dir.path().to_path_buf(),
            env: HashMap::new(),
            extra_args: vec![],
        })
        .unwrap();
        for value in [
            json!({"id":"turn-1","method":"turn/steer","params":{"expectedTurnId":"active","text":"你好 🙂"}}),
            json!({"type":"control_response","response":{"request_id":"approval-1","subtype":"success","response":{"behavior":"deny","message":"Declined"}}}),
            json!({"type":"user","uuid":"same-uuid","message":{"role":"user","content":"a".repeat(256*1024)}}),
        ] {
            connection.send(&value).await.unwrap();
            let event = tokio::time::timeout(Duration::from_secs(2), connection.events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                Event::Message(actual) => assert_eq!(actual, value),
                _ => panic!("Unexpected closed connection"),
            }
        }
        connection.close().await;
    }
}
#[tokio::test]
async fn malformed_frames_and_oversized_input_fail_without_replay() {
    let mut read = tokio::io::BufReader::with_capacity(2, "{\"text\":\"你好\"}\n".as_bytes());
    let value: serde_json::Value =
        serde_json::from_slice(&agent::frame(&mut read).await.unwrap().unwrap()).unwrap();
    assert_eq!(value["text"], "你好");
    assert!(
        agent::frame(&mut tokio::io::BufReader::new(&b"{incomplete"[..]))
            .await
            .is_err()
    );
    let bytes = vec![b'x'; agent::MAX_FRAME + 1];
    assert!(
        agent::frame(&mut tokio::io::BufReader::new(bytes.as_slice()))
            .await
            .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let connection = Connection::start(Launch {
        agent: Agent::Codex,
        executable: PathBuf::from(env!("CARGO_BIN_EXE_protocol-fixture")),
        cwd: dir.path().to_path_buf(),
        env: HashMap::new(),
        extra_args: vec![],
    })
    .unwrap();
    assert!(connection
        .send(&json!({"oversized":"x".repeat(agent::MAX_INPUT_FRAME)}))
        .await
        .is_err());
    connection.close().await;
}
