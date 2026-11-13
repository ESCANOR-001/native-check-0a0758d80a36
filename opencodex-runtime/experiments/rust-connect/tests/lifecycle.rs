#[cfg(unix)]
#[tokio::test]
async fn termination_removes_invitation_and_preserves_environment_identity() {
    use remodex_connect_native::store::Store;
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"))
            .args([
                directory.path().to_str().unwrap(),
                "127.0.0.1:0",
                "http://127.0.0.1",
            ])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut line = String::new();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["migrationComplete"], false);
        let path = ready["invitationFile"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists());
        unsafe {
            libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
        }
        assert!(tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert!(!std::path::Path::new(path).exists());
        let identity = Store::open(directory.path())
            .unwrap()
            .environment_id()
            .unwrap();
        if let Some(previous) = &previous {
            assert_eq!(&identity, previous);
        }
        previous = Some(identity);
    }
}

#[tokio::test]
async fn duplicate_profile_is_rejected_and_crash_releases_lock() {
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let launch = || {
        let mut command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        command
            .args([
                dir.path().to_str().unwrap(),
                "127.0.0.1:0",
                "http://127.0.0.1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        command.spawn().unwrap()
    };
    let mut owner = launch();
    let mut ready = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(owner.stdout.take().unwrap()).read_line(&mut ready),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(ready.contains("\"ready\":true"));
    let old_invitation = serde_json::from_str::<serde_json::Value>(&ready).unwrap()
        ["invitationFile"]
        .as_str()
        .unwrap()
        .to_owned();
    let duplicate = launch();
    let output = tokio::time::timeout(Duration::from_secs(5), duplicate.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already has a running bridge"));
    assert!(owner.try_wait().unwrap().is_none());
    owner.kill().await.unwrap();
    owner.wait().await.unwrap();
    let mut restarted = launch();
    ready.clear();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(restarted.stdout.take().unwrap()).read_line(&mut ready),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(ready.contains("\"ready\":true"));
    assert!(!std::path::Path::new(&old_invitation).exists());
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("pairing-"))
            .count(),
        1
    );
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
}

#[tokio::test]
async fn help_and_version_do_not_create_profiles_or_start_agents() {
    use std::{process::Stdio, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h", "--version"] {
        let mut command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        command
            .arg(flag)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let out = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        if flag == "--version" {
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(v["runtime"], "rust");
            assert_eq!(v["configurationVersion"], 1);
            assert_eq!(v["storageFormatVersion"], 1);
            assert_eq!(v["migrationComplete"], false);
            assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        } else {
            assert!(String::from_utf8(out.stdout)
                .unwrap()
                .contains("ABSOLUTE_ISOLATED_STATE_DIR"));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn failed_startup_does_not_advertise_an_invitation() {
    use std::{process::Stdio, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("invalid-config.json");
    std::fs::write(&config, b"{}").unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
    command
        .args([
            dir.path().to_str().unwrap(),
            "127.0.0.1:0",
            "http://127.0.0.1",
            config.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        command.spawn().unwrap().wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ready"));
    assert!(!std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().starts_with("pairing-")));
}

#[tokio::test]
async fn dashboard_stop_closes_sockets_and_restart_keeps_phone_credentials() {
    use futures_util::StreamExt;
    use serde_json::{json, Value};
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let gui = root.path().join("gui");
    std::fs::create_dir(&gui).unwrap();
    std::fs::write(
        gui.join("index.html"),
        "<!doctype html><head></head><body>Fixture</body>",
    )
    .unwrap();
    let config = root.path().join("config.json");
    let agent = json!({"executable":env!("CARGO_BIN_EXE_chat-fixture"),"profile":root.path(),"model":"test"});
    std::fs::write(
        &config,
        json!({"workspace":root.path(),"codex":agent,"claude":agent}).to_string(),
    )
    .unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut phone = None;
    let mut environment = None;
    for _ in 0..2 {
        let mut command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        command
            .args([
                state.as_os_str(),
                std::ffi::OsStr::new("127.0.0.1:0"),
                std::ffi::OsStr::new("http://127.0.0.1"),
                config.as_os_str(),
                gui.as_os_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        let base = format!("http://{}", ready["address"].as_str().unwrap());
        let invitation = std::path::PathBuf::from(ready["invitationFile"].as_str().unwrap());
        let invite: Value = serde_json::from_slice(&std::fs::read(&invitation).unwrap()).unwrap();
        let descriptor: Value = http
            .get(format!("{base}/.well-known/t3/environment"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(descriptor["environmentId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        if let Some(old) = &environment {
            assert_eq!(old, &descriptor["environmentId"]);
        }
        environment = Some(descriptor["environmentId"].clone());
        if phone.is_none() {
            let response: Value = http
                .post(format!("{base}/oauth/token"))
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
            phone = Some(response["access_token"].as_str().unwrap().to_owned());
        }
        let auth: Value = http
            .get(format!("{base}/api/auth/session"))
            .bearer_auth(phone.as_ref().unwrap())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(auth["authenticated"], true);
        let ticket: Value = http
            .post(format!("{base}/api/auth/websocket-ticket"))
            .bearer_auth(phone.as_ref().unwrap())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let url = format!(
            "ws://{}/ws?wsTicket={}",
            ready["address"].as_str().unwrap(),
            ticket["ticket"].as_str().unwrap()
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let html = http
            .get(format!("{base}/opencodex-session"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        let meta = |key| {
            html.split(&format!("name=\"{key}\" content=\""))
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_owned()
        };
        let response: Value = http
            .post(format!("{base}/api/stop"))
            .header("origin", &base)
            .header("x-opencodex-gui-origin", &base)
            .header("x-opencodex-api-key", meta("opencodex-session-token"))
            .header("x-opencodex-csrf-token", meta("opencodex-session-csrf"))
            .header("x-opencodex-expected-pid", child.id().unwrap())
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["success"], true);
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap();
        assert!(next.is_none() || next.is_some_and(|f| f.is_err() || f.unwrap().is_close()));
        drop(socket);
        assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert!(!invitation.exists());
        let diagnostics = std::fs::read_to_string(state.join("service.log")).unwrap();
        assert!(diagnostics.contains("runtime.ready"));
        assert!(diagnostics.contains("runtime.stopping"));
        assert!(diagnostics.contains("runtime.stopped"));
        assert!(!diagnostics.contains(phone.as_ref().unwrap()));
    }
}
