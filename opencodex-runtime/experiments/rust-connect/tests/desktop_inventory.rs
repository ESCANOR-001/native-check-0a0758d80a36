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
use std::time::Duration;
async fn shell(hub: &Chats) -> Value {
    hub.call(
        AgentKind::Codex,
        "phone",
        "orchestration.subscribeShell",
        json!({}),
    )
    .await
    .unwrap()["snapshot"]
        .clone()
}
async fn until(hub: &Chats, id: &str, seconds: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        loop {
            let value = shell(hub).await;
            if value["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == id)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn discovery_backfills_pages_finds_post_start_chats_and_survives_restart_without_activation()
{
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let profile = root.join("profile");
    std::fs::create_dir(&profile).unwrap();
    let socket = profile.join("ipc.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let row = |i: u32| {
        json!({"id":format!("12345678-1234-4234-8234-{i:012x}"),"name":format!("Desktop chat {i}"),
        "source":"vscode","cwd":root,"createdAt":1700000000,"updatedAt":1700000010,"status":{"type":"notLoaded"}})
    };
    let mut rows: Vec<_> = (0..130).map(row).collect();
    std::fs::write(
        profile.join("inventory.json"),
        serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    let config = Config {
        storage: Default::default(),
        native_installation: None,
        native_profile_handoff: None,
        connection: None,
        max_active_chats: 1,
        workspace: root.into(),
        claude: None,
        codex: Some(AgentConfig {
            executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
            profile: profile.clone(),
            model: "test".into(),
            tools: Default::default(),
            remote_control: false,
            shared_server: Some(SharedServer::Desktop {
                desktop_endpoint: Endpoint { path: socket },
                reader_executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
                reader_workspace: root.into(),
                activation: None,
            }),
        }),
    };
    let db = AuthService::new(Auth::new(Store::open(&root.join("store")).unwrap())).unwrap();
    let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
    let first = until(&hub, rows[127]["id"].as_str().unwrap(), 10).await;
    assert_eq!(first["threads"].as_array().unwrap().len(), 128);
    let newest = row(999);
    let newest_id = newest["id"].as_str().unwrap().to_owned();
    rows.insert(0, newest);
    // Arrival after startup must be picked up while the older-page scan continues.
    std::fs::write(
        profile.join("inventory.json"),
        serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    let next = until(&hub, &newest_id, 40).await;
    assert!(next["threads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == row(129)["id"]));
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
    let hub = Chats::start(db.clone(), config).await.unwrap();
    let restored = until(&hub, &newest_id, 10).await;
    assert_eq!(restored["threads"].as_array().unwrap().len(), 131);
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
    let logged = std::fs::read_to_string(profile.join("requests.jsonl")).unwrap();
    let calls: Vec<Value> = logged
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        calls
            .iter()
            .filter(|v| v["method"] == "thread/list")
            .count()
            >= 4
    );
    assert!(calls.iter().all(|v| [
        "initialize",
        "initialized",
        "thread/list",
        "model/list",
        "thread/read",
        "thread/turns/list",
        "account/rateLimits/read"
    ]
    .contains(&v["method"].as_str().unwrap())));
}
