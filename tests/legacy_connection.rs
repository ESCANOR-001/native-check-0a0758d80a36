use remodex_connect_native::{
    auth::{self, Auth},
    legacy_connection::Prepared,
    store::Store,
};
use serde_json::{json, Value};
const ENVIRONMENT: &str = "opencodex-0123456789abcdef01234567";
const TOKEN: &str = "ocx_android_disposable-migration-fixture";
fn source() -> Value {
    json!({"version":1,"settings":{"controlEnabled":true,"keepAwake":true,"localNetworkEnabled":false,"tunnelMode":"quick","connectionChoice":"local"},"clients":[{"id":"phone-one","label":"Fixture phone","os":"android","deviceType":"mobile","scopes":auth::SCOPES.split(' ').collect::<Vec<_>>(),"createdAt":"2020-01-01T00:00:00.000Z","credentialDigest":auth::digest(TOKEN),"installationDigest":auth::digest("fixture-installation")}],"threadAliases":[{"remoteThreadId":"keep-this-original-identity"}],"taskSelections":[{"nativeThreadId":"original-native"}],"futureMetadata":{"keep":"exact bytes"}})
}
fn parsed(source: &Value) -> Prepared {
    Prepared::parse(&serde_json::to_vec(source).unwrap(), ENVIRONMENT).unwrap()
}
fn connect(path: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path.join("native-connect.sqlite")).unwrap()
}
#[test]
fn preserves_credentials_expiry_environment_and_exact_legacy_backup_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let input = format!("  {}\n", source());
    let prepared = Prepared::parse(input.as_bytes(), ENVIRONMENT).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let report = store.prepare_legacy_connection(&prepared).unwrap();
    assert_eq!(report["pairedDevices"], 1);
    assert_eq!(report["threadAliasesPending"], 1);
    assert!(!report.to_string().contains("credential"));
    assert!(store.assert_migration_ready().is_err());
    assert_eq!(store.prepare_legacy_connection(&prepared).unwrap(), report);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.environment_id().unwrap(), ENVIRONMENT);
    let mut auth = Auth::new(store);
    assert_eq!(
        auth.authenticate(TOKEN, auth::now()).unwrap().unwrap().id,
        "phone-one"
    );
    assert!(auth.authenticate("other", auth::now()).unwrap().is_none());
    assert!(!auth.local_network_enabled().unwrap());
    assert!(auth.keep_awake().unwrap());
    assert!(auth.quick_tunnel_enabled().unwrap());
    let ticket = auth.ticket("phone-one", auth::now()).unwrap().unwrap();
    assert_eq!(
        auth.consume_ticket(ticket["ticket"].as_str().unwrap(), auth::now())
            .unwrap()
            .as_deref(),
        Some("phone-one")
    );
    let backup: Vec<u8> = connect(dir.path())
        .query_row("SELECT raw FROM legacy_connection_import", [], |r| r.get(0))
        .unwrap();
    assert_eq!(backup, input.as_bytes());
    let rolled = auth.store.rollback_legacy_connection().unwrap();
    assert_eq!(rolled["backupRetained"], true);
    assert!(auth.authenticate(TOKEN, auth::now()).unwrap().is_none());
    assert_eq!(auth.store.rollback_legacy_connection().unwrap(), rolled);
    assert!(auth.store.assert_migration_ready().is_err());
}
#[test]
fn expired_and_disabled_authorizations_remain_disabled() {
    for disabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut value = source();
        if disabled {
            value["settings"]["controlEnabled"] = json!(false);
        } else {
            value["clients"][0]["credentialExpiresAt"] = json!("2021-01-01T00:00:00Z");
        }
        let mut store = Store::open(dir.path()).unwrap();
        store.prepare_legacy_connection(&parsed(&value)).unwrap();
        let auth = Auth::new(store);
        assert!(auth.authenticate(TOKEN, auth::now()).unwrap().is_none());
        assert!(auth.store.assert_migration_ready().is_err());
    }
}
#[test]
fn malformed_or_privilege_changing_sources_are_rejected_without_partial_import() {
    for (path, value) in [
        ("/version", json!(2)),
        ("/clients/0/scopes", json!(["orchestration:read"])),
        ("/clients/0/credentialDigest", json!("invalid")),
        ("/clients/0/createdAt", json!("bad-date")),
        ("/settings/tunnelMode", json!("named")),
        ("/settings/connectionChoice", json!("named")),
        ("/settings/tunnelMode", json!(12)),
        ("/settings/controlEnabled", json!("true")),
        (
            "/clients/0/credentialExpiresAt",
            json!("2019-01-01T00:00:00Z"),
        ),
    ] {
        let mut data = source();
        if path.ends_with("credentialExpiresAt") {
            data["clients"][0]["credentialExpiresAt"] = value;
        } else {
            *data.pointer_mut(path).unwrap() = value;
        }
        assert!(
            Prepared::parse(&serde_json::to_vec(&data).unwrap(), ENVIRONMENT).is_err(),
            "{path}"
        );
    }
    let mut duplicate = source();
    let row = duplicate["clients"][0].clone();
    duplicate["clients"].as_array_mut().unwrap().push(row);
    assert!(Prepared::parse(&serde_json::to_vec(&duplicate).unwrap(), ENVIRONMENT).is_err());
    assert!(Prepared::parse(&vec![b' '; 16 * 1024 * 1024 + 1], ENVIRONMENT).is_err());
    assert!(Prepared::parse(b"{}", "different-environment").is_err());
}
#[test]
fn import_failure_is_atomic_and_existing_or_modified_destinations_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = connect(dir.path());
    conn.execute_batch("CREATE TRIGGER fail_metadata BEFORE INSERT ON metadata BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(store.prepare_legacy_connection(&parsed(&source())).is_err());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM clients", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM legacy_connection_import", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    conn.execute_batch("DROP TRIGGER fail_metadata").unwrap();
    store.prepare_legacy_connection(&parsed(&source())).unwrap();
    conn.execute("UPDATE clients SET label='Changed after import'", [])
        .unwrap();
    assert!(store.prepare_legacy_connection(&parsed(&source())).is_err());
    assert!(store.rollback_legacy_connection().is_err());
    assert_eq!(
        conn.query_row("SELECT label FROM clients", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "Changed after import"
    );
    let fresh = tempfile::tempdir().unwrap();
    let mut store = Store::open(fresh.path()).unwrap();
    let environment = store.environment_id().unwrap();
    assert!(store.prepare_legacy_connection(&parsed(&source())).is_err());
    assert_eq!(store.environment_id().unwrap(), environment);
}
#[test]
fn partial_migration_is_never_advertised_as_a_running_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.prepare_legacy_connection(&parsed(&source())).unwrap();
    drop(store);
    assert!(remodex_connect_native::server::App::new(
        Auth::new(Store::open(dir.path()).unwrap()),
        "fixture"
    )
    .is_err());
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .arg(dir.path())
        .args(["127.0.0.1:0", "http://127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pending migration must exit before serving");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("offline migration profile"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("ready"));
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("pairing-")));
}

#[test]
fn corrupt_backup_and_unexpected_destination_rows_block_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let prepared = parsed(&source());
    store.prepare_legacy_connection(&prepared).unwrap();
    let conn = connect(dir.path());
    conn.execute("UPDATE legacy_connection_import SET raw=x'7b7d'", [])
        .unwrap();
    assert!(store.prepare_legacy_connection(&prepared).is_err());
    assert!(store.rollback_legacy_connection().is_err());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM clients", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let fresh = tempfile::tempdir().unwrap();
    let mut store = Store::open(fresh.path()).unwrap();
    store.prepare_legacy_connection(&prepared).unwrap();
    connect(fresh.path()).execute_batch("CREATE TABLE unexpected_history(value TEXT); INSERT INTO unexpected_history VALUES ('preserve');").unwrap();
    assert!(store.rollback_legacy_connection().is_err());
}
