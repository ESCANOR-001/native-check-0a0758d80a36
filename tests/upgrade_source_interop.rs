use remodex_connect_native::upgrade_source;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

#[tokio::test]
async fn bun_owner_retires_and_rust_recovers_the_exact_durable_plan() {
    for mode in ["", "preflight", "warming", "bad-config", "wrong-port"] {
        qualify(mode).await;
    }
}
async fn qualify(mode: &str) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let module = [
        manifest.join("../../src/android-remote/upgrade-control.ts"),
        manifest.join("migration/src/android-remote/upgrade-control.ts"),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .expect("Source-owner module is required for the migration qualification");
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let installation = base.join("installation");
    let mut command = Command::new("bun");
    command
        .arg(manifest.join("tests/fixtures/upgrade-owner.ts"))
        .arg(module)
        .arg(&base)
        .arg(&installation)
        .arg(mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .expect("Bun is needed only to qualify the legacy source handoff");
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(line.len() < 8192);
    let descriptor = PathBuf::from(line);
    assert_eq!(
        upgrade_source::registered(&installation).unwrap(),
        descriptor
    );
    if mode == "bad-config" || mode == "wrong-port" {
        let result = if mode == "wrong-port" {
            upgrade_source::acquire_on_port(&installation, &descriptor, 4321).await
        } else {
            upgrade_source::acquire(&installation, &descriptor).await
        };
        assert!(result.is_err());
        assert!(
            !descriptor.parent().unwrap().join("attempt.json").exists(),
            "Failed preflight must not start source shutdown"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "Working source remains running"
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        return;
    }
    let plan = tokio::time::timeout(
        Duration::from_secs(10),
        upgrade_source::acquire(&installation, &descriptor),
    )
    .await
    .unwrap()
    .unwrap_or_else(|error| {
        // Synthetic fixture diagnostics only: never log the bearer descriptor.
        let directory = descriptor.parent().unwrap();
        panic!(
            "{error}; attempt={}, plan={}, closedOwner={}",
            directory.join("attempt.json").exists(),
            directory.join("npm-plan.json").exists(),
            directory.join("export/closed-owner.json").exists()
        );
    });
    let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("The old source must release its last listener after publication")
        .unwrap();
    assert!(exit.success());
    assert_eq!(plan, descriptor.parent().unwrap().join("npm-plan.json"));
    assert_eq!(
        upgrade_source::acquire(&installation, &descriptor)
            .await
            .unwrap(),
        plan
    );
    assert_eq!(
        std::fs::read_dir(&installation).unwrap().count(),
        1,
        "Only the source locator exists; acquisition must not create an execution profile"
    );
    assert_eq!(
        upgrade_source::registered(&installation).unwrap(),
        descriptor
    );
}
