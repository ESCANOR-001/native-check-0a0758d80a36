use remodex_connect_native::{
    agent::{Agent, Connection, Launch},
    claude_host::{Delivery, HostedSession, Identity, Ledger},
};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, time::Duration};
const NATIVE: &str = "00000000-0000-4000-8000-000000000001";
const FIRST: &str = "00000000-0000-4000-8000-000000000002";
const SECOND: &str = "00000000-0000-4000-8000-000000000003";
fn start(dir: &std::path::Path, behavior: &str) -> Connection {
    Connection::start(Launch {
        agent: Agent::Claude,
        executable: PathBuf::from(env!("CARGO_BIN_EXE_protocol-fixture")),
        cwd: dir.into(),
        env: HashMap::from([
            ("REMODEX_FIXTURE_BEHAVIOR".into(), behavior.into()),
            ("REMODEX_FIXTURE_NATIVE".into(), NATIVE.into()),
        ]),
        extra_args: vec![],
    })
    .unwrap()
}
async fn attach(
    dir: &std::path::Path,
    behavior: &str,
    prior: Option<Identity>,
) -> Result<HostedSession, String> {
    HostedSession::attach_with_timeout(
        start(dir, behavior),
        NATIVE.into(),
        prior,
        "Isolated shared fixture",
        Duration::from_secs(2),
    )
    .await
}
fn commands(dir: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("host-commands.jsonl"))
        .unwrap()
        .lines()
        .map(|v| serde_json::from_str(v).unwrap())
        .collect()
}
async fn result(host: &mut HostedSession) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = host.next().await.unwrap().unwrap();
            if frame.value["type"] == "result" {
                return frame.receipt.completed;
            }
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn result_keeps_host_alive_and_identity_is_saved_before_prompt_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = attach(dir.path(), "claude-host", None).await.unwrap();
    assert!(host
        .send_prompt(FIRST, json!("first"), false)
        .await
        .is_err());
    let identity = host.identity().clone();
    assert!(host
        .confirm_persisted(&Identity {
            native_session: NATIVE.into(),
            bridge_session: "cse_another_link".into()
        })
        .is_err());
    host.confirm_persisted(&identity).unwrap();
    host.send_prompt(FIRST, json!("first"), false)
        .await
        .unwrap();
    assert_eq!(result(&mut host).await, vec![FIRST]);
    assert!(host
        .send_prompt(FIRST, json!("duplicate"), false)
        .await
        .is_err());
    // Idle waiting is cancellation-safe and never closes the worker.
    assert!(tokio::time::timeout(Duration::from_millis(30), host.next())
        .await
        .is_err());
    host.send_prompt(SECOND, json!("second"), false)
        .await
        .unwrap();
    assert_eq!(result(&mut host).await, vec![SECOND]);
    host.close().await;
    let log = commands(dir.path());
    assert_eq!(
        log.iter()
            .filter(|v| v["request"]["subtype"] == "initialize")
            .count(),
        1
    );
    assert_eq!(log.iter().filter(|v| v["type"] == "user").count(), 2);
    let mut resumed = attach(dir.path(), "claude-host", Some(identity.clone()))
        .await
        .unwrap();
    assert_eq!(resumed.identity(), &identity);
    resumed.confirm_persisted(&identity).unwrap();
    resumed.close().await;
    let log = commands(dir.path());
    assert_eq!(
        log.iter().filter(|v| v["type"] == "user").count(),
        2,
        "reattach must never replay prompts"
    );
    assert!(log
        .iter()
        .any(|v| v["request"]["reattach_session_id"] == identity.bridge_session));
}
#[tokio::test]
async fn external_result_does_not_finish_pending_android_input_and_exit_is_uncertain() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = attach(dir.path(), "claude-host", None).await.unwrap();
    host.confirm_persisted(&host.identity().clone()).unwrap();
    host.send_prompt(FIRST, json!("hold"), false).await.unwrap();
    host.send_control(&json!({"type":"control_request","request_id":"other","request":{"subtype":"fixture_external"}})).await.unwrap();
    assert_eq!(
        result(&mut host).await,
        vec!["00000000-0000-4000-8000-000000000010"]
    );
    assert_eq!(host.ledger.delivery(FIRST), Some(Delivery::Offered));
    host.send_prompt(SECOND, json!("exit"), true).await.unwrap();
    while host.next().await.unwrap().is_some() {}
    assert_eq!(host.ledger.delivery(FIRST), Some(Delivery::Uncertain));
    assert_eq!(host.ledger.delivery(SECOND), Some(Delivery::Uncertain));
    assert!(host
        .send_prompt(SECOND, json!("retry"), false)
        .await
        .is_err());
    host.close().await;
    assert_eq!(
        commands(dir.path())
            .iter()
            .filter(|v| v["type"] == "user")
            .count(),
        2
    );
}
#[tokio::test]
async fn failures_do_not_deliver_prompts_or_expose_auth_details() {
    let prior = Identity {
        native_session: NATIVE.into(),
        bridge_session: "cse_existing_link".into(),
    };
    for behavior in [
        "claude-host-auth",
        "claude-host-wrong-link",
        "claude-host-foreign",
        "claude-host-overflow",
        "claude-host-hang",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let error = match attach(dir.path(), behavior, Some(prior.clone())).await {
            Ok(_) => panic!("Expected handshake rejection"),
            Err(e) => e,
        };
        assert!(!error.contains("SECRET_WORK_TOKEN") && !error.contains("https://"));
        let log = commands(dir.path());
        assert!(!log.iter().any(|v| v["type"] == "user"));
        if behavior == "claude-host-wrong-link" {
            assert!(log
                .iter()
                .any(|v| v["request"]["subtype"] == "remote_control"
                    && v["request"]["enabled"] == false));
        }
    }
}
#[test]
fn receipts_require_native_consumption_and_never_complete_unrelated_prompts() {
    let mut ledger = Ledger::default();
    ledger.offer(FIRST).unwrap();
    ledger.offer(SECOND).unwrap();
    assert!(ledger
        .observe(&json!({"type":"user","uuid":FIRST,"isReplay":true}))
        .unwrap()
        .consumed
        .is_empty());
    assert_eq!(ledger.delivery(FIRST), Some(Delivery::Offered));
    assert!(ledger
        .observe(
            &json!({"type":"assistant","parent_tool_use_id":"child","user_message_uuid":FIRST})
        )
        .unwrap()
        .consumed
        .is_empty());
    assert_eq!(
        ledger
            .observe(&json!({"type":"assistant","user_message_uuid":FIRST}))
            .unwrap()
            .consumed,
        vec![FIRST]
    );
    assert_eq!(ledger.delivery(FIRST), Some(Delivery::Consumed));
    assert_eq!(
        ledger
            .observe(&json!({"type":"result","user_message_uuids":[FIRST]}))
            .unwrap()
            .completed,
        vec![FIRST]
    );
    assert_eq!(ledger.delivery(SECOND), Some(Delivery::Offered));
    assert!(ledger
        .observe(&json!({"type":"result"}))
        .unwrap()
        .completed
        .is_empty());
    assert_eq!(ledger.pending_count(), 1);
    assert_eq!(
        ledger
            .observe(&json!({"type":"command_lifecycle","state":"cancelled","command_uuid":SECOND}))
            .unwrap()
            .cancelled,
        vec![SECOND]
    );
    assert_eq!(ledger.pending_count(), 0);
}
#[test]
fn receipt_validation_is_atomic_bounded_and_duplicate_resistant() {
    let mut ledger = Ledger::default();
    ledger.offer(FIRST).unwrap();
    for frame in [
        json!({"type":"result","user_message_uuids":[FIRST,"invalid"]}),
        json!({"type":"result","user_message_uuids":[FIRST],"user_message_uuid":SECOND}),
        json!({"type":"result","user_message_uuids":vec![FIRST;65]}),
    ] {
        assert!(ledger.observe(&frame).is_err());
        assert_eq!(ledger.delivery(FIRST), Some(Delivery::Offered));
    }
    assert!(ledger.offer(FIRST).is_err());
    for _ in 1..64 {
        ledger.offer(&uuid::Uuid::new_v4().to_string()).unwrap();
    }
    assert!(ledger.offer(&uuid::Uuid::new_v4().to_string()).is_err());
    for _ in 0..1000 {
        ledger
            .observe(&json!({"type":"result","user_message_uuid":uuid::Uuid::new_v4().to_string()}))
            .unwrap();
    }
    assert_eq!(ledger.pending_count(), 64);
}

#[tokio::test]
async fn chat_transport_requires_identity_commit_and_reuses_its_host() {
    use remodex_connect_native::agent::Event;
    let dir = tempfile::tempdir().unwrap();
    let mut connection = Connection::host_claude(
        start(dir.path(), "claude-host"),
        NATIVE.into(),
        None,
        "Hosted chat fixture".into(),
    );
    connection.send(&json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}})).await.unwrap();
    let Some(Event::Message(init)) = connection.events.recv().await else {
        panic!("Missing initialization")
    };
    let identity = init["response"]["response"]["remodexRemoteIdentity"].clone();
    assert_eq!(identity["native_session"], NATIVE);
    connection
        .send(&json!({"type":"remodex_identity_commit","identity":identity}))
        .await
        .unwrap();
    for id in [FIRST, SECOND] {
        connection.send(&json!({"type":"user","uuid":id,"session_id":NATIVE,"parent_tool_use_id":null,"message":{"role":"user","content":"hosted"}})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(Event::Message(v)) = connection.events.recv().await {
                if v["type"] == "result" {
                    assert_eq!(v["user_message_uuids"][0], id);
                    return;
                }
            }
            panic!("Host ended before its turn");
        })
        .await
        .unwrap();
    }
    connection.close().await;
    let log = commands(dir.path());
    assert_eq!(
        log.iter()
            .filter(|v| v["request"]["subtype"] == "initialize")
            .count(),
        1
    );
    assert_eq!(log.iter().filter(|v| v["type"] == "user").count(), 2);
    assert!(!log.iter().any(|v| v["type"] == "remodex_identity_commit"));
    let mut uncommitted = Connection::host_claude(
        start(dir.path(), "claude-host"),
        NATIVE.into(),
        None,
        "Uncommitted fixture".into(),
    );
    uncommitted.send(&json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}})).await.unwrap();
    assert!(matches!(
        uncommitted.events.recv().await,
        Some(Event::Message(_))
    ));
    assert!(uncommitted.send(&json!({"type":"user","uuid":uuid::Uuid::new_v4().to_string(),"session_id":NATIVE,"message":{"role":"user","content":"blocked"}})).await.is_err());
    uncommitted.close().await;
    assert_eq!(
        commands(dir.path())
            .iter()
            .filter(|v| v["type"] == "user")
            .count(),
        2
    );
}
