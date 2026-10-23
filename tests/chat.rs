use remodex_connect_native::{
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    store::Store,
};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
// The `many` fixture writes 160 completed 4-KiB messages with FULL-sync
// transactions. Give only that disk-stress preparation a separate budget.
const BULK_HISTORY_TIMEOUT: Duration = Duration::from_secs(30);
struct Test {
    dir: tempfile::TempDir,
    db: AuthService,
    config: Config,
    hub: Chats,
}
impl Test {
    async fn hosted() -> Self {
        let mut test = Self::new().await;
        test.stop().await;
        test.config.claude.remote_control = true;
        test.hub = Chats::start(test.db.clone(), test.config.clone())
            .await
            .unwrap();
        test
    }
    async fn new() -> Self {
        Self::with_workers(1).await
    }
    async fn with_workers(workers: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage: Default::default(),
            max_active_chats: workers,
            workspace: dir.path().into(),
            codex: AgentConfig {
                executable: PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture")),
                profile: dir.path().into(),
                model: "test".into(),
                tools: Default::default(),
                shared_server: None,
                remote_control: false,
            },
            claude: AgentConfig {
                executable: PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture")),
                profile: dir.path().into(),
                model: "test".into(),
                tools: Default::default(),
                shared_server: None,
                remote_control: false,
            },
        };
        let db = AuthService::new(Auth::new(Store::open(dir.path()).unwrap())).unwrap();
        let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
        Self {
            dir,
            db,
            config,
            hub,
        }
    }
    async fn call(&self, a: AgentKind, c: Value) -> Result<Value, String> {
        self.hub
            .call(a, "phone", "orchestration.dispatchCommand", c)
            .await
    }
    async fn create(&self, a: AgentKind, id: &str) {
        self.call(a,json!({"type":"thread.create","commandId":format!("create-{id}"),"threadId":id,"projectId":"native-test-project"})).await.unwrap();
    }
    async fn detail(&self, a: AgentKind, id: &str) -> Value {
        self.hub
            .call(
                a,
                "phone",
                "orchestration.subscribeThread",
                json!({"threadId":id}),
            )
            .await
            .unwrap()["snapshot"]["thread"]
            .clone()
    }
    async fn wait(&self, a: AgentKind, id: &str, status: &str) -> Value {
        self.wait_for(a, id, status, Duration::from_secs(8)).await
    }
    async fn wait_bulk_history(&self, a: AgentKind, id: &str) -> Value {
        let started = std::time::Instant::now();
        let result = self.wait_for(a, id, "ready", BULK_HISTORY_TIMEOUT).await;
        eprintln!(
            "Durable long-turn fixture {a:?}/{id}: {:?}",
            started.elapsed()
        );
        result
    }
    async fn wait_for(&self, a: AgentKind, id: &str, status: &str, limit: Duration) -> Value {
        let started = std::time::Instant::now();
        let mut last = Value::Null;
        let result = tokio::time::timeout(limit, async {
            loop {
                let t = self.detail(a, id).await;
                if t["session"]["status"] == status {
                    return t;
                }
                last = t;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        result.unwrap_or_else(|_| {
            panic!(
                "{a:?}/{id} did not reach {status} after {:?}; session={}, history={}",
                started.elapsed(),
                last["session"],
                last["historyPage"]
            )
        })
    }
    async fn stop(&self) {
        self.hub
            .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
            .await
            .unwrap();
    }
}

async fn configure_switches(test: &Test, value: Value) -> Value {
    let start = std::time::Instant::now();
    loop {
        match test.hub.configure_claude(value.clone()).await {
            Ok(v) => return v,
            Err(e)
                if e.starts_with("Wait for agent setup")
                    && start.elapsed() < Duration::from_secs(10) =>
            {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Err(e) => panic!("settings failed: {e}"),
        }
    }
}

fn alternate_claude(test: &Test) -> PathBuf {
    let folder = test.dir.path().join("CLI with spaces");
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join(format!("claude-fixture{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&test.config.claude.executable, &path).unwrap();
    path
}

#[tokio::test]
async fn claude_executable_changes_use_new_probe_reconnect_same_host_and_survive_restart() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    let alternate = alternate_claude(&test);
    test.create(a, "binary-host").await;
    test.call(a, prompt("binary-host", "before-binary", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "binary-host", "ready").await;
    let initial = configure_switches(&test, json!({"enabled":true})).await;
    assert_eq!(initial["binary"], json!(test.config.claude.executable));
    let codex = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let v = test
                .hub
                .call(AgentKind::Codex, "phone", "server.getConfig", json!({}))
                .await
                .unwrap();
            if v["providers"][0]["status"] == "ready" {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let changed = configure_switches(&test, json!({"binary":alternate})).await;
    assert_eq!(changed["binary"], json!(alternate));
    wait_links(&test, 2).await;
    assert_eq!(
        test.detail(a, "binary-host").await["messages"],
        before["messages"]
    );
    let probes: Vec<PathBuf> =
        std::fs::read_to_string(test.dir.path().join("setup-auth-probes.log"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    assert_eq!(
        probes.len(),
        2,
        "an executable change must invalidate cached sign-in status"
    );
    assert_eq!(
        probes[1].canonicalize().unwrap(),
        alternate.canonicalize().unwrap()
    );
    let now = test
        .hub
        .call(AgentKind::Codex, "phone", "server.getConfig", json!({}))
        .await
        .unwrap();
    assert_eq!(
        now["providers"][0]["models"],
        codex["providers"][0]["models"]
    );
    test.call(a, prompt("binary-host", "after-binary", "normal"))
        .await
        .unwrap();
    test.wait(a, "binary-host", "ready").await;
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    wait_links(&test, 3).await;
    let restored = test
        .hub
        .call(a, "phone", "claude.getSyncSettings", json!({}))
        .await
        .unwrap();
    assert_eq!(restored["binary"], json!(alternate));
    let reset = configure_switches(&test, json!({"binary":""})).await;
    assert_eq!(reset["binary"], json!(test.config.claude.executable));
    wait_links(&test, 4).await;
    let links: Vec<Value> =
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    for link in &links[1..] {
        assert_eq!(link["reattach"], links[0]["bridge"]);
    }
    for link in &links[1..3] {
        assert_eq!(
            PathBuf::from(link["executable"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            alternate.canonicalize().unwrap()
        );
    }
    assert_eq!(links[3]["executable"], links[0]["executable"]);
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    test.stop().await;
}

#[tokio::test]
async fn claude_executable_same_path_does_not_reconnect_and_invalid_or_failed_writes_do_nothing() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "binary-safe").await;
    test.call(a, prompt("binary-safe", "before", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "binary-safe", "ready").await;
    for path in [json!(test.config.claude.executable), json!("")] {
        let status = configure_switches(&test, json!({"binary":path})).await;
        assert_eq!(status["connectedSessions"], 1);
    }
    let alternate = alternate_claude(&test);
    for value in [
        json!("relative-claude"),
        json!(test.dir.path().join("missing")),
        json!(test.dir.path()),
        json!(true),
        json!("/bad\npath"),
        json!("x".repeat(2049)),
    ] {
        assert!(test
            .hub
            .configure_claude(json!({"binary":value}))
            .await
            .is_err());
    }
    let db = rusqlite::Connection::open(test.dir.path().join("native-connect.sqlite")).unwrap();
    let old: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='native.models'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Failure after the settings write exercises rollback of the entire transaction.
    db.execute_batch("CREATE TRIGGER fail_catalog BEFORE INSERT ON metadata WHEN NEW.key='native.models' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let error = test
        .hub
        .configure_claude(json!({"binary":alternate}))
        .await
        .unwrap_err();
    assert!(
        error.contains("Cannot save Claude bridge settings"),
        "{error}"
    );
    let saved: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='native.claudeSettings'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&saved).unwrap()["binary"],
        Value::Null
    );
    let unchanged: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='native.models'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old, unchanged);
    let current = test
        .hub
        .call(a, "phone", "claude.getSyncSettings", json!({}))
        .await
        .unwrap();
    assert_eq!(current["binary"], json!(test.config.claude.executable));
    assert_eq!(current["connectedSessions"], 1);
    assert_eq!(
        test.detail(a, "binary-safe").await["messages"],
        before["messages"]
    );
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.call(a, prompt("binary-safe", "after-rejection", "normal"))
        .await
        .unwrap();
    test.wait(a, "binary-safe", "ready").await;
    test.stop().await;
}

#[tokio::test]
async fn claude_setup_switches_are_durable_preserve_history_and_leave_codex_running() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "switch-host").await;
    test.call(a, prompt("switch-host", "first", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "switch-host", "ready").await;
    test.create(AgentKind::Codex, "independent").await;
    test.call(AgentKind::Codex, prompt("independent", "hold", "normal"))
        .await
        .unwrap();
    test.wait(AgentKind::Codex, "independent", "running").await;
    let off = configure_switches(&test, json!({"enabled":false})).await;
    assert_eq!(off["enabled"], false);
    assert_eq!(off["ready"], false);
    assert_eq!(off["connectedSessions"], 0);
    assert_eq!(
        test.detail(a, "switch-host").await["messages"],
        before["messages"]
    );
    assert_eq!(
        test.detail(a, "switch-host").await["latestTurn"],
        before["latestTurn"]
    );
    assert_eq!(
        test.detail(AgentKind::Codex, "independent").await["session"]["status"],
        "running"
    );
    assert!(test
        .call(a, prompt("switch-host", "must-not-send", "normal"))
        .await
        .unwrap_err()
        .contains("Enable Claude Code"));
    assert!(test
        .hub
        .call(a, "phone", "claude.setAutoConnect", json!({"enabled":true}))
        .await
        .is_err());
    let config = test
        .hub
        .call(a, "phone", "server.getConfig", json!({}))
        .await
        .unwrap();
    assert_eq!(config["providers"][0]["enabled"], false);
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "disabled hosts must not restart"
    );
    assert_eq!(
        test.hub
            .call(a, "phone", "claude.getSyncSettings", json!({}))
            .await
            .unwrap()["enabled"],
        false
    );
    configure_switches(&test, json!({"enabled":true})).await;
    wait_links(&test, 2).await;
    assert_eq!(
        test.detail(a, "switch-host").await["messages"],
        before["messages"]
    );
    let off = configure_switches(&test, json!({"desktopSync":false})).await;
    assert_eq!(off["enabled"], true);
    assert_eq!(off["desktopSync"], false);
    assert_eq!(off["connectedSessions"], 0);
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    assert_eq!(
        test.hub
            .call(a, "phone", "claude.getSyncSettings", json!({}))
            .await
            .unwrap()["desktopSync"],
        false
    );
    configure_switches(&test, json!({"desktopSync":true})).await;
    wait_links(&test, 3).await;
    test.call(a, prompt("switch-host", "second", "normal"))
        .await
        .unwrap();
    test.wait(a, "switch-host", "ready").await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2,
        "settings changes must not replay input"
    );
    let links: Vec<Value> =
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    assert_eq!(links[1]["reattach"], links[0]["bridge"]);
    assert_eq!(links[2]["reattach"], links[0]["bridge"]);
    test.stop().await;
}

#[tokio::test]
async fn claude_setup_refuses_busy_work_and_failed_persistence_without_disconnecting() {
    let test = Test::hosted().await;
    let alternate = alternate_claude(&test);
    configure_switches(&test, json!({"enabled":true})).await;
    let a = AgentKind::Claude;
    test.create(a, "protected").await;
    test.call(a, prompt("protected", "hold", "normal"))
        .await
        .unwrap();
    test.wait(a, "protected", "running").await;
    assert!(test
        .hub
        .configure_claude(json!({"binary":alternate}))
        .await
        .is_err());
    assert!(test
        .hub
        .configure_claude(json!({"enabled":false}))
        .await
        .is_err());
    assert!(test
        .hub
        .configure_claude(json!({"desktopSync":false}))
        .await
        .is_err());
    assert_eq!(
        test.detail(a, "protected").await["session"]["status"],
        "running"
    );
    test.call(
        a,
        json!({"type":"thread.turn.interrupt","commandId":"stop-protected","threadId":"protected"}),
    )
    .await
    .unwrap();
    test.wait(a, "protected", "interrupted").await;
    rusqlite::Connection::open(test.dir.path().join("native-connect.sqlite")).unwrap().execute_batch("CREATE TRIGGER fail_setup BEFORE INSERT ON metadata WHEN NEW.key='native.claudeSettings' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let error = test
        .hub
        .configure_claude(json!({"enabled":false,"desktopSync":false}))
        .await
        .unwrap_err();
    assert!(
        error.contains("Cannot save Claude bridge settings"),
        "{error}"
    );
    let current = test
        .hub
        .call(a, "phone", "claude.getSyncSettings", json!({}))
        .await
        .unwrap();
    assert_eq!(current["enabled"], true);
    assert_eq!(current["desktopSync"], true);
    assert_eq!(current["connectedSessions"], 1);
    assert!(test
        .hub
        .configure_claude(json!({"enabled":false,"extra":true}))
        .await
        .is_err());
    test.call(a, prompt("protected", "after-failure", "normal"))
        .await
        .unwrap();
    test.wait(a, "protected", "ready").await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
}

#[tokio::test]
async fn native_claude_setup_preferences_do_not_disconnect_hosts_or_change_codex() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "setup-host").await;
    test.call(a, prompt("setup-host", "first", "normal"))
        .await
        .unwrap();
    test.wait(a, "setup-host", "ready").await;
    let before = test.detail(a, "setup-host").await;
    let codex = test
        .hub
        .call(AgentKind::Codex, "phone", "server.getConfig", json!({}))
        .await
        .unwrap();
    for enabled in [true, false, true] {
        let value = test
            .hub
            .call(
                a,
                "phone",
                "claude.setAutoConnect",
                json!({"enabled":enabled}),
            )
            .await
            .unwrap();
        assert_eq!(value["autoConnect"]["preference"], enabled);
        assert_eq!(value["connection"]["connectedSessions"], 1);
        assert_eq!(value["autoConnect"]["desktopPreference"], "unknown");
        assert_eq!(value["desktopSync"], true);
        assert_eq!(
            test.detail(a, "setup-host").await["messages"],
            before["messages"]
        );
    }
    let after = test
        .hub
        .call(AgentKind::Codex, "phone", "server.getConfig", json!({}))
        .await
        .unwrap();
    assert_eq!(after["settings"], codex["settings"]);
    assert!(test
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "claude.setAutoConnect",
            json!({"enabled":false})
        )
        .await
        .is_err());
    assert!(test
        .hub
        .call(
            a,
            "phone",
            "claude.setAutoConnect",
            json!({"enabled":false,"extra":true})
        )
        .await
        .is_err());
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
    let hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    assert_eq!(
        hub.call(a, "phone", "claude.getSyncSettings", json!({}))
            .await
            .unwrap()["autoConnect"]["preference"],
        true
    );
    hub.call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn external_claude_restart_keeps_owner_history_without_launching_another_cli() {
    let mut test = Test::new().await;
    let a = AgentKind::Claude;
    test.create(a, "external").await;
    test.stop().await;
    let native = "00000000-0000-4000-8000-000000000010";
    test.db.call(move |a| {
        let mut chat=a.store.load_chat("external").unwrap();
        chat["native_id"]=json!(native);
        chat["external_claude"]=json!(true);
        chat["resumed"]=json!(true);
        chat["remote_identity"]=json!({"native_session":native,"bridge_session":"cse_external_existing"});
        chat["thread"]["session"]["status"]=json!("running");
        chat["thread"]["latestTurn"]=json!({"turnId":"owner-turn","state":"running"});
        chat["thread"]["messages"]=json!([{"id":"original","role":"user","text":"Keep the original task intact","turnId":"owner-turn","streaming":false}]);
        a.store.save_chat(&chat,None).unwrap();
    }).await.unwrap();
    test.config.claude.remote_control = true;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    assert!(
        test.hub
            .configure_claude(json!({"enabled":false}))
            .await
            .is_err(),
        "a disconnected externally-owned task still protects setup"
    );
    // No transcript/auth exists: the observer must fail closed, never resume
    // the UUID in the otherwise functional fixture CLI.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let after = test.detail(a, "external").await;
    assert_eq!(after["latestTurn"]["state"], "running");
    assert_eq!(
        after["messages"][0]["text"],
        "Keep the original task intact"
    );
    assert!(!test.dir.path().join("host-remote-links.jsonl").exists());
    let tools = test
        .hub
        .call(
            a,
            "phone",
            "claude.getCapabilities",
            json!({"threadId":"external"}),
        )
        .await
        .unwrap();
    assert_eq!(tools["status"], "unavailable");
    assert!(!test.dir.path().join("host-remote-links.jsonl").exists());
    assert!(test
        .call(a, prompt("external", "MUST_NOT_SEND", "normal"))
        .await
        .is_err());
    let after = test.detail(a, "external").await;
    assert!(!after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "MUST_NOT_SEND"));
    test.stop().await;
    let saved = test
        .db
        .call(|a| a.store.load_chat("external"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["external_claude"], true);
    assert_eq!(saved["thread"]["latestTurn"]["state"], "running");
}

#[tokio::test]
async fn hosted_claude_retains_idle_connection_and_reattaches_same_link_after_restart() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "hosted").await;
    test.call(a, prompt("hosted", "first", "normal"))
        .await
        .unwrap();
    test.wait(a, "hosted", "ready").await;
    test.call(a, prompt("hosted", "second", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "hosted", "ready").await;
    let links = std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl")).unwrap();
    assert_eq!(
        links.lines().count(),
        1,
        "turn completion must not close the Claude process"
    );
    // An idle shared connection does not occupy the one active-turn slot.
    test.create(AgentKind::Codex, "codex-beside-idle-host")
        .await;
    test.call(
        AgentKind::Codex,
        prompt("codex-beside-idle-host", "works", "normal"),
    )
    .await
    .unwrap();
    test.wait(AgentKind::Codex, "codex-beside-idle-host", "ready")
        .await;
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    wait_links(&test, 2).await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2,
        "reattach cannot resend a prompt"
    );
    let after = test.detail(a, "hosted").await;
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["latestTurn"], before["latestTurn"]);
    test.call(a, prompt("hosted", "third", "normal"))
        .await
        .unwrap();
    test.wait(a, "hosted", "ready").await;
    let links: Vec<Value> =
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
    assert_eq!(links.len(), 2);
    assert_eq!(links[1]["reattach"], links[0]["bridge"]);
    test.stop().await;
}

async fn wait_links(test: &Test, count: usize) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
                .unwrap_or_default()
                .lines()
                .count()
                >= count
            {
                tokio::time::sleep(Duration::from_millis(150)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_claude_recovers_process_loss_without_replaying_or_reopening() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "recover").await;
    test.call(a, prompt("recover", "crash-after-reply", "normal"))
        .await
        .unwrap();
    wait_links(&test, 2).await;
    let t = test.detail(a, "recover").await;
    assert_eq!(t["session"]["status"], "ready");
    assert_eq!(t["latestTurn"]["state"], "completed");
    assert_eq!(
        t["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["text"] == "BEFORE_CRASH_OK")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(t["activities"].as_array().unwrap().iter().any(|a| a["kind"]
        == "claude.desktop-sync.updated"
        && a["payload"]["status"] == "connected"));
    test.call(a, prompt("recover", "AFTER_RECOVERY_OK", "normal"))
        .await
        .unwrap();
    test.wait(a, "recover", "ready").await;
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_restart_failure_preserves_history_and_recovers_when_available() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "retry").await;
    test.call(a, prompt("retry", "KEPT", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "retry", "ready").await;
    test.stop().await;
    std::fs::write(test.dir.path().join("host-attach-fail"), "").unwrap();
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let failed = test.detail(a, "retry").await;
    assert_eq!(failed["messages"], before["messages"]);
    assert_eq!(failed["latestTurn"], before["latestTurn"]);
    assert_eq!(failed["session"]["status"], "ready");
    assert!(failed["activities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["kind"] == "claude.desktop-sync.updated"
            && v["payload"]["status"] == "disconnected"));
    std::fs::remove_file(test.dir.path().join("host-attach-fail")).unwrap();
    wait_links(&test, 2).await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let recovered = test.detail(a, "retry").await;
    assert_eq!(recovered["messages"], before["messages"]);
    assert_eq!(recovered["latestTurn"], before["latestTurn"]);
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_desktop_reply_during_local_preparation_keeps_both_turns() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "host-race").await;
    test.call(a, prompt("host-race", "first", "normal"))
        .await
        .unwrap();
    test.wait(a, "host-race", "ready").await;
    std::fs::write(test.dir.path().join("host-race"), "").unwrap();
    test.call(a, prompt("host-race", "second", "normal"))
        .await
        .unwrap();
    let thread = test.wait(a, "host-race", "ready").await;
    let rows = thread["messages"].as_array().unwrap();
    for text in ["first", "second", "DESKTOP_RACE_PROMPT", "DESKTOP_RACE_OK"] {
        assert!(
            rows.iter().any(|v| v["text"] == text),
            "Missing {text}: {rows:?}"
        );
    }
    assert!(!thread.to_string().contains("PRIVATE_BOOTSTRAP"));
    let desktop = rows
        .iter()
        .find(|v| v["text"] == "DESKTOP_RACE_OK")
        .unwrap();
    let local = rows
        .iter()
        .find(|v| v["text"] == "second" && v["role"] == "user")
        .unwrap();
    assert_ne!(desktop["turnId"], local["turnId"]);
    assert_eq!(thread["latestTurn"]["turnId"], local["turnId"]);
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_routes_early_start_and_hidden_meta_input_without_dropping_connection() {
    for meta in [false, true] {
        let test = Test::hosted().await;
        let a = AgentKind::Claude;
        test.create(a, "host-order").await;
        test.call(a, prompt("host-order", "first", "normal"))
            .await
            .unwrap();
        test.wait(a, "host-order", "ready").await;
        std::fs::write(test.dir.path().join("host-race"), "").unwrap();
        std::fs::write(test.dir.path().join("host-start-first"), "").unwrap();
        if meta {
            std::fs::write(test.dir.path().join("host-meta"), "").unwrap();
        }
        test.call(a, prompt("host-order", "second", "normal"))
            .await
            .unwrap();
        let thread = test.wait(a, "host-order", "ready").await;
        let rows = thread["messages"].as_array().unwrap();
        assert!(rows.iter().any(|v| v["text"] == "DESKTOP_RACE_OK"));
        assert_eq!(
            rows.iter()
                .filter(|v| v["text"] == "DESKTOP_RACE_PROMPT")
                .count(),
            usize::from(!meta)
        );
        assert!(!thread.to_string().contains("PRIVATE_BOOTSTRAP"));
        assert!(rows
            .iter()
            .any(|v| v["text"] == "second" && v["role"] == "assistant"));
        assert_eq!(
            std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        test.stop().await;
    }
}

#[tokio::test]
async fn hosted_claude_remote_turn_during_reattach_cannot_replace_handshake_or_replay_local_prompt()
{
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "reconnect-race").await;
    test.call(
        a,
        prompt("reconnect-race", "LOCAL_BEFORE_RECONNECT", "normal"),
    )
    .await
    .unwrap();
    test.wait(a, "reconnect-race", "ready").await;
    test.stop().await;
    std::fs::write(test.dir.path().join("host-attach-message"), "").unwrap();
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    wait_links(&test, 2).await;
    // The saved idle state can be visible before the new remote turn is read.
    // Wait for that turn's reply as well, rather than accepting the old state.
    let t =
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let t = test.detail(a, "reconnect-race").await;
                if t["session"]["status"] == "ready"
                    && t["messages"].as_array().unwrap().iter().any(|m| {
                        m["role"] == "assistant" && m["text"] == "REMOTE_DURING_RECONNECT_OK"
                    })
                {
                    return t;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("remote reply must complete after identity commit");
    for text in ["REMOTE_DURING_RECONNECT", "REMOTE_DURING_RECONNECT_OK"] {
        assert_eq!(
            t["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["text"] == text)
                .count(),
            1
        );
    }
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(t["activities"].as_array().unwrap().iter().any(|v| v["kind"]
        == "claude.desktop-sync.updated"
        && v["payload"]["status"] == "connected"));
    test.call(
        a,
        prompt("reconnect-race", "AFTER_RECONNECT_RACE", "normal"),
    )
    .await
    .unwrap();
    test.wait(a, "reconnect-race", "ready").await;
    test.stop().await;
}

#[tokio::test]
async fn hosted_stop_control_notice_cannot_create_a_pending_prompt_or_block_the_next_task() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "stop-notice").await;
    test.call(a, prompt("stop-notice", "hold", "normal"))
        .await
        .unwrap();
    test.wait(a, "stop-notice", "running").await;
    test.call(a,json!({"type":"thread.turn.interrupt","commandId":"stop-notice-once","threadId":"stop-notice"})).await.unwrap();
    let stopped = test.wait(a, "stop-notice", "interrupted").await;
    assert!(!stopped.to_string().contains("[Request interrupted"));
    test.call(a, prompt("stop-notice", "AFTER_STOP_OK", "normal"))
        .await
        .unwrap();
    let done = test.wait(a, "stop-notice", "ready").await;
    assert!(done.to_string().contains("AFTER_STOP_OK"));
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_recovery_keeps_interrupted_queue_paused_and_respects_opt_out() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "queued-recovery").await;
    test.call(a, prompt("queued-recovery", "hold", "normal"))
        .await
        .unwrap();
    test.wait(a, "queued-recovery", "running").await;
    test.call(a, prompt("queued-recovery", "DO_NOT_REPLAY", "queue"))
        .await
        .unwrap();
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    wait_links(&test, 2).await;
    let t = test.detail(a, "queued-recovery").await;
    assert_eq!(t["latestTurn"]["state"], "interrupted");
    assert_eq!(
        t["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["text"] == "DO_NOT_REPLAY")
            .unwrap()["queueStatus"],
        "paused"
    );
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
    test.config.claude.remote_control = false;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_paused_queue_sends_a_chosen_message_now_and_resumes_the_rest() {
    let mut test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "send-now").await;
    test.call(a, prompt("send-now", "hold", "normal"))
        .await
        .unwrap();
    test.wait(a, "send-now", "running").await;
    for text in ["FIRST_QUEUED", "SECOND_QUEUED"] {
        test.call(a, prompt("send-now", text, "queue"))
            .await
            .unwrap();
    }
    test.stop().await;
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    wait_links(&test, 2).await;
    let prompts = |test: &Test| {
        std::fs::read_to_string(test.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(prompts(&test), 1, "a restart never replays the queue");
    // "Steer instead" on an idle chat sends that message now, as the
    // TypeScript bridge does; the remaining queue then resumes after it.
    let send_now = json!({"type":"thread.turn.queue.steer","threadId":"send-now","commandId":"send-second-now","messageId":"message-send-now-SECOND_QUEUED-queue"});
    test.call(a, send_now.clone()).await.unwrap();
    let thread = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let thread = test.detail(a, "send-now").await;
            let texts: Vec<_> = thread["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user" && m["queueStatus"].is_null())
                .map(|m| m["text"].clone())
                .collect();
            if texts.len() == 3 && thread["session"]["status"] == "ready" {
                return (thread, texts);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the chosen and remaining queued messages did not run");
    assert_eq!(
        thread.1,
        [json!("hold"), json!("SECOND_QUEUED"), json!("FIRST_QUEUED")]
    );
    assert_eq!(prompts(&test), 3);
    // A retry answers from the command receipt; nothing is sent again.
    test.call(a, send_now).await.unwrap();
    assert_eq!(prompts(&test), 3);
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_wrapped_remote_prompt_has_one_bubble_without_internal_wrapper() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "peer-bubble").await;
    test.call(a, prompt("peer-bubble", "first", "normal"))
        .await
        .unwrap();
    test.wait(a, "peer-bubble", "ready").await;
    for file in ["host-peer", "host-race", "host-meta", "host-start-first"] {
        std::fs::write(test.dir.path().join(file), "").unwrap();
    }
    test.call(a, prompt("peer-bubble", "second", "normal"))
        .await
        .unwrap();
    let t = test.wait(a, "peer-bubble", "ready").await;
    let rows = t["messages"].as_array().unwrap();
    let prompts = rows
        .iter()
        .filter(|m| m["role"] == "user" && m["text"] == "DESKTOP_RACE_PROMPT")
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0]["turnId"],
        rows.iter()
            .find(|m| m["text"] == "DESKTOP_RACE_OK")
            .unwrap()["turnId"]
    );
    assert!(!t.to_string().contains("PRIVATE_BOOTSTRAP"));
    assert!(!t.to_string().contains("Another Claude session sent"));
    test.stop().await;
}

#[tokio::test]
async fn hosted_claude_stop_before_native_start_finishes_without_result_or_disconnect() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "host-prestart").await;
    test.call(a, prompt("host-prestart", "hold-before-start", "normal"))
        .await
        .unwrap();
    test.wait(a, "host-prestart", "running").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !test.dir.path().join("host-prompt-waiting").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    test.call(a, prompt("host-prestart", "QUEUED_AFTER_CANCEL", "queue"))
        .await
        .unwrap();
    test.call(a, json!({"type":"thread.turn.interrupt","commandId":"cancel-prestart","threadId":"host-prestart"})).await.unwrap();
    let stopped = test.wait(a, "host-prestart", "interrupted").await;
    assert_eq!(stopped["latestTurn"]["state"], "interrupted");
    let queued = stopped["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["text"] == "QUEUED_AFTER_CANCEL" && v["role"] == "user")
        .unwrap();
    assert_eq!(queued["phase"], "queued");
    assert_eq!(queued["queueStatus"], "paused");
    assert!(!stopped["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["text"] == "QUEUED_AFTER_CANCEL" && v["role"] == "assistant"));
    // A later explicit prompt reuses the still-connected host.
    test.call(a, json!({"type":"thread.turn.queue.cancel","threadId":"host-prestart","commandId":"remove-paused","messageId":queued["id"]})).await.unwrap();
    test.call(a, prompt("host-prestart", "AFTER_PRESTART_STOP", "normal"))
        .await
        .unwrap();
    let after = test.wait(a, "host-prestart", "ready").await;
    assert!(after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["role"] == "assistant" && v["text"] == "AFTER_PRESTART_STOP"));
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
}

#[tokio::test]
async fn claude_background_tools_finish_before_queue_and_do_not_get_killed_at_foreground_result() {
    for hosted in [false, true] {
        let t = if hosted {
            Test::hosted().await
        } else {
            Test::new().await
        };
        let a = AgentKind::Claude;
        t.create(a, "background").await;
        t.call(a, prompt("background", "background-fixture", "normal"))
            .await
            .unwrap();
        let first = t.wait(a, "background", "running").await;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let detail = t.detail(a, "background").await;
                if detail["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["text"] == "FOREGROUND_COMPLETE")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            t.detail(a, "background").await["session"]["status"],
            "running"
        );
        t.call(a, prompt("background", "AFTER_BACKGROUND_QUEUE", "queue"))
            .await
            .unwrap();
        t.call(a, prompt("background", "BACKGROUND_STEER", "steer"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let held = t.detail(a, "background").await;
        assert_eq!(held["session"]["status"], "running");
        assert_eq!(held["latestTurn"]["turnId"], first["latestTurn"]["turnId"]);
        assert!(!held["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "assistant" && m["text"] == "AFTER_BACKGROUND_QUEUE"));
        std::fs::write(t.dir.path().join("background-release"), "finish").unwrap();
        let done = t.wait(a, "background", "ready").await;
        assert_eq!(
            std::fs::read_to_string(t.dir.path().join("background-finished")).unwrap(),
            "TOOL_REALLY_FINISHED"
        );
        for text in ["BACKGROUND_STEER", "AFTER_BACKGROUND_QUEUE"] {
            assert_eq!(
                done["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] == "assistant" && m["text"] == text)
                    .count(),
                1
            );
        }
        t.stop().await;
    }
}

#[tokio::test]
async fn android_folder_picker_legacy_model_placeholder_uses_each_agents_default() {
    let t = Test::new().await;
    for (agent, id) in [
        (AgentKind::Codex, "codex-project"),
        (AgentKind::Claude, "claude-project"),
    ] {
        let command = json!({"type":"project.create","commandId":id,"projectId":id,"title":id,"workspaceRoot":t.dir.path().join(id),"createWorkspaceRootIfMissing":true,"defaultModelSelection":{"instanceId":"codex","model":"gpt-5.4"}});
        t.call(agent, command.clone()).await.unwrap();
        t.call(agent, command).await.unwrap();
        let shell = t
            .hub
            .call(agent, "phone", "orchestration.subscribeShell", json!({}))
            .await
            .unwrap();
        let p = shell["snapshot"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap();
        assert_eq!(p["defaultModelSelection"]["model"], "test");
        assert_eq!(p["defaultModelSelection"]["instanceId"], agent.instance());
        assert!(t.call(agent,json!({"type":"project.meta.update","commandId":format!("edit-{id}"),"projectId":id,"defaultModelSelection":{"instanceId":"codex","model":"gpt-5.4"}})).await.is_err());
    }
    t.stop().await;
}

#[tokio::test]
async fn hosted_claude_steer_queue_and_stop_keep_native_connection() {
    let test = Test::hosted().await;
    let a = AgentKind::Claude;
    test.create(a, "host-controls").await;
    test.call(a, prompt("host-controls", "hold", "normal"))
        .await
        .unwrap();
    let before = test.wait(a, "host-controls", "running").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    test.call(a, prompt("host-controls", "QUEUED_OK", "queue"))
        .await
        .unwrap();
    test.call(a, prompt("host-controls", "STEER_OK", "steer"))
        .await
        .unwrap();
    let thread = test.wait(a, "host-controls", "ready").await;
    let rows = thread["messages"].as_array().unwrap();
    let steer = rows
        .iter()
        .find(|v| v["role"] == "assistant" && v["text"] == "STEER_OK")
        .unwrap();
    assert_eq!(steer["turnId"], before["latestTurn"]["turnId"]);
    assert!(rows
        .iter()
        .any(|v| v["role"] == "assistant" && v["text"] == "QUEUED_OK"));
    test.call(a, prompt("host-controls", "hold-no-context", "normal"))
        .await
        .unwrap();
    test.wait(a, "host-controls", "running").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    test.call(
        a,
        json!({"type":"thread.turn.interrupt","commandId":"host-stop","threadId":"host-controls"}),
    )
    .await
    .unwrap();
    test.wait(a, "host-controls", "interrupted").await;
    assert_eq!(
        std::fs::read_to_string(test.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    test.stop().await;
}
fn prompt(id: &str, text: &str, mode: &str) -> Value {
    json!({"type":"thread.turn.start","commandId":format!("{id}-{text}-{mode}"),"threadId":id,"message":{"messageId":format!("message-{id}-{text}-{mode}"),"text":text,"attachments":[]},"deliveryMode":mode,"createdAt":"2026-01-01T00:00:00Z"})
}
fn fixture_git(root: &std::path::Path, args: &[&str]) -> String {
    let mut c = std::process::Command::new("git");
    c.current_dir(root)
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if args.first() == Some(&"init") {
        // Worktree creation runs through the production helper, which rightly
        // respects repository settings. Pin fixture bytes in the repository,
        // not only in this helper's individual Git invocations.
        fixture_git(root, &["config", "core.autocrlf", "false"]);
    }
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn codex_ignores_foreign_chat_output_controls_and_completion() {
    let t = Test::new().await;
    t.create(AgentKind::Codex, "foreign").await;
    t.call(
        AgentKind::Codex,
        prompt("foreign", "foreign-events", "normal"),
    )
    .await
    .unwrap();
    let detail = t.wait(AgentKind::Codex, "foreign", "ready").await;
    let rendered = detail.to_string();
    assert!(rendered.contains("OWN_THREAD_OK"));
    assert!(rendered.contains("FOREIGN_EVENTS_FILTERED"));
    assert!(!rendered.contains("FOREIGN_PRIVATE_TEXT"));
    assert!(!rendered.contains("echo foreign"));
    assert!(!rendered.contains("CLI request could not continue"));
    assert_eq!(detail["hasPendingApprovals"], false);
    t.stop().await;
}

#[tokio::test]
async fn archives_remain_separate_and_restore_history_after_restart_for_both_agents() {
    let mut t = Test::new().await;
    for (agent, id) in [
        (AgentKind::Codex, "archive-codex"),
        (AgentKind::Claude, "archive-claude"),
    ] {
        t.create(agent, id).await;
        t.call(agent, prompt(id, "keep history", "normal"))
            .await
            .unwrap();
        let original = t.wait(agent, id, "ready").await;
        let cmd =
            json!({"type":"thread.archive","commandId":format!("archive-{id}"),"threadId":id});
        t.call(agent, cmd.clone()).await.unwrap();
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd).await.unwrap();
        let recent = t
            .hub
            .call(agent, "phone", "orchestration.subscribeShell", json!({}))
            .await
            .unwrap();
        assert!(!recent["snapshot"]["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == id));
        let archived = t
            .hub
            .call(
                agent,
                "phone",
                "orchestration.getArchivedShellSnapshot",
                json!({}),
            )
            .await
            .unwrap();
        let rows = archived["threads"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], id);
        assert!(rows[0].get("messages").is_none());
        assert_eq!(t.detail(agent, id).await["messages"], original["messages"]);
        t.call(
            agent,
            json!({"type":"thread.unarchive","commandId":format!("unarchive-{id}"),"threadId":id}),
        )
        .await
        .unwrap();
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "orchestration.getArchivedShellSnapshot",
                json!({})
            )
            .await
            .unwrap()["threads"]
            .as_array()
            .unwrap()
            .is_empty());
        let recent = t
            .hub
            .call(agent, "phone", "orchestration.subscribeShell", json!({}))
            .await
            .unwrap();
        assert_eq!(recent["snapshot"]["threads"][0]["id"], id);
        assert_eq!(t.detail(agent, id).await["messages"], original["messages"]);
        t.call(
            agent,
            json!({"type":"thread.delete","commandId":format!("delete-{id}"),"threadId":id}),
        )
        .await
        .unwrap();
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "orchestration.getArchivedShellSnapshot",
                json!({})
            )
            .await
            .unwrap()["threads"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    t.stop().await;
}
#[tokio::test]
async fn native_control_requests_are_answered_and_repeated_unknowns_stop() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "controls").await;
        t.call(agent, prompt("controls", "unknown-control", "normal"))
            .await
            .unwrap();
        let d = t.wait(agent, "controls", "ready").await;
        assert!(d["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "CONTROL_REJECTED"));
        assert!(!d.to_string().contains("PRIVATE_CONTROL"));
        assert_eq!(d["hasPendingApprovals"], false);
        assert!(d["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "session.notice"));
        if agent == AgentKind::Codex {
            t.call(agent, prompt("controls", "time-control", "normal"))
                .await
                .unwrap();
            let d = t.wait(agent, "controls", "ready").await;
            assert!(d["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["text"] == "TIME_REPLIED"));
        }
        t.call(agent, prompt("controls", "control-loop", "normal"))
            .await
            .unwrap();
        let d = t.wait(agent, "controls", "error").await;
        assert!(d
            .to_string()
            .contains("repeatedly requested unsupported controls"));
        assert_eq!(d["hasPendingApprovals"], false);
        t.stop().await;
    }
}
#[tokio::test]
async fn mcp_forms_reject_approvals_and_invalid_answers_then_resume_once() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "forms").await;
        for round in 0..2 {
            let mut p = prompt("forms", "elicitation", "normal");
            p["commandId"] = json!(format!("elicit-{round}"));
            p["message"]["messageId"] = json!(format!("elicit-{round}"));
            t.call(agent, p).await.unwrap();
            let d = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let d = t.detail(agent, "forms").await;
                    if d["hasPendingUserInput"] == true {
                        break d;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(d["hasPendingApprovals"], false);
            let a = d["activities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["kind"] == "user-input.requested")
                .unwrap();
            let rid = a["payload"]["requestId"].clone();
            assert_eq!(a["payload"]["questions"][0]["options"][0]["label"], "Blue");
            assert!(t.call(agent,json!({"type":"thread.approval.respond","threadId":"forms","commandId":format!("wrong-{round}"),"requestId":rid,"decision":"approved"})).await.is_err());
            if round == 0 {
                let mut c = json!({"type":"thread.user-input.respond","threadId":"forms","commandId":"answer-form","requestId":rid,"answers":{"mcp-field-0":{"answers":["Wrong"]}}});
                assert!(t.call(agent, c.clone()).await.is_err());
                assert_eq!(t.detail(agent, "forms").await["hasPendingUserInput"], true);
                c["answers"]["mcp-field-0"]["answers"] = json!(["Blue"]);
                t.call(agent, c.clone()).await.unwrap();
                let d = t.wait(agent, "forms", "ready").await;
                assert_eq!(d["hasPendingUserInput"], false);
                assert!(d["messages"].as_array().unwrap().iter().any(|m| {
                    serde_json::from_str::<Value>(m["text"].as_str().unwrap_or(""))
                        .is_ok_and(|v| v["action"] == "accept" && v["content"]["color"] == "blue")
                }));
                t.call(agent, c).await.unwrap();
            } else {
                t.call(agent,json!({"type":"thread.turn.interrupt","threadId":"forms","commandId":"stop-form"})).await.unwrap();
                let d = t.wait(agent, "forms", "interrupted").await;
                assert_eq!(d["hasPendingUserInput"], false);
            }
        }
        t.stop().await;
    }
}
#[tokio::test]
async fn mcp_browser_steps_cancel_without_hanging_or_granting_consent() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "browser-step").await;
        t.call(agent, prompt("browser-step", "elicitation-url", "normal"))
            .await
            .unwrap();
        let d = t.wait(agent, "browser-step", "ready").await;
        assert_eq!(d["hasPendingUserInput"], false);
        assert!(d["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "{\"action\":\"cancel\"}"));
        assert!(d["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "session.notice"
                && a["payload"]["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("browser")));
        t.stop().await;
    }
}
#[tokio::test]
async fn worktree_chat_isolated_idempotent_and_recovers_after_checkout_before_receipt() {
    let mut t = Test::new().await;
    let root = t.dir.path().join("project with spaces");
    std::fs::create_dir(&root).unwrap();
    fixture_git(&root, &["init", "-b", "main"]);
    fixture_git(&root, &["config", "user.email", "fixture@example.invalid"]);
    fixture_git(&root, &["config", "user.name", "Fixture"]);
    std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
    fixture_git(&root, &["add", "tracked.txt"]);
    fixture_git(&root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("tracked.txt"), "staged\n").unwrap();
    fixture_git(&root, &["add", "tracked.txt"]);
    std::fs::write(root.join("tracked.txt"), "dirty\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "keep\n").unwrap();
    let source_head = fixture_git(&root, &["rev-parse", "HEAD"]);
    let source_status = fixture_git(&root, &["status", "--porcelain=v1"]);
    t.call(AgentKind::Claude,json!({"type":"project.create","commandId":"repo","projectId":"repo","workspaceRoot":root,"title":"Repo"})).await.unwrap();
    t.call(AgentKind::Claude,json!({"type":"thread.create","commandId":"source","threadId":"source","projectId":"repo","runtimeMode":"full-access"})).await.unwrap();
    let request = json!({"threadId":"source","commandId":"worktree-one","action":"newWorktree"});
    let first = t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "claude.sessionAction",
            request.clone(),
        )
        .await
        .unwrap();
    let id = first["threadId"].as_str().unwrap().to_owned();
    let d = t.detail(AgentKind::Claude, &id).await;
    let checkout = PathBuf::from(d["worktreePath"].as_str().unwrap());
    assert_ne!(checkout, root);
    assert_eq!(d["runtimeMode"], "approval-required");
    assert_eq!(d["projectId"], "repo");
    assert!(d["messages"].as_array().unwrap().is_empty());
    assert_eq!(fixture_git(&checkout, &["rev-parse", "HEAD"]), source_head);
    assert_eq!(
        std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
        "committed\n"
    );
    assert!(!checkout.join("untracked.txt").exists());
    assert_eq!(
        fixture_git(&root, &["status", "--porcelain=v1"]),
        source_status
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "dirty\n"
    );
    assert_eq!(
        fixture_git(&root, &["symbolic-ref", "--short", "HEAD"]),
        "main\n"
    );
    assert_eq!(
        t.hub
            .call(
                AgentKind::Claude,
                "phone",
                "claude.sessionAction",
                request.clone()
            )
            .await
            .unwrap(),
        first
    );
    t.stop().await;
    // Simulate a crash after Git succeeded but before the chat + receipt committed.
    let stored = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    stored
        .execute("DELETE FROM chats WHERE id=?1", [&id])
        .unwrap();
    stored
        .execute(
            "DELETE FROM native_tool_actions WHERE id='claude-worktree:phone:worktree-one'",
            [],
        )
        .unwrap();
    drop(stored);
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(
                AgentKind::Claude,
                "phone",
                "claude.sessionAction",
                request.clone()
            )
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        fixture_git(&root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        2
    );
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(
                AgentKind::Claude,
                "phone",
                "claude.sessionAction",
                request.clone()
            )
            .await
            .unwrap(),
        first
    );
    let mut conflict = request.clone();
    conflict["threadId"] = json!(id);
    assert!(t
        .hub
        .call(AgentKind::Claude, "phone", "claude.sessionAction", conflict)
        .await
        .unwrap_err()
        .contains("different content"));
    t.call(AgentKind::Claude, prompt("source", "hold", "normal"))
        .await
        .unwrap();
    let mut busy = request;
    busy["commandId"] = json!("busy");
    assert!(t
        .hub
        .call(AgentKind::Claude, "phone", "claude.sessionAction", busy)
        .await
        .unwrap_err()
        .contains("Finish the task"));
    t.stop().await;
}
#[tokio::test]
async fn reused_native_request_ids_do_not_overwrite_old_controls_or_hide_new_questions() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "request-pages").await;
        let mut previous = String::new();
        for round in 0..2 {
            let mut c = prompt("request-pages", "question", "normal");
            c["commandId"] = json!(format!("request-page-{round}"));
            c["message"]["messageId"] = json!(format!("question-{round}"));
            t.call(agent, c).await.unwrap();
            let d = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let d = t.detail(agent, "request-pages").await;
                    if d["hasPendingUserInput"] == true {
                        break d;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let row = d["activities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["kind"] == "user-input.requested")
                .expect("new question must remain in the recent page");
            let rid = row["payload"]["requestId"].as_str().unwrap();
            assert!(rid.starts_with("request-"));
            assert_ne!(rid, previous);
            let key = row["payload"]["questions"][0]["id"].as_str().unwrap();
            let answer = json!({"type":"thread.user-input.respond","threadId":"request-pages","commandId":format!("answer-page-{round}"),"requestId":rid,"answers":{key:["Blue"]}});
            if !previous.is_empty() {
                let mut stale = answer.clone();
                stale["requestId"] = json!(previous);
                assert!(t.call(agent, stale).await.is_err());
            }
            t.call(agent, answer).await.unwrap();
            t.wait(agent, "request-pages", "ready").await;
            previous = rid.to_owned();
            if round == 0 {
                t.call(agent, prompt("request-pages", "many", "normal"))
                    .await
                    .unwrap();
                t.wait_bulk_history(agent, "request-pages").await;
            }
        }
        let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
        let resolved:i64=db.query_row("SELECT count(*) FROM chat_rows WHERE chat='request-pages' AND kind='activities' AND json_extract(data,'$.kind')='user-input.resolved'",[],|r|r.get(0)).unwrap();
        assert_eq!(resolved, 2, "both original request records must survive");
        t.stop().await;
    }
}
#[tokio::test]
async fn unanswered_controls_remain_visible_after_parallel_output_spills_their_page() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "spilled-question").await;
        t.call(
            agent,
            prompt("spilled-question", "question-spill", "normal"),
        )
        .await
        .unwrap();
        let started = std::time::Instant::now();
        let d = tokio::time::timeout(BULK_HISTORY_TIMEOUT, async {
            loop {
                let d = t.detail(agent, "spilled-question").await;
                if d["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["text"].as_str().unwrap_or("").starts_with("history-159:"))
                {
                    break d;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        eprintln!(
            "Durable question history {agent:?}: {:?}",
            started.elapsed()
        );
        assert_eq!(d["hasPendingUserInput"], true);
        assert!(d["messages"].as_array().unwrap().len() <= 24);
        let row = d["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "user-input.requested")
            .expect("pending form must remain reachable");
        let rid = &row["payload"]["requestId"];
        let key = row["payload"]["questions"][0]["id"].as_str().unwrap();
        t.call(agent,json!({"type":"thread.user-input.respond","threadId":"spilled-question","commandId":"answer-spilled","requestId":rid,"answers":{key:["Blue"]}})).await.unwrap();
        let d = t.wait(agent, "spilled-question", "ready").await;
        assert_eq!(d["hasPendingUserInput"], false);
        assert!(!d["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "user-input.requested"));
        t.stop().await;
    }
}
#[tokio::test]
async fn worktree_without_repository_is_rejected_without_creating_a_chat() {
    let t = Test::new().await;
    t.create(AgentKind::Claude, "plain").await;
    let result = t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "claude.sessionAction",
            json!({"threadId":"plain","commandId":"no-repo","action":"newWorktree"}),
        )
        .await;
    assert!(result.is_err());
    assert!(!t.dir.path().join(".remodex-worktrees").exists());
    assert_eq!(
        t.db.call(|a| a.store.chat_headers().unwrap().len())
            .await
            .unwrap(),
        1
    );
    t.stop().await;
}
#[tokio::test]
async fn worktree_never_nests_its_checkout_inside_the_source_repository() {
    let t = Test::new().await;
    let root = t.dir.path();
    fixture_git(root, &["init", "-b", "main"]);
    std::fs::write(root.join("source.txt"), "keep").unwrap();
    fixture_git(root, &["add", "source.txt"]);
    fixture_git(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    t.create(AgentKind::Claude, "root-project").await;
    let request =
        json!({"threadId":"root-project","commandId":"no-nested-checkout","action":"newWorktree"});
    assert!(t
        .hub
        .call(AgentKind::Claude, "phone", "claude.sessionAction", request)
        .await
        .unwrap_err()
        .contains("outside this repository"));
    assert!(!root.join(".remodex-worktrees").exists());
    assert_eq!(
        fixture_git(root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
    t.stop().await;
}
#[tokio::test]
async fn claude_checkpoints_preview_changed_files_restore_and_receipts_preserve_history() {
    let mut t = Test::new().await;
    let a = AgentKind::Claude;
    let file = t.dir.path().join("restore-fixture.txt");
    std::fs::write(&file, "before").unwrap();
    let untouched = t.dir.path().join("bash-change.txt");
    std::fs::write(&untouched, "keep").unwrap();
    t.create(a, "restore").await;
    t.call(a, prompt("restore", "checkpoint-write", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "restore", "ready").await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    let points = t
        .hub
        .call(
            a,
            "phone",
            "claude.sessionAction",
            json!({"threadId":"restore","action":"checkpoints"}),
        )
        .await
        .unwrap();
    assert_eq!(points["points"].as_array().unwrap().len(), 1);
    assert_eq!(points["points"][0]["label"], "checkpoint-write");
    let preview_request = json!({"threadId":"restore","commandId":"preview","action":"previewRestore","point":points["points"][0]["id"]});
    let preview = t
        .hub
        .call(a, "phone", "claude.sessionAction", preview_request.clone())
        .await
        .unwrap();
    assert_eq!(preview["files"], json!(["restore-fixture.txt"]));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    let restore_request = json!({"threadId":"restore","commandId":"restore-command","action":"restoreFiles","token":preview["token"]});
    assert!(t
        .hub
        .call(
            a,
            "other-phone",
            "claude.sessionAction",
            restore_request.clone()
        )
        .await
        .unwrap_err()
        .contains("expired"));
    std::fs::write(&file, "user-edited-after-preview").unwrap();
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", restore_request.clone())
        .await
        .unwrap_err()
        .contains("changed after the preview"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "user-edited-after-preview"
    );
    assert!(!t.dir.path().join("restore-count").exists());
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", restore_request)
        .await
        .unwrap_err()
        .contains("expired"));
    let preview = t
        .hub
        .call(a, "phone", "claude.sessionAction", preview_request.clone())
        .await
        .unwrap();
    let restore_request = json!({"threadId":"restore","commandId":"restore-command-two","action":"restoreFiles","token":preview["token"]});
    let result = t
        .hub
        .call(a, "phone", "claude.sessionAction", restore_request.clone())
        .await
        .unwrap();
    assert_eq!(result["restored"], true);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
    assert_eq!(std::fs::read_to_string(&untouched).unwrap(), "keep");
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("restore-count")).unwrap(),
        "1"
    );
    let after = t.detail(a, "restore").await;
    assert_eq!(before["messages"], after["messages"]);
    assert!(after["activities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["summary"] == "Tracked files restored"));
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", restore_request)
            .await
            .unwrap(),
        result
    );
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("restore-count")).unwrap(),
        "1"
    );
    let preview = t
        .hub
        .call(a, "phone", "claude.sessionAction", preview_request)
        .await
        .unwrap();
    assert_eq!(preview["files"], json!([]));
    assert!(preview["token"].is_null());
    let invalid = json!({"threadId":"restore","commandId":"bad-point","action":"previewRestore","point":"not-this-chat"});
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", invalid)
        .await
        .unwrap_err()
        .contains("does not belong"));
    t.stop().await;
}
#[tokio::test]
async fn branching_pins_native_history_and_copies_all_pages_with_durable_receipts() {
    let mut t = Test::new().await;
    let a = AgentKind::Claude;
    t.create(a, "branch-source").await;
    t.call(a, prompt("branch-source", "many", "normal"))
        .await
        .unwrap();
    t.wait_bulk_history(a, "branch-source").await;
    let source =
        t.db.call(|a| a.store.load_chat("branch-source"))
            .await
            .unwrap()
            .unwrap();
    let folder = t.dir.path().join("projects/fixture");
    std::fs::create_dir_all(&folder).unwrap();
    let point = uuid::Uuid::new_v4().to_string();
    let source_native = source["native_id"].as_str().unwrap();
    let transcript = folder.join(format!("{source_native}.jsonl"));
    std::fs::write(
        &transcript,
        format!(
            "{}\n",
            json!({"type":"assistant","parentUuid":null,"uuid":point,"sessionId":source_native})
        ),
    )
    .unwrap();
    let request = json!({"threadId":"branch-source","commandId":"fork","action":"branchChat"});
    let fork = t
        .hub
        .call(a, "phone", "claude.sessionAction", request.clone())
        .await
        .unwrap();
    let id = fork["threadId"].as_str().unwrap().to_owned();
    let fork_id = id.clone();
    let state =
        t.db.call(move |a| a.store.load_chat(&fork_id))
            .await
            .unwrap()
            .unwrap();
    assert_ne!(state["native_id"], source["native_id"]);
    assert_eq!(state["fork_from"]["session"], source["native_id"]);
    assert_eq!(state["fork_from"]["point"], point);
    assert_eq!(state["thread"]["runtimeMode"], "approval-required");
    let conn = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    let rows = |id: &str| {
        conn.query_row(
            "SELECT count(*) FROM chat_rows WHERE chat=?1 AND kind='messages'",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(rows(&id), rows("branch-source"));
    assert!(rows(&id) > 100);
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", request.clone())
            .await
            .unwrap(),
        fork
    );
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", request)
            .await
            .unwrap(),
        fork
    );
    // A later source append must not move the pinned branch point.
    use std::io::Write;
    writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(),"{}",json!({"type":"user","parentUuid":point,"uuid":uuid::Uuid::new_v4().to_string(),"sessionId":source_native})).unwrap();
    t.call(a, prompt(&id, "branch-arguments", "normal"))
        .await
        .unwrap();
    let detail = t.wait(a, &id, "ready").await;
    let args: Vec<String> = serde_json::from_str(
        detail["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "assistant")
            .unwrap()["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(args.iter().any(|s| s == "--fork-session"));
    assert!(args
        .windows(2)
        .any(|s| s == ["--resume-session-at", point.as_str()]));
    assert!(args.windows(2).any(|s| s == ["--resume", source_native]));
    let fork_id = id.clone();
    let state =
        t.db.call(move |a| a.store.load_chat(&fork_id))
            .await
            .unwrap()
            .unwrap();
    assert!(state["fork_from"].is_null());
    assert!(rows(&id) > rows("branch-source"));
    let points = t
        .hub
        .call(
            a,
            "phone",
            "claude.sessionAction",
            json!({"threadId":id,"action":"checkpoints"}),
        )
        .await
        .unwrap();
    assert_eq!(points["points"].as_array().unwrap().len(), 1);
    t.stop().await;
}
#[tokio::test]
async fn starting_a_turn_invalidates_a_file_preview_without_restoring() {
    let t = Test::new().await;
    let a = AgentKind::Claude;
    let file = t.dir.path().join("restore-fixture.txt");
    std::fs::write(&file, "original").unwrap();
    t.create(a, "restore-busy").await;
    t.call(a, prompt("restore-busy", "checkpoint-write", "normal"))
        .await
        .unwrap();
    t.wait(a, "restore-busy", "ready").await;
    let points = t
        .hub
        .call(
            a,
            "phone",
            "claude.sessionAction",
            json!({"threadId":"restore-busy","action":"checkpoints"}),
        )
        .await
        .unwrap();
    let request = json!({"threadId":"restore-busy","commandId":"preview","action":"previewRestore","point":points["points"][0]["id"]});
    let preview = t
        .hub
        .call(a, "phone", "claude.sessionAction", request.clone())
        .await
        .unwrap();
    t.call(a, prompt("restore-busy", "hold", "normal"))
        .await
        .unwrap();
    t.wait(a, "restore-busy", "running").await;
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", request)
        .await
        .unwrap_err()
        .contains("Finish the task"));
    t.call(
        a,
        json!({"threadId":"restore-busy","commandId":"stop","type":"thread.turn.interrupt"}),
    )
    .await
    .unwrap();
    t.wait(a, "restore-busy", "interrupted").await;
    let restore = json!({"threadId":"restore-busy","commandId":"old-preview","action":"restoreFiles","token":preview["token"]});
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", restore)
        .await
        .unwrap_err()
        .contains("expired"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    t.stop().await;
}
#[tokio::test]
async fn claude_tool_controls_persist_per_chat_and_apply_before_the_next_prompt() {
    let mut t = Test::new().await;
    let a = AgentKind::Claude;
    t.create(a, "controlled-tools").await;
    let toggle = json!({"threadId":"controlled-tools","commandId":"disable-tools","action":"mcpToggle","name":"fixture","enabled":false});
    let first = t
        .hub
        .call(a, "phone", "claude.sessionAction", toggle.clone())
        .await
        .unwrap();
    assert_eq!(first["updated"], true);
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", toggle.clone())
            .await
            .unwrap(),
        first
    );
    let mut conflict = toggle.clone();
    conflict["enabled"] = json!(true);
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", conflict)
        .await
        .unwrap_err()
        .contains("different content"));
    let catalog = t
        .hub
        .call(
            a,
            "phone",
            "claude.getCapabilities",
            json!({"threadId":"controlled-tools"}),
        )
        .await
        .unwrap();
    assert_eq!(catalog["mcpServers"][0]["status"], "disabled");
    assert!(t.detail(a, "controlled-tools").await["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", toggle.clone())
            .await
            .unwrap(),
        first
    );
    t.call(a, prompt("controlled-tools", "tools-state", "normal"))
        .await
        .unwrap();
    let d = t.wait(a, "controlled-tools", "ready").await;
    assert!(d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "TOOLS_DISABLED"));
    let mut enable = toggle;
    enable["commandId"] = json!("enable-tools");
    enable["enabled"] = json!(true);
    t.hub
        .call(a, "phone", "claude.sessionAction", enable)
        .await
        .unwrap();
    let mut p = prompt("controlled-tools", "tools-state", "normal");
    p["commandId"] = json!("tools-state-enabled");
    p["message"]["messageId"] = json!("enabled-check");
    t.call(a, p).await.unwrap();
    let d = t.wait(a, "controlled-tools", "ready").await;
    assert!(d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "TOOLS_ENABLED"));
    for action in ["mcpReconnect", "reloadSkills", "reloadPlugins"] {
        let result=t.hub.call(a,"phone","claude.sessionAction",json!({"threadId":"controlled-tools","commandId":action,"action":action,"name":"fixture"})).await.unwrap();
        assert_eq!(result["updated"], true);
        assert!(!result.to_string().contains("PRIVATE_METADATA"));
    }
    let unknown = json!({"threadId":"controlled-tools","commandId":"unknown","action":"mcpToggle","name":"not-configured","enabled":false});
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", unknown)
        .await
        .unwrap_err()
        .contains("configured"));
    t.call(a, prompt("controlled-tools", "hold", "normal"))
        .await
        .unwrap();
    t.wait(a, "controlled-tools", "running").await;
    assert!(t
        .hub
        .call(
            a,
            "phone",
            "claude.sessionAction",
            json!({"threadId":"controlled-tools","commandId":"busy","action":"reloadSkills"})
        )
        .await
        .unwrap_err()
        .contains("Finish the task"));
    assert_eq!(
        t.detail(a, "controlled-tools").await["session"]["status"],
        "running"
    );
    t.stop().await;
}
#[tokio::test]
async fn idle_claude_tools_are_available_without_starting_or_changing_a_turn() {
    let t = Test::new().await;
    let a = AgentKind::Claude;
    t.create(a, "idle-tools").await;
    let before = t.detail(a, "idle-tools").await;
    let mut reads = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let h = t.hub.clone();
        reads.spawn(async move {
            h.call(
                a,
                "phone",
                "claude.getCapabilities",
                json!({"threadId":"idle-tools"}),
            )
            .await
        });
    }
    while let Some(result) = reads.join_next().await {
        let value = result.unwrap().unwrap();
        assert_eq!(value["scope"], "configuration");
        assert_eq!(value["mcpServers"][0]["tools"][0], "read_fixture");
        assert!(!value.to_string().contains("PRIVATE_METADATA"));
    }
    let after = t.detail(a, "idle-tools").await;
    assert_eq!(after["latestTurn"], before["latestTurn"]);
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["session"], before["session"]);
    t.stop().await;
}
#[tokio::test]
async fn idle_host_metadata_uses_its_existing_connection_without_creating_a_prompt() {
    let mut t = Test::hosted().await;
    let a = AgentKind::Claude;
    t.create(a, "idle-metadata").await;
    t.call(a, prompt("idle-metadata", "first", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "idle-metadata", "ready").await;
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    wait_links(&t, 2).await;
    let capabilities = t
        .hub
        .call(
            a,
            "phone",
            "claude.getCapabilities",
            json!({"threadId":"idle-metadata"}),
        )
        .await
        .unwrap();
    assert_eq!(capabilities["status"], "ready");
    assert_eq!(capabilities["mcpServers"][0]["tools"][0], "read_fixture");
    t.hub
        .call(
            a,
            "phone",
            "claude.getUsage",
            json!({"threadId":"idle-metadata"}),
        )
        .await
        .unwrap();
    let after = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let d = t.detail(a, "idle-metadata").await;
            if d["activities"].as_array().unwrap().iter().any(|a| {
                a["kind"] == "context-window.updated" && a["payload"]["usedTokens"] == 123.0
            }) {
                break d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["latestTurn"], before["latestTurn"]);
    assert_eq!(after["session"]["status"], "ready");
    assert!(!after.to_string().contains("PRIVATE_METADATA"));
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-remote-links.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "recovering metadata must not send or replay a model prompt"
    );
    t.stop().await;
}
#[tokio::test]
async fn hosted_controls_use_one_worker_and_restore_policy_before_reconnecting() {
    let mut t = Test::hosted().await;
    let a = AgentKind::Claude;
    t.create(a, "live-tools").await;
    t.call(a, prompt("live-tools", "first", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "live-tools", "ready").await;
    // Real native controls need more than one scheduler tick. A millisecond /
    // second mismatch must not report an applied operation as uncertain.
    std::fs::write(t.dir.path().join("control-delay"), "").unwrap();
    let request = json!({"threadId":"live-tools","commandId":"disable-live","action":"mcpToggle","name":"fixture","enabled":false});
    for _ in 0..2 {
        assert_eq!(
            t.hub
                .call(a, "phone", "claude.sessionAction", request.clone())
                .await
                .unwrap()["updated"],
            true
        );
    }
    let mut conflict = request.clone();
    conflict["enabled"] = json!(true);
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", conflict)
        .await
        .unwrap_err()
        .contains("different content"));
    for action in ["reloadSkills", "reloadPlugins", "mcpReconnect"] {
        assert_eq!(t.hub.call(a,"phone","claude.sessionAction",json!({"threadId":"live-tools","commandId":action,"action":action,"name":"fixture"})).await.unwrap()["updated"],true);
    }
    let capabilities = t
        .hub
        .call(
            a,
            "phone",
            "claude.getCapabilities",
            json!({"threadId":"live-tools"}),
        )
        .await
        .unwrap();
    assert_eq!(capabilities["mcpServers"][0]["status"], "disabled");
    assert_eq!(capabilities["plugins"][0]["name"], "fixture");
    assert!(!capabilities.to_string().contains("PRIVATE_METADATA"));
    let after = t.detail(a, "live-tools").await;
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["latestTurn"], before["latestTurn"]);
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-controls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    wait_links(&t, 2).await;
    let links: Vec<Value> = std::fs::read_to_string(t.dir.path().join("host-remote-links.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        links[1]["disabled"], true,
        "restore before remote clients can send a prompt"
    );
    assert_eq!(links[1]["reattach"], links[0]["bridge"]);
    assert_eq!(
        t.hub
            .call(a, "phone", "claude.sessionAction", request)
            .await
            .unwrap()["updated"],
        true
    );
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-controls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    t.call(a, prompt("live-tools", "tools-state", "normal"))
        .await
        .unwrap();
    let after = t.wait(a, "live-tools", "ready").await;
    assert!(after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["text"] == "TOOLS_DISABLED"));
    t.stop().await;
}
fn access_controls(t: &Test) -> Vec<Value> {
    std::fs::read_to_string(t.dir.path().join("host-controls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[tokio::test]
async fn shared_access_waits_for_worker_ack_and_deduplicates_across_restart() {
    let mut t = Test::hosted().await;
    let a = AgentKind::Claude;
    t.create(a, "access").await;
    t.call(a, prompt("access", "first", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "access", "ready").await;
    std::fs::write(t.dir.path().join("control-delay"), "").unwrap();
    let request = json!({"type":"thread.runtime-mode.set","threadId":"access","commandId":"access-change","runtimeMode":"auto-accept-edits"});
    let request_copy = request.clone();
    let hub = t.hub.clone();
    let send = tokio::spawn(async move {
        hub.call(a, "phone", "orchestration.dispatchCommand", request_copy)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while access_controls(&t).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pending = t.detail(a, "access").await;
    assert_eq!(
        pending["runtimeMode"], before["runtimeMode"],
        "a sent control is not confirmation"
    );
    assert!(t
        .call(a, prompt("access", "pending-prompt", "normal"))
        .await
        .unwrap_err()
        .contains("finish"));
    assert!(send.await.unwrap().unwrap()["sequence"].is_number());
    assert_eq!(
        access_controls(&t)[0]["request"],
        json!({"subtype":"set_permission_mode","mode":"acceptEdits"})
    );
    assert_eq!(
        t.detail(a, "access").await["runtimeMode"],
        "auto-accept-edits"
    );
    for (i, mode, native) in [(1, "plan", "plan"), (2, "default", "acceptEdits")] {
        t.call(a,json!({"type":"thread.interaction-mode.set","threadId":"access","commandId":format!("plan-{i}"),"interactionMode":mode})).await.unwrap();
        assert_eq!(access_controls(&t)[i]["request"]["mode"], native);
        assert_eq!(t.detail(a, "access").await["interactionMode"], mode);
    }
    let mut conflicting = prompt("access", "access-change", "normal");
    conflicting["commandId"] = request["commandId"].clone();
    assert!(t
        .call(a, conflicting.clone())
        .await
        .unwrap_err()
        .contains("different content"));
    conflicting["bootstrap"] = json!({"prepareWorktree":true});
    assert!(t
        .call(a, conflicting)
        .await
        .unwrap_err()
        .contains("different content"));
    let mut reused_ordinary = request.clone();
    reused_ordinary["commandId"] = json!("create-access");
    assert!(t
        .call(a, reused_ordinary)
        .await
        .unwrap_err()
        .contains("different content"));
    t.call(a, request.clone()).await.unwrap();
    assert_eq!(access_controls(&t).len(), 3);
    let after = t.detail(a, "access").await;
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["latestTurn"], before["latestTurn"]);
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    wait_links(&t, 2).await;
    t.call(a, request).await.unwrap();
    assert_eq!(
        access_controls(&t).len(),
        3,
        "a recovered receipt cannot resend settings"
    );
    assert_eq!(
        t.detail(a, "access").await["runtimeMode"],
        "auto-accept-edits"
    );
    t.stop().await;
}
#[tokio::test]
async fn shared_access_rejection_and_lost_ack_never_claim_success_or_replay() {
    for (fixture, expected) in [
        ("control-reject", "previous settings are retained"),
        ("control-crash", "not confirmed"),
    ] {
        let mut t = Test::hosted().await;
        let a = AgentKind::Claude;
        t.create(a, "access-failure").await;
        t.call(a, prompt("access-failure", "first", "normal"))
            .await
            .unwrap();
        let before = t.wait(a, "access-failure", "ready").await;
        std::fs::write(t.dir.path().join(fixture), "").unwrap();
        let request = json!({"type":"thread.runtime-mode.set","threadId":"access-failure","commandId":"access-failure","runtimeMode":"full-access"});
        let error = t.call(a, request.clone()).await.unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("PRIVATE_METADATA"));
        if fixture == "control-crash" {
            wait_links(&t, 2).await;
        }
        assert!(t
            .call(a, request.clone())
            .await
            .unwrap_err()
            .contains(expected));
        let after = t.detail(a, "access-failure").await;
        assert_eq!(after["runtimeMode"], before["runtimeMode"]);
        assert_eq!(after["messages"], before["messages"]);
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        assert!(t.call(a, request).await.unwrap_err().contains(expected));
        assert_eq!(access_controls(&t).len(), 1);
        t.stop().await;
    }
}
#[tokio::test]
async fn lost_native_control_ack_is_not_replayed_after_worker_or_bridge_restart() {
    let mut t = Test::hosted().await;
    let a = AgentKind::Claude;
    t.create(a, "lost-control").await;
    t.call(a, prompt("lost-control", "first", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "lost-control", "ready").await;
    std::fs::write(t.dir.path().join("control-crash"), "").unwrap();
    let request = json!({"threadId":"lost-control","commandId":"lost","action":"mcpToggle","name":"fixture","enabled":false});
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", request.clone())
        .await
        .unwrap_err()
        .contains("not confirmed"));
    wait_links(&t, 2).await;
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", request.clone())
        .await
        .unwrap_err()
        .contains("not replayed"));
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    wait_links(&t, 3).await;
    assert!(t
        .hub
        .call(a, "phone", "claude.sessionAction", request)
        .await
        .unwrap_err()
        .contains("not replayed"));
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-controls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let saved =
        t.db.call(|a| a.store.load_chat("lost-control"))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(saved["disabled_mcp"], json!([]));
    assert!(saved["native_control"]["result"].is_null());
    assert_eq!(
        t.detail(a, "lost-control").await["messages"],
        before["messages"]
    );
    t.stop().await;
}
#[tokio::test]
async fn rejected_native_control_retains_settings_and_does_not_fail_the_chat() {
    let t = Test::hosted().await;
    let a = AgentKind::Claude;
    t.create(a, "rejected-control").await;
    t.call(a, prompt("rejected-control", "first", "normal"))
        .await
        .unwrap();
    let before = t.wait(a, "rejected-control", "ready").await;
    std::fs::write(t.dir.path().join("control-reject"), "").unwrap();
    let request = json!({"threadId":"rejected-control","commandId":"reject","action":"mcpToggle","name":"fixture","enabled":false});
    for _ in 0..2 {
        let error = t
            .hub
            .call(a, "phone", "claude.sessionAction", request.clone())
            .await
            .unwrap_err();
        assert!(error.contains("previous settings are retained"));
        assert!(!error.contains("PRIVATE_METADATA"));
    }
    let after = t.detail(a, "rejected-control").await;
    assert_eq!(after["messages"], before["messages"]);
    assert_eq!(after["session"]["status"], "ready");
    assert_eq!(
        std::fs::read_to_string(t.dir.path().join("host-controls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let saved =
        t.db.call(|a| a.store.load_chat("rejected-control"))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(saved["disabled_mcp"], json!([]));
    t.stop().await;
}
#[tokio::test]
async fn claude_capability_reads_do_not_block_stop_or_leak_private_metadata() {
    let t = Test::new().await;
    let a = AgentKind::Claude;
    t.create(a, "caps").await;
    for text in ["hold", "hold-no-context", "hold-capabilities"] {
        t.call(a, prompt("caps", text, "normal")).await.unwrap();
        t.wait(a, "caps", "running").await;
        let before = t.detail(a, "caps").await["messages"].clone();
        let h = t.hub.clone();
        let read = tokio::spawn(async move {
            h.call(
                a,
                "phone",
                "claude.getCapabilities",
                json!({"threadId":"caps"}),
            )
            .await
        });
        if text == "hold-capabilities" {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t.call(a,json!({"type":"thread.turn.interrupt","threadId":"caps","commandId":format!("stop-{text}")})).await.unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(3), read)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()["status"],
                "unavailable"
            );
            t.wait(a, "caps", "interrupted").await;
            continue;
        }
        let result = read.await.unwrap();
        if text == "hold" {
            let v = result.unwrap();
            assert_eq!(v["mcpServers"][0]["tools"][0], "read_fixture");
            assert!(!v.to_string().contains("PRIVATE_METADATA"));
        } else {
            assert!(!result.unwrap_err().contains("PRIVATE_METADATA"));
        }
        let after = t.detail(a, "caps").await;
        assert_eq!(after["session"]["status"], "running");
        assert_eq!(after["messages"], before);
        t.call(a,json!({"type":"thread.turn.interrupt","threadId":"caps","commandId":format!("stop-{text}")})).await.unwrap();
        t.wait(a, "caps", "interrupted").await;
    }
    t.stop().await;
}
#[tokio::test]
async fn more_than_sixty_four_idle_chats_restart_without_loading_their_transcripts() {
    let mut t = Test::new().await;
    for n in 0..100 {
        t.create(AgentKind::Codex, &format!("idle-{n}")).await;
    }
    t.call(AgentKind::Codex, prompt("idle-99", "first-title", "normal"))
        .await
        .unwrap();
    t.wait(AgentKind::Codex, "idle-99", "ready").await;
    t.call(AgentKind::Codex, prompt("idle-99", "second-text", "normal"))
        .await
        .unwrap();
    let done = t.wait(AgentKind::Codex, "idle-99", "ready").await;
    assert_eq!(done["title"], "first-title");
    t.stop().await;
    let headers =
        t.db.call(|a| a.store.chat_headers())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(headers.len(), 100);
    assert!(headers
        .iter()
        .all(|h| h["thread"]["messages"].as_array().unwrap().is_empty()));
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.detail(AgentKind::Codex, "idle-99").await["messages"],
        done["messages"]
    );
    t.stop().await;
}
#[tokio::test]
async fn codex_steer_releases_a_question_only_after_matching_turn_confirmation() {
    let t = Test::new().await;
    let a = AgentKind::Codex;
    t.create(a, "steer-question").await;
    t.call(a, prompt("steer-question", "question", "normal"))
        .await
        .unwrap();
    let before = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let d = t.detail(a, "steer-question").await;
            if d["hasPendingUserInput"] == true {
                break d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for rejected in ["reject-steer", "wrong-turn-steer"] {
        t.call(a, prompt("steer-question", rejected, "steer"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let d = t.detail(a, "steer-question").await;
        assert_eq!(d["hasPendingUserInput"], true);
        assert_eq!(d["session"]["status"], "running");
    }
    t.call(a, prompt("steer-question", "accepted-steer", "steer"))
        .await
        .unwrap();
    let done = t.wait(a, "steer-question", "ready").await;
    assert_eq!(done["latestTurn"]["turnId"], before["latestTurn"]["turnId"]);
    assert_eq!(done["hasPendingUserInput"], false);
    assert!(done["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "assistant" && m["text"] == "accepted-steer"));
    assert!(!done.to_string().contains("BAD_ANSWER"));
    t.stop().await;
}
#[tokio::test]
async fn concurrent_agents_keep_controls_separate_and_enforce_worker_budget() {
    let t = Test::with_workers(2).await;
    for (agent, id) in [
        (AgentKind::Codex, "codex"),
        (AgentKind::Claude, "claude"),
        (AgentKind::Codex, "extra"),
    ] {
        t.create(agent, id).await;
    }
    for (agent, id) in [(AgentKind::Codex, "codex"), (AgentKind::Claude, "claude")] {
        t.call(agent, prompt(id, "hold", "normal")).await.unwrap();
        t.wait(agent, id, "running").await;
    }
    assert!(t
        .call(AgentKind::Codex, prompt("extra", "hello", "normal"))
        .await
        .unwrap_err()
        .contains("limit"));
    assert!(t.detail(AgentKind::Codex, "extra").await["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    t.call(
        AgentKind::Claude,
        prompt("claude", "queued-claude", "queue"),
    )
    .await
    .unwrap();
    t.call(
        AgentKind::Codex,
        json!({"type":"thread.turn.interrupt","threadId":"codex","commandId":"stop-codex"}),
    )
    .await
    .unwrap();
    t.wait(AgentKind::Codex, "codex", "interrupted").await;
    assert_eq!(
        t.detail(AgentKind::Claude, "claude").await["session"]["status"],
        "running"
    );
    t.call(AgentKind::Codex, prompt("extra", "free-slot", "normal"))
        .await
        .unwrap();
    t.wait(AgentKind::Codex, "extra", "ready").await;
    t.call(AgentKind::Claude, prompt("claude", "steer-claude", "steer"))
        .await
        .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let d = t.detail(AgentKind::Claude, "claude").await;
            if d["session"]["status"] == "ready"
                && d["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "assistant" && m["text"] == "queued-claude")
            {
                break d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(done["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "steer-claude"));
    assert!(!done.to_string().contains("free-slot"));
    for (agent, id) in [(AgentKind::Codex, "codex"), (AgentKind::Claude, "claude")] {
        let mut p = prompt(id, "hold", "normal");
        p["commandId"] = json!(format!("again-{id}"));
        p["message"]["messageId"] = json!(format!("again-{id}"));
        t.call(agent, p).await.unwrap();
        t.wait(agent, id, "running").await;
    }
    t.stop().await;
    let persisted = t.db.call(|a| a.store.chats()).await.unwrap().unwrap();
    for c in persisted {
        assert_ne!(c["thread"]["session"]["status"], "running");
    }
}
#[tokio::test]
async fn discovers_models_before_a_chat_without_creating_history() {
    let t = Test::new().await;
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let config = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let value = t
                    .hub
                    .call(agent, "phone", "server.getConfig", json!({}))
                    .await
                    .unwrap();
                if value["providers"][0]["models"][0]["name"] != "test" {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(config["providers"][0]["status"], "ready");
        assert_eq!(config["providers"][0]["auth"]["status"], "unknown");
        assert_eq!(
            config["providers"][0]["models"][0]["capabilities"]["optionDescriptors"][0]["options"]
                [0]["id"],
            "auto"
        );
        let shell = t
            .hub
            .call(agent, "phone", "orchestration.subscribeShell", json!({}))
            .await
            .unwrap();
        assert!(shell["snapshot"]["threads"].as_array().unwrap().is_empty());
    }
    t.stop().await;
}
#[tokio::test]
async fn implementation_links_validate_and_survive_restart_without_duplicate_delivery() {
    let mut t = Test::new().await;
    let agent = AgentKind::Codex;
    t.create(agent, "plan-chat").await;
    t.call(agent, prompt("plan-chat", "plan", "normal"))
        .await
        .unwrap();
    let d = t.wait(agent, "plan-chat", "ready").await;
    assert_eq!(d["hasActionableProposedPlan"], true);
    let mut cmd = prompt("plan-chat", "Implement the plan", "normal");
    cmd["sourceProposedPlan"] = json!({"threadId":"wrong","planId":"fixture-plan"});
    assert!(t.call(agent, cmd.clone()).await.is_err());
    cmd["sourceProposedPlan"]["threadId"] = json!("plan-chat");
    t.call(agent, cmd.clone()).await.unwrap();
    let done = t.wait(agent, "plan-chat", "ready").await;
    assert_eq!(done["hasActionableProposedPlan"], false);
    assert_eq!(
        done["proposedPlans"][0]["implementationThreadId"],
        "plan-chat"
    );
    assert!(done["proposedPlans"][0]["implementedAt"].is_string());
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    t.call(agent, cmd).await.unwrap();
    assert_eq!(
        t.detail(agent, "plan-chat").await["messages"],
        done["messages"]
    );
    t.stop().await;
}
#[tokio::test]
async fn usage_reads_report_numbers_without_prompts_or_interrupting_on_metadata_errors() {
    let t = Test::new().await;
    let agent = AgentKind::Claude;
    t.create(agent, "usage").await;
    for text in ["hold", "hold-no-context"] {
        t.call(agent, prompt("usage", text, "normal"))
            .await
            .unwrap();
        t.wait(agent, "usage", "running").await;
        let before = t.detail(agent, "usage").await;
        t.hub
            .call(
                agent,
                "phone",
                "claude.getUsage",
                json!({"threadId":"usage"}),
            )
            .await
            .unwrap();
        let d = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = t.detail(agent, "usage").await;
                let rows = d["activities"].as_array().unwrap();
                if rows.iter().any(|r| r["kind"] == "provider.usage.updated")
                    && rows.iter().any(|r| {
                        r["kind"] == "context-window.updated"
                            && r["turnId"] == before["latestTurn"]["turnId"]
                    })
                {
                    break d;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(d["session"]["status"], "running");
        assert_eq!(d["messages"], before["messages"]);
        let rows = d["activities"].as_array().unwrap();
        let context = rows
            .iter()
            .find(|r| r["kind"] == "context-window.updated")
            .unwrap();
        if text == "hold" {
            assert_eq!(context["payload"]["usedTokens"], 123.0);
        } else {
            assert_eq!(context["payload"]["status"], "unavailable");
        }
        let limits = rows
            .iter()
            .find(|r| r["kind"] == "provider.usage.updated")
            .unwrap();
        assert_eq!(limits["payload"]["windows"][0]["usedPercent"], 12.0);
        assert!(!d.to_string().contains("PRIVATE_METADATA"));
        t.call(agent,json!({"type":"thread.turn.interrupt","threadId":"usage","commandId":format!("stop-{text}")})).await.unwrap();
        t.wait(agent, "usage", "interrupted").await;
    }
    t.stop().await;
}
#[tokio::test]
async fn both_agents_create_stream_resume_and_deduplicate_across_restart() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        t.create(a, "one").await;
        let c = prompt("one", "hello", "normal");
        t.call(a, c.clone()).await.unwrap();
        let first = t.wait(a, "one", "ready").await;
        assert_eq!(first["messages"].as_array().unwrap().len(), 2);
        assert!(!first.to_string().contains("PRIVATE_FIXTURE_THINKING"));
        t.call(a, c.clone()).await.unwrap();
        assert_eq!(t.detail(a, "one").await["messages"], first["messages"]);
        let mut reused = c.clone();
        reused["message"]["text"] = json!("different");
        assert!(t
            .call(a, reused)
            .await
            .unwrap_err()
            .contains("different content"));
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(a, c).await.unwrap();
        assert_eq!(t.detail(a, "one").await["messages"], first["messages"]);
        t.call(a, prompt("one", "again", "normal")).await.unwrap();
        assert_eq!(
            t.wait(a, "one", "ready").await["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        t.stop().await;
        assert!(t.dir.path().join("native-connect.sqlite").exists());
    }
}
#[tokio::test]
async fn queued_steer_cannot_jump_to_a_successor_turn() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "race").await;
        t.call(a, prompt("race", "hold", "normal")).await.unwrap();
        let original = t.wait(a, "race", "running").await;
        let mut next = prompt("race", "hold", "queue");
        next["commandId"] = json!("next-hold");
        next["message"]["messageId"] = json!("next-hold");
        t.call(a, next).await.unwrap();
        let stale = prompt("race", "late-steering", "queue");
        let mid = stale["message"]["messageId"].clone();
        t.call(a, stale).await.unwrap();
        t.call(a, prompt("race", "finish-original", "steer"))
            .await
            .unwrap();
        let successor = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let d = t.detail(a, "race").await;
                if d["session"]["status"] == "running"
                    && d["session"]["activeTurnId"] != original["session"]["activeTurnId"]
                {
                    break d;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let action = json!({"type":"thread.turn.queue.steer","threadId":"race","commandId":"late-tap","messageId":mid});
        assert!(t
            .call(a, action.clone())
            .await
            .unwrap_err()
            .contains("remains queued"));
        assert!(t
            .call(a, action)
            .await
            .unwrap_err()
            .contains("remains queued"));
        let after = t.detail(a, "race").await;
        assert_eq!(
            after["session"]["activeTurnId"],
            successor["session"]["activeTurnId"]
        );
        assert_eq!(after["session"]["status"], "running");
        let messages = after["messages"].as_array().unwrap();
        assert_eq!(messages.iter().filter(|m| m["id"] == mid).count(), 1);
        assert_eq!(
            messages.iter().find(|m| m["id"] == mid).unwrap()["phase"],
            "queued"
        );
        t.stop().await;
    }
}

#[tokio::test]
async fn both_agents_queue_steer_and_stop_keep_worker_ownership() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "one").await;
        t.create(a, "two").await;
        t.call(a, prompt("one", "hold", "normal")).await.unwrap();
        t.wait(a, "one", "running").await;
        assert!(t
            .call(a, prompt("two", "hello", "normal"))
            .await
            .unwrap_err()
            .contains("limit"));
        t.call(a, prompt("one", "queued", "queue")).await.unwrap();
        assert_eq!(
            t.detail(a, "one").await["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["phase"],
            "queued"
        );
        let steer = prompt("one", "steered", "queue");
        let mid = steer["message"]["messageId"].clone();
        t.call(a, steer).await.unwrap();
        t.call(a,json!({"type":"thread.turn.queue.steer","threadId":"one","commandId":"steer-from-queue","messageId":mid})).await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let d = t.detail(a, "one").await;
                if d["session"]["status"] == "ready"
                    && d["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["text"] == "queued" && m["role"] == "assistant")
                {
                    break d;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert!(done["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["summary"] == "Steering message received"));
        let mut hold = prompt("one", "hold", "normal");
        hold["commandId"] = json!("hold2");
        hold["message"]["messageId"] = json!("hold2");
        t.call(a, hold).await.unwrap();
        t.wait(a, "one", "running").await;
        t.call(a, prompt("one", "paused", "queue")).await.unwrap();
        t.call(
            a,
            json!({"type":"thread.turn.interrupt","commandId":"stop","threadId":"one"}),
        )
        .await
        .unwrap();
        let stopped = t.wait(a, "one", "interrupted").await;
        assert!(stopped["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "paused" && m["phase"] == "queued"));
        t.stop().await;
    }
}
#[tokio::test]
async fn approval_and_crash_are_explicit_and_never_replayed() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        t.create(a, "one").await;
        t.call(a, prompt("one", "approval", "normal"))
            .await
            .unwrap();
        let rid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = t.detail(a, "one").await;
                if let Some(r) = d["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["kind"] == "approval.requested")
                {
                    if a == AgentKind::Claude {
                        assert_eq!(r["payload"]["data"]["toolCallId"], "approval-tool");
                    }
                    break r["payload"]["requestId"].clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        t.call(a,json!({"type":"thread.approval.respond","threadId":"one","commandId":"approve","requestId":rid,"decision":"approved"})).await.unwrap();
        let done = t.wait(a, "one", "ready").await;
        if a == AgentKind::Claude {
            assert!(done["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["kind"] == "approval.resolved"
                    && r["payload"]["data"]["toolCallId"] == "approval-tool"));
        }
        let crash = prompt("one", "crash", "normal");
        t.call(a, crash.clone()).await.unwrap();
        t.wait(a, "one", "error").await;
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(a, crash).await.unwrap();
        assert_eq!(t.detail(a, "one").await["session"]["status"], "error");
        t.stop().await;
    }
}
#[tokio::test]
async fn cross_agent_and_unmigrated_features_are_rejected() {
    let t = Test::new().await;
    t.create(AgentKind::Codex, "one").await;
    assert!(t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "orchestration.subscribeThread",
            json!({"threadId":"one"})
        )
        .await
        .is_err());
    let mut c = prompt("one", "hello", "normal");
    c["message"]["attachments"] = json!([{"url":"secret"}]);
    assert!(t.call(AgentKind::Codex, c).await.is_err());
    assert!(t
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "filesystem.browse",
            json!({"partialPath":"../outside"})
        )
        .await
        .is_err());
    t.stop().await;
}

#[tokio::test]
async fn large_output_is_bounded_without_erasing_saved_history() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "one").await;
        t.call(a, prompt("one", "hello", "normal")).await.unwrap();
        let old = t.wait(a, "one", "ready").await;
        t.call(a, prompt("one", "large", "normal")).await.unwrap();
        let failed = t.wait(a, "one", "error").await;
        assert!(failed["session"]["lastError"]
            .as_str()
            .unwrap()
            .contains("display limit"));
        assert!(failed.to_string().len() < 512 * 1024);
        assert_eq!(failed["messages"][0], old["messages"][0]);
        assert_eq!(failed["messages"][1], old["messages"][1]);
        t.call(a, prompt("one", "large-error", "normal"))
            .await
            .unwrap();
        let diagnostic = t.wait(a, "one", "error").await;
        assert!(diagnostic["session"]["lastError"].as_str().unwrap().len() <= 1024);
        assert!(diagnostic.to_string().len() < 512 * 1024);
        assert_eq!(diagnostic["messages"][1], old["messages"][1]);
        t.stop().await;
    }
}
#[tokio::test]
async fn mobile_cancel_approval_stops_instead_of_approving() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "one").await;
        t.call(a, prompt("one", "approval", "normal"))
            .await
            .unwrap();
        let rid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = t.detail(a, "one").await;
                if let Some(row) = d["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["kind"] == "approval.requested")
                {
                    break row["payload"]["requestId"].clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        t.call(a,json!({"type":"thread.approval.respond","threadId":"one","commandId":"cancel","requestId":rid,"decision":"cancel"})).await.unwrap();
        let stopped = t.wait(a, "one", "interrupted").await;
        assert_eq!(stopped["hasPendingApprovals"], false);
        t.stop().await;
    }
}
#[tokio::test]
async fn rejected_steer_does_not_stop_the_original_task() {
    let t = Test::new().await;
    let a = AgentKind::Codex;
    t.create(a, "one").await;
    t.call(a, prompt("one", "hold", "normal")).await.unwrap();
    t.wait(a, "one", "running").await;
    t.call(a, prompt("one", "reject-steer", "steer"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let d = t.detail(a, "one").await;
            if d["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["payload"]["status"] == "failed")
            {
                assert_eq!(d["session"]["status"], "running");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    t.stop().await;
}

#[tokio::test]
async fn attachments_survive_queue_restart_and_rejected_delivery_is_rolled_back() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        t.create(a, "uploads").await;
        t.call(a, prompt("uploads", "hold", "normal"))
            .await
            .unwrap();
        t.wait(a, "uploads", "running").await;
        let mut queued = prompt("uploads", "read note", "queue");
        queued["message"]["attachments"] = json!([{"type":"file","name":"note.txt","mimeType":"text/plain","sizeBytes":4,"dataUrl":"data:text/plain;base64,Tk9URQ=="}]);
        t.call(a, queued.clone()).await.unwrap();
        let before = t.db.call(|a| a.store.chats()).await.unwrap().unwrap();
        assert!(!serde_json::to_string(&before).unwrap().contains("base64"));
        let path = before[0]["queue"][0]["message"]["attachments"][0]["path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(std::fs::read(&path).unwrap(), b"NOTE");
        // Replay is a receipt lookup and does not stage duplicate uploads.
        t.call(a, queued.clone()).await.unwrap();
        t.call(
            a,
            json!({"type":"thread.turn.interrupt","commandId":"stop","threadId":"uploads"}),
        )
        .await
        .unwrap();
        t.wait(a, "uploads", "interrupted").await;
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(a,json!({"type":"thread.turn.queue.steer","commandId":"resume-queue","threadId":"uploads","messageId":queued["message"]["messageId"]})).await.unwrap();
        let done = t.wait(a, "uploads", "ready").await;
        assert!(done["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"].as_str().unwrap().contains("note.txt")));
        assert!(!done.to_string().contains("base64"));
        t.stop().await;
    }
}

#[tokio::test]
async fn both_protocols_spill_long_turns_and_update_old_tools_without_losing_rows() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "many").await;
        t.call(a, prompt("many", "many", "normal")).await.unwrap();
        let done = t.wait_bulk_history(a, "many").await;
        assert!(done.to_string().len() < 180 * 1024);
        assert!(done["historyPage"]["hasOlder"].as_bool().unwrap());
        let tool =
            t.db.call(|a| a.store.history_row("many", "activities", "old-tool"))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert_eq!(tool["payload"]["status"], "completed");
        let mut cursor = done["historyPage"]["olderCursor"].clone();
        let mut count = done["messages"].as_array().unwrap().len();
        while !cursor.is_null() {
            let page = t
                .hub
                .call(
                    a,
                    "phone",
                    "orchestration.getThreadPage",
                    json!({"threadId":"many","cursor":cursor}),
                )
                .await
                .unwrap();
            count += page["messages"].as_array().unwrap().len();
            cursor = page["pageInfo"]["olderCursor"].clone();
        }
        assert_eq!(count, 162);
        t.call(a, prompt("many", "after long turn", "normal"))
            .await
            .unwrap();
        assert_eq!(
            t.wait(a, "many", "ready").await["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["text"],
            "after long turn"
        );
        t.stop().await;
    }
}

#[tokio::test]
async fn projects_and_projectless_chats_persist_for_both_agents() {
    let mut t = Test::new().await;
    let root = t.dir.path().join("project");
    let c = json!({"type":"project.create","commandId":"add-project","projectId":"project","title":"Shared folder","workspaceRoot":root,"createWorkspaceRootIfMissing":true});
    t.call(AgentKind::Codex, c.clone()).await.unwrap();
    t.call(AgentKind::Codex, c).await.unwrap();
    assert!(root.is_dir());
    for (agent, id) in [
        (AgentKind::Codex, "codex-folder"),
        (AgentKind::Claude, "claude-folder"),
    ] {
        t.call(
            agent,
            json!({"type":"thread.create","commandId":id,"threadId":id,"projectId":"project"}),
        )
        .await
        .unwrap();
        assert_eq!(
            t.detail(agent, id).await["worktreePath"],
            json!(root.canonicalize().unwrap())
        );
        let scratch = format!("{id}-scratch");
        t.call(agent,json!({"type":"thread.create","commandId":scratch,"threadId":scratch,"projectId":"no-project","workspaceKind":"projectless"})).await.unwrap();
        let d = t.detail(agent, &scratch).await;
        assert_eq!(d["workspaceKind"], "projectless");
        assert_eq!(d["projectId"], "codex-project-chats");
        assert!(PathBuf::from(d["worktreePath"].as_str().unwrap()).is_dir());
        t.call(agent, prompt(&scratch, "hello", "normal"))
            .await
            .unwrap();
        t.wait(agent, &scratch, "ready").await;
    }
    assert!(t
        .call(
            AgentKind::Claude,
            json!({"type":"project.delete","projectId":"project","commandId":"delete-project"})
        )
        .await
        .is_err());
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    let shell = t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "orchestration.subscribeShell",
            json!({}),
        )
        .await
        .unwrap();
    let projects = shell["snapshot"]["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3);
    let scratch = projects
        .iter()
        .find(|p| p["id"] == "codex-project-chats")
        .unwrap();
    assert_eq!(scratch["title"], "No Project");
    assert_eq!(
        t.detail(AgentKind::Claude, "claude-folder-scratch").await["projectId"],
        scratch["id"]
    );
    assert_eq!(
        t.detail(AgentKind::Claude, "claude-folder").await["worktreePath"],
        json!(root.canonicalize().unwrap())
    );
    t.stop().await;
}
#[tokio::test]
async fn existing_chat_file_previews_use_the_chat_workspace_not_the_bridge_default() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        t.create(agent, "desktop-files").await;
        t.stop().await;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("note.txt"), "DESKTOP_FILE_OK").unwrap();
        std::fs::write(project.path().join("large.txt"), "a".repeat(600 * 1024)).unwrap();
        let cwd = project.path().to_path_buf();
        t.db.call(move |a| {
            let mut chat = a.store.load_chat("desktop-files").unwrap();
            chat["thread"]["worktreePath"] = json!(cwd);
            a.store.save_chat(&chat, None).unwrap();
        })
        .await
        .unwrap();
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        for path in [
            "note.txt".to_owned(),
            project
                .path()
                .join("note.txt")
                .to_string_lossy()
                .into_owned(),
        ] {
            let result = t
                .hub
                .call(
                    agent,
                    "phone",
                    "filesystem.readTextFile",
                    json!({"threadId":"desktop-files","path":path,"cwd":t.dir.path()}),
                )
                .await
                .unwrap();
            assert_eq!(result["content"], "DESKTOP_FILE_OK");
            assert!(result.get("contents").is_none());
        }
        for method in [
            "projects.listEntries",
            "projects.searchEntries",
            "projects.readFile",
        ] {
            let params = json!({"cwd":project.path(),"relativePath":"note.txt","query":"note"});
            let result = t
                .hub
                .call(agent, "phone", method, params.clone())
                .await
                .unwrap();
            if method == "projects.readFile" {
                assert_eq!(result["contents"], "DESKTOP_FILE_OK");
            } else {
                assert!(result["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["path"] == "note.txt"));
            }
            let other = if agent == AgentKind::Codex {
                AgentKind::Claude
            } else {
                AgentKind::Codex
            };
            assert!(t.hub.call(other, "phone", method, params).await.is_err());
            assert!(t
                .hub
                .call(
                    agent,
                    "phone",
                    method,
                    json!({"cwd":project.path().parent(),"relativePath":"note.txt","query":"note"})
                )
                .await
                .is_err());
        }
        // Read authorization must not expand the write boundary.
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "projects.writeFile",
                json!({"cwd":project.path(),"relativePath":"note.txt","contents":"changed"})
            )
            .await
            .is_err());
        let large = t
            .hub
            .call(
                agent,
                "phone",
                "filesystem.readTextFile",
                json!({"threadId":"desktop-files","path":"large.txt"}),
            )
            .await
            .unwrap();
        assert_eq!(large["content"].as_str().unwrap().len(), 256 * 1024);
        assert_eq!(large["truncated"], true);
        assert_eq!(large["sizeBytes"], 600 * 1024);
        std::fs::write(t.dir.path().join("unrelated.txt"), "private").unwrap();
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "filesystem.readTextFile",
                json!({"threadId":"desktop-files","path":t.dir.path().join("unrelated.txt")})
            )
            .await
            .is_err());
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "filesystem.readTextFile",
                json!({"threadId":"missing","path":project.path().join("note.txt")})
            )
            .await
            .is_err());
        t.stop().await;
    }
}

#[tokio::test]
async fn file_apis_preserve_project_boundaries_and_deleted_chats_stay_out_of_shell() {
    let t = Test::new().await;
    let a = AgentKind::Claude;
    t.create(a, "files").await;
    t.hub
        .call(
            a,
            "phone",
            "projects.writeFile",
            json!({"cwd":t.dir.path(),"relativePath":"note.txt","contents":"note"}),
        )
        .await
        .unwrap();
    let read = t
        .hub
        .call(
            a,
            "phone",
            "projects.readFile",
            json!({"cwd":t.dir.path(),"relativePath":"note.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(read["contents"], "note");
    t.call(a,json!({"type":"thread.meta.update","threadId":"files","commandId":"rename","title":"Renamed"})).await.unwrap();
    assert_eq!(t.detail(a, "files").await["title"], "Renamed");
    t.call(
        a,
        json!({"type":"thread.delete","threadId":"files","commandId":"delete"}),
    )
    .await
    .unwrap();
    let shell = t
        .hub
        .call(a, "phone", "orchestration.subscribeShell", json!({}))
        .await
        .unwrap();
    assert!(shell["snapshot"]["threads"].as_array().unwrap().is_empty());
    t.stop().await;
}

#[tokio::test]
async fn questions_are_answered_once_and_cleared_when_stopped() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "one").await;
        for round in 0..2 {
            let mut cmd = prompt("one", "question", "normal");
            cmd["commandId"] = json!(format!("question-{round}"));
            cmd["message"]["messageId"] = json!(format!("question-{round}"));
            t.call(agent, cmd).await.unwrap();
            let row = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let d = t.detail(agent, "one").await;
                    if d["hasPendingUserInput"] == true {
                        assert_eq!(d["hasPendingApprovals"], false);
                        break d["activities"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|v| v["kind"] == "user-input.requested")
                            .unwrap()
                            .clone();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let rid = &row["payload"]["requestId"];
            assert!(t.call(agent,json!({"type":"thread.approval.respond","threadId":"one","commandId":format!("bad-{round}"),"requestId":rid,"decision":"approved"})).await.is_err());
            if round == 0 {
                let key = row["payload"]["questions"][0]["id"].as_str().unwrap();
                let answer = json!({"type":"thread.user-input.respond","threadId":"one","commandId":"answer","requestId":rid,"answers":{key:{"answers":["Blue"]}}});
                t.call(agent, answer.clone()).await.unwrap();
                let done = t.wait(agent, "one", "ready").await;
                assert_eq!(done["hasPendingUserInput"], false);
                assert!(done["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["text"] == "QUESTION_ANSWER_OK"));
                t.call(agent, answer).await.unwrap();
            } else {
                t.call(agent,json!({"type":"thread.turn.interrupt","threadId":"one","commandId":"stop-question"})).await.unwrap();
                let done = t.wait(agent, "one", "interrupted").await;
                assert_eq!(done["hasPendingUserInput"], false);
                assert!(!done["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["kind"] == "user-input.requested"));
            }
        }
        t.stop().await;
    }
}
#[tokio::test]
async fn chat_settings_are_retained_and_reach_each_cli() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.call(agent,json!({"type":"thread.create","threadId":"one","commandId":"create","workspaceKind":"projectless","runtimeMode":"full-access","interactionMode":"plan","modelSelection":{"instanceId":agent.instance(),"model":"test"}})).await.unwrap();
        t.call(agent, prompt("one", "settings", "normal"))
            .await
            .unwrap();
        let done = t.wait(agent, "one", "ready").await;
        assert_eq!(done["interactionMode"], "plan");
        assert_eq!(done["runtimeMode"], "full-access");
        let response = done["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["role"] == "assistant")
            .unwrap()["text"]
            .as_str()
            .unwrap();
        let response: Value = serde_json::from_str(response).unwrap();
        if agent == AgentKind::Codex {
            assert_eq!(response["collaborationMode"]["mode"], "plan");
        } else {
            let args = response.as_array().unwrap();
            let at = args.iter().position(|v| v == "--permission-mode").unwrap();
            assert_eq!(args[at + 1], "plan");
        }
        let cwd = done["worktreePath"].as_str().unwrap();
        std::fs::write(PathBuf::from(cwd).join("specific.txt"), "right workspace").unwrap();
        let file = t
            .hub
            .call(
                agent,
                "phone",
                "filesystem.readTextFile",
                json!({"threadId":"one","path":"specific.txt"}),
            )
            .await
            .unwrap();
        assert_eq!(file["content"], "right workspace");
        t.stop().await;
    }
}

#[tokio::test]
async fn storage_pressure_stops_owned_workers_preserves_queue_and_rejects_new_prompts() {
    use remodex_connect_native::storage::Limits;
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(a, "quota").await;
        t.call(a, prompt("quota", "approval", "normal"))
            .await
            .unwrap();
        t.wait(a, "quota", "running").await;
        t.call(a, prompt("quota", "queued", "queue")).await.unwrap();
        t.db.call(|a|{
            a.store.save_chat(&json!({"agent":"claude","thread":{"id":"disk-fixture","messages":[{"id":"old","text":"x".repeat(1100*1024)}],"activities":[],"proposedPlans":[]}}),None).unwrap();
            a.store.configure_storage(Limits{history_bytes:1024*1024,..Default::default()}).unwrap();
        }).await.unwrap();
        let failed = t.wait(a, "quota", "error").await;
        assert!(failed["session"]["lastError"]
            .as_str()
            .unwrap()
            .contains("storage limit"));
        assert!(failed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "approval"));
        assert!(failed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "queued" && m["queueStatus"] == "paused"));
        let denied = prompt("quota", "must-not-run", "normal");
        assert!(t
            .call(a, denied.clone())
            .await
            .unwrap_err()
            .contains("storage limit"));
        assert!(!t.detail(a, "quota").await["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "must-not-run"));
        // Cancellation is allowed at capacity, and changing the limit is enough
        // to continue; no conversation or file is deleted to recover.
        t.call(a,json!({"type":"thread.turn.queue.cancel","commandId":"cancel-full","threadId":"quota","messageId":"message-quota-queued-queue"})).await.unwrap();
        t.db.call(|a| a.store.configure_storage(Limits::default()))
            .await
            .unwrap()
            .unwrap();
        t.call(a, denied).await.unwrap();
        let ready = t.wait(a, "quota", "ready").await;
        assert!(ready["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "assistant"));
        t.stop().await;
    }
}

#[tokio::test]
async fn prompt_edit_preserves_a_revision_and_retries_without_reediting() {
    for a in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        t.create(a, "edit").await;
        t.call(a, prompt("edit", "keep-first", "normal"))
            .await
            .unwrap();
        t.wait(a, "edit", "ready").await;
        t.call(a, prompt("edit", "replace-second", "normal"))
            .await
            .unwrap();
        let second = t.wait(a, "edit", "ready").await;
        let target = second["latestTurn"]["turnId"].clone();
        t.call(a, prompt("edit", "many", "normal")).await.unwrap();
        t.wait_bulk_history(a, "edit").await;
        let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
        let raw: String = db
            .query_row("SELECT data FROM chats WHERE id='edit'", [], |r| r.get(0))
            .unwrap();
        let before: Value = serde_json::from_str(&raw).unwrap();
        let original_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM chat_rows WHERE chat='edit'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if a == AgentKind::Claude {
            let native: String = db
                .query_row(
                    "SELECT native FROM native_turns WHERE chat='edit' AND turn=?1",
                    [target.as_str().unwrap()],
                    |r| r.get(0),
                )
                .unwrap();
            let folder = t.dir.path().join("projects/fixture");
            std::fs::create_dir_all(&folder).unwrap();
            let parent = uuid::Uuid::new_v4().to_string();
            std::fs::write(folder.join(format!("{}.jsonl",before["native_id"].as_str().unwrap())),format!("{}\n{}\n",json!({"type":"assistant","uuid":parent,"sessionId":before["native_id"],"parentUuid":null}),json!({"type":"user","uuid":native,"parentUuid":parent,"sessionId":before["native_id"]}))).unwrap();
        }
        let mut c = prompt(
            "edit",
            if a == AgentKind::Claude {
                "branch-arguments"
            } else {
                "EDITED_OK"
            },
            "normal",
        );
        c["promptEdit"] = json!({"targetTurnId":target,"turnCount":1});
        t.call(a, c.clone()).await.unwrap();
        let d = t.wait(a, "edit", "ready").await;
        assert!(d["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "keep-first"));
        assert!(!d["messages"].to_string().contains("replace-second"));
        assert!(!d.to_string().contains("history-159"));
        let raw: String = db
            .query_row("SELECT data FROM chats WHERE id='edit'", [], |r| r.get(0))
            .unwrap();
        let after: Value = serde_json::from_str(&raw).unwrap();
        assert_ne!(before["native_id"], after["native_id"]);
        let revision: String = db
            .query_row(
                "SELECT id FROM chat_revisions WHERE source='edit'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let saved: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM chat_rows WHERE chat=?1",
                [&revision],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(original_count, saved);
        assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND instr(data,'replace-second')>0)",[&revision],|r|r.get::<_,bool>(0)).unwrap());
        t.call(a, c.clone()).await.unwrap();
        let mut stale = c.clone();
        stale["commandId"] = json!("stale-edit");
        stale["message"]["messageId"] = json!("stale-edit");
        assert!(t.call(a, stale).await.is_err());
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(a, c).await.unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM chat_revisions WHERE source='edit'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        t.stop().await;
    }
}
#[tokio::test]
async fn rejected_or_busy_prompt_edits_leave_history_unchanged() {
    let t = Test::new().await;
    let a = AgentKind::Codex;
    t.create(a, "edit-failure").await;
    t.call(a, prompt("edit-failure", "first", "normal"))
        .await
        .unwrap();
    let d = t.wait(a, "edit-failure", "ready").await;
    let mut c = prompt("edit-failure", "edited", "normal");
    c["promptEdit"] = json!({"targetTurnId":d["latestTurn"]["turnId"],"turnCount":0});
    let mut bad_upload = c.clone();
    bad_upload["message"]["attachments"] =
        json!([{"type":"image","dataUrl":"data:image/png;base64,bm90IGEgcG5n"}]);
    assert!(t
        .call(a, bad_upload)
        .await
        .unwrap_err()
        .contains("Invalid PNG"));
    assert!(!t.dir.path().join("fixture-forks.jsonl").exists());
    assert_eq!(t.detail(a, "edit-failure").await["messages"], d["messages"]);
    std::fs::write(t.dir.path().join("reject-fixture-fork"), "").unwrap();
    assert!(t
        .call(a, c.clone())
        .await
        .unwrap_err()
        .contains("original history is intact"));
    assert_eq!(t.detail(a, "edit-failure").await["messages"], d["messages"]);
    t.call(a, prompt("edit-failure", "hold", "normal"))
        .await
        .unwrap();
    t.wait(a, "edit-failure", "running").await;
    assert!(t.call(a, c).await.unwrap_err().contains("Stop the task"));
    t.stop().await;
}

#[tokio::test]
async fn bootstrap_first_prompt_is_atomic_and_idempotent() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let mut t = Test::new().await;
        let mut cmd = prompt("bootstrap", "BOOTSTRAP_OK", "normal");
        cmd["bootstrap"] = json!({"createThread":{"projectId":"native-test-project","title":"One request chat","runtimeMode":"full-access","interactionMode":"default","modelSelection":{"instanceId":agent.instance(),"model":"test"},"branch":null,"worktreePath":null}});
        let mut invalid = cmd.clone();
        invalid["message"]["attachments"] =
            json!([{"type":"image","dataUrl":"data:image/png;base64,bm90IGEgcG5n"}]);
        assert!(t.call(agent, invalid).await.is_err());
        let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
        assert_eq!(
            db.query_row("select count(*) from chats where id='bootstrap'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(t
            .hub
            .call(
                agent,
                "phone",
                "orchestration.subscribeThread",
                json!({"threadId":"bootstrap"})
            )
            .await
            .is_err());
        t.call(agent, cmd.clone()).await.unwrap();
        let d = t.wait(agent, "bootstrap", "ready").await;
        assert_eq!(d["title"], "One request chat");
        assert_eq!(d["runtimeMode"], "full-access");
        assert_eq!(
            d["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user")
                .count(),
            1
        );
        t.call(agent, cmd.clone()).await.unwrap();
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd.clone()).await.unwrap();
        let d = t.detail(agent, "bootstrap").await;
        assert_eq!(
            d["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user")
                .count(),
            1
        );
        cmd["message"]["text"] = json!("changed");
        assert!(t
            .call(agent, cmd)
            .await
            .unwrap_err()
            .contains("different content"));
        t.stop().await;
    }
}
#[tokio::test]
async fn bootstrap_rejects_unsupported_side_effects_and_busy_capacity() {
    let t = Test::new().await;
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        for unsupported in [
            json!({"prepareWorktree":{}}),
            json!({"runSetupScript":true}),
            json!({"runSetupScript":"false"}),
            json!({"unknown":true}),
        ] {
            let mut cmd = prompt("reject-bootstrap", "no-op", "normal");
            cmd["bootstrap"] = unsupported;
            cmd["bootstrap"]["createThread"] = json!({"projectId":"native-test-project"});
            assert!(t.call(agent, cmd).await.is_err());
        }
    }
    let mut existing = prompt("no-project", "hold", "normal");
    existing["bootstrap"] = json!({"createThread":{"projectId":"codex-project-chats"}});
    t.call(AgentKind::Codex, existing).await.unwrap();
    let d = t.wait(AgentKind::Codex, "no-project", "running").await;
    assert_eq!(d["workspaceKind"], "projectless");
    let mut next = prompt("busy-bootstrap", "no-op", "normal");
    next["bootstrap"] = json!({"createThread":{"projectId":"native-test-project"}});
    assert!(t
        .call(AgentKind::Claude, next)
        .await
        .unwrap_err()
        .contains("free chat worker"));
    t.stop().await;
}

#[tokio::test]
async fn project_settings_persist_without_running_scripts_or_crossing_agents() {
    let mut t = Test::new().await;
    let a = AgentKind::Claude;
    for _ in 0..100 {
        let value = t
            .hub
            .call(a, "phone", "server.getConfig", json!({}))
            .await
            .unwrap();
        if value.to_string().contains("high") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let script = json!({"id":"fixture","name":"Fixture","command":"echo never-executed > script-must-not-run","icon":"test","runOnWorktreeCreate":true,"previewUrl":"http://localhost:18966","autoOpenPreview":false});
    let update = json!({"type":"project.meta.update","commandId":"project-settings","projectId":"native-test-project","title":"Native project","defaultModelSelection":{"instanceId":"claude-code","model":"test","options":[{"id":"effort","value":"high"}]},"scripts":[script]});
    t.call(a, update.clone()).await.unwrap();
    t.call(a, update.clone()).await.unwrap();
    assert!(!t.dir.path().join("script-must-not-run").exists());
    t.create(a, "inherit-settings").await;
    assert_eq!(
        t.detail(a, "inherit-settings").await["modelSelection"]["options"][0]["value"],
        "high"
    );
    let automatic=t.hub.call(a,"phone","claude.sessionAction",json!({"threadId":"inherit-settings","action":"newWorktree","commandId":"must-not-skip-setup"})).await;
    assert!(automatic.unwrap_err().contains("Git could not prepare"));
    t.create(AgentKind::Codex, "other-agent-settings").await;
    assert_eq!(
        t.detail(AgentKind::Codex, "other-agent-settings").await["modelSelection"]["options"],
        json!([])
    );
    t.call(a,json!({"type":"thread.create","commandId":"explicit-settings","threadId":"explicit-settings","projectId":"native-test-project","modelSelection":{"model":"test","options":[{"id":"effort","value":"auto"}]}})).await.unwrap();
    assert_eq!(
        t.detail(a, "explicit-settings").await["modelSelection"]["options"],
        json!([])
    );
    let mut bad = update.clone();
    bad["commandId"] = json!("bad-script");
    bad["scripts"][0]["previewUrl"] = json!("javascript:bad");
    assert!(t.call(a, bad).await.is_err());
    let mut bad = update.clone();
    bad["commandId"] = json!("wrong-agent");
    bad["defaultModelSelection"]["instanceId"] = json!("codex");
    assert!(t.call(a, bad).await.is_err());
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    t.call(a, update).await.unwrap();
    let shell = t
        .hub
        .call(a, "phone", "orchestration.subscribeShell", json!({}))
        .await
        .unwrap();
    let p = &shell["snapshot"]["projects"][0];
    assert_eq!(p["scripts"], json!([script]));
    assert_eq!(p["title"], "Native project");
    assert_eq!(p["defaultModelSelection"]["options"][0]["value"], "high");
    t.call(a,json!({"type":"project.meta.update","commandId":"clear-settings","projectId":"native-test-project","defaultModelSelection":null,"scripts":[]})).await.unwrap();
    t.create(a, "cleared-settings").await;
    assert_eq!(
        t.detail(a, "cleared-settings").await["modelSelection"]["options"],
        json!([])
    );
    t.stop().await;
}

#[cfg(unix)]
async fn setup_fixture(scripts: Value) -> (Test, PathBuf) {
    let t = Test::new().await;
    let root = t.dir.path().join("setup source with spaces");
    std::fs::create_dir(&root).unwrap();
    fixture_git(&root, &["init", "-b", "main"]);
    std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
    fixture_git(&root, &["add", "tracked.txt"]);
    fixture_git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    std::fs::write(root.join("tracked.txt"), "source changes\n").unwrap();
    t.call(AgentKind::Claude,json!({"type":"project.create","commandId":"setup-project","projectId":"setup-project","workspaceRoot":root,"scripts":scripts})).await.unwrap();
    t.call(AgentKind::Claude,json!({"type":"thread.create","commandId":"setup-source","threadId":"setup-source","projectId":"setup-project"})).await.unwrap();
    (t, root)
}
#[cfg(unix)]
fn setup_script(id: &str, command: &str, automatic: bool) -> Value {
    json!({"id":id,"name":id,"command":command,"icon":"configure","runOnWorktreeCreate":automatic})
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_setup_uses_configured_default_and_never_repeats_after_receipt_loss() {
    let scripts = json!([
        setup_script("first", "printf 'one\\n' >> setup-order.txt; printf '%s\\n%s\\n' \"$T3CODE_PROJECT_ROOT\" \"$T3CODE_WORKTREE_PATH\" > setup-env.txt", true),
        setup_script("second", "printf 'two\\n' >> setup-order.txt", true),
        setup_script("manual", "touch must-not-run", false)
    ]);
    let (mut t, root) = setup_fixture(scripts.clone()).await;
    let shell = t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "orchestration.subscribeShell",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(
        shell["snapshot"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "setup-project")
            .unwrap()["scripts"],
        scripts
    );
    assert!(!root.join("setup-order.txt").exists());
    let cmd = json!({"action":"newWorktree","threadId":"setup-source","commandId":"setup-run"});
    let result = t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "claude.sessionAction",
            cmd.clone(),
        )
        .await
        .unwrap();
    let id = result["threadId"].as_str().unwrap().to_owned();
    let detail = t.detail(AgentKind::Claude, &id).await;
    let checkout = PathBuf::from(detail["worktreePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(checkout.join("setup-order.txt")).unwrap(),
        "one\n"
    );
    assert!(!checkout.join("must-not-run").exists());
    assert_eq!(
        std::fs::read_to_string(checkout.join("setup-env.txt")).unwrap(),
        format!(
            "{}\n{}\n",
            root.canonicalize().unwrap().display(),
            checkout.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "source changes\n"
    );
    assert!(!root.join("setup-order.txt").exists());
    assert_eq!(
        detail["activities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "project.setup.completed")
            .count(),
        1
    );
    assert_eq!(
        t.hub
            .call(
                AgentKind::Claude,
                "phone",
                "claude.sessionAction",
                cmd.clone()
            )
            .await
            .unwrap(),
        result
    );
    t.stop().await;
    // Completed setup but receipt lost: recover the checkout, never replay scripts.
    let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    db.execute(
        "DELETE FROM native_tool_actions WHERE id='claude-worktree:phone:setup-run'",
        [],
    )
    .unwrap();
    db.execute("DELETE FROM chats WHERE id=?1", [&id]).unwrap();
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert_eq!(
        t.hub
            .call(AgentKind::Claude, "phone", "claude.sessionAction", cmd)
            .await
            .unwrap(),
        result
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("setup-order.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        t.detail(AgentKind::Claude, &id).await["activities"],
        detail["activities"]
    );
    t.stop().await;
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_setup_failure_keeps_files_and_does_not_create_or_replay_a_chat() {
    let (mut t, root) = setup_fixture(json!([setup_script(
        "failing",
        "echo once >> attempted.txt; exit 7",
        true
    )]))
    .await;
    let cmd = json!({"action":"newWorktree","threadId":"setup-source","commandId":"setup-fail"});
    assert!(t
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "claude.sessionAction",
            cmd.clone()
        )
        .await
        .unwrap_err()
        .contains("code 7"));
    let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    let intent: String = db
        .query_row(
            "SELECT data FROM native_worktrees WHERE id='claude-worktree:phone:setup-fail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let intent: Value = serde_json::from_str(&intent).unwrap();
    assert_eq!(intent["setup"], "failed");
    let path = PathBuf::from(intent["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join("attempted.txt")).unwrap(),
        "once\n"
    );
    assert!(!root.join("attempted.txt").exists());
    assert_eq!(
        db.query_row("SELECT count(*) FROM chats", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    t.stop().await;
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert!(t
        .hub
        .call(AgentKind::Claude, "phone", "claude.sessionAction", cmd)
        .await
        .unwrap_err()
        .contains("not be repeated"));
    assert_eq!(
        std::fs::read_to_string(path.join("attempted.txt")).unwrap(),
        "once\n"
    );
    t.stop().await;
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_setup_shutdown_marks_uncertain_execution_and_stops_descendants() {
    let (mut t, _) = setup_fixture(json!([setup_script(
        "long",
        "sleep 30 & echo $! > owned-pid; echo once >> attempted.txt; wait",
        true
    )]))
    .await;
    let cmd =
        json!({"action":"newWorktree","threadId":"setup-source","commandId":"setup-interrupt"});
    let hub = t.hub.clone();
    let task_cmd = cmd.clone();
    let pending = tokio::spawn(async move {
        hub.call(AgentKind::Claude, "phone", "claude.sessionAction", task_cmd)
            .await
    });
    let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    let path=tokio::time::timeout(Duration::from_secs(5),async{
        loop {
            let result=db.query_row("SELECT data FROM native_worktrees WHERE id='claude-worktree:phone:setup-interrupt'",[],|r|r.get::<_,String>(0));
            if let Ok(data)=result {
                let i:Value=serde_json::from_str(&data).unwrap();
                let path=PathBuf::from(i["path"].as_str().unwrap());
                // Redirection creates the PID file before echo writes either
                // marker. Stop only after the fixture has reached its wait;
                // otherwise a fast shutdown legitimately interrupts the echo.
                let pid_written=std::fs::read_to_string(path.join("owned-pid")).is_ok_and(|s|s.trim().parse::<i32>().is_ok_and(|pid|pid>0));
                let marker_written=std::fs::read_to_string(path.join("attempted.txt")).is_ok_and(|s|s=="once\n");
                if pid_written && marker_written {break path;}
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    t.stop().await;
    assert!(pending.await.unwrap().is_err());
    let pid: i32 = std::fs::read_to_string(path.join("owned-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert!(t
        .hub
        .call(AgentKind::Claude, "phone", "claude.sessionAction", cmd)
        .await
        .unwrap_err()
        .contains("not be repeated"));
    assert_eq!(
        std::fs::read_to_string(path.join("attempted.txt")).unwrap(),
        "once\n"
    );
    t.stop().await;
}

#[tokio::test]
async fn phone_question_drafts_never_submit_or_replace_pending_native_requests() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let t = Test::new().await;
        t.create(agent, "drafts").await;
        t.call(agent, prompt("drafts", "question", "normal"))
            .await
            .unwrap();
        let before = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let d = t.detail(agent, "drafts").await;
                if d["hasPendingUserInput"] == true {
                    break d;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let rid = before["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"] == "user-input.requested")
            .unwrap()["payload"]["requestId"]
            .clone();
        let command = json!({"type":"thread.user-input.draft.update","threadId":"drafts","commandId":"typing","requestId":rid,"questionIndex":0,"answers":{"choice":{"selectedOptionLabels":["Blue"],"customAnswer":"PRIVATE_UNSUBMITTED_TEXT"}}});
        t.call(agent, command.clone()).await.unwrap();
        t.call(agent, command.clone()).await.unwrap();
        let after = t.detail(agent, "drafts").await;
        assert_eq!(after["messages"], before["messages"]);
        assert_eq!(after["hasPendingUserInput"], true);
        assert!(!after.to_string().contains("PRIVATE_UNSUBMITTED_TEXT"));
        assert_eq!(
            after["activities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["kind"] == "user-input.requested")
                .unwrap()["payload"]["requestId"],
            rid
        );
        let mut bad = command;
        bad["answers"]["choice"]["customAnswer"] = json!("x".repeat(8193));
        assert!(t.call(agent, bad).await.is_err());
        t.call(
            agent,
            json!({"type":"thread.turn.interrupt","threadId":"drafts","commandId":"draft-stop"}),
        )
        .await
        .unwrap();
        t.wait(agent, "drafts", "interrupted").await;
        t.stop().await;
    }
}

#[cfg(unix)]
fn worktree_bootstrap(agent: AgentKind, root: &std::path::Path, id: &str) -> Value {
    let mut c = prompt(id, "worktree first message", "normal");
    c["bootstrap"] = json!({"createThread":{"projectId":"setup-project","title":"Bootstrap worktree","modelSelection":{"instanceId":agent.instance(),"model":"test"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null},"prepareWorktree":{"projectCwd":root,"baseBranch":"main","branch":format!("native-test/{id}")},"runSetupScript":true});
    c
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_bootstrap_runs_setup_before_first_prompt_for_both_agents() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (mut t, root) = setup_fixture(json!([setup_script(
            "prepare",
            "echo once >> setup-ready.txt",
            true
        )]))
        .await;
        let cmd = worktree_bootstrap(agent, &root, "worktree-first");
        t.call(agent, cmd.clone()).await.unwrap();
        let d = t.wait(agent, "worktree-first", "ready").await;
        assert_eq!(d["messages"].as_array().unwrap().len(), 2);
        assert_eq!(d["title"], "Bootstrap worktree");
        assert_eq!(d["branch"], "native-test/worktree-first");
        assert_eq!(d["runtimeMode"], "full-access");
        assert_eq!(d["session"]["providerInstanceId"], agent.instance());
        let checkout = PathBuf::from(d["worktreePath"].as_str().unwrap());
        assert_ne!(checkout, root);
        assert_eq!(
            std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
            "once\n"
        );
        assert!(!root.join("setup-ready.txt").exists());
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd).await.unwrap();
        assert_eq!(
            t.detail(agent, "worktree-first").await["messages"],
            d["messages"]
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
            "once\n"
        );
        t.call(agent, prompt("worktree-first", "continued", "normal"))
            .await
            .unwrap();
        assert_eq!(
            t.wait(agent, "worktree-first", "ready").await["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "source changes\n"
        );
        t.stop().await;
    }
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_bootstrap_preflights_prompt_and_project_before_git_mutation() {
    let (t, root) = setup_fixture(json!([])).await;
    let good = worktree_bootstrap(AgentKind::Codex, &root, "invalid-first");
    let mut cases = vec![];
    let mut c = good.clone();
    c["message"]["attachments"] =
        json!([{"type":"image","dataUrl":"data:image/png;base64,invalid"}]);
    cases.push(c);
    let mut c = good.clone();
    c["message"]["text"] = json!("x".repeat(32001));
    cases.push(c);
    let mut c = good.clone();
    c["bootstrap"]["prepareWorktree"]["projectCwd"] = json!(t.dir.path());
    cases.push(c);
    let mut c = good.clone();
    c["bootstrap"]["prepareWorktree"]["branch"] = json!("-bad");
    cases.push(c);
    let mut c = good.clone();
    c["bootstrap"]["prepareWorktree"]["branch"] = json!("main");
    cases.push(c);
    let mut c = good.clone();
    c["bootstrap"]["prepareWorktree"]["startFromOrigin"] = json!(true);
    cases.push(c);
    let mut c = good.clone();
    c["deliveryMode"] = json!("queue");
    cases.push(c);
    let mut c = good;
    c["modelSelection"] = json!({"instanceId":"claude-code","model":"test"});
    cases.push(c);
    for case in cases {
        assert!(t.call(AgentKind::Codex, case).await.is_err());
    }
    assert_eq!(
        fixture_git(&root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
    assert!(!t.dir.path().join(".remodex-worktrees").exists());
    let shell = t
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.subscribeShell",
            json!({}),
        )
        .await
        .unwrap();
    assert!(shell["snapshot"]["threads"].as_array().unwrap().is_empty());
    t.stop().await;
}
#[cfg(unix)]
#[tokio::test]
async fn worktree_bootstrap_recovers_saved_setup_when_first_prompt_commit_fails() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (mut t, root) = setup_fixture(json!([setup_script(
            "prepare",
            "echo once >> setup-ready.txt",
            true
        )]))
        .await;
        let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_first_chat BEFORE INSERT ON chats WHEN NEW.id='commit-failure' BEGIN SELECT RAISE(ABORT,'fixture save failure'); END;").unwrap();
        let cmd = worktree_bootstrap(agent, &root, "commit-failure");
        assert!(t.call(agent, cmd.clone()).await.is_err());
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM chat_rows WHERE chat='commit-failure'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(db.query_row("SELECT COUNT(*) FROM chat_commands WHERE id LIKE '%commit-failure-worktree first message-normal'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        let intent: String = db
            .query_row("SELECT data FROM native_worktrees LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let intent: Value = serde_json::from_str(&intent).unwrap();
        assert_eq!(intent["setup"], "done");
        let checkout = PathBuf::from(intent["path"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
            "once\n"
        );
        t.stop().await;
        db.execute_batch("DROP TRIGGER fail_first_chat").unwrap();
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd).await.unwrap();
        let d = t.wait(agent, "commit-failure", "ready").await;
        assert_eq!(d["messages"].as_array().unwrap().len(), 2);
        assert_eq!(d["worktreePath"], json!(checkout.canonicalize().unwrap()));
        assert_eq!(
            std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
            "once\n"
        );
        assert_eq!(
            fixture_git(&root, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            2
        );
        t.stop().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_bootstrap_fetches_origin_without_changing_source_or_repeating_fetch() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (mut t, root) = setup_fixture(json!([])).await;
        let origin = t.dir.path().join("origin.git");
        fixture_git(
            t.dir.path(),
            &[
                "clone",
                "--bare",
                root.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let writer = t.dir.path().join("remote-writer");
        fixture_git(
            t.dir.path(),
            &["clone", origin.to_str().unwrap(), writer.to_str().unwrap()],
        );
        fixture_git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let commit_remote = |content: &str| {
            std::fs::write(writer.join("tracked.txt"), content).unwrap();
            fixture_git(&writer, &["add", "tracked.txt"]);
            fixture_git(
                &writer,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-m",
                    "remote change",
                ],
            );
            fixture_git(&writer, &["push", "origin", "main"]);
            fixture_git(&writer, &["rev-parse", "HEAD"])
        };
        let old_head = fixture_git(&root, &["rev-parse", "HEAD"]);
        let remote_head = commit_remote("new remote contents\n");
        let mut cmd = worktree_bootstrap(agent, &root, "fetch-first");
        cmd["bootstrap"]["prepareWorktree"]["startFromOrigin"] = json!(true);
        cmd["bootstrap"]["prepareWorktree"]["baseBranch"] = json!(if agent == AgentKind::Codex {
            "main"
        } else {
            "origin/main"
        });
        // A configured fetch mapping must never update the checked-out local branch.
        fixture_git(
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
            ],
        );
        t.call(agent, cmd.clone()).await.unwrap();
        let d = t.wait(agent, "fetch-first", "ready").await;
        let checkout = PathBuf::from(d["worktreePath"].as_str().unwrap());
        assert_eq!(fixture_git(&checkout, &["rev-parse", "HEAD"]), remote_head);
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "new remote contents\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "source changes\n"
        );
        assert_eq!(fixture_git(&root, &["rev-parse", "HEAD"]), old_head);
        assert_eq!(fixture_git(&root, &["show", ":tracked.txt"]), "committed\n");
        assert!(!root.join(".git/FETCH_HEAD").exists());
        let later_head = commit_remote("later remote contents\n");
        assert_ne!(later_head, remote_head);
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd).await.unwrap();
        assert_eq!(
            t.detail(agent, "fetch-first").await["messages"],
            d["messages"]
        );
        assert_eq!(
            fixture_git(&root, &["rev-parse", "refs/remotes/origin/main"]),
            remote_head
        );
        assert_eq!(fixture_git(&checkout, &["rev-parse", "HEAD"]), remote_head);
        // An unavailable remote never falls back silently to a stale local branch.
        fixture_git(
            &root,
            &[
                "remote",
                "set-url",
                "origin",
                t.dir.path().join("missing.git").to_str().unwrap(),
            ],
        );
        let mut fail = worktree_bootstrap(agent, &root, "fetch-failure");
        fail["bootstrap"]["prepareWorktree"]["startFromOrigin"] = json!(true);
        let err = t.call(agent, fail).await.unwrap_err();
        assert!(err.contains("fetch"));
        assert_eq!(
            fixture_git(&root, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            2
        );
        t.stop().await;
    }
}

#[cfg(unix)]
fn existing_worktree_bootstrap(agent: AgentKind, path: &std::path::Path, id: &str) -> Value {
    let mut c = prompt(id, "existing checkout first message", "normal");
    c["bootstrap"] = json!({"createThread":{"projectId":"setup-project","title":"Existing checkout","modelSelection":{"instanceId":agent.instance(),"model":"test"},"runtimeMode":"approval-required","interactionMode":"default","branch":"existing-work","worktreePath":path},"runSetupScript":false});
    c
}

#[cfg(unix)]
#[tokio::test]
async fn existing_worktree_bootstrap_preserves_files_and_receipts_for_both_agents() {
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (mut t, root) = setup_fixture(json!([setup_script(
            "auto",
            "echo wrong > must-not-run",
            true
        )]))
        .await;
        let checkout = t.dir.path().join("existing checkout");
        fixture_git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "existing-work",
                checkout.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(checkout.join("tracked.txt"), "keep staged\n").unwrap();
        fixture_git(&checkout, &["add", "tracked.txt"]);
        std::fs::write(checkout.join("tracked.txt"), "keep unstaged\n").unwrap();
        std::fs::write(checkout.join("untracked.txt"), "keep untracked\n").unwrap();
        let cmd = existing_worktree_bootstrap(agent, &checkout, "existing-chat");
        t.call(agent, cmd.clone()).await.unwrap();
        let d = t.wait(agent, "existing-chat", "ready").await;
        assert_eq!(d["worktreePath"], json!(checkout.canonicalize().unwrap()));
        assert_eq!(d["branch"], "existing-work");
        assert_eq!(d["messages"].as_array().unwrap().len(), 2);
        assert!(!checkout.join("must-not-run").exists());
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "keep unstaged\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("untracked.txt")).unwrap(),
            "keep untracked\n"
        );
        assert_eq!(
            fixture_git(&checkout, &["show", ":tracked.txt"]),
            "keep staged\n"
        );
        t.stop().await;
        t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
        t.call(agent, cmd).await.unwrap();
        assert_eq!(
            t.detail(agent, "existing-chat").await["messages"],
            d["messages"]
        );
        assert_eq!(
            fixture_git(&root, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            2
        );
        t.call(agent, prompt("existing-chat", "continued", "normal"))
            .await
            .unwrap();
        assert_eq!(
            t.wait(agent, "existing-chat", "ready").await["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        t.stop().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn existing_worktree_bootstrap_rejects_foreign_stale_and_outside_paths() {
    let (t, root) = setup_fixture(json!([])).await;
    let checkout = t.dir.path().join("existing checkout");
    fixture_git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "existing-work",
            checkout.to_str().unwrap(),
            "main",
        ],
    );
    let foreign = t.dir.path().join("foreign");
    fixture_git(
        t.dir.path(),
        &["clone", root.to_str().unwrap(), foreign.to_str().unwrap()],
    );
    let outside = tempfile::tempdir().unwrap();
    let sub = checkout.join("sub");
    std::fs::create_dir(&sub).unwrap();
    for (i, path) in [&foreign, &outside.path().to_path_buf(), &sub]
        .iter()
        .enumerate()
    {
        let mut cmd = existing_worktree_bootstrap(AgentKind::Codex, path, &format!("bad-{i}"));
        cmd["bootstrap"]["createThread"]["branch"] = Value::Null;
        assert!(t.call(AgentKind::Codex, cmd).await.is_err());
    }
    let mut stale = existing_worktree_bootstrap(AgentKind::Claude, &checkout, "stale");
    stale["bootstrap"]["createThread"]["branch"] = json!("other-branch");
    assert!(t
        .call(AgentKind::Claude, stale)
        .await
        .unwrap_err()
        .contains("branch changed"));
    let mut no_project = existing_worktree_bootstrap(AgentKind::Codex, &checkout, "no-project");
    no_project["bootstrap"]["createThread"]["projectId"] = json!("codex-project-chats");
    assert!(t.call(AgentKind::Codex, no_project).await.is_err());
    // A detached checkout needs no invented branch and is safe to use explicitly.
    fixture_git(&checkout, &["checkout", "--detach"]);
    let mut detached = existing_worktree_bootstrap(AgentKind::Claude, &checkout, "detached");
    detached["bootstrap"]["createThread"]["branch"] = Value::Null;
    t.call(AgentKind::Claude, detached).await.unwrap();
    let d = t.wait(AgentKind::Claude, "detached", "ready").await;
    assert!(d["branch"].is_null());
    assert_eq!(
        fixture_git(&root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        2
    );
    t.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn existing_worktree_bootstrap_setup_recovery_never_repeats_or_recreates_checkout() {
    let (mut t, root) = setup_fixture(json!([setup_script(
        "prepare",
        "echo once >> setup-ready.txt",
        true
    )]))
    .await;
    let checkout = t.dir.path().join("existing checkout");
    fixture_git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "existing-work",
            checkout.to_str().unwrap(),
            "main",
        ],
    );
    let db = rusqlite::Connection::open(t.dir.path().join("native-connect.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_existing BEFORE INSERT ON chats WHEN NEW.id='existing-commit' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let mut cmd = existing_worktree_bootstrap(AgentKind::Claude, &checkout, "existing-commit");
    cmd["bootstrap"]["runSetupScript"] = json!(true);
    assert!(t.call(AgentKind::Claude, cmd.clone()).await.is_err());
    assert_eq!(
        std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
        "once\n"
    );
    t.stop().await;
    db.execute_batch("DROP TRIGGER fail_existing").unwrap();
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    // A changed branch after failed persistence must not be silently switched back.
    fixture_git(&checkout, &["checkout", "-b", "changed-work"]);
    assert!(t.call(AgentKind::Claude, cmd.clone()).await.is_err());
    fixture_git(&checkout, &["checkout", "existing-work"]);
    t.call(AgentKind::Claude, cmd).await.unwrap();
    t.wait(AgentKind::Claude, "existing-commit", "ready").await;
    assert_eq!(
        std::fs::read_to_string(checkout.join("setup-ready.txt")).unwrap(),
        "once\n"
    );
    // An intent for an existing checkout must never create a replacement after deletion.
    db.execute_batch("CREATE TRIGGER fail_missing BEFORE INSERT ON chats WHEN NEW.id='existing-missing' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let missing = existing_worktree_bootstrap(AgentKind::Codex, &checkout, "existing-missing");
    assert!(t.call(AgentKind::Codex, missing.clone()).await.is_err());
    t.stop().await;
    fixture_git(
        &root,
        &["worktree", "remove", "--force", checkout.to_str().unwrap()],
    );
    db.execute_batch("DROP TRIGGER fail_missing").unwrap();
    t.hub = Chats::start(t.db.clone(), t.config.clone()).await.unwrap();
    assert!(t.call(AgentKind::Codex, missing).await.is_err());
    assert!(!checkout.exists());
    t.stop().await;
}

#[tokio::test]
async fn repository_clone_does_not_block_either_agent_and_shutdown_never_replays_it() {
    use tokio::io::AsyncReadExt;
    let mut test = Test::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "git://127.0.0.1:{}/slow",
        listener.local_addr().unwrap().port()
    );
    let p = json!({"remoteUrl":url,"destinationPath":test.dir.path().join("clone-withheld")});
    let hub = test.hub.clone();
    let params = p.clone();
    let task = tokio::spawn(async move {
        hub.call(
            AgentKind::Codex,
            "phone",
            "sourceControl.cloneRepository",
            params,
        )
        .await
    });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut bytes = [0u8; 1024];
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut bytes))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    assert!(test
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "sourceControl.cloneRepository",
            p.clone()
        )
        .await
        .unwrap_err()
        .contains("already running"));
    assert!(test
        .hub
        .call(
            AgentKind::Claude,
            "phone",
            "sourceControl.cloneRepository",
            p.clone()
        )
        .await
        .unwrap_err()
        .contains("not available"));
    for (agent, id) in [
        (AgentKind::Codex, "clone-code"),
        (AgentKind::Claude, "clone-claude"),
    ] {
        test.create(agent, id).await;
        test.call(agent, prompt(id, "while-cloning", "normal"))
            .await
            .unwrap();
        let t = test.wait(agent, id, "ready").await;
        assert_eq!(t["messages"].as_array().unwrap().len(), 2);
    }
    test.stop().await;
    assert!(tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let closed = tokio::time::timeout(Duration::from_secs(4), stream.read(&mut bytes))
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(0))
            || matches!(closed,Err(ref e) if e.kind()==std::io::ErrorKind::ConnectionReset),
        "terminated Git connection remained open: {closed:?}"
    );
    test.hub = Chats::start(test.db.clone(), test.config.clone())
        .await
        .unwrap();
    assert!(test
        .hub
        .call(
            AgentKind::Codex,
            "phone",
            "sourceControl.cloneRepository",
            p
        )
        .await
        .unwrap_err()
        .contains("not be retried"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    test.stop().await;
}
