//! Recover truncated native image results from the verified local rollout only.
//! Text and instructions never enter the projection. Reads advance in bounded
//! slices, so a long session cannot require a whole-history allocation.
use crate::{auth_service::AuthService, store::Store};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
const LINE: usize = 12 * 1024 * 1024;
const SLICE: u64 = 8 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
struct Wanted {
    item: String,
    mime: String,
    prefix: String,
}
impl Wanted {
    fn key(&self) -> String {
        format!("{}:{:x}", self.item, Sha256::digest(self.prefix.as_bytes()))
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Position {
    offset: u64,
    header: String,
    in_turn: bool,
    complete: bool,
    paths: BTreeMap<String, Vec<String>>,
}
pub(crate) struct Images {
    pub complete: bool,
    pub paths: BTreeMap<String, Vec<String>>,
}
fn find(value: &Value, item: &str, depth: usize, budget: &mut usize, wanted: &mut Vec<Wanted>) {
    if depth > 12 || wanted.len() >= 8 {
        return;
    }
    if let Some(rows) = value.as_array() {
        for row in rows.iter().take(100) {
            find(row, item, depth + 1, budget, wanted);
        }
        return;
    }
    if value["type"] == "text" {
        if let Some(text) = value["text"]
            .as_str()
            .filter(|s| s.len() <= *budget && s.trim_start().starts_with('{'))
        {
            *budget -= text.len();
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                if v["content"].is_array() {
                    find(&v["content"], item, depth + 1, budget, wanted);
                }
            }
        }
        return;
    }
    if value["type"] == "image" {
        if let (Some(mime), Some(data)) = (value["mimeType"].as_str(), value["data"].as_str()) {
            if ["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&mime)
                && data.len() >= 512
                && STANDARD.decode(data).is_err()
            {
                let n = data
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
                    .count();
                let prefix = &data.as_bytes()[..n];
                if prefix.len() >= 512 {
                    wanted.push(Wanted {
                        item: item.into(),
                        mime: mime.into(),
                        prefix: String::from_utf8(prefix.to_vec()).unwrap(),
                    });
                }
            }
        }
    }
    for key in ["content", "resource"] {
        if let Some(v) = value.get(key) {
            find(v, item, depth + 1, budget, wanted);
        }
    }
}
fn targets(rows: &[Value]) -> Vec<Wanted> {
    let mut out = vec![];
    let mut budget = crate::agent::MAX_FRAME;
    for row in rows {
        let item = &row["item"];
        if item["type"] == "mcpToolCall" {
            if let Some(id) = item["id"].as_str() {
                find(&item["result"], id, 0, &mut budget, &mut out);
            }
        }
    }
    out
}
fn line(reader: &mut BufReader<File>) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    let n = reader
        .take((LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Cannot read native image history")?;
    if n > LINE {
        return Err("Native image history record exceeds its bounded reader".into());
    }
    if n == 0 || bytes.last() != Some(&b'\n') {
        return Ok(None);
    }
    Ok(Some(bytes))
}
fn open(
    profile: &Path,
    path: &Path,
    native: &str,
) -> Result<(BufReader<File>, String, u64), String> {
    let profile = profile
        .canonicalize()
        .map_err(|_| "Native image profile unavailable")?;
    let actual = path
        .canonicalize()
        .map_err(|_| "Native image history unavailable")?;
    if ![profile.join("sessions"), profile.join("archived_sessions")]
        .iter()
        .any(|root| actual.starts_with(root))
        || actual.extension().is_none_or(|x| x != "jsonl")
    {
        return Err("Native image history is outside its configured profile".into());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| "Native image history unavailable")?;
    if !meta.is_file() {
        return Err("Native image history must be a regular file".into());
    }
    let file = File::open(&actual).map_err(|_| "Native image history unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "Native image history unavailable")?
        .is_file()
    {
        return Err("Invalid native image file".into());
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let first = line(&mut reader)?.ok_or("Native image history header unavailable")?;
    let header: Value =
        serde_json::from_slice(&first).map_err(|_| "Invalid native image header")?;
    if header["type"] != "session_meta" || header["payload"]["id"] != native {
        return Err("Native image session identity changed".into());
    }
    Ok((
        reader,
        format!("{:x}", Sha256::digest(&first)),
        first.len() as u64,
    ))
}
struct Scan {
    position: Position,
    images: Vec<(String, Value)>,
}
fn scan(
    profile: PathBuf,
    path: PathBuf,
    native: String,
    turn: String,
    wanted: Vec<Wanted>,
    mut position: Position,
) -> Result<Scan, String> {
    if position
        .paths
        .values()
        .flatten()
        .any(|p| !valid_asset(Path::new(p)))
    {
        position = Position::default();
    }
    if position.complete {
        return Ok(Scan {
            position,
            images: vec![],
        });
    }
    let (mut reader, header, start) = open(&profile, &path, &native)?;
    if !position.header.is_empty() && position.header != header {
        return Err("Native image history was replaced".into());
    }
    position.header = header;
    if position.offset == 0 {
        position.offset = start;
    }
    if position.offset
        > reader
            .get_ref()
            .metadata()
            .map_err(|_| "Native image history unavailable")?
            .len()
    {
        return Err("Native image history was shortened".into());
    }
    reader
        .seek(SeekFrom::Start(position.offset))
        .map_err(|_| "Cannot resume native image reader")?;
    let boundary = position.offset.saturating_add(SLICE);
    let mut images = vec![];
    let mut remaining = 8 * 1024 * 1024;
    while !position.complete && position.offset < boundary {
        let Some(bytes) = line(&mut reader)? else {
            break;
        };
        let v: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid native image history record")?;
        let p = &v["payload"];
        if (v["type"] == "event_msg" && p["type"] == "task_started") || v["type"] == "turn_context"
        {
            if let Some(id) = p["turn_id"].as_str() {
                if position.in_turn && id != turn {
                    position.complete = true;
                }
                position.in_turn = id == turn;
            }
        }
        if position.in_turn
            && v["type"] == "response_item"
            && ["custom_tool_call_output", "function_call_output"]
                .contains(&p["type"].as_str().unwrap_or(""))
        {
            for block in p["output"].as_array().into_iter().flatten().take(100) {
                if block["type"] != "input_image" {
                    continue;
                }
                let Some(url) = block["image_url"].as_str() else {
                    continue;
                };
                let Some((mime, data)) = url
                    .strip_prefix("data:")
                    .and_then(|s| s.split_once(";base64,"))
                else {
                    continue;
                };
                for target in &wanted {
                    if target.mime == mime
                        && data.starts_with(&target.prefix)
                        && images.len() < 8
                        && data.len() <= remaining * 4 / 3 + 4
                    {
                        // Match actual image content in the exact native turn;
                        // do not guess a relation from nearby tool names.
                        images.push((
                            target.key(),
                            json!({"type":"image","mimeType":mime,"data":data}),
                        ));
                        remaining = remaining.saturating_sub(data.len() * 3 / 4);
                    }
                }
            }
        }
        position.offset += bytes.len() as u64;
        if position.in_turn
            && v["type"] == "event_msg"
            && p["type"] == "task_complete"
            && p["turn_id"] == turn
        {
            position.complete = true;
        }
    }
    Ok(Scan { position, images })
}
impl Store {
    fn image_position(&self, key: &str) -> Result<Position, String> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_image_scan (id TEXT PRIMARY KEY, data TEXT NOT NULL)").map_err(|_|"Cannot prepare native image cursor")?;
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM native_image_scan WHERE id=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "Cannot read native image cursor")?;
        raw.map(|s| serde_json::from_str(&s).map_err(|_| "Invalid native image cursor".into()))
            .unwrap_or(Ok(Position::default()))
    }
    fn save_image_scan(
        &mut self,
        chat: &str,
        key: &str,
        mut scan: Scan,
    ) -> Result<Position, String> {
        for (item, image) in scan.images {
            for path in self.capture_tool_images(chat, &image)? {
                let paths = scan.position.paths.entry(item.clone()).or_default();
                if !paths.contains(&path) && paths.len() < 8 {
                    paths.push(path);
                }
            }
        }
        self.check_history(32768)?;
        self.db.execute("INSERT INTO native_image_scan(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",params![key,serde_json::to_string(&scan.position).map_err(|_|"Cannot encode native image cursor")?]).map_err(|_|"Cannot save native image cursor")?;
        Ok(scan.position)
    }
}
pub(crate) async fn recover(
    db: AuthService,
    profile: PathBuf,
    path: PathBuf,
    chat: String,
    native: String,
    turn: String,
    rows: &[Value],
) -> Result<Images, String> {
    let wanted = targets(rows);
    if wanted.is_empty() {
        return Ok(Images {
            complete: true,
            paths: Default::default(),
        });
    }
    let key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&chat, &native, &turn, &wanted))
                .map_err(|_| "Cannot identify native image cursor")?
        )
    );
    let lookup = key.clone();
    let position = db.call(move |a| a.store.image_position(&lookup)).await??;
    let lookup = wanted.clone();
    let scan =
        tokio::task::spawn_blocking(move || scan(profile, path, native, turn, wanted, position))
            .await
            .map_err(|_| "Native image reader stopped")??;
    let position = db
        .call(move |a| a.store.save_image_scan(&chat, &key, scan))
        .await??;
    Ok(resolved(position, &lookup))
}

fn valid_asset(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hash.update(&buffer[..n]),
            Err(_) => return false,
        }
    }
    path.file_stem().and_then(|s| s.to_str()) == Some(format!("{:x}", hash.finalize()).as_str())
}

fn resolved(position: Position, wanted: &[Wanted]) -> Images {
    let mut paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in wanted {
        if let Some(found) = position.paths.get(&target.key()).filter(|p| p.len() == 1) {
            let images = paths.entry(target.item.clone()).or_default();
            if !images.contains(&found[0]) {
                images.push(found[0].clone());
            }
        }
    }
    Images {
        complete: position.complete,
        paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use std::io::Write;
    struct Fixture {
        _dir: tempfile::TempDir,
        profile: PathBuf,
        path: PathBuf,
        db: AuthService,
        rows: Vec<Value>,
        bytes: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let profile = dir.path().join("profile");
            std::fs::create_dir_all(profile.join("sessions")).unwrap();
            let path = profile.join("sessions/rollout.jsonl");
            let db = AuthService::new(Auth::new(Store::open(&dir.path().join("state")).unwrap()))
                .unwrap();
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.extend(vec![7; 2048]);
            let data = STANDARD.encode(&bytes);
            let rows = vec![
                json!({"item":{"id":"tool-one","type":"mcpToolCall","result":{"content":[{"type":"text","text":json!({"content":[{"type":"image","mimeType":"image/png","data":format!("{}...[truncated]",&data[..900])}]}).to_string()}]}}}),
            ];
            let f = Self {
                _dir: dir,
                profile,
                path,
                db,
                rows,
                bytes,
            };
            let mut out = File::create(&f.path).unwrap();
            writeln!(out,"{}",json!({"type":"session_meta","payload":{"id":"native-one","base_instructions":"PRIVATE_MUST_NOT_APPEAR"}})).unwrap();
            f
        }
        fn append(&self, value: Value) {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap();
            writeln!(f, "{value}").unwrap();
        }
        fn turn(&self, id: &str, bytes: &[u8]) {
            self.append(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}}));
            self.append(json!({"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"RAW_OUTPUT_MUST_NOT_APPEAR"},{"type":"input_image","image_url":format!("data:image/png;base64,{}",STANDARD.encode(bytes))}]}}));
            self.append(
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id}}),
            );
        }
        async fn recover(&self) -> Result<Images, String> {
            recover(
                self.db.clone(),
                self.profile.clone(),
                self.path.clone(),
                "chat-one".into(),
                "native-one".into(),
                "turn-one".into(),
                &self.rows,
            )
            .await
        }
    }
    #[tokio::test]
    async fn matching_local_images_recover_incrementally_and_repair_corrupt_copies() {
        let f = Fixture::new();
        for _ in 0..18 {
            f.append(json!({"type":"response_item","payload":{"type":"message","content":"private".repeat(80000)}}));
        }
        f.turn("turn-other", &f.bytes);
        f.turn("turn-one", &f.bytes);
        let first = f.recover().await.unwrap();
        assert!(!first.complete);
        assert!(first.paths.is_empty());
        let done = f.recover().await.unwrap();
        assert!(done.complete);
        let paths = &done.paths["tool-one"];
        assert_eq!(paths.len(), 1);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), f.bytes);
        assert!(!serde_json::to_string(&done.paths)
            .unwrap()
            .contains("PRIVATE"));
        let cached = f.recover().await.unwrap();
        assert_eq!(cached.paths, done.paths);
        std::fs::write(&paths[0], vec![0; f.bytes.len()]).unwrap();
        assert!(!f.recover().await.unwrap().complete);
        let repaired = f.recover().await.unwrap();
        assert_eq!(repaired.paths, done.paths);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), f.bytes);
    }
    #[tokio::test]
    async fn rejects_foreign_paths_and_sessions_and_never_fetches_remote_images() {
        let mut f = Fixture::new();
        f.turn("turn-other", &f.bytes);
        f.turn("turn-one", b"\x89PNG\r\n\x1a\nnot-the-target");
        let done = f.recover().await.unwrap();
        assert!(done.complete);
        assert!(done.paths.is_empty());
        // Change the target identity to force a fresh cursor lookup.
        f.rows[0]["item"]["id"] = json!("new-item");
        let outside = f._dir.path().join("foreign.jsonl");
        std::fs::copy(&f.path, &outside).unwrap();
        f.path = outside;
        assert!(f.recover().await.err().unwrap().contains("outside"));
        f.path = f.profile.join("sessions/wrong.jsonl");
        std::fs::write(
            &f.path,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":"another-session"}})
            ),
        )
        .unwrap();
        assert!(f.recover().await.err().unwrap().contains("identity"));
    }
    #[test]
    fn ambiguous_matches_are_hidden_and_record_reads_are_bounded() {
        let f = Fixture::new();
        let wanted = targets(&f.rows);
        assert_eq!(wanted.len(), 1);
        let mut position = Position {
            complete: true,
            ..Default::default()
        };
        position.paths.insert(
            wanted[0].key(),
            vec!["first.png".into(), "different.png".into()],
        );
        assert!(resolved(position, &wanted).paths.is_empty());
        std::fs::write(&f.path, vec![b'x'; LINE + 2]).unwrap();
        let mut reader = BufReader::new(File::open(&f.path).unwrap());
        assert!(line(&mut reader).unwrap_err().contains("bounded"));
        let ordinary = vec![
            json!({"item":{"id":"ordinary","type":"mcpToolCall","result":{"content":[{"type":"text","text":"/etc/passwd https://example.com/image.png"}]}}}),
        ];
        assert!(targets(&ordinary).is_empty());
    }
}
