use remodex_connect_native::{
    auth, legacy_rollback, legacy_stage, migration_snapshot, profile_lock, store::Store,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const TIME: &str = "2026-01-01T12:00:00.000Z";
const CODEX: &str = "00000000-0000-4000-8000-000000000011";
const PROMPT: &str = "00000000-0000-4000-8000-000000000001";
struct Fixture {
    _dir: tempfile::TempDir,
    source: PathBuf,
    snapshot: PathBuf,
    generation: PathBuf,
    original: PathBuf,
}
impl Fixture {
    fn activation_config(&self) -> PathBuf {
        self.activation_config_using(None)
    }
    fn activation_config_using(&self, existing_claude: Option<PathBuf>) -> PathBuf {
        use remodex_connect_native::profile_handoff::{self, Input, Plan};
        let claude = existing_claude.unwrap_or_else(|| {
            let path = self._dir.path().join("claude-profile");
            fs::create_dir(&path).unwrap();
            fs::write(path.join("settings.json"), b"{}").unwrap();
            path
        });
        let claude_snapshot = self._dir.path().join("claude-profile-export");
        migration_snapshot::capture(&claude, &claude_snapshot, ENV).unwrap();
        let codex = self._dir.path().join("codex-export");
        let handoff = self._dir.path().join("native-generation");
        profile_handoff::prepare(
            &handoff,
            &Plan {
                version: 1,
                environment: ENV.into(),
                codex: Input {
                    original_profile: codex.clone(),
                    snapshot: self.source.join("codex-native"),
                },
                claude: Input {
                    original_profile: claude.clone(),
                    snapshot: claude_snapshot,
                },
            },
        )
        .unwrap();
        let config = self._dir.path().join("config.json");
        write(
            &config,
            &json!({"native_profile_handoff":handoff,"workspace":self._dir.path(),"codex":{"executable":std::env::current_exe().unwrap(),"profile":codex,"model":"test"},"claude":{"executable":std::env::current_exe().unwrap(),"profile":claude,"model":"test","remote_control":true}}),
        );
        config
    }
    fn new(count: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("export");
        fs::create_dir(&source).unwrap();
        let original = dir.path().join("original-upload-root-never-opened");
        let files = dir.path().join("file-export");
        let relative = format!("client/{CODEX}/note-00000000-0000-4000-8000-000000000003.txt");
        fs::create_dir_all(files.join("client").join(CODEX)).unwrap();
        fs::write(files.join(&relative), b"KEEP ATTACHMENT").unwrap();
        migration_snapshot::capture(&files, &source.join("uploads"), ENV).unwrap();
        write(
            &source.join("uploads.json"),
            &json!({"version":1,"files":[{"message":PROMPT,"original":original.join(&relative),"relative":relative,"bytes":15,"sha256":format!("{:x}",Sha256::digest(b"KEEP ATTACHMENT"))}]}),
        );
        let uploads =
            json!({"original_root":original,"snapshot":"uploads","manifest":"uploads.json"});
        let messages = json!([
            {"id":PROMPT,"role":"user","turnId":"old-turn","text":format!("See [note](<{}>)",original.join(&relative).display()),"createdAt":TIME,"sequence":1},
            {"id":"reply","role":"assistant","turnId":"old-turn","text":"Keep reply","createdAt":TIME,"sequence":2}
        ]);
        write(
            &source.join("connection.json"),
            &json!({"version":1,"settings":{"controlEnabled":true,"tunnelMode":"quick"},
            "clients":[{"id":"test-phone","label":"Test","os":"android","deviceType":"mobile","scopes":auth::SCOPES.split(' ').collect::<Vec<_>>(),"createdAt":TIME,"credentialDigest":auth::digest("TEST_ONLY_PHONE")}],
            "threadAliases":[{"remoteThreadId":"codex","nativeThreadId":CODEX,"projectId":"project","title":"Keep Codex","cwd":dir.path(),"instanceId":"openai","model":"model","nativeModelProvider":"openai","runtimeMode":"approval-required","interactionMode":"default","createdAt":TIME,"updatedAt":TIME}],"taskSelections":[]}),
        );
        write(
            &source.join("settings.json"),
            &json!({"enabled":true,"projects":[]}),
        );
        write(&source.join("projects.json"), &json!([]));
        write(
            &source.join("codex.json"),
            &json!({"version":1,"threadId":"codex","nativeId":CODEX,"nativeStatus":"idle","hasPendingApprovals":false,"hasPendingUserInput":false,"hasQueuedPrompts":false,"hasActionableProposedPlan":false,"updatedAt":TIME,"latestUserMessageAt":TIME,"latestTurn":{"turnId":"old-turn","state":"completed","startedAt":TIME,"completedAt":TIME},"turns":[{"publicId":"old-turn","nativeId":"native-old-turn","status":"completed"}],"messages":messages,"activities":[],"proposedPlans":[]}),
        );
        let mut claude = vec![];
        for i in 0..count {
            let id = format!("claude-{i}");
            let native = format!("00000000-0000-4000-8000-{:012}", 100 + i);
            let history = format!("claude-{i}.json");
            write(
                &source.join(&history),
                &json!({"version":1,"nativeId":native,"thread":{"id":id,"projectId":"project","title":"Keep Claude","worktreePath":dir.path(),"createdAt":TIME,"updatedAt":TIME,"modelSelection":{"instanceId":"claude-code","model":"sonnet"},"runtimeMode":"approval-required","interactionMode":"default","session":{"threadId":id,"status":"ready"},"messages":messages,"activities":[],"proposedPlans":[]}}),
            );
            claude.push(json!({"id":id,"history":history,"uploads":if i==0 {uploads.clone()} else {Value::Null}}));
        }
        write(
            &source.join("stage.json"),
            &json!({"version":1,"connection":"connection.json","claude_settings":"settings.json","codex_projects":"projects.json","codex":[{"id":"codex","history":"codex.json","uploads":uploads}],"claude":claude}),
        );
        let snapshot = dir.path().join("snapshot");
        let generation = dir.path().join("generation");
        Self {
            _dir: dir,
            source,
            snapshot,
            generation,
            original,
        }
    }
    fn capture(&self) {
        migration_snapshot::capture(&self.source, &self.snapshot, ENV).unwrap();
    }
    fn codex_native_export(&self, mutate: impl FnOnce(&mut Vec<Value>)) {
        let profile = self._dir.path().join("codex-export");
        fs::create_dir_all(profile.join("sessions")).unwrap();
        let mut events = vec![
            json!({"type":"session_meta","payload":{"id":CODEX}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":CODEX,"turn_id":"native-old-turn","item":{"type":"UserMessage","id":"native-user","client_id":PROMPT,"content":[{"type":"text","text":"Native content is retained only in the offline snapshot"}]}}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":CODEX,"turn_id":"native-old-turn","item":{"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":"Keep reply"}]}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"native-old-turn"}}),
        ];
        mutate(&mut events);
        let raw = events.iter().map(|v| format!("{v}\n")).collect::<String>();
        fs::write(profile.join("sessions/test.jsonl"), raw).unwrap();
        migration_snapshot::capture(&profile, &self.source.join("codex-native"), ENV).unwrap();
        let mut plan = read(&self.source.join("stage.json"));
        plan["codex"][0]["native"] =
            json!({"snapshot":"codex-native","transcript":"sessions/test.jsonl"});
        write(&self.source.join("stage.json"), &plan);
    }
    fn run(&self) -> Result<Value, String> {
        legacy_stage::run(&self.snapshot, &self.generation, ENV)
    }
    fn db(&self) -> Connection {
        Connection::open(self.generation.join("state/native-connect.sqlite")).unwrap()
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        c.arg("--legacy-stage")
            .arg(&self.snapshot)
            .arg(&self.generation)
            .arg(ENV);
        c
    }
}

#[test]
fn verified_owned_activation_is_idempotent_and_unused_generation_rolls_back_atomically() {
    use remodex_connect_native::{launch_config, legacy_activation};
    let f = Fixture::new(0);
    f.codex_native_export(|_| {});
    f.capture();
    f.run().unwrap();
    let state = f.generation.join("state");
    let original = rows(&f.db());
    let path = f.activation_config();
    let config = launch_config::load(&path).unwrap();
    assert!(legacy_activation::before_start(&state, Some(&config)).is_err());
    assert_eq!(legacy_activation::run(&state, &path).unwrap()["chats"], 1);
    assert_eq!(
        legacy_activation::run(&state, &path).unwrap()["alreadyActive"],
        true
    );
    let mut wrong = config.clone();
    wrong.native_profile_handoff = None;
    assert!(legacy_activation::before_start(&state, Some(&wrong)).is_err());
    assert!(legacy_activation::before_start(&state, None).is_err());
    assert_eq!(rows(&f.db()), original);
    assert_eq!(
        legacy_rollback::run(&state, false).unwrap()["phase"],
        "rollback-verified"
    );
    assert_eq!(rows(&f.db()), original);
    legacy_rollback::run(&state, true).unwrap();
    assert!(rows(&f.db()).is_empty());
    assert!(legacy_activation::before_start(&state, Some(&config)).is_err());
    assert!(legacy_activation::run(&state, &path).is_err());
    assert!(config
        .native_profile_handoff
        .unwrap()
        .join("attempt-1/working/codex/data/sessions/test.jsonl")
        .exists());
    migration_snapshot::verify_tree(
        &f.source.join("codex-native"),
        &f._dir.path().join("codex-export"),
        ENV,
    )
    .unwrap();
    assert_eq!(
        legacy_rollback::run(&state, true).unwrap()["alreadyRolledBack"],
        true
    );
}

#[test]
fn startup_claim_preserves_new_history_and_never_reimports_it_on_restart_or_rollback() {
    use remodex_connect_native::{launch_config, legacy_activation};
    let f = Fixture::new(0);
    f.codex_native_export(|_| {});
    f.capture();
    f.run().unwrap();
    let state = f.generation.join("state");
    let path = f.activation_config();
    let config = launch_config::load(&path).unwrap();
    legacy_activation::run(&state, &path).unwrap();
    legacy_activation::before_start(&state, Some(&config)).unwrap();
    f.db().execute("UPDATE chat_rows SET data=json_set(data,'$.text','NEW WORK MUST STAY') WHERE kind='messages' AND id='reply'",[]).unwrap();
    let after = rows(&f.db());
    assert!(after.iter().any(|s| s.contains("NEW WORK MUST STAY")));
    legacy_activation::before_start(&state, Some(&config)).unwrap();
    assert!(legacy_rollback::run(&state, false)
        .unwrap_err()
        .contains("current state"));
    assert!(legacy_rollback::run(&state, true).is_err());
    assert_eq!(rows(&f.db()), after);
    assert_eq!(
        legacy_activation::run(&state, &path).unwrap()["alreadyActive"],
        true
    );
    assert_eq!(rows(&f.db()), after);
}

#[tokio::test]
async fn runtime_api_requires_the_explicit_migration_configuration() {
    use remodex_connect_native::{auth::Auth, launch_config, legacy_activation, server::App};
    let f = Fixture::new(0);
    f.codex_native_export(|_| {});
    f.capture();
    f.run().unwrap();
    let state = f.generation.join("state");
    let path = f.activation_config();
    let config = launch_config::load(&path).unwrap();
    legacy_activation::run(&state, &path).unwrap();
    legacy_activation::before_start(&state, Some(&config)).unwrap();
    assert!(App::new(Auth::new(Store::open(&state).unwrap()), "isolated").is_err());
    let mut wrong = config.clone();
    wrong.native_profile_handoff = None;
    assert!(App::with_configuration(
        Auth::new(Store::open(&state).unwrap()),
        "isolated",
        Some(&wrong)
    )
    .is_err());
    let app = App::with_configuration(
        Auth::new(Store::open(&state).unwrap()),
        "isolated",
        Some(&config),
    )
    .unwrap();
    let before = rows(&f.db());
    let chats = remodex_connect_native::chat::Chats::start(app.auth.clone(), config)
        .await
        .unwrap();
    let result = chats
        .call(
            remodex_connect_native::chat::AgentKind::Codex,
            "test-phone",
            "orchestration.subscribeThread",
            json!({"threadId":"codex"}),
        )
        .await
        .unwrap();
    assert_eq!(result["snapshot"]["thread"]["session"]["status"], "ready");
    assert_eq!(
        result["snapshot"]["thread"]["session"]["statusConfidence"],
        "confirmed"
    );
    assert_eq!(rows(&f.db()), before);
    chats
        .call(
            remodex_connect_native::chat::AgentKind::Codex,
            "local",
            "native.shutdown",
            json!({}),
        )
        .await
        .unwrap();
    drop(chats);
    drop(app);
}

#[test]
fn saving_a_loaded_page_keeps_legacy_message_bytes_and_native_identity_proofs() {
    let f = Fixture::new(1);
    f.codex_native_export(|_| {});
    for name in ["codex.json", "claude-0.json"] {
        let file = f.source.join(name);
        let mut value = read(&file);
        let thread = if name == "codex.json" {
            &mut value
        } else {
            &mut value["thread"]
        };
        for (i, row) in thread["messages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            row["sequence"] = json!(900 + i);
        }
        write(&file, &value);
    }
    f.capture();
    f.run().unwrap();
    let old = rows(&f.db());
    let mut store = Store::open(&f.generation.join("state")).unwrap();
    for id in ["codex", "claude-0"] {
        let mut state = store.load_chat(id).unwrap();
        assert_ne!(state["thread"]["messages"][0]["sequence"], 900);
        state["paused"] = json!(true);
        store.save_chat(&state, None).unwrap();
    }
    assert_eq!(rows(&f.db()), old);
    assert!(store
        .legacy_codex_replayed_item(
            "codex",
            CODEX,
            "old-turn",
            "native-old-turn",
            &json!({"id":"native-user","type":"userMessage","clientId":PROMPT})
        )
        .unwrap());
}

#[test]
fn changed_working_profile_or_missing_native_proof_blocks_activation_and_retains_rows() {
    use remodex_connect_native::{launch_config, legacy_activation};
    for missing in [true, false] {
        let f = Fixture::new(0);
        f.codex_native_export(|_| {});
        if missing {
            let mut plan = read(&f.source.join("stage.json"));
            plan["codex"][0].as_object_mut().unwrap().remove("native");
            write(&f.source.join("stage.json"), &plan);
        }
        f.capture();
        f.run().unwrap();
        let before = rows(&f.db());
        let state = f.generation.join("state");
        let path = f.activation_config();
        let config = launch_config::load(&path).unwrap();
        if missing {
            assert!(legacy_activation::run(&state, &path).is_err());
        } else {
            legacy_activation::run(&state, &path).unwrap();
            fs::write(
                config
                    .native_profile_handoff
                    .as_ref()
                    .unwrap()
                    .join("attempt-1/working/codex/data/new-work"),
                b"KEEP",
            )
            .unwrap();
            assert!(legacy_activation::before_start(&state, Some(&config)).is_err());
            assert!(legacy_activation::run(&state, &path).is_err());
            assert!(legacy_rollback::run(&state, true).is_err());
            assert_eq!(
                f.db()
                    .query_row("SELECT phase FROM legacy_activation", [], |r| r
                        .get::<_, String>(0))
                    .unwrap(),
                "active"
            );
        }
        assert_eq!(rows(&f.db()), before);
    }
}
fn write(path: &std::path::Path, value: &Value) {
    fs::write(path, format!(" {value}\n")).unwrap();
}
fn read(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn rows(db: &Connection) -> Vec<String> {
    db.prepare("SELECT data FROM chat_rows ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn stages_both_agents_pairing_and_uploads_then_retry_and_atomic_rollback_keep_sources() {
    let f = Fixture::new(2);
    f.capture();
    let before = fs::read(f.snapshot.join("snapshot.json")).unwrap();
    let report = f.run().unwrap();
    assert_eq!(report["codexChats"], 1);
    assert_eq!(report["claudeChats"], 2);
    let db = f.db();
    let old = rows(&db);
    assert_eq!(old.len(), 6);
    assert_eq!(
        db.query_row("SELECT count(*) FROM clients", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(f.run().unwrap()["resumed"], true);
    assert_eq!(rows(&db), old);
    let store = Store::open(&f.generation.join("state")).unwrap();
    assert!(store.assert_migration_ready().is_err());
    let relative = format!("client/{CODEX}/note-00000000-0000-4000-8000-000000000003.txt");
    for chat in ["codex", "claude-0"] {
        let path = store
            .resolve_legacy_upload(chat, f.original.join(&relative).to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), b"KEEP ATTACHMENT");
    }
    drop(store);
    legacy_rollback::run(&f.generation.join("state"), true).unwrap();
    assert!(f.run().is_err()); // explicit rollback never implicitly resurrects chats
    assert_eq!(fs::read(f.snapshot.join("snapshot.json")).unwrap(), before);
    assert!(!f.original.exists());
    assert!(f.generation.join("export/data/codex.json").exists());
}

#[test]
fn retry_rejects_new_messages_changed_settings_and_asset_ledgers_without_repair() {
    for change in [
        "UPDATE chat_rows SET data=json_set(data,'$.text','Newer message') WHERE kind='messages'",
        "UPDATE metadata SET value='false' WHERE key='phone_control_enabled'",
        "UPDATE storage_assets SET bytes=bytes+1",
    ] {
        let f = Fixture::new(1);
        f.capture();
        f.run().unwrap();
        let db = f.db();
        db.execute(change, []).unwrap();
        let old = rows(&db);
        let bytes: i64 = db
            .query_row("SELECT sum(bytes) FROM storage_assets", [], |r| r.get(0))
            .unwrap();
        assert!(f.run().is_err());
        assert_eq!(rows(&db), old);
        assert_eq!(
            db.query_row("SELECT sum(bytes) FROM storage_assets", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            bytes
        );
    }
}

#[test]
fn foreign_destination_live_owner_wrong_export_and_unsafe_paths_are_refused() {
    let f = Fixture::new(1);
    f.capture();
    fs::create_dir(&f.generation).unwrap();
    fs::write(f.generation.join("keep"), b"UNCHANGED").unwrap();
    assert!(f.run().is_err());
    assert!(!f.generation.join("server.lock").exists());
    assert_eq!(fs::read(f.generation.join("keep")).unwrap(), b"UNCHANGED");
    let f = Fixture::new(1);
    f.capture();
    f.run().unwrap();
    let old = rows(&f.db());
    let lease = profile_lock::acquire(&f.generation.join("state")).unwrap();
    assert!(f.run().is_err());
    drop(lease);
    write(
        &f.source.join("settings.json"),
        &json!({"enabled":false,"projects":[]}),
    );
    let second = f._dir.path().join("different-snapshot");
    migration_snapshot::capture(&f.source, &second, ENV).unwrap();
    assert!(legacy_stage::run(&second, &f.generation, ENV).is_err());
    assert_eq!(rows(&f.db()), old);
    for path in [
        "../connection.json",
        "/connection.json",
        "C:\\connection.json",
    ] {
        let f = Fixture::new(0);
        let mut p = read(&f.source.join("stage.json"));
        p["connection"] = json!(path);
        write(&f.source.join("stage.json"), &p);
        f.capture();
        assert!(f.run().is_err());
    }
}

#[test]
fn missing_history_and_duplicate_plan_entries_do_not_claim_complete_stage() {
    for duplicate in [false, true] {
        let f = Fixture::new(1);
        let mut p = read(&f.source.join("stage.json"));
        if duplicate {
            let c = p["claude"][0].clone();
            p["claude"].as_array_mut().unwrap().push(c);
        } else {
            p["codex"] = json!([]);
        }
        write(&f.source.join("stage.json"), &p);
        f.capture();
        assert!(f.run().is_err());
        let journal = Connection::open(f.generation.join("staging.sqlite")).unwrap();
        assert_eq!(
            journal
                .query_row("SELECT phase FROM staging", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "preparing"
        );
    }
}

#[test]
fn killed_import_resumes_committed_chats_once_and_cli_does_not_print_private_export() {
    let f = Fixture::new(64);
    f.capture();
    let mut child = f
        .command()
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut reached = false;
    while Instant::now() < deadline {
        let path = f.generation.join("state/native-connect.sqlite");
        if path.exists() {
            if let Ok(db) =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            {
                let n = db
                    .query_row(
                        "SELECT count(*) FROM legacy_claude_binding_import",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0);
                if n > 0 {
                    reached = true;
                    break;
                }
            }
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(reached, "did not reach a committed Claude import");
    let db = f.db();
    let old = rows(&db);
    assert!(!old.is_empty());
    let n: i64 = db
        .query_row("SELECT count(*) FROM chats", [], |r| r.get(0))
        .unwrap();
    assert!(n < 65);
    drop(db);
    let result = f.command().output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["resumed"], true);
    let now = rows(&f.db());
    assert_eq!(now.len(), 130);
    assert!(now.starts_with(&old));
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(
        !output.contains("TEST_ONLY")
            && !output.contains("Keep reply")
            && !output.contains("original-upload-root")
    );
    assert_eq!(f.run().unwrap()["claudeChats"], 64);
}

#[test]
fn retries_incomplete_database_initialization_and_unreceipted_partial_file_copies() {
    for partial_files in [false, true] {
        let f = Fixture::new(1);
        f.capture();
        f.run().unwrap();
        let journal = Connection::open(f.generation.join("staging.sqlite")).unwrap();
        journal
            .execute("UPDATE staging SET phase='preparing'", [])
            .unwrap();
        drop(journal);
        if partial_files {
            let db = f.db();
            db.execute_batch("DELETE FROM legacy_uploads; DELETE FROM legacy_upload_evidence; DELETE FROM legacy_upload_display; DELETE FROM legacy_upload_display_import;").unwrap();
            let bundle = fs::read_dir(
                f.generation
                    .join("state/legacy-uploads")
                    .canonicalize()
                    .unwrap(),
            )
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
            let copied = bundle
                .join("data/client")
                .join(CODEX)
                .join("note-00000000-0000-4000-8000-000000000003.txt");
            for path in [copied, bundle.join("restored.json")] {
                fs::remove_file(&path).unwrap();
                let removed = db
                    .execute(
                        "DELETE FROM storage_assets WHERE path=?1",
                        [path.to_str().unwrap()],
                    )
                    .unwrap();
                assert_eq!(removed, 1, "simulate an unindexed interrupted copy");
            }
        } else {
            fs::remove_dir_all(f.generation.join("state")).unwrap();
            fs::create_dir(f.generation.join("state")).unwrap();
            // Same empty schema that may survive a crash between startup DDLs.
            f.db().execute_batch("CREATE TABLE clients(id TEXT PRIMARY KEY,label TEXT NOT NULL,os TEXT NOT NULL,created_at INTEGER NOT NULL,expires_at INTEGER NOT NULL,credential_digest TEXT NOT NULL UNIQUE,installation_digest TEXT);").unwrap();
        }
        assert_eq!(f.run().unwrap()["resumed"], true);
        assert_eq!(rows(&f.db()).len(), 4);
        legacy_rollback::run(&f.generation.join("state"), false)
            .unwrap_or_else(|error| panic!("partial_files={partial_files}: {error}"));
    }
}

#[cfg(unix)]
#[test]
fn rollback_accepts_an_alias_of_the_generation_parent_without_reindexing() {
    let mut f = Fixture::new(1);
    let alias = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(f._dir.path(), alias.path().join("parent")).unwrap();
    f.generation = alias.path().join("parent/generation");
    f.capture();
    f.run().unwrap();
    let db = f.db();
    let before = rows(&db);
    let assets = |db: &Connection| -> Vec<(String, i64)> {
        db.prepare("SELECT path,bytes FROM storage_assets ORDER BY path")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before_assets = assets(&db);
    legacy_rollback::run(&f.generation.join("state"), false).unwrap();
    assert_eq!(rows(&db), before);
    assert_eq!(assets(&db), before_assets);
    let (path, bytes) = &before_assets[0];
    let root = f.generation.join("state");
    let duplicate = root.join(
        std::path::Path::new(path)
            .strip_prefix(root.canonicalize().unwrap())
            .unwrap(),
    );
    db.execute(
        "INSERT INTO storage_assets VALUES(?1,?2)",
        rusqlite::params![duplicate.to_str().unwrap(), bytes],
    )
    .unwrap();
    let changed = assets(&db);
    assert!(legacy_rollback::run(&root, false).is_err());
    assert_eq!(rows(&db), before);
    assert_eq!(assets(&db), changed);
}

#[test]
fn owned_cli_only_claude_activates_without_inventing_a_shared_link_or_cursor() {
    use remodex_connect_native::legacy_activation;
    for (linked, remote_control) in [(false, false), (false, true), (true, false), (true, true)] {
        let f = Fixture::new(1);
        f.codex_native_export(|_| {});
        let mut public = read(&f.source.join("claude-0.json"));
        if linked {
            public["bridgeId"] = json!("cse_saved_link_requires_its_cursor");
            write(&f.source.join("claude-0.json"), &public);
        }
        let native = public["nativeId"].as_str().unwrap();
        let profile = f._dir.path().join("claude-export");
        fs::create_dir_all(profile.join("projects/fixture")).unwrap();
        fs::write(profile.join("settings.json"), b"{}").unwrap();
        let transcript = format!("projects/fixture/{native}.jsonl");
        let user = json!({"type":"user","uuid":PROMPT,"sessionId":native,"parentUuid":null,"message":{"role":"user","content":public["thread"]["messages"][0]["text"]}});
        let reply = json!({"type":"assistant","uuid":"00000000-0000-4000-8000-000000000002","sessionId":native,"parentUuid":PROMPT,"message":{"id":"reply","role":"assistant","content":[{"type":"text","text":"Keep reply"}]}});
        let original = format!("{user}\n{reply}\n");
        fs::write(profile.join(&transcript), &original).unwrap();
        migration_snapshot::capture(&profile, &f.source.join("native"), ENV).unwrap();
        let mut plan = read(&f.source.join("stage.json"));
        plan["claude"][0]["native"] = json!({"snapshot":"native","transcript":transcript});
        write(&f.source.join("stage.json"), &plan);
        f.capture();
        f.run().unwrap();
        let config = f.activation_config_using(Some(profile.clone()));
        let mut value = read(&config);
        value["claude"]["remote_control"] = json!(remote_control);
        write(&config, &value);
        let before = rows(&f.db());
        let result = legacy_activation::run(&f.generation.join("state"), &config);
        assert_eq!(
            result.is_ok(),
            !linked,
            "A linked chat still requires a verified saved cursor"
        );
        assert_eq!(rows(&f.db()), before);
        assert_eq!(
            fs::read_to_string(profile.join(&transcript)).unwrap(),
            original
        );
        let db = f.db();
        assert_eq!(
            db.query_row("SELECT count(*) FROM native_sync", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let saved: Value = serde_json::from_str(
            &db.query_row("SELECT data FROM chats WHERE id='claude-0'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["remote_identity"].is_null(), !linked);
        assert_eq!(saved["native_id"], native);
        legacy_rollback::run(&f.generation.join("state"), true).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM chats", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn stages_verified_native_claude_history_and_keeps_startup_blocked() {
    let f = Fixture::new(1);
    let mut plan = read(&f.source.join("stage.json"));
    let public = read(&f.source.join("claude-0.json"));
    let native = public["nativeId"].as_str().unwrap();
    let profile = f._dir.path().join("native-export");
    fs::create_dir_all(profile.join("projects/fixture")).unwrap();
    let transcript = format!("projects/fixture/{native}.jsonl");
    let user = json!({"type":"user","uuid":PROMPT,"sessionId":native,"parentUuid":null,"message":{"role":"user","content":public["thread"]["messages"][0]["text"]}});
    let reply = json!({"type":"assistant","uuid":"00000000-0000-4000-8000-000000000002","sessionId":native,"parentUuid":PROMPT,"message":{"id":"reply","role":"assistant","content":[{"type":"text","text":"Keep reply"}]}});
    fs::write(profile.join(&transcript), format!("{user}\n{reply}\n")).unwrap();
    migration_snapshot::capture(&profile, &f.source.join("native"), ENV).unwrap();
    plan["claude"][0]["native"] = json!({"snapshot":"native","transcript":transcript});
    write(&f.source.join("stage.json"), &plan);
    f.capture();
    f.run().unwrap();
    f.run().unwrap();
    assert_eq!(
        f.db()
            .query_row(
                "SELECT count(*) FROM legacy_claude_native WHERE phase='prepared'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert!(Store::open(&f.generation.join("state"))
        .unwrap()
        .assert_migration_ready()
        .is_err());
    legacy_rollback::run(&f.generation.join("state"), true).unwrap();
    assert!(profile.join(transcript).exists());
}

#[test]
fn codex_native_ids_survive_staging_retry_and_rollback_with_original_history() {
    let f = Fixture::new(1);
    f.codex_native_export(|_| {});
    f.capture();
    f.run().unwrap();
    let db = f.db();
    let before = rows(&db);
    let report: String = db
        .query_row(
            "SELECT report FROM legacy_codex_native WHERE chat='codex'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let parsed: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(parsed["items"].as_array().unwrap().len(), 2);
    assert!(parsed["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["native"] == "native-user" && i["public"] == PROMPT));
    assert!(!report.contains("Native content"));
    f.run().unwrap();
    assert_eq!(rows(&db), before);
    let store = Store::open(&f.generation.join("state")).unwrap();
    assert!(store.assert_migration_ready().is_err());
    let user = json!({"type":"userMessage","id":"native-user","clientId":PROMPT});
    assert!(store
        .legacy_codex_replayed_item("codex", CODEX, "old-turn", "native-old-turn", &user)
        .unwrap());
    assert!(store
        .legacy_codex_replayed_item(
            "codex",
            CODEX,
            "old-turn",
            "native-old-turn",
            &json!({"type":"agentMessage","id":"reply"})
        )
        .unwrap());
    assert!(!store
        .legacy_codex_replayed_item("codex", CODEX, "new-turn", "native-new-turn", &user)
        .unwrap());
    assert!(store
        .legacy_codex_replayed_item(
            "codex",
            "wrong-native",
            "old-turn",
            "native-old-turn",
            &user
        )
        .is_err());
    assert!(store
        .legacy_codex_replayed_item(
            "codex",
            CODEX,
            "old-turn",
            "native-old-turn",
            &json!({"type":"userMessage","id":"unknown"})
        )
        .is_err());
    assert!(store
        .legacy_codex_replayed_item(
            "codex",
            CODEX,
            "old-turn",
            "native-old-turn",
            &json!({"type":"userMessage","id":"native-user","clientId":"conflicting-client"})
        )
        .is_err());
    let original: String = db
        .query_row(
            "SELECT data FROM chat_rows WHERE chat='codex' AND id=?1",
            [PROMPT],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("UPDATE chat_rows SET data=json_set(data,'$.text','Changed history') WHERE chat='codex' AND id=?1",[PROMPT]).unwrap();
    assert!(store
        .legacy_codex_replayed_item("codex", CODEX, "old-turn", "native-old-turn", &user)
        .is_err());
    db.execute(
        "UPDATE chat_rows SET data=?1 WHERE chat='codex' AND id=?2",
        rusqlite::params![original, PROMPT],
    )
    .unwrap();
    drop(store);
    legacy_rollback::run(&f.generation.join("state"), true).unwrap();
    assert_eq!(
        db.query_row("SELECT phase FROM legacy_codex_native", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "rolled-back"
    );
    assert!(f.generation.join("state/legacy-native").exists());
    assert!(f.run().is_err());
}

#[test]
fn missing_wrong_ambiguous_and_incomplete_codex_native_identities_stay_offline() {
    for change in 0..8 {
        let f = Fixture::new(1);
        f.codex_native_export(|events| match change {
            0 => events[0]["payload"]["id"] = json!("wrong-session"),
            1 => events[1]["payload"]["item"]["client_id"] = json!("unknown-public-id"),
            2 => events[2]["payload"]["item"]["id"] = json!("unknown-reply-id"),
            3 => {
                events.pop();
            }
            4 => events[1]["payload"]["turn_id"] = json!("wrong-turn"),
            5 => events.push(events[1].clone()),
            6 => events[1]["payload"]["thread_id"] = json!("wrong-thread"),
            _ => events.push(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"native-old-turn"}})),
        });
        f.capture();
        assert!(f.run().is_err(), "change={change}");
        assert!(Store::open(&f.generation.join("state"))
            .unwrap()
            .assert_migration_ready()
            .is_err());
        assert_eq!(rows(&f.db()).len(), 2); // Imported Codex history is retained.
    }
}

#[test]
fn a_changed_copied_codex_rollout_refuses_retry_and_rollback_without_repair() {
    let f = Fixture::new(1);
    f.codex_native_export(|_| {});
    f.capture();
    f.run().unwrap();
    let db = f.db();
    let before = rows(&db);
    let bundle: String = db
        .query_row("SELECT bundle FROM legacy_codex_native", [], |r| r.get(0))
        .unwrap();
    let file = f
        .generation
        .join("state/legacy-native")
        .join(bundle)
        .join("data/sessions/test.jsonl");
    fs::write(&file, b"changed native history\n").unwrap();
    assert!(f.run().is_err());
    assert!(legacy_rollback::run(&f.generation.join("state"), true).is_err());
    assert_eq!(rows(&db), before);
    assert_eq!(fs::read(file).unwrap(), b"changed native history\n");
}
