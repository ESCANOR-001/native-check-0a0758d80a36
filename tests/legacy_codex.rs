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

fn desktop_header(cwd: &std::path::Path) -> Value {
    json!({"version":1,"futureExportData":{"retain":"raw"},"headers":[{
        "alias":source(cwd)["threadAliases"][0],
        "native":{"id":NATIVE,"cwd":cwd,"modelProvider":"openai","status":{"type":"idle"}},
        "activeTurnId":null,"hasPendingApprovals":false,"hasPendingUserInput":false,
        "hasActionableProposedPlan":false,"hasQueuedPrompts":false,
        "archivedAt":TIME,"deletedAt":null,"branch":"retained-branch"
    }]})
}
fn desktop_source(cwd: &std::path::Path) -> Value {
    let mut v = source(cwd);
    v["threadAliases"] = json!([]);
    v
}
#[test]
fn desktop_only_headers_keep_public_identity_settings_and_exact_exports() {
    let cwd = tempfile::tempdir().unwrap();
    let input = desktop_source(cwd.path());
    let (dir, mut store, db, original) = setup(&input);
    assert!(store.prepare_legacy_codex().is_err());
    let headers = format!("  {}\n", desktop_header(cwd.path())).into_bytes();
    let staged = store.prepare_legacy_codex_headers(&headers).unwrap();
    assert_eq!(staged["chats"], 1);
    assert_eq!(count(&db, "chats"), 0);
    assert!(store.assert_migration_ready().is_err());
    let report = store.prepare_legacy_codex().unwrap();
    let saved = state(&db);
    assert_eq!(saved["thread"]["id"], "original-android-id");
    assert_eq!(saved["native_id"], NATIVE);
    assert_eq!(saved["thread"]["projectId"], "original-project");
    assert_eq!(saved["thread"]["branch"], "retained-branch");
    assert_eq!(saved["thread"]["archivedAt"], TIME);
    assert_eq!(saved["thread"]["deletedAt"], Value::Null);
    assert_eq!(saved["thread"]["modelSelection"]["model"], "selected-model");
    assert_eq!(
        saved["thread"]["modelSelection"]["options"],
        json!([{"id":"effort","value":"low"},{"id":"serviceTier","value":"fast"}])
    );
    assert_eq!(saved["paused"], true);
    assert!(store.rollback_legacy_codex_headers().is_err());
    assert!(store.rollback_legacy_connection().is_err());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.prepare_legacy_codex_headers(&headers).unwrap(),
        staged
    );
    assert_eq!(store.prepare_legacy_codex().unwrap(), report);
    assert_eq!(state(&db), saved);
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    assert!(store.prepare_legacy_codex_headers(&headers).is_err());
    assert!(store.prepare_legacy_codex().is_err());
    store.rollback_legacy_connection().unwrap();
    store.rollback_legacy_connection().unwrap();
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_connection_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        original
    );
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_codex_headers", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        headers
    );
}
#[test]
fn headers_refuse_live_ambiguous_foreign_or_incomplete_metadata_atomically() {
    let cwd = tempfile::tempdir().unwrap();
    for (path, value) in [
        ("/headers/0/native/id", json!(SECOND)),
        ("/headers/0/native/cwd", json!("other")),
        ("/headers/0/native/modelProvider", json!("other-provider")),
        ("/headers/0/native/status/type", json!("active")),
        ("/headers/0/activeTurnId", json!("running")),
        ("/headers/0/hasPendingApprovals", json!(true)),
        ("/headers/0/hasPendingUserInput", json!(true)),
        ("/headers/0/hasQueuedPrompts", json!(true)),
        ("/headers/0/hasActionableProposedPlan", json!(true)),
        ("/headers/0/alias/model", json!("")),
        ("/headers/0/alias/runtimeMode", json!("guess")),
        ("/headers/0/alias/remoteThreadId", json!("different")),
        ("/headers/0/archivedAt", json!("unknown-date")),
        ("/headers/0/branch", json!("bad\nbranch")),
    ] {
        let (_dir, mut store, db, _) = setup(&desktop_source(cwd.path()));
        let mut header = desktop_header(cwd.path());
        *header.pointer_mut(path).unwrap() = value;
        assert!(
            store
                .prepare_legacy_codex_headers(header.to_string().as_bytes())
                .is_err(),
            "{path}"
        );
        assert!(store.prepare_legacy_codex().is_err());
        store.init_chats().unwrap();
        assert_eq!(count(&db, "chats"), 0);
        if db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='legacy_codex_headers')",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        {
            assert_eq!(count(&db, "legacy_codex_headers"), 0);
        }
    }
    for key in [
        "activeTurnId",
        "hasQueuedPrompts",
        "archivedAt",
        "deletedAt",
        "branch",
    ] {
        let (_dir, mut store, _, _) = setup(&desktop_source(cwd.path()));
        let mut header = desktop_header(cwd.path());
        header["headers"][0].as_object_mut().unwrap().remove(key);
        assert!(
            store
                .prepare_legacy_codex_headers(header.to_string().as_bytes())
                .is_err(),
            "{key}"
        );
    }
}
#[test]
fn headers_cannot_replace_existing_aliases_or_ignore_other_selections() {
    let cwd = tempfile::tempdir().unwrap();
    let header = desktop_header(cwd.path());
    let (_dir, mut store, _, _) = setup(&source(cwd.path()));
    assert!(store
        .prepare_legacy_codex_headers(header.to_string().as_bytes())
        .is_err());
    let mut duplicate = header.clone();
    duplicate["headers"]
        .as_array_mut()
        .unwrap()
        .push(header["headers"][0].clone());
    let (_dir, mut store, _, _) = setup(&desktop_source(cwd.path()));
    assert!(store
        .prepare_legacy_codex_headers(duplicate.to_string().as_bytes())
        .is_err());
    let mut input = desktop_source(cwd.path());
    let mut selection = input["taskSelections"][0].clone();
    selection["nativeThreadId"] = json!(SECOND);
    selection["remoteThreadId"] = json!("second-public");
    input["taskSelections"]
        .as_array_mut()
        .unwrap()
        .push(selection);
    let (_dir, mut store, db, _) = setup(&input);
    assert!(store
        .prepare_legacy_codex_headers(header.to_string().as_bytes())
        .is_err());
    assert_eq!(count(&db, "legacy_codex_headers"), 0);
    assert_eq!(count(&db, "chats"), 0);
}
#[test]
fn partial_header_write_retries_and_changed_receipts_cannot_import_or_rollback() {
    let cwd = tempfile::tempdir().unwrap();
    let headers = desktop_header(cwd.path()).to_string();
    let (_dir, mut store, db, _) = setup(&desktop_source(cwd.path()));
    // Create the empty schema through a combined-validation failure.
    let mut bad = desktop_header(cwd.path());
    bad["headers"][0]["alias"]["title"] = json!("");
    assert!(store
        .prepare_legacy_codex_headers(bad.to_string().as_bytes())
        .is_err());
    db.execute_batch("CREATE TRIGGER fail_headers BEFORE INSERT ON legacy_codex_headers BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(store
        .prepare_legacy_codex_headers(headers.as_bytes())
        .is_err());
    assert_eq!(count(&db, "legacy_codex_headers"), 0);
    db.execute_batch("DROP TRIGGER fail_headers").unwrap();
    store
        .prepare_legacy_codex_headers(headers.as_bytes())
        .unwrap();
    store.prepare_legacy_codex().unwrap();
    let original: (String, String, Vec<u8>) = db
        .query_row(
            "SELECT source_hash,digest,raw FROM legacy_codex_headers",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    for sql in [
        "UPDATE legacy_codex_headers SET digest='changed'",
        "UPDATE legacy_codex_headers SET source_hash='changed'",
        "UPDATE legacy_codex_headers SET raw=x'7b7d'",
    ] {
        db.execute_batch(sql).unwrap();
        assert!(store.prepare_legacy_codex().is_err(), "{sql}");
        assert!(store.rollback_legacy_codex().is_err());
        assert!(store.rollback_legacy_codex_headers().is_err());
        assert_eq!(count(&db, "chats"), 1);
        db.execute(
            "UPDATE legacy_codex_headers SET source_hash=?1,digest=?2,raw=?3",
            rusqlite::params![original.0, original.1, original.2],
        )
        .unwrap();
    }
}
#[test]
fn all_desktop_headers_and_existing_aliases_can_be_staged_together_without_selections() {
    let cwd = tempfile::tempdir().unwrap();
    let mut input = source(cwd.path());
    input["taskSelections"] = json!([]);
    let (_dir, mut store, db, _) = setup(&input);
    let mut header = desktop_header(cwd.path());
    let row = &mut header["headers"][0];
    row["alias"]["nativeThreadId"] = json!(SECOND);
    row["native"]["id"] = json!(SECOND);
    row["alias"]["remoteThreadId"] = json!(SECOND);
    row["alias"]["projectId"] = json!("codex-project-chats");
    row["alias"]["workspaceKind"] = json!("projectless");
    row["native"]["status"]["type"] = json!("notLoaded");
    row["deletedAt"] = json!(TIME);
    store
        .prepare_legacy_codex_headers(header.to_string().as_bytes())
        .unwrap();
    assert_eq!(store.prepare_legacy_codex().unwrap()["chats"], 2);
    let data: String = db
        .query_row("SELECT data FROM chats WHERE id=?1", [SECOND], |r| r.get(0))
        .unwrap();
    let saved: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(saved["thread"]["workspaceKind"], "projectless");
    assert_eq!(saved["thread"]["deletedAt"], TIME);
    assert_eq!(saved["thread"]["modelSelection"]["model"], "old-model");
    for table in ["chat_rows", "native_turns", "chat_commands"] {
        assert_eq!(count(&db, table), 0);
    }
    assert!(store.assert_migration_ready().is_err());
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    store.rollback_legacy_connection().unwrap();
}
#[test]
fn staged_headers_can_roll_back_without_conversion_and_cannot_be_added_late() {
    let cwd = tempfile::tempdir().unwrap();
    let header = desktop_header(cwd.path()).to_string();
    let (_dir, mut store, db, _) = setup(&desktop_source(cwd.path()));
    store
        .prepare_legacy_codex_headers(header.as_bytes())
        .unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    assert!(store.prepare_legacy_codex().is_err());
    store.rollback_legacy_connection().unwrap();
    assert_eq!(count(&db, "chats"), 0);
    let mut input = desktop_source(cwd.path());
    input["taskSelections"] = json!([]);
    let (_dir, mut store, db, _) = setup(&input);
    store.prepare_legacy_codex().unwrap();
    assert!(store
        .prepare_legacy_codex_headers(header.as_bytes())
        .is_err());
    assert_eq!(count(&db, "chats"), 0);
}

fn history() -> Value {
    json!({"version":1,"threadId":"original-android-id","nativeId":NATIVE,"nativeStatus":"idle",
        "hasPendingApprovals":false,"hasPendingUserInput":false,"hasQueuedPrompts":false,"hasActionableProposedPlan":false,
        "updatedAt":TIME,"latestUserMessageAt":TIME,"futureField":{"preserved":true},
        "latestTurn":{"turnId":"old-turn","state":"completed","requestedAt":TIME,"startedAt":TIME,"completedAt":TIME,"assistantMessageId":"old-reply"},
        "turns":[{"publicId":"old-turn","nativeId":"native-old-turn","status":"completed"}],
        "messages":[{"id":"old-prompt","turnId":"old-turn","role":"user","text":"Original prompt","streaming":false,"sequence":1,"createdAt":TIME,"updatedAt":TIME},
            {"id":"old-reply","turnId":"old-turn","role":"assistant","text":"Original reply","streaming":false,"sequence":3,"createdAt":TIME,"updatedAt":TIME}],
        "activities":[{"id":"old-tool","turnId":"old-turn","kind":"tool.completed","summary":"Read saved file","payload":{"status":"completed","unknown":{"kept":true}},"sequence":2,"createdAt":TIME}],"proposedPlans":[]})
}
#[test]
fn codex_transcript_rows_turns_and_raw_bytes_survive_reopen_retry_and_whole_rollback() {
    let cwd = tempfile::tempdir().unwrap();
    let (dir, mut store, db, original) = setup(&source(cwd.path()));
    store.prepare_legacy_codex().unwrap();
    let raw = format!("  {}\n", history()).into_bytes();
    let before = state(&db);
    let report = store
        .prepare_legacy_codex_history("original-android-id", &raw)
        .unwrap();
    assert_eq!(report["rows"], 3);
    assert_eq!(report["turns"], 1);
    assert_eq!(report["migrationComplete"], false);
    let page = store
        .history_page("original-android-id", None, None)
        .unwrap();
    assert_eq!(page["messages"], history()["messages"]);
    assert_eq!(page["activities"], history()["activities"]);
    assert_eq!(
        state(&db)["thread"]["modelSelection"],
        before["thread"]["modelSelection"]
    );
    assert_eq!(state(&db)["thread"]["latestTurn"], history()["latestTurn"]);
    assert_eq!(state(&db)["thread"]["session"]["status"], "disconnected");
    assert_eq!(state(&db)["paused"], true);
    let mut q = db.prepare("SELECT id FROM chat_rows ORDER BY seq").unwrap();
    let order: Vec<String> = q
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(order, vec!["old-prompt", "old-tool", "old-reply"]);
    assert!(store.assert_migration_ready().is_err());
    assert!(store.rollback_legacy_connection().is_err());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .prepare_legacy_codex_history("original-android-id", &raw)
            .unwrap(),
        report
    );
    store.prepare_legacy_codex().unwrap();
    assert_eq!(count(&db, "chat_rows"), 3);
    assert_eq!(count(&db, "native_sync"), 0);
    assert_eq!(count(&db, "chat_commands"), 0);
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_connection().unwrap();
    for table in ["chats", "chat_rows", "native_turns"] {
        assert_eq!(count(&db, table), 0);
    }
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_codex_history", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        raw
    );
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_connection_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        original
    );
}
#[test]
fn codex_history_rejects_unsettled_or_conflicting_exports_without_partial_rows() {
    let cwd = tempfile::tempdir().unwrap();
    for (path, value) in [
        ("/threadId", json!("wrong-chat")),
        ("/nativeId", json!(SECOND)),
        ("/nativeStatus", json!("active")),
        ("/hasPendingApprovals", json!(true)),
        ("/hasPendingUserInput", json!(true)),
        ("/hasQueuedPrompts", json!(true)),
        ("/hasActionableProposedPlan", json!(true)),
        ("/messages/0/turnId", json!("unknown-turn")),
        ("/messages/0/role", json!("system")),
        ("/messages/0/streaming", json!(true)),
        ("/latestTurn/state", json!("running")),
        ("/latestTurn/assistantMessageId", json!("other-reply")),
        ("/turns/0/status", json!("running")),
        ("/activities/0/createdAt", json!("no-date")),
        ("/updatedAt", json!("2000-01-01T00:00:00Z")),
    ] {
        let (_dir, mut store, db, _) = setup(&source(cwd.path()));
        store.prepare_legacy_codex().unwrap();
        let before = state(&db);
        let mut raw = history();
        *raw.pointer_mut(path).unwrap() = value;
        assert!(
            store
                .prepare_legacy_codex_history("original-android-id", raw.to_string().as_bytes())
                .is_err(),
            "{path}"
        );
        assert_eq!(state(&db), before);
        assert_eq!(count(&db, "chat_rows"), 0);
        assert_eq!(count(&db, "native_turns"), 0);
    }
    let (_dir, mut store, db, _) = setup(&source(cwd.path()));
    store.prepare_legacy_codex().unwrap();
    let mut raw = history();
    let dup = raw["messages"][0].clone();
    raw["messages"].as_array_mut().unwrap().push(dup);
    assert!(store
        .prepare_legacy_codex_history("original-android-id", raw.to_string().as_bytes())
        .is_err());
    assert_eq!(count(&db, "chat_rows"), 0);
}
#[test]
fn codex_history_write_failure_rolls_back_header_rows_and_turn_mapping() {
    let cwd = tempfile::tempdir().unwrap();
    let (_dir, mut store, db, _) = setup(&source(cwd.path()));
    store.prepare_legacy_codex().unwrap();
    let before = state(&db);
    db.execute_batch("CREATE TRIGGER fail_last BEFORE INSERT ON chat_rows WHEN NEW.id='old-reply' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let raw = history().to_string();
    assert!(store
        .prepare_legacy_codex_history("original-android-id", raw.as_bytes())
        .is_err());
    assert_eq!(state(&db), before);
    for table in ["chat_rows", "native_turns", "legacy_codex_history"] {
        assert_eq!(count(&db, table), 0);
    }
    db.execute_batch("DROP TRIGGER fail_last").unwrap();
    store
        .prepare_legacy_codex_history("original-android-id", raw.as_bytes())
        .unwrap();
    assert_eq!(count(&db, "chat_rows"), 3);
}
#[test]
fn changed_transcript_receipts_rows_or_turns_block_rollback_and_keep_new_data() {
    let cwd = tempfile::tempdir().unwrap();
    for sql in [
        "UPDATE legacy_codex_history SET digest='changed'",
        "UPDATE legacy_codex_history SET source_hash='changed'",
        "UPDATE legacy_codex_history SET raw=x'7b7d'",
        "UPDATE chat_rows SET data=json_set(data,'$.text','changed') WHERE kind='messages'",
        "UPDATE chat_rows SET turn='different' WHERE id='old-prompt'",
        "INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES('original-android-id','messages','new','new-turn','{}')",
        "UPDATE native_turns SET native='different'",
        "UPDATE chats SET data=json_set(data,'$.thread.title','changed')",
    ]{
        let (_dir,mut store,db,_)=setup(&source(cwd.path()));store.prepare_legacy_codex().unwrap();let raw=history().to_string();store.prepare_legacy_codex_history("original-android-id",raw.as_bytes()).unwrap();
        db.execute_batch(sql).unwrap();assert!(store.prepare_legacy_codex_history("original-android-id",raw.as_bytes()).is_err(),"{sql}");
        assert!(store.prepare_legacy_codex().is_err(),"{sql}");assert!(store.rollback_legacy_codex().is_err(),"{sql}");assert_eq!(count(&db,"chats"),1);
    }
}
#[test]
fn desktop_header_transcript_migration_pages_history_and_orders_restarted_sequences() {
    let cwd = tempfile::tempdir().unwrap();
    let (_dir, mut store, db, _) = setup(&desktop_source(cwd.path()));
    store
        .prepare_legacy_codex_headers(desktop_header(cwd.path()).to_string().as_bytes())
        .unwrap();
    store.prepare_legacy_codex().unwrap();
    let mut raw = history();
    for n in 0..28 {
        raw["messages"].as_array_mut().unwrap().push(json!({"id":format!("later-{n}"),"role":"assistant","text":format!("Later reply {n}"),"turnId":"old-turn","sequence":n,"createdAt":"2026-01-01T13:00:00Z","streaming":false}));
    }
    raw["updatedAt"] = json!("2026-01-01T13:00:00Z");
    store
        .prepare_legacy_codex_history("original-android-id", raw.to_string().as_bytes())
        .unwrap();
    let newest = store
        .history_page("original-android-id", None, None)
        .unwrap();
    assert_eq!(newest["messages"].as_array().unwrap().len(), 24);
    assert_eq!(
        newest["messages"].as_array().unwrap().last().unwrap()["id"],
        "later-27"
    );
    assert_eq!(count(&db, "chat_rows"), 31);
    store.rollback_legacy_codex().unwrap();
    store.rollback_legacy_codex_headers().unwrap();
    store.rollback_legacy_connection().unwrap();
}
