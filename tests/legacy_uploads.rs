use remodex_connect_native::{legacy_connection::Prepared, migration_snapshot, store::Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const NATIVE: &str = "00000000-0000-4000-8000-000000000001";
const CHAT: &str = "old-public-chat";
const TIME: &str = "2026-01-01T12:00:00Z";
struct Fixture {
    dir: tempfile::TempDir,
    export: tempfile::TempDir,
    store: Store,
    db: rusqlite::Connection,
    old: PathBuf,
    snapshot: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    plan: Value,
    agent: &'static str,
}
impl Fixture {
    fn new(agent: &'static str) -> Self {
        Self::with_text(agent, b"OLD_FILE_MARKER".to_vec())
    }
    fn with_text(agent: &'static str, text: Vec<u8>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let export = tempfile::tempdir().unwrap();
        let old = dir.path().join("original-not-accessed");
        let source = export.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(320, 160)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let files = vec![
            (
                format!("client/{NATIVE}/original note-00000000-0000-4000-8000-000000000002.txt"),
                text,
            ),
            (
                format!("client/{NATIVE}/image-00000000-0000-4000-8000-000000000003.png"),
                png.into_inner(),
            ),
        ];
        fs::create_dir_all(source.join("client").join(NATIVE)).unwrap();
        for (relative, bytes) in &files {
            fs::write(source.join(relative), bytes).unwrap();
        }
        fs::write(source.join("unknown-future-metadata"), b"KEEP_UNKNOWN").unwrap();
        let snapshot = export.path().join("snapshot");
        migration_snapshot::capture(&source, &snapshot, ENV).unwrap();
        let messages = json!([
            {"id":"prompt","turnId":"old-turn","role":"user","text":format!("See [note](<{}>)",old.join(&files[0].0).display()),"createdAt":TIME,"updatedAt":TIME,"streaming":false,"attachments":[{"path":old.join(&files[1].0),"type":"image"}]},
            {"id":"reply","turnId":"old-turn","role":"assistant","text":"Original reply","createdAt":TIME,"updatedAt":TIME,"streaming":false}
        ]);
        let mut store = Store::open(dir.path()).unwrap();
        let connection = json!({"version":1,"settings":{"controlEnabled":false,"tunnelMode":"quick"},"clients":[],"taskSelections":[],"threadAliases":if agent == "codex" {json!([{"remoteThreadId":CHAT,"nativeThreadId":NATIVE,"projectId":"project","title":"Old chat","cwd":dir.path(),"instanceId":"openai","model":"model","nativeModelProvider":"openai","runtimeMode":"approval-required","interactionMode":"default","createdAt":TIME,"updatedAt":TIME}])} else {json!([])}});
        store
            .prepare_legacy_connection(
                &Prepared::parse(connection.to_string().as_bytes(), ENV).unwrap(),
            )
            .unwrap();
        if agent == "claude" {
            let raw = json!({"version":1,"nativeId":NATIVE,"thread":{"id":CHAT,"projectId":"project","title":"Old chat","worktreePath":dir.path(),"modelSelection":{"instanceId":"claude-code","model":"sonnet"},"runtimeMode":"approval-required","interactionMode":"default","session":{"threadId":CHAT,"status":"ready"},"messages":messages,"activities":[],"proposedPlans":[],"createdAt":TIME,"updatedAt":TIME}});
            store
                .prepare_legacy_claude(raw.to_string().as_bytes())
                .unwrap();
        } else {
            store.prepare_legacy_codex().unwrap();
            let raw = json!({"version":1,"threadId":CHAT,"nativeId":NATIVE,"nativeStatus":"idle","hasPendingApprovals":false,"hasPendingUserInput":false,"hasQueuedPrompts":false,"hasActionableProposedPlan":false,"updatedAt":TIME,"latestUserMessageAt":TIME,"latestTurn":{"turnId":"old-turn","state":"completed","startedAt":TIME,"completedAt":TIME},"turns":[{"publicId":"old-turn","nativeId":"native-old-turn","status":"completed"}],"messages":messages,"activities":[],"proposedPlans":[]});
            store
                .prepare_legacy_codex_history(CHAT, raw.to_string().as_bytes())
                .unwrap();
        }
        let plan = json!({"version":1,"files":files.iter().map(|(relative,bytes)| json!({"message":"prompt","original":old.join(relative),"relative":relative,"sha256":format!("{:x}",Sha256::digest(bytes)),"bytes":bytes.len()})).collect::<Vec<_>>()});
        let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
        Self {
            dir,
            export,
            store,
            db,
            old,
            snapshot,
            files,
            plan,
            agent,
        }
    }
    fn prepare(&mut self) -> Result<Value, String> {
        self.store.prepare_legacy_uploads(
            CHAT,
            &self.old,
            &self.snapshot,
            self.plan.to_string().as_bytes(),
        )
    }
    fn path(&self, index: usize) -> PathBuf {
        self.store
            .resolve_legacy_upload(CHAT, self.old.join(&self.files[index].0).to_str().unwrap())
            .unwrap()
            .unwrap()
    }
    fn rows(&self) -> Vec<String> {
        self.db
            .prepare("SELECT data FROM chat_rows ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn rollback(&mut self) -> Result<Value, String> {
        if self.agent == "claude" {
            self.store.rollback_legacy_claude(CHAT)
        } else {
            self.store.rollback_legacy_codex()
        }
    }
}
#[test]
fn both_agents_keep_exact_history_and_private_file_bytes_across_retry_restart_and_rollback() {
    for agent in ["claude", "codex"] {
        let mut f = Fixture::new(agent);
        let before = f.rows();
        assert_eq!(f.prepare().unwrap()["files"], 2);
        assert!(!f.old.exists());
        let first = f.path(0);
        for i in 0..2 {
            assert_eq!(fs::read(f.path(i)).unwrap(), f.files[i].1);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(f.path(i)).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        assert_eq!(f.prepare().unwrap()["migrationComplete"], false);
        let reopened = Store::open(f.dir.path()).unwrap();
        assert_eq!(
            reopened
                .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
                .unwrap(),
            Some(first.clone())
        );
        assert!(reopened
            .resolve_legacy_upload("another-chat", f.old.join(&f.files[0].0).to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(f
            .store
            .resolve_legacy_upload(CHAT, first.to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(f
            .store
            .resolve_legacy_upload(
                CHAT,
                f.old.join("unknown-future-metadata").to_str().unwrap()
            )
            .unwrap()
            .is_none());
        assert_eq!(f.rows(), before);
        assert!(f.store.assert_migration_ready().is_err());
        drop(reopened);
        f.rollback().unwrap();
        f.rollback().unwrap();
        assert!(first.exists());
        assert!(f
            .store
            .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(f.prepare().is_err());
        migration_snapshot::verify(&f.snapshot, ENV).unwrap();
        assert!(f
            .export
            .path()
            .join("source/unknown-future-metadata")
            .exists());
    }
}
#[test]
fn original_text_links_and_image_previews_use_verified_copies_without_exposing_copy_paths() {
    for agent in ["claude", "codex"] {
        let mut f = Fixture::new(agent);
        f.prepare().unwrap();
        let original = f.old.join(&f.files[0].0);
        let result = f
            .store
            .read_legacy_upload_text(CHAT, original.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(result["path"], original.to_str().unwrap());
        assert_eq!(result["relativePath"], original.to_str().unwrap());
        assert_eq!(result["content"], "OLD_FILE_MARKER");
        assert!(result.get("contents").is_none());
        assert_eq!(result["truncated"], false);
        assert!(!result.to_string().contains("legacy-uploads"));
        assert!(f
            .store
            .read_legacy_upload_text("another-chat", original.to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(f
            .store
            .read_legacy_upload_text(CHAT, f.old.join(&f.files[1].0).to_str().unwrap())
            .is_err());
        let small = remodex_connect_native::attachments::thumbnail(&f.path(1)).unwrap();
        let image = image::load_from_memory(&small).unwrap();
        assert_eq!((image.width(), image.height()), (256, 128));
        assert!(!f.old.exists());
        let before = f.rows();
        f.db.execute("UPDATE chat_rows SET data=json_set(data,'$.text','reference removed') WHERE kind='messages' AND id='prompt'",[]).unwrap();
        assert!(f
            .store
            .read_legacy_upload_text(CHAT, original.to_str().unwrap())
            .is_err());
        assert!(f.prepare().is_err());
        assert_ne!(f.rows(), before);
    }
}
#[test]
fn large_migrated_text_previews_are_bounded_without_changing_the_original_file() {
    let bytes = "€".repeat(100_000).into_bytes();
    let mut f = Fixture::with_text("claude", bytes.clone());
    f.prepare().unwrap();
    let result = f
        .store
        .read_legacy_upload_text(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["sizeBytes"], bytes.len());
    let text = result["content"].as_str().unwrap();
    assert_eq!(text.len(), 256 * 1024 - 1);
    assert_eq!(text.as_bytes(), &bytes[..text.len()]);
    assert_eq!(fs::read(f.path(0)).unwrap(), bytes);
}
#[test]
fn missing_unreferenced_changed_and_outside_files_never_grant_access_or_change_rows() {
    for change in [
        "missing",
        "hash",
        "length",
        "reference",
        "outside",
        "parent",
        "duplicate",
        "too-large",
    ] {
        let mut f = Fixture::new("claude");
        let before = f.rows();
        match change {
            "missing" => {
                let path = f.files[0].0.replace("original note", "missing");
                f.plan["files"][0]["relative"] = json!(path);
            }
            "hash" => f.plan["files"][0]["sha256"] = json!("0".repeat(64)),
            "length" => f.plan["files"][0]["bytes"] = json!(1),
            "reference" => f.plan["files"][0]["message"] = json!("reply"),
            "outside" => f.plan["files"][0]["original"] = json!(f.dir.path().join("outside.txt")),
            "parent" => f.plan["files"][0]["relative"] = json!("../private/secret.txt"),
            "duplicate" => {
                let row = f.plan["files"][0].clone();
                f.plan["files"].as_array_mut().unwrap().push(row);
            }
            "too-large" => f.plan["files"][0]["bytes"] = json!(11 * 1024 * 1024),
            _ => unreachable!(),
        }
        assert!(f.prepare().is_err(), "{change}");
        assert_eq!(f.rows(), before);
        assert!(f
            .store
            .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(!f.old.exists());
    }
}
#[test]
fn damaged_copies_and_receipts_block_retry_and_rollback_without_replacing_data() {
    for agent in ["claude", "codex"] {
        for change in ["file", "source", "plan", "root", "bundle"] {
            let mut f = Fixture::new(agent);
            f.prepare().unwrap();
            let before = f.rows();
            let path = f.path(0);
            match change {
                "file" => fs::write(&path, b"CHANGED_FILE!!").unwrap(),
                "source" => {
                    f.db.execute("UPDATE legacy_uploads SET source_hash='changed'", [])
                        .unwrap();
                }
                "plan" => {
                    f.db.execute("UPDATE legacy_uploads SET plan=X'7B7D'", [])
                        .unwrap();
                }
                "root" => {
                    f.db.execute("UPDATE legacy_uploads SET original_root='relative'", [])
                        .unwrap();
                }
                "bundle" => {
                    f.db.execute("UPDATE legacy_uploads SET bundle='../outside'", [])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(f.prepare().is_err(), "{agent}/{change}");
            assert!(f.rollback().is_err(), "{agent}/{change}");
            assert!(f
                .store
                .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
                .is_err());
            assert_eq!(f.rows(), before);
            assert!(path.exists());
        }
    }
}
#[test]
fn failed_database_receipt_can_retry_verified_copy_without_duplicate_files() {
    let mut f = Fixture::new("claude");
    let before = f.rows();
    f.db.execute_batch("CREATE TABLE legacy_uploads(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,original_root TEXT NOT NULL,bundle TEXT NOT NULL,plan BLOB NOT NULL,phase TEXT NOT NULL);CREATE TRIGGER fail_receipt BEFORE INSERT ON legacy_uploads BEGIN SELECT RAISE(ABORT,'fixture full');END;").unwrap();
    assert!(f.prepare().is_err());
    assert_eq!(f.rows(), before);
    assert!(f
        .store
        .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .unwrap()
        .is_none());
    let buckets = fs::read_dir(f.dir.path().join("legacy-uploads"))
        .unwrap()
        .count();
    assert_eq!(buckets, 1);
    f.db.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    f.prepare().unwrap();
    assert_eq!(
        fs::read_dir(f.dir.path().join("legacy-uploads"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(f.rows(), before);
}
#[test]
fn interrupted_restore_is_resumed_from_its_exact_plan_and_changed_export_is_refused() {
    let mut f = Fixture::new("claude");
    f.prepare().unwrap();
    let path = f.path(0);
    let data = path.ancestors().nth(3).unwrap();
    assert_eq!(data.file_name().unwrap(), "data");
    let destination = data.parent().unwrap();
    fs::remove_file(destination.join("restored.json")).unwrap();
    fs::remove_file(&path).unwrap();
    f.db.execute("DELETE FROM legacy_uploads", []).unwrap();
    f.prepare().unwrap();
    assert_eq!(fs::read(f.path(0)).unwrap(), f.files[0].1);
    let hash = f.plan["files"][0]["sha256"].as_str().unwrap();
    fs::write(f.snapshot.join("objects").join(hash), b"changed export").unwrap();
    assert!(f.prepare().is_err());
    assert_eq!(fs::read(f.path(0)).unwrap(), f.files[0].1);
}
#[cfg(unix)]
#[test]
fn symbolic_link_destinations_are_never_followed_even_for_identical_bytes() {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new("claude");
    f.prepare().unwrap();
    let path = f.path(0);
    let outside = f.export.path().join("outside");
    fs::write(&outside, &f.files[0].1).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(f
        .store
        .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .is_err());
    assert!(f.prepare().is_err());
    assert!(f.rollback().is_err());
    assert_eq!(fs::read(outside).unwrap(), f.files[0].1);
}
#[test]
fn stage_limits_refuse_before_copying_files() {
    let mut f = Fixture::new("claude");
    f.store
        .configure_storage(remodex_connect_native::storage::Limits {
            asset_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    assert!(f.prepare().is_err());
    assert_eq!(
        fs::read_dir(f.dir.path().join("legacy-uploads"))
            .unwrap()
            .count(),
        0
    );
}
