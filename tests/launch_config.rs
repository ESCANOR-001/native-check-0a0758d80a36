use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

fn configuration(root: &Path) -> Value {
    let agent =
        json!({"executable":env!("CARGO_BIN_EXE_chat-fixture"),"profile":root,"model":"test"});
    json!({"workspace":root,"codex":agent,"claude":agent})
}
async fn run(args: &[&std::ffi::OsStr]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn configuration_preflight_never_starts_agents_or_opens_state() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.json");
    std::fs::write(&config, configuration(root.path()).to_string()).unwrap();
    let output = run(&["--check-config".as_ref(), config.as_os_str()]).await;
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["configurationValid"], true);
    assert_eq!(report["runtimeStarted"], false);
    assert_eq!(report["migrationComplete"], false);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    assert!(output.stderr.is_empty());
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(!report.contains(root.path().to_str().unwrap()));
}

#[tokio::test]
async fn failed_candidate_preserves_existing_invitation_permissions_and_state_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).unwrap();
    }
    let database = state.join("native-connect.sqlite");
    std::fs::write(
        &database,
        b"State must never be opened for an invalid candidate",
    )
    .unwrap();
    let invitation = state.join(format!("pairing-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&invitation, b"SYNTHETIC_OLD_INVITATION").unwrap();
    let config = root.path().join("config.json");
    let valid = configuration(root.path());
    let mut unsupported = valid.clone();
    unsupported["futureUnsupportedField"] = json!("PRIVATE_CONFIG_SENTINEL");
    let mut invalid = valid.clone();
    invalid["max_active_chats"] = json!(999);
    for bytes in [
        unsupported.to_string().into_bytes(),
        invalid.to_string().into_bytes(),
        b"PRIVATE_CONFIG_SENTINEL".to_vec(),
        vec![b'x'; 256 * 1024 + 1],
    ] {
        std::fs::write(&config, bytes).unwrap();
        for supervised in [false, true] {
            let mut args = vec![
                state.as_os_str(),
                "127.0.0.1:0".as_ref(),
                "http://127.0.0.1".as_ref(),
                config.as_os_str(),
            ];
            if supervised {
                args.insert(0, "--supervise".as_ref());
            }
            let output = run(&args).await;
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_CONFIG_SENTINEL"));
            assert!(!state.join("server.lock").exists());
            assert_eq!(std::fs::read_dir(&state).unwrap().count(), 2);
            assert_eq!(
                std::fs::read(&database).unwrap(),
                b"State must never be opened for an invalid candidate"
            );
            assert_eq!(
                std::fs::read(&invitation).unwrap(),
                b"SYNTHETIC_OLD_INVITATION"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
                    0o750
                );
            }
        }
    }
}

#[tokio::test]
async fn incompatible_configuration_does_not_create_a_new_destination() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let state = root.path().join("not-created");
    let output = run(&[
        state.as_os_str(),
        "127.0.0.1:0".as_ref(),
        "http://127.0.0.1".as_ref(),
        config.as_os_str(),
    ])
    .await;
    assert!(!output.status.success());
    assert!(!state.exists());
}
