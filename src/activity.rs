//! Incremental, read-only native Codex activity for the existing dashboard.
//! Never retain prompts/tool content; bounded files, lines, work and cache size.
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::{self, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

const MAX_FILES: usize = 200;
const MAX_ROWS: usize = 400;
const MAX_LINE: usize = 256 * 1024;
const PER_FILE: usize = 4 * 1024 * 1024;
const READ_BUDGET: usize = 32 * 1024 * 1024;
const SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn opaque(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))[..16].into()
}
fn label(v: &Value, fallback: &str) -> String {
    v.as_str().unwrap_or(fallback).chars().take(160).collect()
}
fn count(v: &Value) -> u64 {
    v.as_u64().filter(|n| *n <= SAFE_INTEGER).unwrap_or(0)
}
fn totals(v: &Value) -> [u64; 3] {
    [
        count(&v["input_tokens"]),
        count(&v["output_tokens"]),
        count(&v["cached_input_tokens"]),
    ]
}
fn add(a: &mut u64, b: u64) {
    *a = a.saturating_add(b).min(SAFE_INTEGER);
}

#[derive(Clone, Serialize)]
struct Row {
    id: String,
    thread: String,
    at: i64,
    model: String,
    state: &'static str,
    input: u64,
    output: u64,
    cached: u64,
    measured: bool,
}
#[derive(Clone, Serialize)]
struct Usage {
    day: String,
    model: String,
    turns: u64,
    input: u64,
    output: u64,
    cached: u64,
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostics {
    files: usize,
    pending_files: usize,
    skipped_records: u64,
    unreadable_files: usize,
    limited: bool,
    missing_home: bool,
    config_read_only: bool,
}
#[derive(PartialEq, Eq, Clone)]
struct Identity {
    created: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}
fn identity(m: &Metadata) -> Identity {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Identity {
        created: m.created().ok(),
        #[cfg(unix)]
        dev: m.dev(),
        #[cfg(unix)]
        ino: m.ino(),
    }
}
fn plain(m: &Metadata) -> bool {
    if m.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
struct FileState {
    identity: Identity,
    modified: Option<SystemTime>,
    offset: u64,
    pending: Vec<u8>,
    discarding: bool,
    thread: String,
    provider: String,
    model: String,
    turn: String,
    cumulative: Option<[u64; 3]>,
    fork: bool,
    rows: BTreeMap<String, Row>,
    order: VecDeque<String>,
    usage: BTreeMap<String, Usage>,
    limited: bool,
}
impl FileState {
    fn new(path: &Path, m: &Metadata) -> Self {
        Self {
            identity: identity(m),
            modified: m.modified().ok(),
            offset: 0,
            pending: Vec::new(),
            discarding: false,
            thread: opaque(&path.to_string_lossy()),
            provider: "openai".into(),
            model: String::new(),
            turn: String::new(),
            cumulative: None,
            fork: false,
            rows: BTreeMap::new(),
            order: VecDeque::new(),
            usage: BTreeMap::new(),
            limited: false,
        }
    }
    fn consume(&mut self, line: &[u8]) -> bool {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            return false;
        };
        let p = &v["payload"];
        match v["type"].as_str() {
            Some("session_meta") => {
                self.thread = opaque(&label(&p["id"], &self.thread));
                self.provider = label(&p["model_provider"], "openai");
                self.fork = p["forked_from_id"].as_str().is_some_and(|s| !s.is_empty());
                return true;
            }
            Some("turn_context") => {
                self.model = label(&p["model"], &self.model);
                self.turn = label(&p["turn_id"], &self.turn);
                self.provider = label(&p["model_provider"], &self.provider);
                return true;
            }
            Some("event_msg") => {}
            _ => return true,
        }
        let Ok(at) = time::OffsetDateTime::parse(
            v["timestamp"].as_str().unwrap_or(""),
            &time::format_description::well_known::Rfc3339,
        ) else {
            return true;
        };
        let Some(kind) = p["type"].as_str() else {
            return true;
        };
        let token = &p["info"]["total_token_usage"];
        let measured = token.as_object().is_some_and(|o| !o.is_empty());
        let total = totals(token);
        if self.provider != "openai" {
            if kind == "token_count" && measured {
                self.cumulative = Some(total);
            }
            return true;
        }
        if ![
            "task_started",
            "task_complete",
            "turn_aborted",
            "token_count",
        ]
        .contains(&kind)
        {
            return true;
        }
        let millis = (at.unix_timestamp_nanos() / 1_000_000) as i64;
        let fallback = if kind == "task_started" {
            millis.to_string()
        } else if self.turn.is_empty() {
            "unattributed".into()
        } else {
            self.turn.clone()
        };
        self.turn = label(&p["turn_id"], &fallback);
        let id = opaque(&format!("{}:{}", self.thread, self.turn));
        if kind == "token_count" && !measured {
            return true;
        }
        let mut row = self.rows.get(&id).cloned().unwrap_or(Row {
            id: id.clone(),
            thread: self.thread.clone(),
            at: millis,
            model: if self.model.is_empty() {
                "Codex".into()
            } else {
                self.model.clone()
            },
            state: "unknown",
            input: 0,
            output: 0,
            cached: 0,
            measured: false,
        });
        row.at = row.at.max(millis);
        if !self.model.is_empty() {
            row.model = self.model.clone();
        }
        match kind {
            "task_started" => row.state = "running",
            "task_complete" => row.state = "completed",
            "turn_aborted" => row.state = "interrupted",
            _ => {}
        }
        if kind == "token_count" {
            let last = totals(&p["info"]["last_token_usage"]);
            let delta = match self.cumulative {
                None if self.fork => [0; 3],
                None => {
                    if (0..3).any(|i| total[i] > last[i]) {
                        self.limited = true;
                    }
                    std::array::from_fn(|i| last[i].min(total[i]))
                }
                Some(old) if (0..3).any(|i| total[i] < old[i]) => last,
                Some(old) => std::array::from_fn(|i| total[i] - old[i]),
            };
            self.cumulative = Some(total);
            if delta == [0; 3] {
                return true;
            }
            add(&mut row.input, delta[0]);
            add(&mut row.output, delta[1]);
            add(&mut row.cached, delta[2].min(delta[0]));
            row.measured = true;
            let day = at.to_offset(time::UtcOffset::UTC).date().to_string();
            let key = format!("{}:{}:{}", id, day, row.model);
            let bucket = self.usage.entry(key).or_insert_with(|| Usage {
                day,
                model: row.model.clone(),
                turns: 1,
                input: 0,
                output: 0,
                cached: 0,
            });
            add(&mut bucket.input, delta[0]);
            add(&mut bucket.output, delta[1]);
            add(&mut bucket.cached, delta[2].min(delta[0]));
            while self.usage.len() > MAX_ROWS * 2 {
                self.usage.pop_first();
                self.limited = true;
            }
        }
        if !self.rows.contains_key(&id) {
            self.order.push_back(id.clone());
        }
        self.rows.insert(id, row);
        while self.order.len() > MAX_ROWS {
            if let Some(id) = self.order.pop_front() {
                self.rows.remove(&id);
            }
            self.limited = true;
        }
        true
    }
    fn bytes(&mut self, bytes: &[u8], skipped: &mut u64) {
        for piece in bytes.split_inclusive(|b| *b == b'\n') {
            let complete = piece.last() == Some(&b'\n');
            if !self.discarding {
                if self.pending.len() + piece.len() > MAX_LINE {
                    self.pending.clear();
                    self.discarding = true;
                    *skipped = skipped.saturating_add(1);
                } else {
                    self.pending.extend_from_slice(piece);
                }
            }
            if complete {
                if !self.discarding && self.pending.iter().any(|b| !b.is_ascii_whitespace()) {
                    let line = std::mem::take(&mut self.pending);
                    if !self.consume(&line) {
                        *skipped = skipped.saturating_add(1);
                    }
                    self.pending = line;
                    self.pending.clear();
                }
                self.pending.clear();
                if self.pending.capacity() > 64 * 1024 {
                    self.pending = Vec::new();
                }
                self.discarding = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn row(kind: &str, payload: Value) -> String {
        format!(
            "{}\n",
            json!({"type":kind,"timestamp":"2026-01-02T00:30:00+01:00","payload":payload})
        )
    }
    fn context(fork: bool) -> String {
        row(
            "session_meta",
            json!({"id":"test-native-chat","model_provider":"openai","forked_from_id":if fork {json!("parent")}else{Value::Null},"cwd":"PRIVATE_PATH"}),
        ) + &row(
            "turn_context",
            json!({"model":"test-model","turn_id":"turn-one","developer_instructions":"PRIVATE_INSTRUCTIONS"}),
        ) + &row(
            "event_msg",
            json!({"type":"task_started","turn_id":"turn-one"}),
        )
    }
    fn tokens(total: [u64; 3], last: [u64; 3]) -> String {
        let usage = |a: [u64; 3]| json!({"input_tokens":a[0],"output_tokens":a[1],"cached_input_tokens":a[2]});
        row(
            "event_msg",
            json!({"type":"token_count","info":{"total_token_usage":usage(total),"last_token_usage":usage(last)}}),
        )
    }
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("sessions");
        fs::create_dir(&folder).unwrap();
        let path = folder.join("session.jsonl");
        (root, path)
    }
    fn append(path: &Path, text: &str) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    #[test]
    fn activity_is_incremental_redacted_and_deduplicated_across_archive_copies() {
        let (root, path) = fixture();
        let source = context(false)
            + &tokens([100, 20, 50], [100, 20, 50])
            + &row(
                "response_item",
                json!({"text":"PRIVATE_PROMPT","output":"PRIVATE_TOOL"}),
            );
        fs::write(&path, &source).unwrap();
        let mut state = State::default();
        let first = state.scan(root.path());
        assert_eq!(first["rows"][0]["state"], "running");
        assert_eq!(first["usage"][0]["day"], "2026-01-01");
        assert_eq!(first["usage"][0]["input"], 100);
        assert!(!first.to_string().contains("PRIVATE"));
        append(
            &path,
            &(tokens([120, 25, 60], [20, 5, 10])
                + &row(
                    "event_msg",
                    json!({"type":"task_complete","turn_id":"turn-one"}),
                )),
        );
        let archive = root.path().join("archived_sessions");
        fs::create_dir(&archive).unwrap();
        fs::copy(&path, archive.join("copy.jsonl")).unwrap();
        let second = state.scan(root.path());
        assert_eq!(second["rows"].as_array().unwrap().len(), 1);
        assert_eq!(second["rows"][0]["state"], "completed");
        assert_eq!(
            second["usage"][0],
            json!({"day":"2026-01-01","model":"test-model","turns":1,"input":120,"output":25,"cached":60})
        );
        assert_eq!(state.scan(root.path())["usage"], second["usage"]);
        fs::remove_file(path).unwrap();
        assert_eq!(state.scan(root.path())["usage"], second["usage"]);
    }
    #[test]
    fn activity_forks_resets_and_non_native_providers_do_not_invent_usage() {
        let (root, path) = fixture();
        fs::write(
            &path,
            context(true) + &tokens([1000, 200, 500], [100, 20, 50]),
        )
        .unwrap();
        let mut state = State::default();
        let first = state.scan(root.path());
        assert_eq!(first["usage"], json!([]));
        assert_eq!(first["rows"][0]["measured"], false);
        append(&path, &tokens([1010, 203, 505], [10, 3, 5]));
        let second = state.scan(root.path());
        assert_eq!(second["usage"][0]["input"], 10);
        append(&path, &tokens([4, 2, 2], [4, 2, 2]));
        assert_eq!(state.scan(root.path())["usage"][0]["input"], 14);
        append(
            &path,
            &(row("turn_context", json!({"model_provider":"other"}))
                + &tokens([40, 20, 20], [36, 18, 18])),
        );
        assert_eq!(state.scan(root.path())["usage"][0]["input"], 14);
        fs::write(&path, context(false) + &tokens([50, 10, 10], [5, 1, 1])).unwrap();
        let reset = state.scan(root.path());
        assert_eq!(reset["usage"][0]["input"], 5);
        assert_eq!(reset["diagnostics"]["limited"], true);
    }
    #[test]
    fn activity_handles_partial_utf8_oversized_records_and_bounded_resume() {
        let (root, path) = fixture();
        let original = context(false);
        fs::write(&path, &original).unwrap();
        let m = fs::metadata(&path).unwrap();
        let mut parsed = FileState::new(&path, &m);
        let mut skipped = 0;
        let text = row(
            "turn_context",
            json!({"model":"model-🙂","turn_id":"unicode"}),
        ) + &row(
            "event_msg",
            json!({"type":"task_started","turn_id":"unicode"}),
        );
        for byte in text.as_bytes() {
            parsed.bytes(&[*byte], &mut skipped);
        }
        assert_eq!(skipped, 0);
        assert_eq!(parsed.rows.values().next().unwrap().model, "model-🙂");
        append(
            &path,
            &("x".repeat(PER_FILE + 100) + "\n" + &tokens([7, 2, 3], [7, 2, 3])),
        );
        let mut state = State::default();
        let first = state.scan(root.path());
        assert_eq!(first["diagnostics"]["pendingFiles"], 1);
        assert_eq!(first["diagnostics"]["skippedRecords"], 1);
        let second = state.scan(root.path());
        assert_eq!(second["diagnostics"]["pendingFiles"], 0);
        assert_eq!(second["diagnostics"]["skippedRecords"], 1);
        assert_eq!(second["usage"][0]["input"], 7);
        assert!(state
            .files
            .values()
            .all(|f| f.pending.capacity() <= 64 * 1024));
    }
    #[test]
    fn activity_cache_is_bounded_and_missing_profile_clears_previous_results() {
        let (root, path) = fixture();
        let mut text = context(false);
        for i in 0..MAX_ROWS + 10 {
            text += &row(
                "event_msg",
                json!({"type":"task_started","turn_id":format!("t{i}")}),
            );
        }
        fs::write(path, text).unwrap();
        let mut state = State::default();
        let first = state.scan(root.path());
        assert_eq!(first["rows"].as_array().unwrap().len(), 200);
        assert_eq!(first["diagnostics"]["limited"], true);
        assert_eq!(state.files.values().next().unwrap().rows.len(), MAX_ROWS);
        let missing = state.scan(&root.path().join("missing"));
        assert_eq!(missing["diagnostics"]["missingHome"], true);
        assert!(state.files.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn activity_never_follows_directory_or_file_symlinks() {
        let (root, path) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.jsonl");
        fs::write(&secret, context(false) + &tokens([1, 1, 0], [1, 1, 0])).unwrap();
        std::os::unix::fs::symlink(&secret, path).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("sessions/foreign")).unwrap();
        let mut state = State::default();
        let report = state.scan(root.path());
        assert_eq!(report["diagnostics"]["files"], 0);
        assert_eq!(report["rows"], json!([]));
    }
}
#[derive(Default)]
struct State {
    files: HashMap<PathBuf, FileState>,
    skipped: u64,
    cached: Option<(Instant, Value)>,
}
#[derive(Clone)]
pub(crate) struct Reader {
    home: PathBuf,
    state: Arc<Mutex<State>>,
    gate: Arc<tokio::sync::Semaphore>,
}
impl Reader {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            state: Arc::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    pub async fn read(&self) -> Result<Value, String> {
        let permit = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Activity is being scanned; retry shortly")?;
        let home = self.home.clone();
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut state = state.lock().map_err(|_| "Activity reader unavailable")?;
            if let Some((at, v)) = &state.cached {
                if at.elapsed() < Duration::from_secs(2) {
                    return Ok(v.clone());
                }
            }
            let v = state.scan(&home);
            state.cached = Some((Instant::now(), v.clone()));
            Ok(v)
        })
        .await
        .map_err(|_| "Activity reader stopped".to_owned())?
    }
}
impl State {
    fn scan(&mut self, home: &Path) -> Value {
        let mut d = Diagnostics {
            config_read_only: true,
            ..Default::default()
        };
        if !fs::symlink_metadata(home).is_ok_and(|m| m.is_dir() && plain(&m)) {
            self.files.clear();
            d.missing_home = true;
            return json!({"generatedAt":crate::auth::now(),"source":"native-codex","rows":[],"usage":[],"diagnostics":d});
        }
        let mut directories =
            VecDeque::from([home.join("sessions"), home.join("archived_sessions")]);
        let mut selected = Vec::new();
        let mut inspected = 0;
        while let Some(dir) = directories.pop_front() {
            if inspected >= 10_000 {
                d.limited = true;
                break;
            }
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() && plain(&m) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {
                    d.unreadable_files += 1;
                    continue;
                }
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                d.unreadable_files += 1;
                continue;
            };
            for entry in entries {
                inspected += 1;
                if inspected > 10_000 {
                    d.limited = true;
                    break;
                }
                let Ok(entry) = entry else {
                    d.unreadable_files += 1;
                    continue;
                };
                let path = entry.path();
                let Ok(m) = fs::symlink_metadata(&path) else {
                    d.unreadable_files += 1;
                    continue;
                };
                if !plain(&m) {
                    continue;
                }
                if m.is_dir() {
                    directories.push_back(path);
                } else if m.is_file() && path.extension().is_some_and(|v| v == "jsonl") {
                    selected.push((path, m));
                }
            }
        }
        selected.sort_by(|a, b| {
            b.1.modified()
                .ok()
                .cmp(&a.1.modified().ok())
                .then_with(|| a.0.cmp(&b.0))
        });
        if selected.len() > MAX_FILES {
            d.limited = true;
            selected.truncate(MAX_FILES);
        }
        let paths: HashSet<_> = selected.iter().map(|(p, _)| p.clone()).collect();
        self.files.retain(|p, _| paths.contains(p));
        d.files = selected.len();
        let mut budget = READ_BUDGET;
        for (path, m) in selected {
            if self.files.get(&path).is_none_or(|s| {
                s.identity != identity(&m)
                    || m.len() < s.offset
                    || (m.len() == s.offset && m.modified().ok() != s.modified)
            }) {
                self.files.insert(path.clone(), FileState::new(&path, &m));
            }
            let state = self.files.get_mut(&path).unwrap();
            state.modified = m.modified().ok();
            let size = m
                .len()
                .saturating_sub(state.offset)
                .min(PER_FILE as u64)
                .min(budget as u64) as usize;
            if size > 0 {
                let result = (|| -> std::io::Result<()> {
                    let mut options = OpenOptions::new();
                    options.read(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                    }
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::OpenOptionsExt;
                        options.custom_flags(0x00200000);
                    }
                    let mut file = options.open(&path)?;
                    let opened = file.metadata()?;
                    if !opened.is_file() || !plain(&opened) || identity(&opened) != state.identity {
                        return Err(std::io::Error::other("Session changed"));
                    }
                    file.seek(SeekFrom::Start(state.offset))?;
                    let mut remaining = size;
                    let mut buffer = [0; 64 * 1024];
                    while remaining > 0 {
                        let length = remaining.min(buffer.len());
                        let n = file.read(&mut buffer[..length])?;
                        if n == 0 {
                            break;
                        }
                        state.offset += n as u64;
                        budget -= n;
                        remaining -= n;
                        state.bytes(&buffer[..n], &mut self.skipped);
                    }
                    Ok(())
                })();
                if result.is_err() {
                    d.unreadable_files += 1;
                }
            }
            if m.len() > state.offset {
                d.pending_files += 1;
            }
            d.limited |= state.limited;
        }
        let mut rows: BTreeMap<String, Row> = BTreeMap::new();
        let mut buckets: BTreeMap<String, Usage> = BTreeMap::new();
        for file in self.files.values() {
            for (id, row) in &file.rows {
                if rows.get(id).is_none_or(|old| {
                    old.at < row.at || old.input + old.output < row.input + row.output
                }) {
                    rows.insert(id.clone(), row.clone());
                }
            }
            for (id, row) in &file.usage {
                if buckets
                    .get(id)
                    .is_none_or(|old| old.input + old.output < row.input + row.output)
                {
                    buckets.insert(id.clone(), row.clone());
                }
            }
        }
        let mut usage: BTreeMap<(String, String), Usage> = BTreeMap::new();
        for row in buckets.values() {
            let bucket = usage
                .entry((row.day.clone(), row.model.clone()))
                .or_insert_with(|| Usage {
                    day: row.day.clone(),
                    model: row.model.clone(),
                    turns: 0,
                    input: 0,
                    output: 0,
                    cached: 0,
                });
            add(&mut bucket.turns, 1);
            add(&mut bucket.input, row.input);
            add(&mut bucket.output, row.output);
            add(&mut bucket.cached, row.cached);
        }
        while usage.len() > 2000 {
            usage.pop_first();
            d.limited = true;
        }
        let mut rows: Vec<_> = rows.into_values().collect();
        rows.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
        rows.truncate(200);
        let usage: Vec<_> = usage.into_values().rev().collect();
        d.skipped_records = self.skipped;
        json!({"generatedAt":crate::auth::now(),"source":"native-codex","rows":rows,"usage":usage,"diagnostics":d})
    }
}
