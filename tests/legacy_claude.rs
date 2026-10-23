use remodex_connect_native::{legacy_connection::Prepared, store::Store};
use serde_json::{json, Value};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const NATIVE: &str = "00000000-0000-4000-8000-000000000001";
const TIME: &str = "2026-01-01T12:00:00.000Z";
fn setup() -> (tempfile::TempDir, Store, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let source = json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],"threadAliases":[],"taskSelections":[]});
    store
        .prepare_legacy_connection(&Prepared::parse(source.to_string().as_bytes(), ENV).unwrap())
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    (dir, store, db)
}
fn source(dir: &std::path::Path) -> Value {
    let messages = json!([
        {"id":"user-message","role":"user","text":"Original prompt","createdAt":TIME,"turnId":"old-turn","sequence":50,"streaming":false,"attachments":[]},
        {"id":"assistant-message","role":"assistant","text":"Original reply","createdAt":TIME,"turnId":"old-turn","sequence":53,"streaming":false}
    ]);
    let tool = json!({"id":"tool-row","kind":"tool.completed","createdAt":TIME,"turnId":"old-turn","sequence":52,"payload":{"status":"completed","data":{"inputDetail":"Read example.txt","toolImages":[{"path":"/fixture/preserved.png"}],"changes":[{"patch":"original patch"}]}}});
    let activities = json!([tool,
        {"id":"summary-row","kind":"reasoning.updated","createdAt":TIME,"turnId":"old-turn","sequence":51,"payload":{"text":"Public thinking summary"}},
        {"id":"old-link-status","kind":"claude.desktop-sync.updated","createdAt":TIME,"sequence":54,"payload":{"status":"connected"}}
    ]);
    let thread = json!({
        "id":"original-android-chat","nativeSessionId":NATIVE,"projectId":"original-project","title":"Original title","worktreePath":dir,
        "modelSelection":{"instanceId":"claude-code","model":"sonnet","options":[{"id":"effort","value":"high"}]},"runtimeMode":"approval-required","interactionMode":"plan","createdAt":TIME,"updatedAt":TIME,"archivedAt":null,"deletedAt":null,
        "latestTurn":{"turnId":"old-turn","state":"completed","startedAt":TIME,"completedAt":TIME},
        "session":{"threadId":"original-android-chat","status":"ready","activeTurnId":null},"hasPendingApprovals":false,"hasPendingUserInput":false,
        "messages":messages,"activities":activities,"proposedPlans":[]
    });
    json!({"version":1,"nativeId":NATIVE,"resumed":true,"bridgeId":"cse_legacy_fixture","commands":["fixture-client:old-command"],"userUuids":["old-native-message"],"remoteSequence":98,"remoteEventUuids":["keep-in-original-backup"],"fileCheckpoints":[{"id":"old-file-point","label":"Before change"}],"futureMetadata":{"neverLose":"unknown original field"},"thread":thread})
}
fn raw(value: &Value) -> Vec<u8> {
    format!("  {}\n", value).into_bytes()
}
fn count(db: &rusqlite::Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn imports_exact_messages_tools_choices_identity_and_receipts_with_a_startup_barrier() {
    for external in [false, true] {
        let (dir, mut store, db) = setup();
        let mut original = source(dir.path());
        original["external"] = json!(external);
        let bytes = raw(&original);
        let report = store.prepare_legacy_claude(&bytes).unwrap();
        assert_eq!(report["messages"], 2);
        assert_eq!(report["migrationComplete"], false);
        assert!(!report.to_string().contains("Original prompt"));
        assert_eq!(store.prepare_legacy_claude(&bytes).unwrap(), report);
        assert!(store.assert_migration_ready().is_err());
        let header: Value = serde_json::from_str(
            &db.query_row("SELECT data FROM chats", [], |r| r.get::<_, String>(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(header["native_id"], NATIVE);
        assert_eq!(header["external_claude"], external);
        assert_eq!(
            header["remote_identity"]["bridge_session"],
            "cse_legacy_fixture"
        );
        assert_eq!(
            header["thread"]["modelSelection"],
            original["thread"]["modelSelection"]
        );
        assert_eq!(header["thread"]["runtimeMode"], "approval-required");
        assert_eq!(header["claude_plan_access"], "approval-required");
        assert_eq!(header["paused"], true);
        assert_eq!(header["queue"], json!([]));
        assert!(header["pending_message"].is_null());
        assert_eq!(header["thread"]["session"]["status"], "disconnected");
        for row in original["thread"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .chain(
                original["thread"]["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|r| r["kind"] != "claude.desktop-sync.updated"),
            )
        {
            let found: String = db
                .query_row(
                    "SELECT data FROM chat_rows WHERE id=?1",
                    [row["id"].as_str().unwrap()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(&found).unwrap(), *row);
        }
        let mut q = db.prepare("SELECT id FROM chat_rows ORDER BY seq").unwrap();
        let ids: Vec<String> = q
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec![
                "user-message",
                "summary-row",
                "tool-row",
                "assistant-message",
                "claude-native-sync"
            ]
        );
        let backup: Vec<u8> = db
            .query_row("SELECT raw FROM legacy_claude_import", [], |r| r.get(0))
            .unwrap();
        assert_eq!(backup, bytes);
        assert!(store
            .command_digest("claude-code:fixture-client:old-command")
            .unwrap()
            .is_some());
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        assert_eq!(store.prepare_legacy_claude(&bytes).unwrap(), report);
        assert!(store.assert_migration_ready().is_err());
    }
}
#[test]
fn transaction_failure_rolls_back_every_row_and_rollback_preserves_original_backup() {
    let (dir, mut store, db) = setup();
    store.init_chats().unwrap();
    db.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON native_checkpoints BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let bytes = raw(&source(dir.path()));
    assert!(store.prepare_legacy_claude(&bytes).is_err());
    for table in [
        "chats",
        "chat_rows",
        "chat_commands",
        "native_checkpoints",
        "legacy_claude_import",
    ] {
        assert_eq!(count(&db, table), 0);
    }
    db.execute_batch("DROP TRIGGER fail_checkpoint").unwrap();
    store.prepare_legacy_claude(&bytes).unwrap();
    assert!(store.rollback_legacy_connection().is_err());
    let rolled = store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(
        store
            .rollback_legacy_claude("original-android-chat")
            .unwrap(),
        rolled
    );
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_claude_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        bytes
    );
    for table in ["chats", "chat_rows", "chat_commands", "native_checkpoints"] {
        assert_eq!(count(&db, table), 0);
    }
    assert!(store.prepare_legacy_claude(&bytes).is_err());
    store.rollback_legacy_connection().unwrap();
    assert!(store.assert_migration_ready().is_err());
}
#[test]
fn changed_messages_or_native_side_effects_are_never_erased_on_retry_or_rollback() {
    for change in ["content", "image", "turn-index"] {
        let (dir, mut store, db) = setup();
        let bytes = raw(&source(dir.path()));
        store.prepare_legacy_claude(&bytes).unwrap();
        if change == "image" {
            db.execute(
                "INSERT INTO tool_images VALUES('original-android-chat','new-image')",
                [],
            )
            .unwrap();
        } else if change == "turn-index" {
            db.execute(
                "UPDATE chat_rows SET turn='other-turn' WHERE id='assistant-message'",
                [],
            )
            .unwrap();
        } else {
            db.execute("UPDATE chat_rows SET data=json_set(data,'$.text','new content') WHERE id='assistant-message'",[]).unwrap();
        }
        assert!(store
            .rollback_legacy_claude("original-android-chat")
            .is_err());
        assert!(store.prepare_legacy_claude(&bytes).is_err());
        assert_eq!(count(&db, "chats"), 1);
        assert_eq!(count(&db, "chat_rows"), 5);
    }
}
#[test]
fn active_queued_uncertain_or_permission_state_cannot_be_silently_dropped() {
    for (path, value) in [
        ("/thread/session/status", json!("running")),
        ("/thread/session/activeTurnId", json!("active")),
        ("/thread/hasPendingApprovals", json!(true)),
        ("/thread/hasPendingUserInput", json!(true)),
        ("/thread/messages/0/streaming", json!(true)),
    ] {
        let (dir, mut store, _) = setup();
        let mut original = source(dir.path());
        *original.pointer_mut(path).unwrap() = value;
        assert!(
            store.prepare_legacy_claude(&raw(&original)).is_err(),
            "{path}"
        );
        assert!(store.assert_migration_ready().is_err());
    }
    for (key, value) in [
        ("followUps", json!([{"id":"pending"}])),
        ("deliveries", json!([{"state":"offered"}])),
        ("permissionRules", json!(["Bash(*)"])),
        ("branchImportPending", json!(true)),
    ] {
        let (dir, mut store, _) = setup();
        let mut original = source(dir.path());
        original[key] = value;
        assert!(
            store.prepare_legacy_claude(&raw(&original)).is_err(),
            "{key}"
        );
    }
}
#[test]
fn conflicts_bad_rows_oversized_inputs_and_wrong_agent_never_replace_a_chat() {
    let (dir, mut store, db) = setup();
    let original = source(dir.path());
    let bytes = raw(&original);
    store.prepare_legacy_claude(&bytes).unwrap();
    for (path, value) in [
        ("/thread/title", json!("Changed title")),
        ("/nativeId", json!("invalid")),
        ("/thread/modelSelection/instanceId", json!("codex")),
        ("/thread/worktreePath", json!("relative")),
        ("/thread/messages/0/createdAt", json!("bad date")),
        ("/thread/messages/1/id", json!("user-message")),
    ] {
        let mut value_root = original.clone();
        *value_root.pointer_mut(path).unwrap() = value;
        assert!(
            store.prepare_legacy_claude(&raw(&value_root)).is_err(),
            "{path}"
        );
    }
    let mut duplicate = original.clone();
    duplicate["thread"]["id"] = json!("second-chat");
    duplicate["thread"]["session"]["threadId"] = json!("second-chat");
    assert!(store.prepare_legacy_claude(&raw(&duplicate)).is_err());
    assert!(store
        .prepare_legacy_claude(&vec![b' '; 16 * 1024 * 1024 + 1])
        .is_err());
    assert_eq!(count(&db, "chats"), 1);
    assert_eq!(count(&db, "chat_rows"), 5);
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_claude_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        bytes
    );
}
#[test]
fn timestamp_order_survives_legacy_sequence_reset_between_bridge_runs() {
    let (dir, mut store, db) = setup();
    let mut original = source(dir.path());
    original["thread"]["messages"].as_array_mut().unwrap().push(json!({"id":"after-restart","role":"user","text":"Later prompt","sequence":1,"createdAt":"2026-01-01T13:00:00Z","streaming":false}));
    store.prepare_legacy_claude(&raw(&original)).unwrap();
    let last: String = db
        .query_row(
            "SELECT id FROM chat_rows ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last, "after-restart");
}
#[test]
fn a_live_or_unprepared_profile_cannot_receive_legacy_chats() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    assert!(store
        .prepare_legacy_claude(&raw(&source(dir.path())))
        .is_err());
    assert!(store.assert_migration_ready().is_ok());
}
