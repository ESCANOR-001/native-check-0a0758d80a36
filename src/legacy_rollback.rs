//! One offline transaction for all imported connection, chat and settings data.
//! Uses the same validators as individual rollback. Recovery files are retained;
//! no agent, source profile, service or network connection is started or stopped.
use crate::{legacy_codex_headers::table, store::Store};
use rusqlite::TransactionBehavior;
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const INVALID: &str = "Cannot roll back this offline migration; no imported data was removed";
const MAX_CHATS: usize = 4096;

fn present(db: &rusqlite::Connection, name: &str) -> Result<bool, String> {
    if !table(db, name)? {
        return Ok(false);
    }
    // Names are fixed by the caller, never supplied by an exported record.
    db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {name})"), [], |r| {
        r.get(0)
    })
    .map_err(|_| INVALID.into())
}

impl Store {
    /// Preview executes the complete rollback in a SQLite transaction and then
    /// rolls it back. Apply commits that same sequence atomically. Failure or a
    /// process exit cannot leave half the chats removed. Call only offline.
    pub fn rollback_legacy_imports(&mut self, apply: bool) -> Result<Value, String> {
        let _native_lease = crate::legacy_activation::rollback_lease(&self.db)?;
        let deadline = Instant::now() + Duration::from_secs(300);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| INVALID)?;
        let phase: String = tx
            .query_row(
                "SELECT phase FROM legacy_connection_import WHERE id=1",
                [],
                |r| r.get(0),
            )
            .map_err(|_| INVALID)?;
        let mut claude = 0usize;
        let mut codex = false;
        let mut headers = false;
        let mut preferences = false;
        if phase == "prepared" {
            if table(&tx, "legacy_claude_import")? {
                let mut stmt = tx
                    .prepare("SELECT id FROM legacy_claude_import ORDER BY id LIMIT 4097")
                    .map_err(|_| INVALID)?;
                let ids = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|_| INVALID)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|_| INVALID)?;
                if ids.len() > MAX_CHATS || ids.iter().any(|id| id.is_empty() || id.len() > 128) {
                    return Err(INVALID.into());
                }
                drop(stmt);
                for id in &ids {
                    if Instant::now() >= deadline {
                        return Err(INVALID.into());
                    }
                    crate::legacy_claude::rollback(&tx, &self.directory, id)?;
                }
                claude = ids.len();
            }
            codex = present(&tx, "legacy_codex_import")?;
            if codex {
                crate::legacy_codex::rollback(&tx, &self.directory)?;
            }
            headers = present(&tx, "legacy_codex_headers")?;
            if headers {
                crate::legacy_codex_headers::rollback(&tx)?;
            }
            preferences = present(&tx, "legacy_preferences_import")?;
            if preferences {
                crate::legacy_preferences::rollback(&tx)?;
            }
        } else if phase != "rolled-back" {
            return Err(INVALID.into());
        }
        // Parent verification also rejects any unrelated state, active child
        // receipt, changed copied asset or storage charge. It runs last while
        // every preceding change is still uncommitted.
        if table(&tx, "legacy_activation")? {
            tx.execute_batch("DROP TABLE legacy_activation;")
                .map_err(|_| INVALID)?;
        }
        crate::legacy_connection::rollback(&tx, &self.directory)?;
        if Instant::now() >= deadline {
            return Err(INVALID.into());
        }
        if apply {
            tx.commit().map_err(|_| INVALID)?;
        } else {
            tx.rollback().map_err(|_| INVALID)?;
        }
        Ok(
            json!({"phase":if apply {"rolled-back"} else {"rollback-verified"},
            "alreadyRolledBack":phase=="rolled-back", "claudeReceipts":claude,
            "codexReceipt":codex,"codexHeaders":headers,"preferences":preferences,
            "backupRetained":true,"sourceUnchanged":true,"runtimeStarted":false,
            "migrationComplete":false}),
        )
    }
}

/// CLI entry point: refuse a normal or nonexistent profile without initializing
/// schemas or changing its storage index. The isolated server's
/// exclusive lease stays held through verification and the transaction.
pub fn run(directory: &Path, apply: bool) -> Result<Value, String> {
    if !directory.is_absolute() {
        return Err(INVALID.into());
    }
    let file = directory.join("native-connect.sqlite");
    for (path, is_dir) in [(directory, true), (file.as_path(), false)] {
        let meta = std::fs::symlink_metadata(path).map_err(|_| INVALID)?;
        if !crate::profile_snapshot::regular(&meta)
            || (is_dir && !meta.is_dir())
            || (!is_dir && !meta.is_file())
        {
            return Err(INVALID.into());
        }
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        match std::fs::symlink_metadata(directory.join(format!("native-connect.sqlite{suffix}"))) {
            Ok(meta) if meta.is_file() && crate::profile_snapshot::regular(&meta) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(INVALID.into()),
        }
    }
    let db =
        rusqlite::Connection::open_with_flags(&file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| INVALID)?;
    if !present(&db, "legacy_connection_import")? {
        return Err(INVALID.into());
    }
    drop(db);
    let _lease = crate::profile_lock::acquire(directory)?;
    // Do not use Store::open: its startup storage repair would modify a preview
    // and could conceal a changed asset ledger before rollback validates it.
    let db =
        rusqlite::Connection::open_with_flags(&file, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|_| INVALID)?;
    db.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| INVALID)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA cache_size=-2048;")
        .map_err(|_| INVALID)?;
    let mut store = Store {
        db,
        directory: directory.to_owned(),
        storage: Default::default(),
    };
    store.rollback_legacy_imports(apply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_connection::Prepared;
    const ENV: &str = "opencodex-0123456789abcdef01234567";
    const TIME: &str = "2026-01-01T12:00:00Z";

    #[test]
    fn rollback_crash_child() {
        let Some(path) = std::env::var_os("REMODEX_ROLLBACK_CRASH_FIXTURE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        assert_eq!(
            std::fs::read(path.join("test-owned")).unwrap(),
            b"ROLLBACK_FIXTURE"
        );
        let mut store = Store::open(&path).unwrap();
        // Pause only after all child/parent changes, immediately before the
        // outer COMMIT. The parent kills this disposable helper at that point.
        store
            .db
            .commit_hook(Some(move || {
                std::fs::write(path.join("commit-reached"), b"READY").unwrap();
                std::thread::sleep(Duration::from_secs(30));
                true
            }))
            .unwrap();
        assert!(store.rollback_legacy_imports(true).is_err());
    }

    #[test]
    fn process_exit_before_commit_keeps_children_and_connection_then_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test-owned"), b"ROLLBACK_FIXTURE").unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let input = json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],"threadAliases":[],"taskSelections":[]});
        store
            .prepare_legacy_connection(&Prepared::parse(input.to_string().as_bytes(), ENV).unwrap())
            .unwrap();
        store
            .prepare_legacy_preferences(b"{\"projects\":[]}", b"[]")
            .unwrap();
        for (id, native) in [
            ("one", "00000000-0000-4000-8000-000000000021"),
            ("two", "00000000-0000-4000-8000-000000000022"),
        ] {
            let input = json!({"version":1,"nativeId":native,"thread":{"id":id,"projectId":"project","title":"Retained history","worktreePath":dir.path(),"modelSelection":{"instanceId":"claude-code","model":"sonnet"},"runtimeMode":"approval-required","interactionMode":"default","session":{"threadId":id,"status":"ready"},"messages":[{"id":"prompt","role":"user","text":"KEEP THIS","createdAt":TIME,"sequence":1}],"activities":[],"proposedPlans":[],"createdAt":TIME,"updatedAt":TIME}});
            store
                .prepare_legacy_claude(input.to_string().as_bytes())
                .unwrap();
        }
        let before: Vec<(String, String)> = store
            .db
            .prepare("SELECT id,data FROM chats ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        drop(store);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .args([
                "--exact",
                "legacy_rollback::tests::rollback_crash_child",
                "--nocapture",
            ])
            .env("REMODEX_ROLLBACK_CRASH_FIXTURE", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !dir.path().join("commit-reached").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("rollback helper did not reach its commit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let after: Vec<(String, String)> = store
            .db
            .prepare("SELECT id,data FROM chats ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            store
                .db
                .query_row("SELECT phase FROM legacy_connection_import", [], |r| r
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "prepared"
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chat_rows", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        store.rollback_legacy_imports(true).unwrap();
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chats", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(store.assert_migration_ready().is_err());
    }
}
