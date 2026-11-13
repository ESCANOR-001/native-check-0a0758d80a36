//! Windows-only qualification of the real Bun→Rust process boundary. No
//! cloud service, user profile, firewall change or scheduled task is used.
#![cfg(windows)]
use remodex_connect_native::quick_handoff::{Config, Process};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

#[tokio::test]
async fn retained_connector_survives_bun_without_retaining_its_listening_socket() {
    let root = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/quick-owner.ts");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("bun")
            .arg(script)
            .arg(env!("CARGO_BIN_EXE_remodex-connect-native"))
            .arg(env!("CARGO_BIN_EXE_protocol-fixture"))
            .arg(root.path().join("connector"))
            .env("REMODEX_FIXTURE_BEHAVIOR", "wait")
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Config = serde_json::from_slice(&output.stdout).unwrap();
    let process = Process::open(&config).unwrap();
    struct Cleanup(Process);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.disable();
        }
    }
    let cleanup = Cleanup(process);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        cleanup.0.alive(),
        "Bun exit must not terminate the retained connector"
    );
    let listener = std::net::TcpListener::bind(config.origin)
        .expect("The old Bun listening socket must not leak into the connector");
    assert_eq!(listener.local_addr().unwrap(), config.origin);
    cleanup.0.disable().unwrap();
}
