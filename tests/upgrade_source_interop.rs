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
    let plan = tokio::time::timeout(
        Duration::from_secs(10),
        upgrade_source::acquire(&installation, &descriptor),
    )
    .await
    .unwrap()
    .unwrap();
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
    assert!(
        !installation.exists(),
        "Acquisition must not preempt native validation or create an execution profile"
    );
}
