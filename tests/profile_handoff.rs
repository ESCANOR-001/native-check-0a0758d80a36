use remodex_connect_native::{
    migration_snapshot,
    profile_handoff::{self, Input, Plan},
};
use rusqlite::{params, Connection};
use serde_json::json;
use std::{fs, path::PathBuf};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const ID: &str = "00000000-0000-4000-8000-000000000001";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    plan: Plan,
}
impl Fixture {
    fn new() -> Self {
        Self::with_environment(ENV)
    }
    fn with_environment(environment: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("old");
        fs::create_dir(&original).unwrap();
        let codex = original.join("codex");
        let claude = original.join("claude");
        fs::create_dir_all(codex.join("sessions")).unwrap();
        fs::create_dir_all(claude.join("projects/fixture")).unwrap();
        let rollout = codex.join("sessions/rollout.jsonl");
        fs::write(
            &rollout,
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","payload":{"id":ID,"cwd":"workspace"}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"OLD_PROMPT"}})
            ),
        )
        .unwrap();
        fs::write(claude.join(format!("projects/fixture/{ID}.jsonl")), format!("{}\n", json!({"type":"user","uuid":ID,"message":{"role":"user","content":"OLD_CLAUDE_PROMPT"}}))).unwrap();
        for root in [&codex, &claude] {
            fs::write(root.join("unknown-future-metadata"), "KEEP_UNKNOWN").unwrap();
        }
        let db = Connection::open(codex.join("state_5.sqlite")).unwrap();
        db.execute_batch(include_str!("fixtures/storage-state.sql"))
            .unwrap();
        db.execute("INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode) VALUES(?1,?2,123,456,'cli','openai','workspace','Original title','read-only','on-request')", params![ID,rollout.to_str()]).unwrap();
        db.execute_batch("CREATE TABLE unknown_future(value BLOB); INSERT INTO unknown_future VALUES(x'000102'); CREATE TRIGGER unexpected_timestamp AFTER UPDATE ON threads BEGIN UPDATE threads SET updated_at=9999 WHERE id=NEW.id; END;").unwrap();
        drop(db);
        let plan = Plan {
            version: 1,
            profile_mode: Default::default(),
            environment: environment.into(),
            codex: Input {
                original_profile: codex,
                snapshot: dir.path().join("export-codex"),
            },
            claude: Input {
                original_profile: claude,
                snapshot: dir.path().join("export-claude"),
            },
        };
        for input in [&plan.codex, &plan.claude] {
            migration_snapshot::capture(&input.original_profile, &input.snapshot, environment)
                .unwrap();
        }
        Self {
            root: dir.path().join("handoff"),
            plan,
            _dir: dir,
        }
    }
    fn prepare(&self) {
        profile_handoff::prepare(&self.root, &self.plan).unwrap();
    }
    fn activate(&self) -> profile_handoff::Session {
        profile_handoff::activate(
            &self.root,
            ENV,
            &self.plan.codex.original_profile,
            &self.plan.claude.original_profile,
        )
        .unwrap()
    }
    fn original_intact(&self) {
        for input in [&self.plan.codex, &self.plan.claude] {
            migration_snapshot::verify_tree(
                &input.snapshot,
                &input.original_profile,
                &self.plan.environment,
            )
            .unwrap();
        }
    }
}
#[test]
fn existing_install_keeps_agent_paths_and_new_history_without_rebasing_or_restoring() {
    let mut f = Fixture::new();
    f.plan.profile_mode = profile_handoff::ProfileMode::Existing;
    f.prepare();
    f.prepare();
    assert!(!f.root.join("attempt-1").exists());
    let lease = f.activate();
    assert!(lease.retains_original_profiles());
    assert_eq!(
        lease.profiles(),
        (
            f.plan.codex.original_profile.canonicalize().unwrap(),
            f.plan.claude.original_profile.canonicalize().unwrap()
        )
    );
    f.original_intact();
    drop(lease);
    let path = f
        .plan
        .claude
        .original_profile
        .join(format!("projects/fixture/{ID}.jsonl"));
    let old = fs::read(&path).unwrap();
    let new = [old.as_slice(), b"NEW DESKTOP MESSAGE\n"].concat();
    fs::write(&path, &new).unwrap();
    fs::write(
        f.plan.codex.original_profile.join("new-native-file"),
        b"NEW CODEX MESSAGE",
    )
    .unwrap();
    let lease = f.activate();
    assert!(lease.retains_original_profiles());
    drop(lease);
    assert!(profile_handoff::rollback(&f.root, ENV)
        .unwrap_err()
        .contains("current agent directories"));
    assert_eq!(fs::read(path).unwrap(), new);
    migration_snapshot::verify(&f.plan.claude.snapshot, ENV).unwrap();
}

#[test]
fn existing_install_refuses_source_changes_before_start_and_keeps_unused_rollback_safe() {
    let mut f = Fixture::new();
    f.plan.profile_mode = profile_handoff::ProfileMode::Existing;
    f.prepare();
    let path = f
        .plan
        .claude
        .original_profile
        .join("unknown-future-metadata");
    fs::write(&path, b"CHANGED BEFORE CUTOVER").unwrap();
    assert!(profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    assert!(profile_handoff::rollback(&f.root, ENV).is_err());
    fs::write(&path, b"KEEP_UNKNOWN").unwrap();
    profile_handoff::rollback(&f.root, ENV).unwrap();
    f.original_intact();
    assert!(!f.root.join("attempt-1").exists());
}

#[test]
fn handoff_rebases_only_copied_rollout_paths_and_keeps_all_metadata() {
    let f = Fixture::new();
    f.prepare();
    let lease = f.activate();
    let (codex, claude) = lease.profiles();
    let db = Connection::open(codex.join("state_5.sqlite")).unwrap();
    let row: (String, String, i64, i64, i64) = db
        .query_row(
            "SELECT id,rollout_path,updated_at,updated_at_ms,recency_at_ms FROM threads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            ID.into(),
            codex
                .join("sessions/rollout.jsonl")
                .to_str()
                .unwrap()
                .into(),
            456,
            456000,
            456000
        )
    );
    assert_eq!(
        db.query_row("SELECT value FROM unknown_future", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        [0, 1, 2]
    );
    for root in [&codex, &claude] {
        assert_eq!(
            fs::read_to_string(root.join("unknown-future-metadata")).unwrap(),
            "KEEP_UNKNOWN"
        );
    }
    assert!(fs::read_to_string(codex.join("sessions/rollout.jsonl"))
        .unwrap()
        .contains("OLD_PROMPT"));
    f.original_intact();
}
#[test]
fn unused_generation_rolls_back_idempotently_without_deleting_any_files() {
    let f = Fixture::new();
    f.prepare();
    assert_eq!(
        profile_handoff::prepare(&f.root, &f.plan).unwrap()["phase"],
        "prepared"
    );
    let lease = f.activate();
    let paths = lease.profiles();
    drop(lease);
    assert_eq!(
        profile_handoff::rollback(&f.root, ENV).unwrap()["phase"],
        "rolled-back"
    );
    assert!(profile_handoff::rollback(&f.root, ENV).is_ok());
    assert!(paths.0.join("sessions/rollout.jsonl").exists());
    assert!(paths.1.join("unknown-future-metadata").exists());
    assert!(profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    f.original_intact();
}
#[test]
fn active_owner_excludes_a_second_runtime_prepare_and_rollback() {
    let f = Fixture::new();
    f.prepare();
    let _lease = f.activate();
    assert!(profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    assert!(profile_handoff::rollback(&f.root, ENV).is_err());
    assert!(profile_handoff::prepare(&f.root, &f.plan).is_err());
    f.original_intact();
}
#[test]
fn new_history_survives_restart_and_blocks_destructive_data_rollback() {
    let f = Fixture::new();
    f.prepare();
    let lease = f.activate();
    let (_, claude) = lease.profiles();
    let file = claude.join(format!("projects/fixture/{ID}.jsonl"));
    let mut data = fs::read(&file).unwrap();
    data.extend_from_slice(b"NEW_HISTORY\n");
    fs::write(&file, &data).unwrap();
    drop(lease);
    let lease = f.activate();
    assert_eq!(fs::read(&file).unwrap(), data);
    drop(lease);
    let err = profile_handoff::rollback(&f.root, ENV).unwrap_err();
    assert!(err.contains("new or changed data"));
    assert_eq!(fs::read(file).unwrap(), data);
    f.original_intact();
}
#[test]
fn changed_checkpoint_or_wrong_environment_cannot_activate() {
    let f = Fixture::new();
    f.prepare();
    assert!(profile_handoff::activate(
        &f.root,
        "opencodex-aaaaaaaaaaaaaaaaaaaaaaaa",
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    fs::write(
        f.root
            .join("attempt-1/working/claude/data/unknown-future-metadata"),
        "CHANGED",
    )
    .unwrap();
    assert!(profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    f.original_intact();
}
#[test]
fn interrupted_rebase_is_retained_and_retry_uses_a_fresh_numbered_copy() {
    let f = Fixture::new();
    f.prepare();
    // Simulate power loss after a native index rewrite, before the pair commits.
    let db = Connection::open(f.root.join("handoff.sqlite")).unwrap();
    db.execute("UPDATE handoff SET phase='copying'", [])
        .unwrap();
    drop(db);
    let retained = f
        .root
        .join("attempt-1/working/codex/data/unknown-future-metadata");
    fs::write(&retained, "INTERRUPTED_COPY").unwrap();
    f.prepare();
    let lease = f.activate();
    assert!(lease
        .profiles()
        .0
        .starts_with(f.root.canonicalize().unwrap().join("attempt-2")));
    assert_eq!(fs::read_to_string(retained).unwrap(), "INTERRUPTED_COPY");
    f.original_intact();
}

#[test]
fn source_progress_after_export_refuses_split_history_without_overwriting_either_copy() {
    let f = Fixture::new();
    f.prepare();
    let original = f
        .plan
        .claude
        .original_profile
        .join("unknown-future-metadata");
    fs::write(&original, "NEW_ORIGINAL_WORK").unwrap();
    let error = match profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile,
    ) {
        Ok(_) => panic!("changed original was accepted"),
        Err(e) => e,
    };
    assert!(error.contains("changed after export"));
    assert_eq!(fs::read_to_string(original).unwrap(), "NEW_ORIGINAL_WORK");
    assert_eq!(
        fs::read_to_string(
            f.root
                .join("attempt-1/working/claude/data/unknown-future-metadata")
        )
        .unwrap(),
        "KEEP_UNKNOWN"
    );
}
#[test]
fn unknown_destination_and_foreign_export_are_never_adopted() {
    let f = Fixture::new();
    fs::create_dir(&f.root).unwrap();
    fs::write(f.root.join("unrelated"), "KEEP").unwrap();
    assert!(profile_handoff::prepare(&f.root, &f.plan).is_err());
    assert!(!f.root.join("server.lock").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("unrelated")).unwrap(),
        "KEEP"
    );
    let f = Fixture::new();
    f.prepare();
    let mut other = f.plan.clone();
    other.claude.snapshot = other.codex.snapshot.clone();
    assert!(profile_handoff::prepare(&f.root, &other).is_err());
    f.original_intact();
}
#[test]
fn missing_or_mismatched_native_rollout_refuses_handoff() {
    for wrong_header in [false, true] {
        let f = Fixture::new();
        let source = f.plan.codex.original_profile.join("sessions/rollout.jsonl");
        if wrong_header {
            fs::write(&source,format!("{}\n",json!({"type":"session_meta","payload":{"id":"00000000-0000-4000-8000-000000000099"}}))).unwrap();
        } else {
            fs::remove_file(&source).unwrap();
        }
        let mut plan = f.plan.clone();
        plan.codex.snapshot = f._dir.path().join("bad-export");
        migration_snapshot::capture(&plan.codex.original_profile, &plan.codex.snapshot, ENV)
            .unwrap();
        assert!(profile_handoff::prepare(&f.root, &plan).is_err());
        migration_snapshot::verify_tree(&plan.codex.snapshot, &plan.codex.original_profile, ENV)
            .unwrap();
    }
}
#[cfg(any(unix, windows))]
#[test]
fn linked_working_profile_cannot_redirect_activation() {
    let f = Fixture::new();
    f.prepare();
    let lease = f.activate();
    let (_, claude) = lease.profiles();
    drop(lease);
    let moved = claude.with_file_name("preserved");
    fs::rename(&claude, &moved).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&moved, &claude).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&moved, &claude).unwrap();
    assert!(profile_handoff::activate(
        &f.root,
        ENV,
        &f.plan.codex.original_profile,
        &f.plan.claude.original_profile
    )
    .is_err());
    assert!(profile_handoff::rollback(&f.root, ENV).is_err());
    f.original_intact();
}

#[tokio::test]
async fn runtime_uses_only_handed_off_profiles_and_holds_lease_through_setup() {
    use remodex_connect_native::{
        auth::Auth,
        auth_service::AuthService,
        chat::{AgentConfig, AgentKind, Chats, Config},
        store::Store,
    };
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path()).unwrap();
    let environment = store.environment_id().unwrap();
    let f = Fixture::with_environment(&environment);
    f.prepare();
    let provider = |profile| AgentConfig {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture")),
        profile,
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let config = Config {
        native_profile_handoff: Some(f.root.clone()),
        connection: None,
        storage: Default::default(),
        max_active_chats: 1,
        workspace: state.path().into(),
        codex: provider(f.plan.codex.original_profile.clone()),
        claude: provider(f.plan.claude.original_profile.clone()),
    };
    let service = AuthService::new(Auth::new(store)).unwrap();
    let hub = Chats::start(service.clone(), config.clone()).await.unwrap();
    assert!(Chats::start(service.clone(), config.clone()).await.is_err());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match hub.configure_claude(json!({"enabled":true})).await {
            Ok(_) => break,
            Err(error)
                if error.starts_with("Wait for agent setup")
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(f
        .root
        .join("attempt-1/working/claude/data/setup-auth-probes.log")
        .exists());
    assert!(!f
        .plan
        .claude
        .original_profile
        .join("setup-auth-probes.log")
        .exists());
    hub.call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
    // A retained Chats handle can still own setup work, so it must retain the
    // generation even after its chat workers have shut down.
    assert!(profile_handoff::rollback(&f.root, &environment)
        .unwrap_err()
        .contains("running bridge"));
    drop(hub);
    let mut stale = config;
    stale.native_profile_handoff = None;
    let error = match Chats::start(service, stale).await {
        Ok(_) => panic!("stale configuration reopened originals"),
        Err(error) => error,
    };
    assert!(error.contains("native_profile_handoff configuration"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let error = profile_handoff::rollback(&f.root, &environment).unwrap_err();
        if !error.contains("running bridge") {
            assert!(error.contains("new or changed data"));
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    f.original_intact();
}

#[test]
fn selected_existing_profile_snapshots_keep_unrelated_live_data_out_of_migration() {
    use remodex_connect_native::migration_snapshot;
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("sessions")).unwrap();
    std::fs::write(source.join("sessions/chat.jsonl"), b"original chat\n").unwrap();
    std::fs::write(source.join("auth.json"), b"DO NOT COPY").unwrap();
    #[cfg(unix)]
    let _socket = std::os::unix::net::UnixListener::bind(source.join("ipc.sock")).unwrap();
    let snapshot = t.path().join("snapshot");
    let environment = "01234567-1234-4234-8234-123456789abc";
    let report = migration_snapshot::capture_selected(
        &source,
        &snapshot,
        environment,
        &["sessions/chat.jsonl".into()],
    )
    .unwrap();
    assert_eq!(report.files, 1);
    assert!(migration_snapshot::is_selected(&snapshot, environment).unwrap());
    std::fs::write(source.join("auth.json"), b"AUTH STAYS LIVE IN PLACE").unwrap();
    migration_snapshot::verify_source_entries(&snapshot, &source, environment).unwrap();
    let restored = t.path().join("restored");
    migration_snapshot::restore(&snapshot, &restored, environment).unwrap();
    assert!(!restored.join("data/auth.json").exists());
    std::fs::write(restored.join("data/unexpected"), b"must invalidate restore").unwrap();
    assert!(migration_snapshot::verify_restored(&restored, environment).is_err());
    std::fs::write(source.join("sessions/chat.jsonl"), b"new original chat\n").unwrap();
    assert!(migration_snapshot::verify_source_entries(&snapshot, &source, environment).is_err());
    assert_eq!(
        std::fs::read(source.join("auth.json")).unwrap(),
        b"AUTH STAYS LIVE IN PLACE"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source.join("auth.json"), source.join("sessions/link.jsonl"))
            .unwrap();
        assert!(migration_snapshot::capture_selected(
            &source,
            &t.path().join("bad"),
            environment,
            &["sessions/link.jsonl".into()]
        )
        .is_err());
    }
}
