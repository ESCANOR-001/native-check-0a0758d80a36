use remodex_connect_native::{legacy_connection::Prepared, migration_snapshot, store::Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
const ENV: &str = "opencodex-0123456789abcdef01234567";
const NATIVE: &str = "00000000-0000-4000-8000-000000000001";
const CHAT: &str = "old-public-chat";
const TIME: &str = "2026-01-01T12:00:00Z";
type NativeChange<'a> = Option<Box<dyn FnOnce(&mut Value) + 'a>>;
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
    fn native(change: impl FnOnce(&mut Value)) -> Self {
        Self::build(
            "claude",
            b"NATIVE_ONLY_FILE".to_vec(),
            Some(Box::new(change)),
        )
    }
    fn with_text(agent: &'static str, text: Vec<u8>) -> Self {
        Self::build(agent, text, None)
    }
    fn build<'a>(agent: &'static str, text: Vec<u8>, native: NativeChange<'a>) -> Self {
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
        let mut messages = json!([
            {"id":"prompt","turnId":"old-turn","role":"user","text":format!("See [note](<{}>)",old.join(&files[0].0).display()),"createdAt":TIME,"updatedAt":TIME,"streaming":false,"attachments":[{"path":old.join(&files[1].0),"type":"image"}]},
            {"id":"reply","turnId":"old-turn","role":"assistant","text":"Original reply","createdAt":TIME,"updatedAt":TIME,"streaming":false}
        ]);
        let prompt_id = if native.is_some() {
            "00000000-0000-4000-8000-000000000024"
        } else {
            "prompt"
        };
        if native.is_some() {
            messages[0]["id"] = json!(prompt_id);
            messages[0]["text"] = json!("Inspect these attachments.");
            messages[0].as_object_mut().unwrap().remove("attachments");
        }
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
        if let Some(change) = native {
            use base64::Engine;
            let profile = export.path().join("native-profile");
            let transcript = format!("projects/fixture/{NATIVE}.jsonl");
            fs::create_dir_all(profile.join("projects/fixture")).unwrap();
            let mut prompt = json!({"type":"user","uuid":prompt_id,"parentUuid":null,"sessionId":NATIVE,"message":{"role":"user","content":[{"type":"text","text":format!("Inspect these attachments.\n\nAttached files on this PC:\n{}",old.join(&files[0].0).display())},{"type":"image","source":{"type":"base64","media_type":"image/png","data":base64::engine::general_purpose::STANDARD.encode(&files[1].1)}}]}});
            change(&mut prompt);
            let reply = json!({"type":"assistant","uuid":"00000000-0000-4000-8000-000000000028","parentUuid":prompt_id,"sessionId":NATIVE,"message":{"id":"reply","role":"assistant","content":[{"type":"text","text":"Original reply"}]}});
            fs::write(profile.join(&transcript), format!("{prompt}\n{reply}\n")).unwrap();
            let snapshot = export.path().join("native-snapshot");
            migration_snapshot::capture(&profile, &snapshot, ENV).unwrap();
            store
                .prepare_legacy_claude_native(CHAT, &snapshot, &transcript)
                .unwrap();
        }
        let plan = json!({"version":1,"files":files.iter().map(|(relative,bytes)| json!({"message":prompt_id,"original":old.join(relative),"relative":relative,"sha256":format!("{:x}",Sha256::digest(bytes)),"bytes":bytes.len()})).collect::<Vec<_>>()});
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
fn projected_attachments_count_toward_page_bytes_without_skipping_the_prompt() {
    let mut f = Fixture::native(|_| {});
    f.prepare().unwrap();
    let before = f.rows();
    let old_bytes: i64 =
        f.db.query_row(
            "SELECT sum(length(data)) FROM chat_rows WHERE chat=?1",
            [CHAT],
            |r| r.get(0),
        )
        .unwrap();
    let mut tail = json!({"id":"later-reply","role":"assistant","text":"","turnId":"later-turn"});
    // The original rows fit, but the verified attachment descriptors require
    // a second page. This tests the byte boundary rather than the row count.
    let padding = 160 * 1024 - old_bytes as usize - tail.to_string().len() - 1;
    tail["text"] = json!("x".repeat(padding));
    f.db.execute(
        "INSERT INTO chat_rows(chat,kind,id,turn,data) VALUES(?1,'messages','later-reply','later-turn',?2)",
        rusqlite::params![CHAT,tail.to_string()],
    ).unwrap();
    let raw = f.store.history_page(CHAT, None, None).unwrap();
    assert_eq!(raw["messages"].as_array().unwrap().len(), 3);
    assert_eq!(raw["pageInfo"]["hasOlder"], false);
    let first = f.store.history_page_display(CHAT, None, None).unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 2);
    assert_eq!(first["pageInfo"]["hasOlder"], true);
    let cursor = first["pageInfo"]["olderCursor"]
        .as_str()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let second = f
        .store
        .history_page_display(CHAT, Some(cursor), None)
        .unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 1);
    assert_eq!(second["messages"][0]["role"], "user");
    assert_eq!(
        second["messages"][0]["attachments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(second["pageInfo"]["hasOlder"], false);
    let old = f.rows();
    assert_eq!(&old[..before.len()], before.as_slice());
}

#[test]
fn native_only_attachments_render_without_reading_bytes_or_rewriting_history() {
    let mut f = Fixture::native(|_| {});
    let before = f.rows();
    f.prepare().unwrap();
    let paths = [f.path(0), f.path(1)];
    // Files may be unavailable until a tap. Reading a message page must not
    // load either file, scan native history, or depend on the original PC path.
    for p in &paths {
        fs::remove_file(p).unwrap();
    }
    let page = f.store.history_page_display(CHAT, None, None).unwrap();
    let mut message = page["messages"][0].clone();
    assert_eq!(message["text"], "Inspect these attachments.");
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0]["type"], "file");
    assert_eq!(attachments[1]["type"], "image");
    assert_eq!(attachments[1]["mimeType"], "image/png");
    for (i, a) in attachments.iter().enumerate() {
        assert_eq!(a["path"], f.old.join(&f.files[i].0).to_str().unwrap());
        assert_eq!(a["sizeBytes"], f.files[i].1.len());
        assert!(a.get("dataUrl").is_none());
        assert!(a.get("base64").is_none());
    }
    f.store.present_legacy_uploads(CHAT, &mut message).unwrap();
    assert_eq!(message["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(f.rows(), before);
    assert!(f.store.load_chat(CHAT).unwrap()["thread"]["messages"][0]["attachments"].is_null());
    assert_eq!(
        f.store.load_chat_display(CHAT, &[]).unwrap()["thread"]["messages"][0]["attachments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(f
        .store
        .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .is_err());
    for (i, p) in paths.iter().enumerate() {
        fs::write(p, &f.files[i].1).unwrap();
    }
    assert!(f.prepare().is_ok());
    let reopened = Store::open(f.dir.path()).unwrap();
    assert_eq!(
        reopened.history_page_display(CHAT, None, None).unwrap(),
        page
    );
    assert!(reopened.assert_migration_ready().is_err());
    drop(reopened);
    f.rollback().unwrap();
    let mut original = message.clone();
    original.as_object_mut().unwrap().remove("attachments");
    f.store.present_legacy_uploads(CHAT, &mut original).unwrap();
    assert!(original["attachments"].is_null());
}

#[test]
fn omitted_attachment_projection_is_scoped_to_the_original_message_and_chat() {
    let mut f = Fixture::native(|_| {});
    f.prepare().unwrap();
    let page = f.store.history_page(CHAT, None, None).unwrap();
    let original = page["messages"][0].clone();
    let mut other = original.clone();
    f.store
        .present_legacy_uploads("different-chat", &mut other)
        .unwrap();
    assert_eq!(other, original);
    let mut changed = original.clone();
    changed["text"] = json!("Different prompt");
    assert!(f.store.present_legacy_uploads(CHAT, &mut changed).is_err());
    assert!(changed["attachments"].is_null());
    f.db.execute("UPDATE chat_rows SET data=json_set(data,'$.text','Changed stored prompt') WHERE kind='messages' AND json_extract(data,'$.role')='user'",[]).unwrap();
    let mut changed = original;
    assert!(f.store.present_legacy_uploads(CHAT, &mut changed).is_err());
    assert!(changed["attachments"].is_null());
}

#[test]
fn already_visible_links_and_image_attachments_are_not_duplicated_by_display() {
    for agent in ["claude", "codex"] {
        let mut f = Fixture::new(agent);
        let raw = f.store.history_page(CHAT, None, None).unwrap();
        f.prepare().unwrap();
        assert_eq!(f.store.history_page_display(CHAT, None, None).unwrap(), raw);
        assert_eq!(
            f.store.load_chat_display(CHAT, &[]).unwrap()["thread"]["messages"],
            raw["messages"]
        );
    }
}

#[test]
fn old_prepared_uploads_add_presentation_only_after_storage_capacity_check() {
    let mut f = Fixture::native(|_| {});
    f.prepare().unwrap();
    let before = f.rows();
    f.db.execute_batch("DROP TABLE legacy_upload_display; DROP TABLE legacy_upload_display_import; CREATE TABLE retained_data(bytes BLOB); INSERT INTO retained_data VALUES(zeroblob(1048576));").unwrap();
    f.store
        .configure_storage(remodex_connect_native::storage::Limits {
            history_bytes: 1048576,
            ..Default::default()
        })
        .unwrap();
    assert!(f.prepare().unwrap_err().contains("storage limit"));
    assert_eq!(f.rows(), before);
    f.store.configure_storage(Default::default()).unwrap();
    f.prepare().unwrap();
    assert_eq!(
        f.store.history_page_display(CHAT, None, None).unwrap()["messages"][0]["attachments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(f.rows(), before);
}

#[test]
fn changed_or_oversized_presentation_receipts_never_replace_a_message() {
    for raw in ["[{}]".to_owned(), "x".repeat(49153)] {
        let mut f = Fixture::native(|_| {});
        f.prepare().unwrap();
        let before = f.rows();
        f.db.execute("UPDATE legacy_upload_display SET descriptors=?1", [raw])
            .unwrap();
        assert!(f.store.history_page_display(CHAT, None, None).is_err());
        assert_eq!(f.rows(), before);
        assert!(f.prepare().is_err());
        assert_eq!(f.rows(), before);
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

#[test]
fn retained_copies_consume_upload_capacity_after_restart_and_rollback() {
    let mut f = Fixture::new("claude");
    f.prepare().unwrap();
    let saved = f.store.storage_status().unwrap();
    assert_eq!(saved["assetFiles"], 5); // two uploads, unknown metadata, two manifests
    assert!(
        saved["assetBytes"].as_u64().unwrap() > f.files.iter().map(|(_, b)| b.len() as u64).sum()
    );
    f.rollback().unwrap();
    let mut reopened = Store::open(f.dir.path()).unwrap();
    assert_eq!(
        reopened.storage_status().unwrap()["assetBytes"],
        saved["assetBytes"]
    );
    assert_eq!(
        reopened.storage_status().unwrap()["assetFiles"],
        saved["assetFiles"]
    );
    reopened
        .configure_storage(remodex_connect_native::storage::Limits {
            asset_bytes: saved["assetBytes"].as_u64().unwrap() + 3,
            ..Default::default()
        })
        .unwrap();
    let upload = |data: &str| json!([{"type":"file","name":"new.txt","mimeType":"text/plain","dataUrl":format!("data:text/plain;base64,{data}")}]);
    assert!(reopened.stage_uploads(upload("MTIzNA==")).is_err());
    reopened.stage_uploads(upload("MTIz")).unwrap();
    assert_eq!(
        reopened.storage_status().unwrap()["assetBytes"],
        saved["assetBytes"].as_u64().unwrap() + 3
    );
}

#[test]
fn partial_bundles_count_before_admitting_another_import_and_never_get_deleted() {
    for files_limit in [false, true] {
        let mut f = Fixture::new("claude");
        let partial = f.dir.path().join("legacy-uploads/partial/data");
        fs::create_dir_all(&partial).unwrap();
        fs::write(partial.join("keep"), [1u8; 1000]).unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(f.snapshot.join("snapshot.json")).unwrap()).unwrap();
        let summary = migration_snapshot::verify(&f.snapshot, ENV).unwrap();
        let complete = summary.bytes + manifest.to_string().len() as u64 * 2;
        f.store
            .configure_storage(remodex_connect_native::storage::Limits {
                asset_bytes: if files_limit {
                    2 * 1024 * 1024 * 1024
                } else {
                    complete + 999
                },
                asset_files: if files_limit { 5 } else { 65_536 },
                ..Default::default()
            })
            .unwrap();
        assert!(f.prepare().is_err());
        assert_eq!(fs::read(partial.join("keep")).unwrap(), [1u8; 1000]);
        assert_eq!(f.store.storage_status().unwrap()["assetFiles"], 1);
        assert_eq!(f.store.storage_status().unwrap()["assetBytes"], 1000);
        assert_eq!(
            fs::read_dir(f.dir.path().join("legacy-uploads"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn failed_alias_receipt_counts_files_and_retry_does_not_charge_for_a_second_copy() {
    let mut f = Fixture::new("claude");
    f.db.execute_batch("CREATE TABLE legacy_uploads(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,original_root TEXT NOT NULL,bundle TEXT NOT NULL,plan BLOB NOT NULL,phase TEXT NOT NULL); CREATE TRIGGER fail_upload BEFORE INSERT ON legacy_uploads BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(f.prepare().is_err());
    let saved = f.store.storage_status().unwrap();
    assert_eq!(saved["assetFiles"], 5);
    f.store
        .configure_storage(remodex_connect_native::storage::Limits {
            asset_bytes: saved["assetBytes"].as_u64().unwrap(),
            asset_files: 5,
            ..Default::default()
        })
        .unwrap();
    f.db.execute_batch("DROP TRIGGER fail_upload;").unwrap();
    f.prepare().unwrap();
    assert_eq!(
        f.store.storage_status().unwrap()["assetBytes"],
        saved["assetBytes"]
    );
    assert_eq!(f.store.storage_status().unwrap()["assetFiles"], 5);
    assert_eq!(fs::read(f.path(0)).unwrap(), f.files[0].1);
}

#[cfg(unix)]
#[test]
fn retained_copy_index_refuses_symlinks_without_changing_its_last_committed_totals() {
    let mut f = Fixture::new("claude");
    f.prepare().unwrap();
    let saved = f.store.storage_status().unwrap();
    let outside = f.export.path().join("outside-index");
    fs::write(&outside, b"KEEP_OUTSIDE").unwrap();
    std::os::unix::fs::symlink(&outside, f.dir.path().join("legacy-uploads/link")).unwrap();
    assert!(Store::open(f.dir.path()).is_err());
    assert_eq!(
        f.store.storage_status().unwrap()["assetBytes"],
        saved["assetBytes"]
    );
    assert_eq!(fs::read(outside).unwrap(), b"KEEP_OUTSIDE");
}

#[test]
fn native_only_claude_files_and_images_preserve_history_and_survive_restart() {
    let mut f = Fixture::native(|_| {});
    let before = f.rows();
    f.prepare().unwrap();
    for i in 0..2 {
        assert_eq!(fs::read(f.path(i)).unwrap(), f.files[i].1);
    }
    let path = f.path(0);
    assert_eq!(f.rows(), before);
    assert!(!f.old.exists());
    f.prepare().unwrap();
    let reopened = Store::open(f.dir.path()).unwrap();
    assert_eq!(
        reopened
            .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
            .unwrap(),
        Some(path.clone())
    );
    assert!(reopened
        .resolve_legacy_upload(
            "different-chat",
            f.old.join(&f.files[0].0).to_str().unwrap()
        )
        .unwrap()
        .is_none());
    drop(reopened);
    f.rollback().unwrap();
    assert!(path.exists());
    assert!(f
        .store
        .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .unwrap()
        .is_none());
    assert!(f.store.assert_migration_ready().is_err());
}
#[test]
fn native_attachment_evidence_rejects_changed_prompt_wrong_file_and_wrong_image() {
    for scenario in 0..4 {
        let mut f = Fixture::native(move |v| match scenario {
            0 => v["message"]["content"][0]["text"] = json!("Another prompt entirely"),
            1 => {
                v["message"]["content"][0]["text"] = json!(
                    "Inspect these attachments.\n\nAttached files on this PC:\n/another/file.txt"
                )
            }
            2 => v["message"]["content"][1]["source"]["data"] = json!("bm90LXRoZS1pbWFnZQ=="),
            _ => v["message"]["content"][1]["source"]["type"] = json!("url"),
        });
        let before = f.rows();
        assert!(f.prepare().is_err(), "scenario {scenario}");
        assert_eq!(f.rows(), before);
        assert!(f.store.assert_migration_ready().is_err());
    }
}
#[test]
fn native_attachment_proof_is_bound_to_unchanged_public_message_and_native_receipt() {
    for native in [false, true] {
        let mut f = Fixture::native(|_| {});
        f.prepare().unwrap();
        if native {
            f.db.execute(
                "UPDATE legacy_claude_native SET report=json_set(report,'$.bytes',1)",
                [],
            )
            .unwrap();
        } else {
            f.db.execute("UPDATE chat_rows SET data=json_set(data,'$.text','Changed') WHERE kind='messages' AND json_extract(data,'$.role')='user'",[]).unwrap();
        }
        assert!(f
            .store
            .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
            .is_err());
        assert!(f.prepare().is_err());
        let reopened = Store::open(f.dir.path()).unwrap();
        assert!(reopened
            .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
            .is_err());
        assert!(reopened.assert_migration_ready().is_err());
        assert!(!f.old.exists());
    }
}
#[test]
fn a_public_unreferenced_file_cannot_acquire_native_evidence_without_a_verified_profile() {
    let mut f = Fixture::new("claude");
    f.plan["files"][0]["message"] = json!("reply");
    assert!(f.prepare().is_err());
    assert!(f.store.assert_migration_ready().is_err());
}

#[test]
fn native_attachment_proof_and_alias_commit_atomically_and_retry_retained_copy() {
    let mut f = Fixture::native(|_| {});
    let before = f.rows();
    f.db.execute_batch("CREATE TABLE legacy_uploads(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,original_root TEXT NOT NULL,bundle TEXT NOT NULL,plan BLOB NOT NULL,phase TEXT NOT NULL); CREATE TRIGGER reject_upload BEFORE INSERT ON legacy_uploads BEGIN SELECT RAISE(FAIL,'fixture interruption'); END;").unwrap();
    assert!(f.prepare().is_err());
    let count: i64 =
        f.db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='legacy_upload_evidence'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0); // The proof table and grants rolled back with the receipt.
    assert!(f
        .store
        .resolve_legacy_upload(CHAT, f.old.join(&f.files[0].0).to_str().unwrap())
        .unwrap()
        .is_none());
    f.db.execute_batch("DROP TRIGGER reject_upload").unwrap();
    f.prepare().unwrap();
    assert_eq!(fs::read(f.path(0)).unwrap(), f.files[0].1);
    assert_eq!(f.rows(), before);
}

#[test]
fn connection_rollback_retains_verified_upload_bundles_after_chat_rollback() {
    for scenario in 0..3 {
        let mut f = match scenario {
            0 => Fixture::new("codex"),
            1 => Fixture::new("claude"),
            _ => Fixture::native(|_| {}),
        };
        f.prepare().unwrap();
        let files = [f.path(0), f.path(1)];
        let storage = f.store.storage_status().unwrap();
        assert!(f.store.rollback_legacy_connection().is_err());
        f.rollback().unwrap();
        f.store.rollback_legacy_connection().unwrap();
        f.store.rollback_legacy_connection().unwrap();
        assert!(f.store.assert_migration_ready().is_err());
        assert!(f.rows().is_empty());
        for (i, path) in files.iter().enumerate() {
            assert_eq!(fs::read(path).unwrap(), f.files[i].1);
        }
        assert_eq!(
            f.store.storage_status().unwrap()["assetBytes"],
            storage["assetBytes"]
        );
        let mut reopened = Store::open(f.dir.path()).unwrap();
        reopened.rollback_legacy_connection().unwrap();
        assert_eq!(
            reopened.storage_status().unwrap()["assetBytes"],
            storage["assetBytes"]
        );
        assert!(!f.old.exists());
    }
}

#[test]
fn coordinated_rollback_preview_retains_all_history_and_apply_keeps_copied_files() {
    for scenario in 0..3 {
        let mut f = match scenario {
            0 => Fixture::new("codex"),
            1 => Fixture::new("claude"),
            _ => Fixture::native(|_| {}),
        };
        f.prepare().unwrap();
        let files = [f.path(0), f.path(1)];
        let before = f.rows();
        let storage = f.store.storage_status().unwrap();
        f.store.rollback_legacy_imports(false).unwrap();
        assert_eq!(f.rows(), before);
        for key in ["assetBytes", "assetFiles", "historyBytes"] {
            assert_eq!(f.store.storage_status().unwrap()[key], storage[key]);
        }
        f.store.rollback_legacy_imports(true).unwrap();
        assert!(f.rows().is_empty());
        assert!(f.store.assert_migration_ready().is_err());
        for (i, path) in files.iter().enumerate() {
            assert_eq!(fs::read(path).unwrap(), f.files[i].1);
        }
        assert_eq!(
            f.store.storage_status().unwrap()["assetBytes"],
            storage["assetBytes"]
        );
        assert!(!f.old.exists());
    }
}

#[test]
fn coordinated_cli_does_not_repair_a_changed_ledger_before_validating_rollback() {
    let mut f = Fixture::native(|_| {});
    f.prepare().unwrap();
    f.db.execute("UPDATE storage_assets SET bytes=bytes+1", [])
        .unwrap();
    let rows = f.rows();
    let storage = f.store.storage_status().unwrap();
    for apply in [false, true] {
        assert!(remodex_connect_native::legacy_rollback::run(f.dir.path(), apply).is_err());
        assert_eq!(f.rows(), rows);
        for key in ["assetBytes", "assetFiles", "historyBytes"] {
            assert_eq!(f.store.storage_status().unwrap()[key], storage[key]);
        }
        assert_eq!(
            f.db.query_row("SELECT phase FROM legacy_connection_import", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "prepared"
        );
    }
}

#[test]
fn changed_or_unclassified_retained_uploads_refuse_connection_rollback_without_clearing_state() {
    for scenario in 0..6 {
        let mut f = Fixture::native(|_| {});
        f.prepare().unwrap();
        let path = f.path(0);
        let bundle = path.ancestors().nth(4).unwrap().to_path_buf();
        f.rollback().unwrap();
        match scenario {
            0 => fs::write(&path, b"CHANGED_COPY_MUST_SURVIVE").unwrap(),
            1 => {
                f.db.execute("UPDATE legacy_uploads SET source_hash='changed'", [])
                    .unwrap();
            }
            2 => {
                f.db.execute(
                    "INSERT INTO legacy_upload_display VALUES('another-chat','orphan','hash','[]')",
                    [],
                )
                .unwrap();
            }
            3 => fs::write(bundle.join("unknown-file"), b"KEEP_UNKNOWN").unwrap(),
            4 => fs::write(bundle.join("copying/unknown.copy"), b"KEEP_PARTIAL").unwrap(),
            _ => {
                f.db.execute("UPDATE storage_assets SET bytes=bytes+1", [])
                    .unwrap();
            }
        }
        let bytes = fs::read(&path).unwrap();
        let metadata: i64 =
            f.db.query_row("SELECT count(*) FROM metadata", [], |r| r.get(0))
                .unwrap();
        assert!(metadata > 0);
        assert!(
            f.store.rollback_legacy_connection().is_err(),
            "scenario {scenario}"
        );
        assert_eq!(
            f.db.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            metadata
        );
        assert_eq!(
            f.db.query_row("SELECT phase FROM legacy_connection_import", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "prepared"
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(f.store.assert_migration_ready().is_err());
    }
}
