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
#[test]
fn owned_saved_permissions_preserve_exact_rules_source_and_rollback_without_profile_writes() {
    let (dir, mut store, db) = setup();
    let mut original = source(dir.path());
    original["permissionRules"] = json!(["Read(/fixture/**)", "Bash(echo fixture)"]);
    let bytes = raw(&original);
    store.prepare_legacy_claude(&bytes).unwrap();
    let saved = store.load_chat("original-android-chat").unwrap();
    assert_eq!(
        saved["claude_permission_rules"],
        original["permissionRules"]
    );
    assert!(saved["thread"].get("claude_permission_rules").is_none());
    assert!(!dir.path().join("settings.json").exists());
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_claude_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        bytes
    );
    store.prepare_legacy_claude(&bytes).unwrap();
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(count(&db, "chats"), 0);
    assert!(store.assert_migration_ready().is_err());
    let (dir, mut store, _) = setup();
    let mut original = source(dir.path());
    original["external"] = json!(true);
    original["permissionRules"] = json!(["Read"]);
    assert!(store
        .prepare_legacy_claude(&raw(&original))
        .unwrap_err()
        .contains("owner reconciliation"));
}
fn count(db: &rusqlite::Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

struct Images {
    export: tempfile::TempDir,
    old_root: std::path::PathBuf,
    source: std::path::PathBuf,
    destination: std::path::PathBuf,
    bytes: Vec<u8>,
    raw: Vec<u8>,
}
fn images(dir: &std::path::Path) -> Images {
    use sha2::{Digest, Sha256};
    let export = tempfile::tempdir().unwrap();
    let old_root = dir.join("old-install-not-read");
    let bytes = b"\x89PNG\r\n\x1a\nlegacy-tool-screenshot".to_vec();
    let bucket = format!("{:x}", Sha256::digest(b"original-android-chat"));
    let name = format!("{:x}.png", Sha256::digest(&bytes));
    let source = export.path().join(&bucket).join(&name);
    std::fs::create_dir(source.parent().unwrap()).unwrap();
    std::fs::write(&source, &bytes).unwrap();
    let mut value = source_fixture(dir);
    value["thread"]["activities"][0]["payload"]["data"]["toolImages"] =
        json!([old_root.join(&bucket).join(&name)]);
    Images {
        export,
        old_root,
        source,
        destination: dir.join("tool-images").join(bucket).join(name),
        bytes,
        raw: raw(&value),
    }
}
fn source_fixture(dir: &std::path::Path) -> Value {
    source(dir)
}

#[test]
fn exported_tool_images_keep_bytes_ids_text_and_chat_scoped_access_across_retry_and_rollback() {
    let (dir, mut store, db) = setup();
    let img = images(dir.path());
    store.prepare_legacy_claude(&img.raw).unwrap();
    let before: String = db
        .query_row(
            "SELECT data FROM chat_rows WHERE id='assistant-message'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let report = store
        .prepare_legacy_claude_tool_images(
            "original-android-chat",
            &img.old_root,
            img.export.path(),
        )
        .unwrap();
    assert_eq!(report["images"], 1);
    assert!(!img.old_root.exists()); // serialized original path is never opened or recreated
    assert_eq!(std::fs::read(&img.destination).unwrap(), img.bytes);
    assert!(store.owns_tool_image("original-android-chat", &img.destination));
    assert!(!store.owns_tool_image("other-chat", &img.destination));
    let tool: Value = serde_json::from_str(
        &db.query_row("SELECT data FROM chat_rows WHERE id='tool-row'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        tool["payload"]["data"]["toolImages"],
        json!([img.destination])
    );
    assert_eq!(tool["payload"]["data"]["inputDetail"], "Read example.txt");
    assert_eq!(
        db.query_row(
            "SELECT data FROM chat_rows WHERE id='assistant-message'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        before
    );
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_claude_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        img.raw
    );
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    store.init_chats().unwrap();
    assert!(store.assert_migration_ready().is_err());
    store.prepare_legacy_claude(&img.raw).unwrap();
    assert_eq!(
        store
            .prepare_legacy_claude_tool_images(
                "original-android-chat",
                &img.old_root,
                img.export.path()
            )
            .unwrap(),
        report
    );
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(count(&db, "tool_images"), 0);
    assert_eq!(std::fs::read(&img.destination).unwrap(), img.bytes); // rollback retains copied blobs and original receipt
    assert_eq!(
        db.query_row("SELECT phase FROM legacy_claude_assets", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "rolled-back"
    );
    store.rollback_legacy_connection().unwrap();
    store.rollback_legacy_connection().unwrap();
    db.execute("INSERT INTO storage_assets VALUES('unexpected-file',9)", [])
        .unwrap();
    assert!(store.rollback_legacy_connection().is_err());
}

#[test]
fn tool_image_transaction_failure_keeps_original_rows_and_retry_reuses_verified_blobs() {
    let (dir, mut store, db) = setup();
    let img = images(dir.path());
    store.prepare_legacy_claude(&img.raw).unwrap();
    let before: String = db
        .query_row("SELECT data FROM chat_rows WHERE id='tool-row'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute_batch("CREATE TRIGGER fail_image BEFORE INSERT ON tool_images BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(store
        .prepare_legacy_claude_tool_images(
            "original-android-chat",
            &img.old_root,
            img.export.path()
        )
        .is_err());
    assert_eq!(count(&db, "tool_images"), 0);
    assert_eq!(count(&db, "legacy_claude_assets"), 0);
    assert_eq!(
        db.query_row("SELECT data FROM chat_rows WHERE id='tool-row'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        before
    );
    let file_count = std::fs::read_dir(img.destination.parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(file_count, 1);
    db.execute_batch("DROP TRIGGER fail_image").unwrap();
    store
        .prepare_legacy_claude_tool_images(
            "original-android-chat",
            &img.old_root,
            img.export.path(),
        )
        .unwrap();
    assert_eq!(
        std::fs::read_dir(img.destination.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    assert_eq!(std::fs::read(&img.source).unwrap(), img.bytes);
}

#[test]
fn missing_changed_wrong_chat_and_malformed_tool_images_never_gain_read_access() {
    for defect in [
        "missing",
        "changed",
        "foreign-chat",
        "object",
        "outside-prefix",
        "live-source",
    ] {
        let (dir, mut store, db) = setup();
        let mut img = images(dir.path());
        let mut value: Value = serde_json::from_slice(&img.raw).unwrap();
        match defect {
            "missing" => std::fs::remove_file(&img.source).unwrap(),
            "changed" => std::fs::write(&img.source, b"wrong bytes").unwrap(),
            "foreign-chat" => {
                value["thread"]["activities"][0]["payload"]["data"]["toolImages"] = json!([img
                    .old_root
                    .join("other-chat")
                    .join(img.source.file_name().unwrap())])
            }
            "object" => {
                value["thread"]["activities"][0]["payload"]["data"]["toolImages"] =
                    json!([{"path":img.source}])
            }
            "outside-prefix" => img.old_root = dir.path().join("unrelated"),
            "live-source" => img.old_root = img.export.path().to_owned(),
            _ => unreachable!(),
        }
        img.raw = raw(&value);
        store.prepare_legacy_claude(&img.raw).unwrap();
        assert!(
            store
                .prepare_legacy_claude_tool_images(
                    "original-android-chat",
                    &img.old_root,
                    img.export.path()
                )
                .is_err(),
            "{defect}"
        );
        assert_eq!(count(&db, "tool_images"), 0);
        assert!(!img.destination.exists());
        assert!(store.assert_migration_ready().is_err());
    }
}

#[test]
fn changed_destination_or_receipt_blocks_asset_retry_and_metadata_rollback() {
    for defect in ["bytes", "reference", "extra-access", "ledger"] {
        let (dir, mut store, db) = setup();
        let img = images(dir.path());
        store.prepare_legacy_claude(&img.raw).unwrap();
        store
            .prepare_legacy_claude_tool_images(
                "original-android-chat",
                &img.old_root,
                img.export.path(),
            )
            .unwrap();
        match defect {
            "bytes" => std::fs::write(&img.destination, b"changed").unwrap(),
            "reference" => {
                db.execute("UPDATE chat_rows SET data=json_set(data,'$.payload.data.toolImages',json('[]')) WHERE id='tool-row'",[]).unwrap();
            }
            "extra-access" => {
                db.execute(
                    "INSERT INTO tool_images VALUES('original-android-chat','unrelated')",
                    [],
                )
                .unwrap();
            }
            "ledger" => {
                db.execute("UPDATE legacy_claude_assets SET data='[]'", [])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            store
                .prepare_legacy_claude_tool_images(
                    "original-android-chat",
                    &img.old_root,
                    img.export.path()
                )
                .is_err(),
            "{defect}"
        );
        assert!(store.prepare_legacy_claude(&img.raw).is_err(), "{defect}");
        assert!(
            store
                .rollback_legacy_claude("original-android-chat")
                .is_err(),
            "{defect}"
        );
        assert_eq!(count(&db, "chats"), 1);
    }
}

#[cfg(unix)]
#[test]
fn exported_image_and_bucket_symlinks_are_not_followed() {
    for bucket in [false, true] {
        let (dir, mut store, db) = setup();
        let img = images(dir.path());
        let other = tempfile::tempdir().unwrap();
        let target = other.path().join(img.source.file_name().unwrap());
        std::fs::write(&target, &img.bytes).unwrap();
        std::fs::remove_file(&img.source).unwrap();
        if bucket {
            std::fs::remove_dir(img.source.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(other.path(), img.source.parent().unwrap()).unwrap();
        } else {
            std::os::unix::fs::symlink(target, &img.source).unwrap();
        }
        store.prepare_legacy_claude(&img.raw).unwrap();
        assert!(store
            .prepare_legacy_claude_tool_images(
                "original-android-chat",
                &img.old_root,
                img.export.path()
            )
            .is_err());
        assert_eq!(count(&db, "tool_images"), 0);
    }
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
        ("/thread/session/status", json!("unknown-future-state")),
        ("/thread/session/activeTurnId", json!("active")),
        ("/thread/hasPendingApprovals", json!(true)),
        ("/thread/hasPendingUserInput", json!(true)),
        ("/thread/messages/0/streaming", json!(true)),
        ("/thread/messages/0/streaming", json!("true")),
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
        ("deliveries", json!([{"state":"unknown-future-state"}])),
        ("permissionRules", json!(["invalid\npermission"])),
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

#[test]
fn external_observer_checkpoint_keeps_original_sequence_without_inventing_native_event_identity() {
    let (dir, mut store, db) = setup();
    let mut value = source(dir.path());
    value["external"] = json!(true);
    value["remoteSequence"] = json!(98);
    let raw = raw(&value);
    store.prepare_legacy_claude(&raw).unwrap();
    let report = store
        .prepare_legacy_claude_resume("original-android-chat")
        .unwrap();
    assert_eq!(report["sequence"], 98);
    assert_eq!(report["migrationComplete"], false);
    let identity = remodex_connect_native::claude_host::Identity {
        native_session: NATIVE.into(),
        bridge_session: "cse_legacy_fixture".into(),
    };
    let cursor = store
        .claude_remote_cursor("original-android-chat", identity.clone())
        .unwrap();
    assert_eq!(cursor.sequence, 98);
    assert!(cursor.event_id.is_empty());
    assert!(cursor.digest.is_empty());
    assert_eq!(
        serde_json::to_value(&cursor).unwrap()["legacy_source"],
        remodex_connect_native::auth::digest(std::str::from_utf8(&raw).unwrap())
    );
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .prepare_legacy_claude_resume("original-android-chat")
            .unwrap(),
        report
    );
    assert_eq!(
        store
            .claude_remote_cursor("original-android-chat", identity)
            .unwrap(),
        cursor
    );
    store.prepare_legacy_claude(&raw).unwrap();
    assert_eq!(count(&db, "chat_rows"), 5);
    assert!(store.assert_migration_ready().is_err());
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(count(&db, "native_sync"), 0);
    assert_eq!(
        db.query_row("SELECT phase FROM legacy_claude_resume", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "rolled-back"
    );
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    store.rollback_legacy_connection().unwrap();
}
#[test]
fn missing_owned_or_changed_checkpoints_never_skip_unknown_history() {
    for (external, sequence) in [
        (false, json!(98)),
        (true, Value::Null),
        (true, json!(0)),
        (true, json!(9_007_199_254_740_992_u64)),
        (true, json!("98")),
    ] {
        let (dir, mut store, db) = setup();
        let mut value = source(dir.path());
        value["external"] = json!(external);
        value["remoteSequence"] = sequence;
        store.prepare_legacy_claude(&raw(&value)).unwrap();
        assert!(store
            .prepare_legacy_claude_resume("original-android-chat")
            .is_err());
        assert_eq!(count(&db, "native_sync"), 0);
        assert_eq!(count(&db, "chat_rows"), 5);
    }
    for mutation in [
        "UPDATE native_sync SET cursor=json_set(cursor,'$.sequence',99)",
        "UPDATE legacy_claude_resume SET source_hash='changed'",
        "UPDATE legacy_claude_resume SET cursor='{}'",
    ] {
        let (dir, mut store, db) = setup();
        let mut value = source(dir.path());
        value["external"] = json!(true);
        store.prepare_legacy_claude(&raw(&value)).unwrap();
        store
            .prepare_legacy_claude_resume("original-android-chat")
            .unwrap();
        db.execute_batch(mutation).unwrap();
        assert!(store
            .prepare_legacy_claude_resume("original-android-chat")
            .is_err());
        assert!(store
            .rollback_legacy_claude("original-android-chat")
            .is_err());
        assert_eq!(count(&db, "chats"), 1);
    }
}
#[tokio::test]
async fn first_native_event_after_upgrade_advances_anchor_and_preserves_old_prompt_bubbles() {
    use remodex_connect_native::{
        auth::Auth, auth_service::AuthService, claude_host::Identity, claude_remote::Envelope,
    };
    let (dir, mut store, db) = setup();
    let mut value = source(dir.path());
    value["external"] = json!(true);
    store.prepare_legacy_claude(&raw(&value)).unwrap();
    store
        .prepare_legacy_claude_resume("original-android-chat")
        .unwrap();
    let identity = Identity {
        native_session: NATIVE.into(),
        bridge_session: "cse_legacy_fixture".into(),
    };
    let before = store
        .claude_remote_cursor("original-android-chat", identity.clone())
        .unwrap();
    let initial = store.load_chat("original-android-chat").unwrap();
    let old = initial["thread"]["messages"].clone();
    let service = AuthService::new(Auth::new(store)).unwrap();
    let native = "00000000-0000-4000-8000-000000000019";
    let mut cursor = before.clone();
    let mut current = initial;
    for (sequence, source, payload) in [
        (
            99,
            "client",
            json!({"type":"user","uuid":native,"session_id":NATIVE,"message":{"role":"user","content":"New Desktop prompt"},"client_platform":"desktop"}),
        ),
        (
            100,
            "worker",
            json!({"type":"assistant","uuid":"00000000-0000-4000-8000-000000000020","session_id":NATIVE,"user_message_uuids":[native],"message":{"id":"new-native-reply","role":"assistant","content":[{"type":"text","text":"New reply"}]}}),
        ),
        (
            101,
            "worker",
            json!({"type":"result","session_id":NATIVE,"user_message_uuids":[native],"subtype":"success","result":"New reply"}),
        ),
    ] {
        let mut event = Envelope {
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
            sequence: Some(sequence),
            event_id: Some(format!("native-{sequence}")),
            created_at: Some("2026-01-01T12:00:01.000Z".into()),
            source: source.into(),
            payload: Some(payload),
        };
        event.sequence = Some(98);
        assert!(before.advance(&event, Value::Null).is_err());
        event.sequence = Some(sequence);
        let (state, projection) = remodex_connect_native::claude_history::project(
            &service, current, &cursor, &event, true,
        )
        .await
        .unwrap();
        let after = cursor.advance(&event, projection).unwrap();
        assert!(serde_json::to_value(&after)
            .unwrap()
            .get("legacy_source")
            .is_none());
        let saved = state.clone();
        let start = cursor.clone();
        let finish = after.clone();
        service
            .call(move |a| a.store.save_claude_remote(&saved, &start, &finish))
            .await
            .unwrap()
            .unwrap();
        current = state;
        cursor = after;
    }
    assert_eq!(cursor.sequence, 101);
    assert_eq!(cursor.event_id, "native-101");
    assert_eq!(cursor.digest.len(), 64);
    let final_state = service
        .call(|a| a.store.load_chat("original-android-chat"))
        .await
        .unwrap()
        .unwrap();
    let messages = final_state["thread"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    for original in old.as_array().unwrap() {
        assert_eq!(
            messages.iter().find(|m| m["id"] == original["id"]).unwrap(),
            original
        );
    }
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["text"] == "New Desktop prompt")
            .count(),
        1
    );
    assert_eq!(
        messages.iter().filter(|m| m["text"] == "New reply").count(),
        1
    );
    assert!(service
        .call(|a| a.store.rollback_legacy_claude("original-android-chat"))
        .await
        .unwrap()
        .is_err());
    assert_eq!(count(&db, "chats"), 1);
}

const INPUT_NATIVE: &str = "00000000-0000-4000-8000-000000000025";
fn bound_source(dir: &std::path::Path) -> Value {
    let mut value = source(dir);
    value["deliveries"] = json!([{"uuid":INPUT_NATIVE,"messageId":"user-message","turnId":"old-turn","state":"consumed","steer":false}]);
    value
}
#[test]
fn legacy_prompt_bindings_preserve_aliases_without_fabricating_delivery_receipts() {
    let (dir, mut store, db) = setup();
    let value = bound_source(dir.path());
    let bytes = raw(&value);
    store.prepare_legacy_claude(&bytes).unwrap();
    let before = store.load_chat("original-android-chat").unwrap();
    let report = store
        .prepare_legacy_claude_bindings("original-android-chat")
        .unwrap();
    assert_eq!(report["bindings"], 1);
    let record = store
        .claude_input("original-android-chat", NATIVE, INPUT_NATIVE)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, "legacy-consumed");
    assert_eq!(record.input.message, "user-message");
    assert_eq!(record.input.public_turn, "old-turn");
    assert_eq!(count(&db, "claude_outbox"), 0);
    assert!(store
        .unresolved_claude_inputs("original-android-chat", NATIVE)
        .unwrap()
        .is_empty());
    let payload = json!({"type":"user","uuid":INPUT_NATIVE,"session_id":NATIVE,"message":{"role":"user","content":"Original prompt"}});
    assert!(store
        .offer_claude_input("original-android-chat", NATIVE, &record.input, &payload)
        .unwrap_err()
        .contains("not offered again"));
    assert_eq!(
        store
            .prepare_legacy_claude_bindings("original-android-chat")
            .unwrap(),
        report
    );
    assert_eq!(store.load_chat("original-android-chat").unwrap(), before);
    store.prepare_legacy_claude(&bytes).unwrap();
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(count(&db, "legacy_claude_bindings"), 0);
    assert_eq!(
        db.query_row("SELECT raw FROM legacy_claude_import", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        bytes
    );
    assert!(store.assert_migration_ready().is_err());
    assert!(store
        .claude_input("original-android-chat", NATIVE, INPUT_NATIVE)
        .unwrap()
        .is_none());
}
#[test]
fn invalid_legacy_aliases_fail_without_touching_history_and_tampering_blocks_rollback() {
    for (key, value) in [
        ("uuid", json!("bad-uuid")),
        ("messageId", json!("missing-message")),
        ("turnId", json!("wrong-turn")),
        ("steer", json!("yes")),
    ] {
        let (dir, mut store, _) = setup();
        let mut value_root = bound_source(dir.path());
        value_root["deliveries"][0][key] = value;
        store.prepare_legacy_claude(&raw(&value_root)).unwrap();
        let before = store.load_chat("original-android-chat").unwrap();
        assert!(store
            .prepare_legacy_claude_bindings("original-android-chat")
            .is_err());
        assert_eq!(store.load_chat("original-android-chat").unwrap(), before);
    }
    for sql in [
        "UPDATE legacy_claude_bindings SET message='changed'",
        "UPDATE legacy_claude_binding_import SET source_hash='changed'",
    ] {
        let (dir, mut store, db) = setup();
        let value = bound_source(dir.path());
        store.prepare_legacy_claude(&raw(&value)).unwrap();
        store
            .prepare_legacy_claude_bindings("original-android-chat")
            .unwrap();
        db.execute_batch(sql).unwrap();
        assert!(store
            .prepare_legacy_claude_bindings("original-android-chat")
            .is_err());
        assert!(store
            .rollback_legacy_claude("original-android-chat")
            .is_err());
        assert_eq!(count(&db, "chats"), 1);
    }
}
#[test]
fn legacy_alias_transaction_failure_rolls_back_all_bindings_and_retry_is_scoped() {
    let (dir, mut store, db) = setup();
    let mut value = bound_source(dir.path());
    value["thread"]["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"second-user","role":"user","text":"Second prompt","createdAt":TIME,
            "turnId":"second-turn","sequence":55,"streaming":false
        }));
    let second = "00000000-0000-4000-8000-000000000026";
    value["deliveries"].as_array_mut().unwrap().push(json!({
        "uuid":second,"messageId":"second-user","turnId":"second-turn","state":"cancelled","steer":true
    }));
    let bytes = raw(&value);
    store.prepare_legacy_claude(&bytes).unwrap();
    let before = store.load_chat("original-android-chat").unwrap();
    // Provision only the empty ledger, then fail the second insert to exercise
    // a real mid-transaction storage error rather than a parser rejection.
    db.execute_batch("CREATE TABLE legacy_claude_binding_import(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,phase TEXT NOT NULL);CREATE TABLE legacy_claude_bindings(chat TEXT NOT NULL,session TEXT NOT NULL,native TEXT NOT NULL,message TEXT NOT NULL,turn TEXT NOT NULL,steer INTEGER NOT NULL,state TEXT NOT NULL,PRIMARY KEY(chat,session,native),UNIQUE(chat,session,message));CREATE TRIGGER fail_second BEFORE INSERT ON legacy_claude_bindings WHEN NEW.message='second-user' BEGIN SELECT RAISE(ABORT,'fixture full'); END;").unwrap();
    assert!(store
        .prepare_legacy_claude_bindings("original-android-chat")
        .is_err());
    assert_eq!(count(&db, "legacy_claude_bindings"), 0);
    assert_eq!(count(&db, "legacy_claude_binding_import"), 0);
    assert_eq!(store.load_chat("original-android-chat").unwrap(), before);
    db.execute_batch("DROP TRIGGER fail_second").unwrap();
    assert_eq!(
        store
            .prepare_legacy_claude_bindings("original-android-chat")
            .unwrap()["bindings"],
        2
    );
    let record = store
        .claude_input("original-android-chat", NATIVE, second)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, "cancelled");
    assert!(record.input.steer);
    assert!(store
        .claude_input("other-chat", NATIVE, second)
        .unwrap()
        .is_none());
    assert!(store
        .claude_input("original-android-chat", second, INPUT_NATIVE)
        .unwrap()
        .is_none());
    // A different native UUID must not reuse the same public prompt identity.
    let mut input = record.input;
    input.native = "00000000-0000-4000-8000-000000000027".into();
    let payload = json!({"type":"user","uuid":input.native,"session_id":NATIVE,"message":{"role":"user","content":"Changed"}});
    assert!(store
        .offer_claude_input("original-android-chat", NATIVE, &input, &payload)
        .unwrap_err()
        .contains("not offered again"));
    assert!(store
        .offer_claude_input("other-chat", NATIVE, &input, &payload)
        .is_err());
    assert_eq!(count(&db, "claude_outbox"), 0);
    assert_eq!(store.load_chat("original-android-chat").unwrap(), before);
    store.prepare_legacy_claude(&bytes).unwrap();
    store
        .rollback_legacy_claude("original-android-chat")
        .unwrap();
    assert_eq!(count(&db, "legacy_claude_bindings"), 0);
}
#[tokio::test]
async fn legacy_alias_echo_after_checkpoint_keeps_one_original_prompt_with_exact_old_rows() {
    use remodex_connect_native::{
        auth::Auth, auth_service::AuthService, claude_host::Identity, claude_remote::Envelope,
    };
    let (dir, mut store, _) = setup();
    let mut value = bound_source(dir.path());
    value["external"] = json!(true);
    store.prepare_legacy_claude(&raw(&value)).unwrap();
    store
        .prepare_legacy_claude_bindings("original-android-chat")
        .unwrap();
    store
        .prepare_legacy_claude_resume("original-android-chat")
        .unwrap();
    let identity = Identity {
        native_session: NATIVE.into(),
        bridge_session: "cse_legacy_fixture".into(),
    };
    let mut cursor = store
        .claude_remote_cursor("original-android-chat", identity)
        .unwrap();
    let mut current = store.load_chat("original-android-chat").unwrap();
    let original = current["thread"]["messages"].clone();
    let service = AuthService::new(Auth::new(store)).unwrap();
    for (seq, source, payload) in [
        (
            99,
            "client",
            json!({"type":"user","uuid":INPUT_NATIVE,"session_id":NATIVE,"message":{"role":"user","content":"Original prompt"}}),
        ),
        (
            100,
            "worker",
            json!({"type":"assistant","session_id":NATIVE,"user_message_uuids":[INPUT_NATIVE],"message":{"id":"assistant-message","role":"assistant","content":[{"type":"text","text":"Original reply"}]}}),
        ),
        (
            101,
            "worker",
            json!({"type":"result","session_id":NATIVE,"user_message_uuids":[INPUT_NATIVE],"subtype":"success"}),
        ),
    ] {
        let event = Envelope {
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
            sequence: Some(seq),
            event_id: Some(format!("echo-{seq}")),
            source: source.into(),
            created_at: Some(TIME.into()),
            payload: Some(payload),
        };
        let (next, projection) = remodex_connect_native::claude_history::project(
            &service, current, &cursor, &event, true,
        )
        .await
        .unwrap();
        let after = cursor.advance(&event, projection).unwrap();
        let a = cursor.clone();
        let b = after.clone();
        let saved = next.clone();
        service
            .call(move |auth| auth.store.save_claude_remote(&saved, &a, &b))
            .await
            .unwrap()
            .unwrap();
        current = next;
        cursor = after;
    }
    let saved = service
        .call(|a| a.store.load_chat("original-android-chat"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["thread"]["messages"], original);
    assert_eq!(saved["thread"]["latestTurn"]["turnId"], "old-turn");
    assert_eq!(saved["thread"]["session"]["status"], "ready");
}
