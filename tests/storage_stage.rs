use remodex_connect_native::storage_stage::Lease;
use std::{fs, io::ErrorKind};
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join("archived_sessions")).unwrap();
    d
}
fn archive(d: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
    let rel = format!("archived_sessions/{name}");
    fs::write(d.path().join(&rel), bytes).unwrap();
    rel
}
#[test]
fn locked_journal_round_trip_preserves_plain_and_compressed_bytes_and_metadata() {
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"ONE\n");
    let b = archive(&d, "one.jsonl.zst", &[0, 1, 255, 3]);
    let time = fs::metadata(d.path().join(&a)).unwrap().modified().unwrap();
    let lease = Lease::acquire(d.path()).unwrap();
    let mut stage = lease.prepare(&[a.clone(), b.clone()]).unwrap();
    let id = stage.id().to_owned();
    stage.stage_files().unwrap();
    assert!(!d.path().join(&a).exists());
    stage.reconciled().unwrap();
    drop(stage);
    drop(lease);
    let lease = Lease::acquire(d.path()).unwrap();
    let mut stage = lease.resume(&id).unwrap();
    stage.restore_files().unwrap();
    stage.finish_restore().unwrap();
    assert_eq!(fs::read(d.path().join(&a)).unwrap(), b"ONE\n");
    assert_eq!(fs::read(d.path().join(&b)).unwrap(), [0, 1, 255, 3]);
    assert_eq!(
        fs::metadata(d.path().join(&a)).unwrap().modified().unwrap(),
        time
    );
    assert!(!d.path().join(id).exists());
}
#[test]
fn interrupted_stage_and_link_before_unlink_are_recoverable_without_overwrite() {
    for hard_link_only in [true, false] {
        let d = fixture();
        let a = archive(&d, "one.jsonl", b"ONE");
        let b = archive(&d, "two.jsonl", b"TWO");
        let lease = Lease::acquire(d.path()).unwrap();
        let stage = lease.prepare(&[a.clone(), b.clone()]).unwrap();
        let id = stage.id().to_owned();
        fs::hard_link(d.path().join(&a), stage.directory().join("one.jsonl")).unwrap();
        if !hard_link_only {
            fs::remove_file(d.path().join(&a)).unwrap();
        }
        drop(stage);
        drop(lease);
        let lease = Lease::acquire(d.path()).unwrap();
        let mut stage = lease.resume(&id).unwrap();
        stage.restore_files().unwrap();
        stage.finish_restore().unwrap();
        assert_eq!(fs::read(d.path().join(&a)).unwrap(), b"ONE");
        assert_eq!(fs::read(d.path().join(&b)).unwrap(), b"TWO");
    }
}
#[test]
fn new_destination_and_source_changes_remain_untouched_and_journal_is_retained() {
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"ORIGINAL");
    let lease = Lease::acquire(d.path()).unwrap();
    let mut stage = lease.prepare(std::slice::from_ref(&a)).unwrap();
    let id = stage.id().to_owned();
    stage.stage_files().unwrap();
    fs::write(d.path().join(&a), b"ORIGINAL").unwrap();
    assert!(stage.restore_files().is_err());
    assert_eq!(fs::read(d.path().join(&a)).unwrap(), b"ORIGINAL");
    assert!(stage.directory().join("one.jsonl").exists());
    fs::remove_file(d.path().join(&a)).unwrap();
    stage.restore_files().unwrap();
    stage.finish_restore().unwrap();
    assert!(!d.path().join(id).exists());
    let mut stage = lease.prepare(std::slice::from_ref(&a)).unwrap();
    fs::write(d.path().join(&a), b"USER_EDIT").unwrap();
    assert!(stage.stage_files().is_err());
    assert_eq!(fs::read(d.path().join(a)).unwrap(), b"USER_EDIT");
    assert!(stage.directory().join("native-stage.json").exists());
}
#[test]
fn staging_and_restoring_resume_after_partial_success_and_refuse_unknown_files() {
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"ONE");
    let b = archive(&d, "two.jsonl", b"TWO");
    let lease = Lease::acquire(d.path()).unwrap();
    let mut stage = lease.prepare(&[a.clone(), b.clone()]).unwrap();
    let id = stage.id().to_owned();
    fs::write(stage.directory().join("two.jsonl"), b"DO_NOT_REPLACE").unwrap();
    assert!(stage.stage_files().is_err());
    assert!(!d.path().join(&a).exists());
    assert_eq!(
        fs::read(stage.directory().join("two.jsonl")).unwrap(),
        b"DO_NOT_REPLACE"
    );
    fs::remove_file(stage.directory().join("two.jsonl")).unwrap();
    drop(stage);
    let mut stage = lease.resume(&id).unwrap();
    stage.stage_files().unwrap();
    stage.reconciled().unwrap();
    fs::write(d.path().join(&b), b"USER_NEW").unwrap();
    assert!(stage.restore_files().is_err());
    assert_eq!(fs::read(d.path().join(&a)).unwrap(), b"ONE");
    drop(stage);
    fs::remove_file(d.path().join(&b)).unwrap();
    let mut stage = lease.resume(&id).unwrap();
    stage.restore_files().unwrap();
    fs::write(stage.directory().join("database-backup.json"), b"KEEP").unwrap();
    assert_eq!(
        stage.finish_restore().unwrap_err().kind(),
        ErrorKind::DirectoryNotEmpty
    );
    assert!(d.path().join(id).join("native-stage.json").exists());
}
#[test]
fn invalid_paths_foreign_journals_and_concurrent_operations_fail_closed() {
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"ONE");
    let lease = Lease::acquire(d.path()).unwrap();
    assert!(matches!(Lease::acquire(d.path()),Err(e) if e.kind()==ErrorKind::WouldBlock));
    for path in [
        "../one.jsonl",
        "archived_sessions/../one.jsonl",
        "archived_sessions/a/b.jsonl",
        "archived_sessions/a\\b.jsonl",
        "archived_sessions/a:stream.jsonl",
        "archived_sessions/plain",
        "archived_sessions/\n.jsonl",
    ] {
        assert!(lease.prepare(&[path.into()]).is_err(), "accepted {path:?}");
    }
    assert!(lease.prepare(&[a.clone(), a.clone()]).is_err());
    fs::hard_link(
        d.path().join(&a),
        d.path().join("archived_sessions/alias.jsonl"),
    )
    .unwrap();
    assert!(lease
        .prepare(&[a.clone(), "archived_sessions/alias.jsonl".into()])
        .is_err());
    let stage = lease.prepare(&[a]).unwrap();
    let id = stage.id().to_owned();
    for bad in [
        "../escape",
        ".trash/../x",
        ".trash/1-1/extra",
        ".trash/x",
        ".trash/1-2-3",
    ] {
        assert!(lease.resume(bad).is_err());
    }
    let other = fixture();
    let other_lease = Lease::acquire(other.path()).unwrap();
    fs::create_dir_all(other.path().join(&id)).unwrap();
    fs::copy(
        stage.directory().join("native-stage.json"),
        other.path().join(&id).join("native-stage.json"),
    )
    .unwrap();
    assert!(other_lease.resume(&id).is_err());
    drop(stage);
    drop(lease);
    assert!(Lease::acquire(d.path()).is_ok());
}
#[test]
fn corrupted_or_oversized_journal_does_not_change_archives() {
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"ONE");
    let lease = Lease::acquire(d.path()).unwrap();
    let stage = lease.prepare(std::slice::from_ref(&a)).unwrap();
    let id = stage.id().to_owned();
    let journal = stage.directory().join("native-stage.json");
    drop(stage);
    fs::write(&journal, b"{invalid}").unwrap();
    assert!(lease.resume(&id).is_err());
    fs::write(&journal, vec![b' '; 8 * 1024 * 1024 + 1]).unwrap();
    assert!(lease.resume(&id).is_err());
    assert_eq!(fs::read(d.path().join(&a)).unwrap(), b"ONE");
}
#[cfg(any(unix, windows))]
#[test]
fn linked_paths_are_rejected_before_mutation() {
    fn link_file(from: &std::path::Path, to: &std::path::Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(from, to).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(from, to).unwrap();
    }
    fn link_dir(from: &std::path::Path, to: &std::path::Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(from, to).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(from, to).unwrap();
    }
    let d = fixture();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("one.jsonl");
    fs::write(&file, b"UNCHANGED").unwrap();
    link_file(&file, &d.path().join("archived_sessions/linked.jsonl"));
    let lease = Lease::acquire(d.path()).unwrap();
    assert!(lease
        .prepare(&["archived_sessions/linked.jsonl".into()])
        .is_err());
    link_dir(outside.path(), &d.path().join(".trash"));
    archive(&d, "safe.jsonl", b"SAFE");
    assert!(lease
        .prepare(&["archived_sessions/safe.jsonl".into()])
        .is_err());
    assert_eq!(fs::read(&file).unwrap(), b"UNCHANGED");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[test]
fn isolated_stage_child() {
    let Some(home) = std::env::var_os("REMODEX_STAGE_TEST_HOME") else {
        return;
    };
    let lease = Lease::acquire(std::path::Path::new(&home)).unwrap();
    let mut stage = lease
        .prepare(&["archived_sessions/one.jsonl".into()])
        .unwrap();
    stage.stage_files().unwrap();
    println!("STAGE_READY:{}", stage.id());
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn process_crash_releases_lock_and_keeps_journal_recoverable() {
    use std::{process::Stdio, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
    };
    let d = fixture();
    let a = archive(&d, "one.jsonl", b"KEEP_AFTER_CRASH");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "isolated_stage_child", "--nocapture"])
        .env("REMODEX_STAGE_TEST_HOME", d.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if let Some((_, id)) = line.split_once("STAGE_READY:") {
                break id.trim().to_owned();
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(Lease::acquire(d.path()),Err(e) if e.kind()==ErrorKind::WouldBlock));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let lease = Lease::acquire(d.path()).unwrap();
    let mut stage = lease.resume(&id).unwrap();
    stage.restore_files().unwrap();
    stage.finish_restore().unwrap();
    assert_eq!(fs::read(d.path().join(a)).unwrap(), b"KEEP_AFTER_CRASH");
}
