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
