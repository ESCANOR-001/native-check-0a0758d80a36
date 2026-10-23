use remodex_connect_native::{legacy_connection::Prepared, store::Store};
use serde_json::{json, Value};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const NATIVE: &str = "00000000-0000-4000-8000-000000000011";
const SECOND: &str = "00000000-0000-4000-8000-000000000012";
const TIME: &str = "2026-01-01T12:00:00.000Z";
fn source(cwd: &std::path::Path) -> Value {
    json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],
        "threadAliases":[{"remoteThreadId":"original-android-id","nativeThreadId":NATIVE,"projectId":"original-project","title":"Original task","cwd":cwd,"instanceId":"openai","model":"old-model","nativeModelProvider":"openai","nativeReasoningEffort":"high","nativeServiceTier":null,"runtimeMode":"approval-required","interactionMode":"plan","createdAt":TIME,"updatedAt":TIME}],
        "taskSelections":[{"nativeThreadId":NATIVE,"remoteThreadId":"original-android-id","providerInstanceId":"openai","model":"selected-model","options":{"reasoningEffort":"low","serviceTier":"fast"},"revision":2,"source":"desktop","updateId":"original-update","updatedAt":TIME}],"futureSettings":{"retain":"exactly"}})
}
fn setup(source: &Value) -> (tempfile::TempDir, Store, rusqlite::Connection, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let raw = format!("  {source}\n").into_bytes();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .prepare_legacy_connection(&Prepared::parse(&raw, ENV).unwrap())
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    (dir, store, db, raw)
}
fn count(db: &rusqlite::Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn state(db: &rusqlite::Connection) -> Value {
    serde_json::from_str(
        &db.query_row(
            "SELECT data FROM chats WHERE id='original-android-id'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn aliases_and_selections_survive_restart_retry_and_rollback_without_native_start() {
    let cwd = tempfile::tempdir().unwrap();
    let input = source(cwd.path());
    let (dir, mut store, db, raw) = setup(&input);
    let report = store.prepare_legacy_codex().unwrap();
    assert_eq!(report["chats"], 1);
    assert_eq!(report["historyPending"], 1);
    assert_eq!(report["serviceTierPending"], 1);
    assert_eq!(report["migrationComplete"], false);
    let saved = state(&db);
    assert_eq!(saved["thread"]["id"], "original-android-id");
    assert_eq!(saved["native_id"], NATIVE);
    assert_eq!(saved["thread"]["nativeSessionId"], NATIVE);
    assert_eq!(saved["thread"]["projectId"], "original-project");
    assert_eq!(saved["thread"]["worktreePath"], json!(cwd.path()));
    assert_eq!(
        saved["thread"]["modelSelection"],
        json!({"instanceId":"codex","model":"selected-model","options":[{"id":"effort","value":"low"},{"id":"serviceTier","value":"fast"}]})
    );
    assert_eq!(saved["thread"]["interactionMode"], "plan");
    assert_eq!(saved["thread"]["runtimeMode"], "approval-required");
    assert_eq!(saved["thread"]["session"]["status"], "disconnected");
    assert_eq!(saved["paused"], true);
    for table in ["chat_rows", "native_turns", "native_sync", "chat_commands"] {
        assert_eq!(count(&db, table), 0);
    }
    assert!(store.assert_migration_ready().is_err());
    assert!(store.rollback_legacy_connection().is_err());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(store.prepare_legacy_codex().unwrap(), report);
    assert_eq!(state(&db), saved);
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_connection_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        raw
    );
    let rolled = store.rollback_legacy_codex().unwrap();
    assert_eq!(count(&db, "chats"), 0);
    assert_eq!(store.rollback_legacy_codex().unwrap(), rolled);
    assert!(store.prepare_legacy_codex().is_err()); // never implicitly reapply a rolled-back migration
    assert!(store.assert_migration_ready().is_err());
    store.rollback_legacy_connection().unwrap();
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_connection_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        raw
    );
}
#[test]
fn option_arrays_objects_and_alias_fallback_keep_effort_and_no_project() {
    let cwd = tempfile::tempdir().unwrap();
    for option in [
        json!({"effort":"high"}),
        json!([{"id":"reasoningEffort","value":"high"}]),
        json!({"effort":"high","reasoningEffort":"high"}),
        Value::Null,
    ] {
        let mut input = source(cwd.path());
        if option.is_null() {
            input["taskSelections"] = json!([]);
        } else {
            input["taskSelections"][0]["options"] = option;
        }
        input["threadAliases"][0]["projectId"] = json!("codex-project-chats");
        input["threadAliases"][0]["workspaceKind"] = json!("projectless");
        let (_dir, mut store, db, _) = setup(&input);
        store.prepare_legacy_codex().unwrap();
        let s = state(&db);
        assert_eq!(s["thread"]["workspaceKind"], "projectless");
        assert_eq!(
            s["thread"]["modelSelection"]["options"],
            json!([{"id":"effort","value":"high"}])
        );
    }
}
#[test]
fn unknown_or_ambiguous_records_refuse_the_whole_conversion() {
    let cwd = tempfile::tempdir().unwrap();
    for (path, value) in [
        ("/threadAliases/0/nativeThreadId", json!("not-native")),
        ("/threadAliases/0/cwd", json!("relative")),
        (
            "/threadAliases/0/nativeModelProvider",
            json!("custom-proxy"),
        ),
        ("/threadAliases/0/instanceId", json!("third-party")),
        ("/threadAliases/0/runtimeMode", json!("unknown")),
        ("/taskSelections/0/remoteThreadId", json!("different-chat")),
        ("/taskSelections/0/nativeThreadId", json!(SECOND)),
        ("/taskSelections/0/revision", json!(0)),
        (
            "/taskSelections/0/options",
            json!({"effort":"low","reasoningEffort":"high"}),
        ),
        ("/taskSelections/0/options", json!({"unknown":true})),
        ("/taskSelections/0/options", json!({"effort":true})),
        (
            "/taskSelections/0/options",
            json!([{"id":"effort","value":"high"},{"id":"effort","value":"high"}]),
        ),
    ] {
        let mut input = source(cwd.path());
        *input.pointer_mut(path).unwrap() = value;
        let (_dir, mut store, db, raw) = setup(&input);
        assert!(store.prepare_legacy_codex().is_err(), "{path}");
        store.init_chats().unwrap();
        assert_eq!(count(&db, "chats"), 0);
        assert_eq!(
            db.query_row("SELECT raw FROM legacy_connection_import", [], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            raw
        );
    }
}
#[test]
fn duplicate_mapping_and_late_destination_collision_leave_no_partial_import() {
    let cwd = tempfile::tempdir().unwrap();
    for duplicate in [true, false] {
        let mut input = source(cwd.path());
        let mut second = input["threadAliases"][0].clone();
        if !duplicate {
            second["nativeThreadId"] = json!(SECOND);
            second["remoteThreadId"] = json!("second-chat");
        }
        input["threadAliases"].as_array_mut().unwrap().push(second);
        let (_dir, mut store, db, _) = setup(&input);
        store.init_chats().unwrap();
        if !duplicate {
            db.execute("INSERT INTO chats VALUES('second-chat','codex','{}')", [])
                .unwrap();
        }
        assert!(store.prepare_legacy_codex().is_err());
        assert_eq!(count(&db, "chats"), i64::from(!duplicate));
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM chats WHERE id='original-android-id'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
#[test]
fn destination_changes_block_retry_and_rollback_without_losing_new_data() {
    let cwd = tempfile::tempdir().unwrap();
    for mutation in [
        "UPDATE chats SET data=json_set(data,'$.thread.title','new title')",
        "INSERT INTO chat_rows(chat,kind,id,data) VALUES('original-android-id','messages','new-message','{}')",
        "INSERT INTO native_turns VALUES('original-android-id','new-turn','new-native')",
        "INSERT INTO chat_revisions VALUES('new-revision','original-android-id','{}')",
        "INSERT INTO native_tool_actions VALUES('action','digest','{}')",
        "INSERT INTO native_worktrees VALUES('worktree','digest','{}')",
    ] {
        let (_dir,mut store,db,_)=setup(&source(cwd.path())); store.prepare_legacy_codex().unwrap();
        db.execute_batch(mutation).unwrap();
        assert!(store.prepare_legacy_codex().is_err(),"{mutation}");
        assert!(store.rollback_legacy_codex().is_err(),"{mutation}"); assert_eq!(count(&db,"chats"),1);
    }
}
#[test]
fn corrupted_receipts_and_recreated_rows_block_rollback() {
    let cwd = tempfile::tempdir().unwrap();
    for sql in [
        "UPDATE legacy_codex_import SET data='[]'",
        "UPDATE legacy_connection_import SET raw=x'7b7d'",
        "UPDATE legacy_codex_import SET digest='changed'",
    ] {
        let (_dir, mut store, db, _) = setup(&source(cwd.path()));
        store.prepare_legacy_codex().unwrap();
        db.execute_batch(sql).unwrap();
        assert!(store.rollback_legacy_codex().is_err());
        assert_eq!(count(&db, "chats"), 1);
    }
    let (_dir, mut store, db, _) = setup(&source(cwd.path()));
    store.prepare_legacy_codex().unwrap();
    store.rollback_legacy_codex().unwrap();
    db.execute(
        "INSERT INTO chat_revisions VALUES('late','original-android-id','{}')",
        [],
    )
    .unwrap();
    assert!(store.rollback_legacy_codex().is_err());
}
