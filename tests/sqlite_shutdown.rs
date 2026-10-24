use rusqlite::Connection;
use std::{
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

// SQLite 3.51.1 could invert its unix inode/global mutexes when a WAL
// connection checkpointed on close while another connection closed. Keep the
// real concurrent-close regression in a subprocess so a recurrence fails
// boundedly instead of hanging the entire release suite.
#[test]
fn concurrent_wal_close_preserves_commits_without_hanging() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "wal_close_worker", "--ignored", "--nocapture"])
        .env("REMODEX_SQLITE_CLOSE_TEST", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "Concurrent WAL close failed: {status}");
            break;
        }
        if started.elapsed() > Duration::from_secs(45) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Concurrent WAL close exceeded 45 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "invoked by the bounded concurrent-close parent"]
fn wal_close_worker() {
    assert_eq!(
        std::env::var("REMODEX_SQLITE_CLOSE_TEST").as_deref(),
        Ok("1")
    );
    // Includes both the unix mutex-order fix and the WAL-reset corruption fix.
    assert!(rusqlite::version_number() >= 3_053_002);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE commits(id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    for id in 0..500 {
        let writer = Connection::open(&path).unwrap();
        writer
            .execute("INSERT INTO commits VALUES (?)", [id])
            .unwrap();
        let observer = Connection::open(&path).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let closing = barrier.clone();
        let owner = std::thread::spawn(move || {
            closing.wait();
            drop(writer);
        });
        barrier.wait();
        drop(observer);
        owner.join().unwrap();
    }
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM commits", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        500
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}
