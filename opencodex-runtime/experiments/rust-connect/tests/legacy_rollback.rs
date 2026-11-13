use remodex_connect_native::{
    auth, legacy_connection::Prepared, legacy_rollback, profile_lock, store::Store,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

const ENV: &str = "opencodex-0123456789abcdef01234567";
const TIME: &str = "2026-01-01T12:00:00.000Z";
const NATIVE: &str = "00000000-0000-4000-8000-000000000011";

fn setup() -> (tempfile::TempDir, Store, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let source = json!({"version":1,"settings":{"controlEnabled":true,"tunnelMode":"quick"},
        "clients":[{"id":"fixture-phone","label":"Phone","os":"android","deviceType":"mobile",
            "scopes":auth::SCOPES.split(' ').collect::<Vec<_>>(),"createdAt":TIME,
            "credentialDigest":auth::digest("ocx_android_fixture-only")}],
        "threadAliases":[{"remoteThreadId":"codex-chat","nativeThreadId":NATIVE,"projectId":"project",
            "title":"Codex history","cwd":dir.path(),"instanceId":"openai","model":"model",
            "nativeModelProvider":"openai","nativeReasoningEffort":"high","nativeServiceTier":null,
            "runtimeMode":"approval-required","interactionMode":"default","createdAt":TIME,"updatedAt":TIME}],
        "taskSelections":[]});
    let mut store = Store::open(dir.path()).unwrap();
    store
        .prepare_legacy_connection(&Prepared::parse(source.to_string().as_bytes(), ENV).unwrap())
        .unwrap();
    store
        .prepare_legacy_preferences(b"{\"enabled\":true,\"projects\":[]}", b"[]")
        .unwrap();
    store.prepare_legacy_codex().unwrap();
    for (id, native) in [
        ("claude-one", "00000000-0000-4000-8000-000000000021"),
        ("claude-two", "00000000-0000-4000-8000-000000000022"),
    ] {
        let input = json!({"version":1,"nativeId":native,"resumed":true,"thread":{
            "id":id,"nativeSessionId":native,"projectId":"project","title":"Original Claude chat",
            "worktreePath":dir.path(),"createdAt":TIME,"updatedAt":TIME,
            "modelSelection":{"instanceId":"claude-code","model":"sonnet"},
            "runtimeMode":"approval-required","interactionMode":"default",
            "session":{"threadId":id,"status":"ready","activeTurnId":null},
            "messages":[{"id":"user-message","role":"user","text":"Preserve this prompt", "createdAt":TIME,"turnId":"old-turn","sequence":1}],
            "activities":[],"proposedPlans":[]}});
        store
            .prepare_legacy_claude(input.to_string().as_bytes())
            .unwrap();
    }
    let db = Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    (dir, store, db)
}

// Small synthetic fixtures only. Compare all logical tables including retained
// receipts/storage, so a failed parent step cannot silently remove a child.
fn dump(db: &Connection) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let names: Vec<String> = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap().query_map([],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    names
        .into_iter()
        .map(|name| {
            let mut stmt = db
                .prepare(&format!(
                    "SELECT * FROM \"{}\" ORDER BY rowid",
                    name.replace('"', "\"\"")
                ))
                .unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| r.get(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (name, rows)
        })
        .collect()
}
fn phase(db: &Connection) -> String {
    db.query_row("SELECT phase FROM legacy_connection_import", [], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn preview_is_unchanged_and_apply_removes_all_imports_atomically_with_backups_retained() {
    let (dir, mut store, db) = setup();
    let before = dump(&db);
    let report = store.rollback_legacy_imports(false).unwrap();
    assert_eq!(report["phase"], "rollback-verified");
    assert_eq!(report["claudeReceipts"], 2);
    assert_eq!(report["codexReceipt"], true);
    assert_eq!(report["preferences"], true);
    assert_eq!(dump(&db), before);
    assert!(store.assert_migration_ready().is_err());
    let applied = store.rollback_legacy_imports(true).unwrap();
    assert_eq!(applied["phase"], "rolled-back");
    assert_eq!(phase(&db), "rolled-back");
    for table in ["chats", "chat_rows", "clients", "metadata"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM legacy_claude_import WHERE length(raw)>0 AND phase='rolled-back'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(store.assert_migration_ready().is_err());
    let after = dump(&db);
    drop(store);
    let mut reopened = Store::open(dir.path()).unwrap();
    assert_eq!(
        reopened.rollback_legacy_imports(true).unwrap()["alreadyRolledBack"],
        true
    );
    assert_eq!(dump(&db), after);
}

#[test]
fn failure_in_last_step_preserves_every_chat_setting_pairing_and_receipt() {
    for apply in [false, true] {
        for cause in ["trigger", "connection", "preferences", "foreign", "child"] {
            let (_dir, mut store, db) = setup();
            match cause {
                "trigger"=>db.execute_batch("CREATE TRIGGER stop_final BEFORE DELETE ON clients BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap(),
                "connection"=>{db.execute("UPDATE clients SET label='Keep this edit'",[]).unwrap();},
                "preferences"=>{db.execute("UPDATE metadata SET value='[]' WHERE key='native.claudeSettings'",[]).unwrap();},
                "foreign"=>db.execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('KEEP');").unwrap(),
                _=>{db.execute("UPDATE chats SET data=json_set(data,'$.thread.title','Changed chat') WHERE id='claude-two'",[]).unwrap();}
            }
            let before = dump(&db);
            assert!(store.rollback_legacy_imports(apply).is_err(), "{cause}");
            assert_eq!(dump(&db), before, "{cause}");
            assert_eq!(phase(&db), "prepared");
        }
    }
}

#[test]
fn partially_rolled_back_children_are_verified_and_finish_as_one_transaction() {
    let (_dir, mut store, db) = setup();
    store.rollback_legacy_claude("claude-one").unwrap();
    store.rollback_legacy_codex().unwrap();
    let before = dump(&db);
    store.rollback_legacy_imports(false).unwrap();
    assert_eq!(dump(&db), before);
    store.rollback_legacy_imports(true).unwrap();
    assert_eq!(phase(&db), "rolled-back");
}

#[test]
fn normal_missing_and_running_profiles_are_refused_before_mutating_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    assert!(legacy_rollback::run(&missing, true).is_err());
    assert!(!missing.exists());
    assert!(legacy_rollback::run(Path::new("relative"), true).is_err());
    let store = Store::open(dir.path()).unwrap();
    let original = std::fs::read(dir.path().join("native-connect.sqlite")).unwrap();
    assert!(legacy_rollback::run(dir.path(), true).is_err());
    assert!(!dir.path().join("server.lock").exists());
    assert_eq!(
        std::fs::read(dir.path().join("native-connect.sqlite")).unwrap(),
        original
    );
    drop(store);
    let (dir, _store, db) = setup();
    let lease = profile_lock::acquire(dir.path()).unwrap();
    let before = dump(&db);
    assert!(legacy_rollback::run(dir.path(), true).is_err());
    assert_eq!(dump(&db), before);
    drop(lease);
    assert_eq!(
        legacy_rollback::run(dir.path(), false).unwrap()["phase"],
        "rollback-verified"
    );
    assert_eq!(dump(&db), before);
}

#[test]
fn cli_check_and_apply_use_existing_offline_profile_without_printing_private_data() {
    let (dir, store, db) = setup();
    drop(store);
    let before = dump(&db);
    for action in ["check", "apply", "apply"] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let result = command
            .args(["--legacy-rollback", action])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["migrationComplete"], false);
        assert!(!String::from_utf8_lossy(&result.stdout).contains("fixture-phone"));
        if action == "check" {
            assert_eq!(dump(&db), before);
        }
    }
    assert_eq!(phase(&db), "rolled-back");
    assert!(Store::open(dir.path())
        .unwrap()
        .assert_migration_ready()
        .is_err());
}
