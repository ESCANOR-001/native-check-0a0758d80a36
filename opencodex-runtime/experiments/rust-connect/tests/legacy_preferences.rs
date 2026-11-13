use remodex_connect_native::{legacy_connection::Prepared, store::Store};
use serde_json::{json, Value};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const TIME: &str = "2026-01-01T12:00:00.000Z";
fn setup() -> (tempfile::TempDir, Store, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let root = json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],"threadAliases":[],"taskSelections":[]});
    store
        .prepare_legacy_connection(&Prepared::parse(root.to_string().as_bytes(), ENV).unwrap())
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    (dir, store, db)
}
fn project(cwd: &std::path::Path, id: &str) -> Value {
    json!({"id":id,"title":"Saved folder","workspaceRoot":cwd.join(id),"repositoryIdentity":null,"defaultModelSelection":null,"scripts":[],"createdAt":TIME,"updatedAt":TIME,"futureField":"retain"})
}
fn raw(v: &Value) -> Vec<u8> {
    format!("  {v}\n").into_bytes()
}
fn get(db: &rusqlite::Connection, key: &str) -> Value {
    serde_json::from_str(
        &db.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn folders_defaults_scripts_and_disabled_settings_survive_retry_restart_and_rollback() {
    let (dir, mut store, db) = setup();
    let mut cp = project(dir.path(), "claude-folder");
    cp["defaultModelSelection"] =
        json!({"instanceId":"claude-code","model":"sonnet","options":{"effort":"high"}});
    cp["scripts"] = json!([{"id":"setup","name":"Setup","command":"printf fixture","icon":"configure","runOnWorktreeCreate":true,"previewUrl":"http://localhost:3000","autoOpenPreview":false}]);
    let mut xp = project(dir.path(), "codex-folder");
    xp["defaultModelSelection"] = json!({"instanceId":"openai","model":"saved-model","options":[{"id":"reasoningEffort","value":"low"}]} );
    let c = raw(
        &json!({"enabled":false,"desktopSync":false,"binary":"","projects":[cp.clone()],"future":{"retained":true}}),
    );
    let x = raw(&json!([xp]));
    let report = store.prepare_legacy_preferences(&c, &x).unwrap();
    assert_eq!(report["projects"], 2);
    assert_eq!(report["migrationComplete"], false);
    assert_eq!(
        get(&db, "native.claudeSettings"),
        json!({"enabled":false,"desktop_sync":false,"binary":null})
    );
    let values = get(&db, "native.projects");
    assert_eq!(values[0]["id"], "claude-folder");
    assert_eq!(values[0]["scripts"], cp["scripts"]);
    assert_eq!(values[0]["futureField"], "retain");
    assert_eq!(
        values[1]["defaultModelSelection"],
        json!({"instanceId":"codex","model":"saved-model","options":[{"id":"effort","value":"low"}]})
    );
    assert!(!dir.path().join("claude-folder").exists());
    assert!(!dir.path().join("codex-folder").exists());
    assert!(store.rollback_legacy_connection().is_err());
    assert!(store.assert_migration_ready().is_err());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(store.prepare_legacy_preferences(&c, &x).unwrap(), report);
    assert_eq!(get(&db, "native.projects"), values);
    let rolled = store.rollback_legacy_preferences().unwrap();
    assert_eq!(store.rollback_legacy_preferences().unwrap(), rolled);
    assert!(store.prepare_legacy_preferences(&c, &x).is_err());
    assert!(store.assert_migration_ready().is_err());
    store.rollback_legacy_connection().unwrap();
    let saved: (Vec<u8>, Vec<u8>) = db
        .query_row(
            "SELECT claude_raw,codex_raw FROM legacy_preferences_import",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved, (c, x));
}
#[test]
fn executable_is_preserved_without_running_it_or_changing_external_settings() {
    let (dir, mut store, db) = setup();
    let binary = dir.path().join("must-not-run");
    let c = raw(&json!({"enabled":true,"desktopSync":true,"binary":binary,"projects":[]}));
    let report = store.prepare_legacy_preferences(&c, b"[]").unwrap();
    assert_eq!(report["claudeEnabled"], true);
    assert_eq!(get(&db, "native.claudeSettings")["binary"], json!(binary));
    assert!(!binary.exists());
    assert!(store.assert_migration_ready().is_err());
}
#[test]
fn same_project_can_be_shared_but_conflicting_ids_are_not_renamed() {
    let (dir, mut store, db) = setup();
    let p = project(dir.path(), "shared");
    let c = raw(&json!({"projects":[p.clone()]}));
    let x = raw(&json!([p.clone()]));
    assert_eq!(
        store.prepare_legacy_preferences(&c, &x).unwrap()["projects"],
        1
    );
    assert_eq!(get(&db, "native.projects").as_array().unwrap().len(), 1);
    let (dir, mut store, db) = setup();
    let p = project(dir.path(), "shared");
    let mut conflict = p.clone();
    conflict["workspaceRoot"] = json!(dir.path().join("another"));
    assert!(store
        .prepare_legacy_preferences(&raw(&json!({"projects":[p]})), &raw(&json!([conflict])))
        .is_err());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM metadata WHERE key LIKE 'native.%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
#[test]
fn virtual_no_project_does_not_become_a_directory() {
    let (_dir, mut store, db) = setup();
    let c = raw(
        &json!({"projects":[{"id":"codex-project-chats","title":"No Project","workspaceRoot":null,"scripts":[]}]}),
    );
    assert_eq!(
        store.prepare_legacy_preferences(&c, b"[]").unwrap()["projects"],
        0
    );
    assert_eq!(get(&db, "native.projects"), json!([]));
}
#[test]
fn malformed_settings_unsafe_paths_and_unsupported_defaults_are_refused_atomically() {
    for bad in [
        json!({"enabled":"true"}),
        json!({"desktopSync":1}),
        json!({"binary":"relative-executable"}),
        json!({"projects":{}}),
        json!([]),
    ] {
        let (_dir, mut store, db) = setup();
        assert!(store.prepare_legacy_preferences(&raw(&bad), b"[]").is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
    for (key, bad) in [
        ("workspaceRoot", json!("relative")),
        ("createdAt", json!("bad")),
        (
            "defaultModelSelection",
            json!({"instanceId":"another-provider","model":"m"}),
        ),
        (
            "scripts",
            json!([{"id":"s","name":"s","command":"s","icon":"play","runOnWorktreeCreate":"yes"}]),
        ),
    ] {
        let (dir, mut store, db) = setup();
        let mut p = project(dir.path(), "project");
        p[key] = bad;
        assert!(store
            .prepare_legacy_preferences(&raw(&json!({"projects":[p]})), b"[]")
            .is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
}
#[test]
fn preexisting_settings_and_changed_or_corrupt_receipts_are_never_overwritten() {
    let (_dir, mut store, db) = setup();
    db.execute(
        "INSERT INTO metadata VALUES('native.claudeSettings','preserve')",
        [],
    )
    .unwrap();
    assert!(store.prepare_legacy_preferences(b"{}", b"[]").is_err());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM metadata WHERE key='native.projects'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    for mutation in [
        "UPDATE metadata SET value='changed' WHERE key='native.projects'",
        "UPDATE legacy_preferences_import SET claude_raw=x'7b7d'",
        "UPDATE legacy_preferences_import SET connection_digest='changed'",
    ] {
        let (_dir, mut store, db) = setup();
        let c = b" {\"enabled\":true} ";
        store.prepare_legacy_preferences(c, b"[]").unwrap();
        db.execute_batch(mutation).unwrap();
        assert!(store.prepare_legacy_preferences(c, b"[]").is_err());
        assert!(store.rollback_legacy_preferences().is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM metadata WHERE key LIKE 'native.%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
    }
}
