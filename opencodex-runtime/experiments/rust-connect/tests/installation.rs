use remodex_connect_native::{installation, launch_config, npm_upgrade};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn setup_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // Each fixture chooses a disjoint pair for this entire test process. Asking
    // the OS for port 0 and immediately dropping it lets another fixture (or
    // an outgoing HTTP connection) take either port before setup/retry.
    // Stay below the default Linux and Windows ephemeral client-port ranges.
    static NEXT: AtomicU16 = AtomicU16::new(20_000);
    loop {
        let port = NEXT.fetch_add(6, Ordering::Relaxed);
        assert!(
            port < 32_000,
            "No unused installation test port pair remains"
        );
        if let Ok(_dashboard) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            if let Ok(_phone) = std::net::TcpListener::bind(("0.0.0.0", port + 5)) {
                return port;
            }
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    gui: PathBuf,
    config: PathBuf,
    port: u16,
}

#[cfg(windows)]
#[test]
#[ignore = "writes and removes one random synthetic Windows Credential Manager entry"]
fn windows_credential_helper_roundtrip_preserves_an_existing_secret() {
    use std::{
        io::Write,
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let account = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let entry = keyring::Entry::new("opencodex.android-remote.cloudflare.v1", &account).unwrap();
    assert!(matches!(entry.get_secret(), Err(keyring::Error::NoEntry)));
    struct Cleanup(keyring::Entry);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.delete_credential();
        }
    }
    let guard = Cleanup(entry);
    let save = |secret: &[u8]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"))
            .args(["--save-tunnel-credential", &account])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(secret).unwrap();
        child.wait().unwrap().success()
    };
    assert!(save(b"synthetic-windows-qualification"));
    assert!(save(b"synthetic-windows-qualification"));
    assert!(!save(b"must-not-replace-existing-value"));
    assert!(guard.0.get_secret().unwrap() == b"synthetic-windows-qualification");
    let status = Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"))
        .args(["--named-tunnel-helper", &account])
        .creation_flags(0x08000000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    guard.0.delete_credential().unwrap();
    assert!(matches!(guard.0.get_secret(), Err(keyring::Error::NoEntry)));
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("native setup 测试 ")
            .tempdir()
            .unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("native-installation");
        let gui = base.join("gui with spaces");
        fs::create_dir(&gui).unwrap();
        fs::write(
            gui.join("index.html"),
            "<html><head></head><body>setup</body></html>",
        )
        .unwrap();
        let port = setup_port();
        let config = base.join("setup.json");
        fs::write(
            &config,
            json!({"workspace":base,"connection":{"dashboard_bind":format!("127.0.0.1:{port}")}})
                .to_string(),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            gui,
            config,
            port,
        }
    }
    async fn prepare(&self) -> Value {
        installation::prepare(&self.root, &self.gui, None, Some(&self.config))
            .await
            .unwrap()
    }
    fn launch(&self) -> tokio::process::Child {
        let mut command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        command
            .arg("--native-install")
            .arg("start")
            .arg(&self.root)
            .arg(&self.gui)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        command.spawn().unwrap()
    }
}

#[tokio::test]
async fn first_setup_is_idempotent_and_resumes_a_missing_config_without_resetting_identity() {
    let f = Fixture::new();
    assert!(installation::prepare(
        std::path::Path::new("./relative/native"),
        &f.gui,
        None,
        Some(&f.config)
    )
    .await
    .is_err());
    assert!(!f.root.exists());
    let report = f.prepare().await;
    assert_eq!(report["runtime"], "rust");
    assert!(!f.root.join("state/native-connect.sqlite").exists());
    let receipt = fs::read(f.root.join("native-install.json")).unwrap();
    let config = fs::read(f.root.join("configuration.json")).unwrap();
    fs::remove_file(f.root.join("configuration.json")).unwrap();
    assert_eq!(
        installation::prepare(&f.root, &f.gui, Some(f.port), None)
            .await
            .unwrap(),
        report
    );
    assert_eq!(
        fs::read(f.root.join("native-install.json")).unwrap(),
        receipt
    );
    assert_eq!(fs::read(f.root.join("configuration.json")).unwrap(), config);
    assert!(
        installation::prepare(&f.root, &f.gui, Some(f.port - 1), None)
            .await
            .is_err()
    );
    fs::write(f.root.join("configuration.json"), "changed").unwrap();
    assert!(installation::prepare(&f.root, &f.gui, None, None)
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(f.root.join("configuration.json")).unwrap(),
        "changed"
    );
}

#[tokio::test]
async fn legacy_or_partial_migrations_and_foreign_state_never_become_fresh_installs() {
    for name in [
        "android-remote.json",
        "native-upgrade-source-hold.json",
        "native-installation/npm-source.json",
        "native-installation/npm-upgrade.sqlite",
    ] {
        let f = Fixture::new();
        let path = f.root.parent().unwrap().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "incomplete").unwrap();
        assert!(
            installation::prepare(&f.root, &f.gui, None, Some(&f.config))
                .await
                .is_err()
        );
        assert!(!f.root.join("native-install.json").exists());
        assert_eq!(fs::read_to_string(path).unwrap(), "incomplete");
    }
    let f = Fixture::new();
    fs::create_dir(&f.root).unwrap();
    fs::write(f.root.join("foreign"), "keep").unwrap();
    assert!(
        installation::prepare(&f.root, &f.gui, None, Some(&f.config))
            .await
            .is_err()
    );
    assert!(!f.root.join("server.lock").exists());
}

#[tokio::test]
async fn installed_configuration_is_bound_to_its_state_and_original_profiles() {
    let f = Fixture::new();
    let mut cfg: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    for agent in ["codex", "claude"] {
        let profile = f.root.parent().unwrap().join(format!(".{agent}"));
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("history-sentinel"), "original history").unwrap();
        cfg[agent] = json!({"profile":profile,"executable":env!("CARGO_BIN_EXE_chat-fixture"),"model":"test"});
    }
    fs::write(&f.config, cfg.to_string()).unwrap();
    f.prepare().await;
    let path = f.root.join("configuration.json");
    let config = launch_config::load(&path).unwrap();
    assert!(installation::before_start(&f.root.join("state"), Some(&config)).is_ok());
    assert!(installation::before_start(f.root.parent().unwrap(), Some(&config)).is_err());
    assert!(installation::before_start(&f.root.join("state"), None).is_err());
    let mut altered = config.clone();
    altered.codex.as_mut().unwrap().profile = f.root.parent().unwrap().into();
    assert!(altered.validate().is_err());
    for agent in ["codex", "claude"] {
        assert_eq!(
            fs::read_to_string(
                f.root
                    .parent()
                    .unwrap()
                    .join(format!(".{agent}/history-sentinel"))
            )
            .unwrap(),
            "original history"
        );
    }
}

#[tokio::test]
async fn missing_receipt_cannot_unbind_existing_state_or_clear_pairing() {
    let f = Fixture::new();
    f.prepare().await;
    let config = launch_config::load(&f.root.join("configuration.json")).unwrap();
    let state = f.root.join("state");
    let store = remodex_connect_native::store::Store::open(&state).unwrap();
    installation::bind_store(&store, Some(&config)).unwrap();
    drop(store);
    fs::write(state.join("pairing-sentinel"), "keep pairing").unwrap();
    fs::remove_file(f.root.join("native-install.json")).unwrap();
    assert!(installation::before_start(&state, None).is_err());
    assert!(installation::before_start(&state, Some(&config)).is_err());
    assert_eq!(
        fs::read_to_string(state.join("pairing-sentinel")).unwrap(),
        "keep pairing"
    );
    assert!(
        installation::prepare(&f.root, &f.gui, None, Some(&f.config))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn expanded_setup_record_is_bounded_before_commit_and_can_be_retried() {
    let f = Fixture::new();
    let original = fs::read(&f.config).unwrap();
    let mut config: Value = serde_json::from_slice(&original).unwrap();
    config["codex"] = json!({"executable":env!("CARGO_BIN_EXE_chat-fixture"),
        "profile":f.root.parent().unwrap(),"model":""});
    let overhead = serde_json::to_vec(&config).unwrap().len();
    config["codex"]["model"] = json!("x".repeat(262144 - overhead));
    let bytes = serde_json::to_vec(&config).unwrap();
    assert_eq!(bytes.len(), 262144);
    fs::write(&f.config, bytes).unwrap();
    let error = installation::prepare(&f.root, &f.gui, None, Some(&f.config))
        .await
        .unwrap_err();
    assert!(error.contains("record size"), "{error}");
    assert!(!f.root.join("native-install.json").exists());
    fs::write(&f.config, original).unwrap();
    f.prepare().await;
}

#[tokio::test]
async fn busy_listener_never_commits_setup_and_retry_can_succeed() {
    for phone in [false, true] {
        let f = Fixture::new();
        let busy =
            std::net::TcpListener::bind(("127.0.0.1", f.port + if phone { 5 } else { 0 })).unwrap();
        let error = installation::prepare(&f.root, &f.gui, None, Some(&f.config))
            .await
            .unwrap_err();
        assert!(
            error.contains(if phone {
                "phone port"
            } else {
                "dashboard port"
            }),
            "{error}"
        );
        assert!(!f.root.join("native-install.json").exists());
        drop(busy);
        f.prepare().await;
    }
}

#[tokio::test]
async fn onboarding_discovers_existing_cli_profiles_without_a_legacy_runtime() {
    let f = Fixture::new();
    let base = f.root.parent().unwrap();
    for name in [".codex", ".claude"] {
        fs::create_dir(base.join(name)).unwrap();
    }
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
    command
        .args(["--native-install", "onboard"])
        .arg(&f.root)
        .arg(&f.gui)
        .args(["--port", &f.port.to_string()])
        .current_dir(base)
        .env("HOME", base)
        .env("USERPROFILE", base)
        .env("PATH", "")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env(
            "REMODEX_CODEX_EXECUTABLE",
            env!("CARGO_BIN_EXE_chat-fixture"),
        )
        .env(
            "REMODEX_CLAUDE_EXECUTABLE",
            env!("CARGO_BIN_EXE_chat-fixture"),
        );
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = launch_config::load(&f.root.join("configuration.json")).unwrap();
    assert_eq!(config.codex.unwrap().profile, base.join(".codex"));
    assert_eq!(config.claude.unwrap().profile, base.join(".claude"));
    assert!(!base.join("android-remote.json").exists());
    assert!(!f.root.join("npm-upgrade.sqlite").exists());
}

#[tokio::test]
async fn native_package_lifecycle_keeps_pairing_and_saved_data_across_restart() {
    let f = Fixture::new();
    let prepared = f.prepare().await;
    let mut child = f.launch();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(15), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("bridge ready");
    let ready: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["ready"], true);
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let phone = format!("http://127.0.0.1:{}", f.port + 5);
    let invite: Value =
        serde_json::from_slice(&fs::read(ready["invitationFile"].as_str().unwrap()).unwrap())
            .unwrap();
    let paired: Value = http
        .post(format!("{phone}/oauth/token"))
        .form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            (
                "subject_token_type",
                "urn:t3:params:oauth:token-type:environment-bootstrap",
            ),
            (
                "requested_token_type",
                "urn:ietf:params:oauth:token-type:access_token",
            ),
            ("subject_token", invite["pairingToken"].as_str().unwrap()),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(paired["access_token"].is_string());
    let database = f.root.join("state/native-connect.sqlite");
    let row = || {
        let db = rusqlite::Connection::open(&database).unwrap();
        db.query_row(
            "SELECT id||credential_digest FROM clients LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    let identity = row();
    {
        let db = rusqlite::Connection::open(&database).unwrap();
        db.execute(
            "INSERT INTO metadata VALUES('installation.test.history','saved before update')",
            [],
        )
        .unwrap();
    }
    assert_eq!(
        npm_upgrade::command(&f.root, "status").await.unwrap()["running"],
        true
    );
    let restarted = npm_upgrade::command(&f.root, "restart").await.unwrap();
    assert_eq!(restarted["restarted"], true);
    assert_ne!(restarted["pid"], ready["pid"]);
    assert_eq!(row(), identity);
    let db = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        prepared["environmentId"].as_str().unwrap()
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='installation.test.history'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "saved before update"
    );
    drop(db);
    assert_eq!(
        npm_upgrade::command(&f.root, "stop").await.unwrap()["stopped"],
        true
    );
    assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert_eq!(
        npm_upgrade::command(&f.root, "status").await.unwrap()["running"],
        false
    );
}

#[cfg(unix)]
#[tokio::test]
async fn linked_or_corrupt_installation_records_fail_without_overwriting_targets() {
    let f = Fixture::new();
    let other = f.root.parent().unwrap().join("other");
    fs::create_dir(&other).unwrap();
    std::os::unix::fs::symlink(&other, &f.root).unwrap();
    assert!(
        installation::prepare(&f.root, &f.gui, None, Some(&f.config))
            .await
            .is_err()
    );
    assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    fs::remove_file(&f.root).unwrap();
    f.prepare().await;
    fs::write(f.root.join("native-install.json"), "partial").unwrap();
    assert!(installation::prepare(&f.root, &f.gui, None, None)
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(f.root.join("native-install.json")).unwrap(),
        "partial"
    );
}

#[tokio::test]
async fn configuration_revisions_are_atomic_recoverable_and_do_not_rewind_data() {
    let f = Fixture::new();
    f.prepare().await;
    let original = installation::inspect(&f.root).unwrap();
    let config = launch_config::load(&f.root.join("configuration.json")).unwrap();
    let store = remodex_connect_native::store::Store::open(&f.root.join("state")).unwrap();
    installation::bind_store(&store, Some(&config)).unwrap();
    rusqlite::Connection::open(f.root.join("state/native-connect.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO metadata VALUES('test.history','newest history')",
            [],
        )
        .unwrap();
    drop(store);
    let mut edited = original["configuration"].clone();
    edited["max_active_chats"] = json!(6);
    fs::write(&f.config, edited.to_string()).unwrap();
    assert_eq!(
        installation::configure(&f.root, "check", f.config.to_str().unwrap(), None).unwrap()
            ["revision"],
        0
    );
    assert!(installation::configure(&f.root, "apply", f.config.to_str().unwrap(), None).is_err());
    assert!(
        installation::configure(&f.root, "apply", f.config.to_str().unwrap(), Some(4)).is_err()
    );
    for directory in [&f.root, f.root.join("state").as_path()] {
        let lease = remodex_connect_native::profile_lock::acquire(directory).unwrap();
        assert!(
            installation::configure(&f.root, "apply", f.config.to_str().unwrap(), Some(0)).is_err()
        );
        drop(lease);
    }
    let busy = std::net::TcpListener::bind(("127.0.0.1", f.port + 5)).unwrap();
    assert!(
        installation::configure(&f.root, "apply", f.config.to_str().unwrap(), Some(0)).is_err()
    );
    drop(busy);
    assert_eq!(installation::inspect(&f.root).unwrap(), original);
    assert_eq!(
        installation::configure(&f.root, "apply", f.config.to_str().unwrap(), Some(0)).unwrap()
            ["revision"],
        1
    );
    installation::service_configuration(&f.root).unwrap();
    assert!(
        launch_config::load(&f.root.join("configuration.json")).is_err(),
        "stale config must not launch"
    );
    assert_eq!(
        installation::configure(&f.root, "rollback", "0", Some(1)).unwrap()["revision"],
        2
    );
    assert_eq!(
        installation::inspect(&f.root).unwrap()["configuration"],
        original["configuration"]
    );
    installation::service_configuration(&f.root).unwrap();
    let db = rusqlite::Connection::open(f.root.join("state/native-connect.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='test.history'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "newest history"
    );
    // Re-materialize an interrupted generation without reverting its receipt.
    let generated = fs::read_dir(&f.root)
        .unwrap()
        .map(|p| p.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("configuration-2-")
        })
        .unwrap();
    fs::remove_file(&generated).unwrap();
    installation::prepare(&f.root, &f.gui, None, None)
        .await
        .unwrap();
    assert!(generated.exists());
}

#[tokio::test]
async fn configuration_cannot_silently_switch_existing_agent_profiles() {
    let f = Fixture::new();
    let base = f.root.parent().unwrap();
    let profile = base.join("agent-a");
    let other = base.join("agent-b");
    fs::create_dir(&profile).unwrap();
    fs::create_dir(&other).unwrap();
    let mut c: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    c["claude"] = json!({"executable":env!("CARGO_BIN_EXE_chat-fixture"),"profile":profile,"model":"default"});
    fs::write(&f.config, c.to_string()).unwrap();
    f.prepare().await;
    c = installation::inspect(&f.root).unwrap()["configuration"].clone();
    c["claude"]["profile"] = json!(other);
    fs::write(&f.config, c.to_string()).unwrap();
    assert!(
        installation::configure(&f.root, "check", f.config.to_str().unwrap(), None)
            .unwrap_err()
            .contains("saved chats")
    );
    c["claude"] = Value::Null;
    fs::write(&f.config, c.to_string()).unwrap();
    assert!(
        installation::configure(&f.root, "apply", f.config.to_str().unwrap(), Some(0)).is_err()
    );
    assert_eq!(installation::inspect(&f.root).unwrap()["revision"], 0);
}

#[tokio::test]
async fn remote_choice_applies_once_and_keeps_later_dashboard_preferences() {
    let f = Fixture::new();
    f.prepare().await;
    assert_eq!(
        installation::remote(&f.root, "quick", None, None, 0)
            .await
            .unwrap()["revision"],
        1
    );
    let raw = installation::inspect(&f.root).unwrap()["configuration"].clone();
    let c: remodex_connect_native::chat::Config = serde_json::from_value(raw).unwrap();
    let store = remodex_connect_native::store::Store::open(&f.root.join("state")).unwrap();
    installation::bind_store(&store, Some(&c)).unwrap();
    assert_eq!(
        rusqlite::Connection::open(f.root.join("state/native-connect.sqlite"))
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='quick_tunnel_enabled'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "true"
    );
    rusqlite::Connection::open(f.root.join("state/native-connect.sqlite"))
        .unwrap()
        .execute(
            "UPDATE metadata SET value='false' WHERE key='quick_tunnel_enabled'",
            [],
        )
        .unwrap();
    installation::bind_store(&store, Some(&c)).unwrap();
    assert_eq!(
        rusqlite::Connection::open(f.root.join("state/native-connect.sqlite"))
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='quick_tunnel_enabled'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "false"
    );
    drop(store);
    assert!(installation::remote(&f.root, "local", None, None, 0)
        .await
        .is_err());
    assert_eq!(
        installation::remote(&f.root, "local", None, None, 1)
            .await
            .unwrap()["revision"],
        2
    );
}

#[tokio::test]
async fn service_preparation_pins_assets_and_detects_tampering_without_registering() {
    let f = Fixture::new();
    f.prepare().await;
    let reg = f.root.parent().unwrap().join("service-v1");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture"));
    let result =
        remodex_connect_native::native_service::prepare(&reg, &f.root, &f.gui, &binary).unwrap();
    assert_eq!(result["serviceStarted"], false);
    assert!(
        remodex_connect_native::native_service::prepare(&reg, &f.root, &f.gui, &binary).is_ok()
    );
    let original_gui = fs::read(f.gui.join("index.html")).unwrap();
    fs::write(f.gui.join("index.html"), "new package dashboard").unwrap();
    assert!(
        remodex_connect_native::native_service::prepare(&reg, &f.root, &f.gui, &binary).is_err(),
        "a repeated preparation must not silently reuse an older package"
    );
    fs::write(f.gui.join("index.html"), original_gui).unwrap();
    fs::remove_file(f.gui.join("index.html")).unwrap();
    let copied = reg.join("gui/index.html");
    assert!(copied.exists());
    fs::write(&copied, "changed dashboard").unwrap();
    assert!(
        remodex_connect_native::native_service::prepare(&reg, &f.root, &f.gui, &binary).is_err()
    );
    assert!(f.root.join("native-install.json").exists());
}

#[tokio::test]
async fn configuration_can_repair_a_missing_workspace_without_discarding_the_old_revision() {
    let f = Fixture::new();
    let old = f._dir.path().join("removed-workspace");
    fs::create_dir(&old).unwrap();
    let mut setup: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    setup["workspace"] = json!(old);
    fs::write(&f.config, setup.to_string()).unwrap();
    f.prepare().await;
    fs::remove_dir(&old).unwrap();
    let mut candidate = installation::inspect(&f.root).unwrap()["configuration"].clone();
    candidate["workspace"] = json!(f._dir.path());
    let file = f._dir.path().join("repair.json");
    fs::write(&file, candidate.to_string()).unwrap();
    assert_eq!(
        installation::configure(&f.root, "apply", file.to_str().unwrap(), Some(0)).unwrap()
            ["revision"],
        1
    );
    assert!(f.root.join("installation-revision-0.json").is_file());
    assert!(installation::service_configuration(&f.root).is_ok());
    assert!(
        installation::configure(&f.root, "rollback", "0", Some(1)).is_err(),
        "an unavailable old workspace cannot become active again"
    );
    assert_eq!(installation::inspect(&f.root).unwrap()["revision"], 1);
}
