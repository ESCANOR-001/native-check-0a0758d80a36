//! Saved app-server projections can synthesize `interrupted` for another
//! process's live turn. Accept terminal state only from the native lifecycle.
use serde::Deserialize;
use serde_json::Value;
use std::{
    io::{BufRead, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};
const ERROR: &str = "Original Codex task state is not verified; its task was left running";
fn spelling(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(v) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{v}"));
        }
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }
    #[cfg(not(windows))]
    {
        p.into()
    }
}
/// Per connection, bounded native lifecycle index. Start near the tail when
/// recent events establish state; subsequent polls read only appended records.
#[derive(Clone, Default)]
pub(crate) struct Tracker {
    identity: Option<crate::storage_stage::Identity>,
    path: PathBuf,
    native: String,
    offset: u64,
    anchor: Vec<u8>,
    latest: Option<(String, String)>,
    turns: std::collections::VecDeque<(String, String)>,
    errors: std::collections::BTreeMap<String, Value>,
    timings: std::collections::BTreeMap<String, (Option<i64>, Option<i64>)>,
    usage: Option<Value>,
    compressed_stamp: Option<(u64, std::time::SystemTime)>,
    complete: bool,
}

// History RPCs create short-lived connections. Retain only the small verified
// lifecycle index, not a transcript or an execution/approval connection. Every
// reuse still validates the file, its native ID, identity, length and timestamp.
#[derive(Default)]
struct CompressedCache {
    entries: std::collections::VecDeque<(Tracker, usize)>,
    bytes: usize,
}
impl CompressedCache {
    const BYTES: usize = 8 * 1024 * 1024;
    fn get(
        &mut self,
        path: &Path,
        identity: &crate::storage_stage::Identity,
        stamp: (u64, std::time::SystemTime),
        native: &str,
    ) -> Option<Tracker> {
        let at = self.entries.iter().position(|(t, _)| {
            t.complete
                && t.path == path
                && t.native == native
                && t.identity.as_ref() == Some(identity)
                && t.compressed_stamp == Some(stamp)
        })?;
        let row = self.entries.remove(at)?;
        let result = row.0.clone();
        self.entries.push_back(row);
        Some(result)
    }
    fn insert(&mut self, tracker: &Tracker) {
        let size = tracker.path.as_os_str().len() * 2
            + tracker.native.len()
            + tracker.anchor.len()
            + tracker
                .turns
                .iter()
                .map(|(a, b)| a.len() + b.len() + 128)
                .sum::<usize>()
            + tracker
                .errors
                .iter()
                .map(|(id, e)| id.len() + e.to_string().len() + 128)
                .sum::<usize>()
            + tracker
                .timings
                .keys()
                .map(|id| id.len() + 128)
                .sum::<usize>()
            + 4096;
        if size > Self::BYTES {
            return;
        }
        self.entries
            .retain(|(t, _)| !(t.path == tracker.path && t.native == tracker.native));
        self.bytes = self.entries.iter().map(|(_, n)| n).sum();
        while self.entries.len() >= 32 || self.bytes + size > Self::BYTES {
            if let Some((_, n)) = self.entries.pop_front() {
                self.bytes -= n;
            }
        }
        self.entries.push_back((tracker.clone(), size));
        self.bytes += size;
    }
}
static COMPRESSED: std::sync::OnceLock<std::sync::Mutex<CompressedCache>> =
    std::sync::OnceLock::new();
#[cfg(test)]
thread_local! { static SCANNED_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
impl Tracker {
    /// A terminal native record can prove a saved result even when Codex's
    /// separate paginated index has no rows. This is not proof of a live owner.
    pub(crate) fn saved_terminal(&self) -> Option<Value> {
        if !self.complete {
            return None;
        }
        let (id, status) = self.latest.as_ref()?;
        if !matches!(status.as_str(), "completed" | "interrupted" | "failed") {
            return None;
        }
        let (started, completed) = self.timings.get(id)?;
        let completed = (*completed)?;
        Some(serde_json::json!({
            "id":id,"status":status,"error":self.error(id),"items":[],
            "itemsView":"notLoaded","startedAt":started,
            "completedAt":completed,"remodexStatusOnly":true
        }))
    }
    pub(crate) fn error(&self, turn: &str) -> Option<Value> {
        self.errors.get(turn).cloned()
    }
    /// Provider-reported context, retained independently of turn transitions.
    pub(crate) fn usage(&self) -> Option<Value> {
        self.usage.clone()
    }
    pub(crate) fn state(&self, turn: &str) -> Option<&str> {
        self.turns
            .iter()
            .find(|(id, _)| id == turn)
            .map(|(_, s)| s.as_str())
    }
    pub(crate) fn read(
        &mut self,
        profile: &Path,
        path: &Path,
        native: &str,
    ) -> Result<(String, String), String> {
        let profile = spelling(profile);
        let path = spelling(path);
        crate::storage_stage::open(&profile, true).map_err(|_| ERROR)?;
        let canonical = spelling(&profile.canonicalize().map_err(|_| ERROR)?);
        let relative = path
            .strip_prefix(&profile)
            .or_else(|_| path.strip_prefix(&canonical))
            .map_err(|_| ERROR)?;
        let parts: Vec<_> = relative.components().collect();
        if parts.len() < 2
            || !matches!(parts.first(),Some(Component::Normal(p)) if *p=="sessions" || *p=="archived_sessions")
            || parts.iter().any(|p| !matches!(p, Component::Normal(_)))
        {
            return Err(ERROR.into());
        }
        let mut checked = canonical;
        crate::storage_stage::open(&checked, true).map_err(|_| ERROR)?;
        for part in parts.iter().take(parts.len() - 1) {
            checked.push(part);
            crate::storage_stage::open(&checked, true).map_err(|_| ERROR)?;
        }
        checked.push(parts.last().ok_or(ERROR)?);
        let rollout =
            crate::codex_rollout::open(&checked, 8 * 1024 * 1024 * 1024).map_err(|_| ERROR)?;
        if rollout.compressed {
            let result = self.read_compressed(rollout, native);
            if result.is_err() {
                *self = Self::default();
            }
            return result;
        }
        checked = rollout.physical.clone();
        drop(rollout);
        let mut file = crate::storage_stage::open(&checked, false).map_err(|_| ERROR)?;
        let identity = crate::storage_stage::identity(&file).map_err(|_| ERROR)?;
        let size = file.metadata().map_err(|_| ERROR)?.len();
        if size == 0
            || size > 8 * 1024 * 1024 * 1024
            || crate::storage_stage::links(&file).map_err(|_| ERROR)? != 1
        {
            return Err(ERROR.into());
        }
        let mut header = Vec::new();
        std::io::BufReader::new((&mut file).take(65536))
            .read_until(b'\n', &mut header)
            .map_err(|_| ERROR)?;
        let header: Value = serde_json::from_slice(&header).map_err(|_| ERROR)?;
        if header["type"] != "session_meta" || header["payload"]["id"] != native {
            return Err(ERROR.into());
        }
        let same = self.compressed_stamp.is_none()
            && self.identity.as_ref() == Some(&identity)
            && self.path == checked
            && self.native == native
            && self.offset <= size;
        let anchored = if same && !self.anchor.is_empty() {
            file.seek(SeekFrom::Start(self.offset - self.anchor.len() as u64))
                .map_err(|_| ERROR)?;
            let mut actual = vec![0; self.anchor.len()];
            file.read_exact(&mut actual).map_err(|_| ERROR)?;
            actual == self.anchor
        } else {
            same
        };
        if !anchored {
            *self = Self::default();
        }
        self.identity = Some(identity.clone());
        self.path = checked.clone();
        self.native = native.into();
        // The latest native event is usually near the end. Do not parse old
        // multi-megabyte attachments merely to establish the current status.
        // Align to a complete JSONL record; if no recent event proves state,
        // fall back to the bounded-memory full scan (e.g. a long active tool).
        const RECENT: u64 = 1024 * 1024;
        let tail = !anchored && size > RECENT;
        if tail {
            file.seek(SeekFrom::Start(size - RECENT))
                .map_err(|_| ERROR)?;
            let mut reader = std::io::BufReader::new((&mut file).take(RECENT));
            let mut partial = RecordReader::new(&mut reader);
            std::io::copy(&mut partial, &mut std::io::sink()).map_err(|_| ERROR)?;
            if partial.complete {
                self.offset = size - RECENT + partial.bytes;
            }
        }
        self.scan(&mut file, size)?;
        if tail && self.latest.is_none() {
            self.offset = 0;
            self.anchor.clear();
            self.turns.clear();
            self.errors.clear();
            self.timings.clear();
            self.scan(&mut file, size)?;
        }
        if file.metadata().map_err(|_| ERROR)?.len() < size
            || crate::storage_stage::identity(
                &crate::storage_stage::open(&checked, false).map_err(|_| ERROR)?,
            )
            .map_err(|_| ERROR)?
                != identity
        {
            *self = Self::default();
            return Err(ERROR.into());
        }
        self.latest.clone().ok_or_else(|| ERROR.into())
    }

    fn read_compressed(
        &mut self,
        mut rollout: crate::codex_rollout::Rollout,
        native: &str,
    ) -> Result<(String, String), String> {
        const LIMIT: u64 = 8 * 1024 * 1024 * 1024;
        let stamp = (
            rollout.metadata.len(),
            rollout.metadata.modified().map_err(|_| ERROR)?,
        );
        let mut bounded = (&mut rollout.reader).take(LIMIT + 1);
        let mut reader = std::io::BufReader::new(&mut bounded);
        let mut bytes = Vec::new();
        (&mut reader)
            .take(65536)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| ERROR)?;
        let header: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
        if bytes.last() != Some(&b'\n')
            || header["type"] != "session_meta"
            || header["payload"]["id"] != native
        {
            return Err(ERROR.into());
        }
        if self.compressed_stamp == Some(stamp)
            && self.identity.as_ref() == Some(&rollout.identity)
            && self.path == rollout.physical
            && self.native == native
        {
            rollout.verify(false).map_err(|_| ERROR)?;
            return self.latest.clone().ok_or_else(|| ERROR.into());
        }
        let cache = COMPRESSED.get_or_init(Default::default);
        let cached = cache.lock().map_err(|_| ERROR)?.get(
            &rollout.physical,
            &rollout.identity,
            stamp,
            native,
        );
        if let Some(cached) = cached {
            rollout.verify(false).map_err(|_| ERROR)?;
            *self = cached;
            return self.latest.clone().ok_or_else(|| ERROR.into());
        }
        // A compressed file is immutable. Scan it once with bounded memory,
        // then reuse only its verified identity/size/time until it changes.
        // Plain files retain the fast tail and incremental append reader.
        let mut next = Self {
            identity: Some(rollout.identity.clone()),
            path: rollout.physical.clone(),
            native: native.into(),
            offset: bytes.len() as u64,
            compressed_stamp: Some(stamp),
            ..Self::default()
        };
        let complete = next.scan_records(&mut reader)?;
        if !complete || bounded.limit() == 0 {
            return Err(ERROR.into());
        }
        rollout.verify(false).map_err(|_| ERROR)?;
        let result = next.latest.clone().ok_or(ERROR)?;
        next.complete = true;
        cache.lock().map_err(|_| ERROR)?.insert(&next);
        *self = next;
        Ok(result)
    }
    fn scan(&mut self, file: &mut std::fs::File, size: u64) -> Result<(), String> {
        file.seek(SeekFrom::Start(self.offset)).map_err(|_| ERROR)?;
        let mut reader = std::io::BufReader::new(file.take(size - self.offset));
        self.complete = self.scan_records(&mut reader)?;
        Ok(())
    }
    fn scan_records(&mut self, reader: &mut impl BufRead) -> Result<bool, String> {
        loop {
            // Image/tool records can be much larger than an IPC frame. Read
            // their JSON structure, ignoring unrelated payload fields rather
            // than allocating or rejecting their base64/text contents.
            let mut line = RecordReader::new(&mut *reader);
            let row =
                serde_json::from_reader::<_, LifecycleRecord>(std::io::BufReader::new(&mut line));
            #[cfg(test)]
            SCANNED_BYTES.with(|bytes| bytes.set(bytes.get() + line.bytes));
            if row.is_err() {
                // A damaged old output record must not hide later valid task
                // events. Drain just this record with bounded memory, and drop
                // earlier proof until another complete lifecycle event arrives.
                std::io::copy(&mut line, &mut std::io::sink()).map_err(|_| ERROR)?;
            }
            if !line.complete {
                return Ok(line.bytes == 0); // A partial append cannot establish a terminal state.
            }
            let row = match row {
                Ok(row) => row,
                Err(_) => {
                    self.latest = None;
                    self.turns.clear();
                    self.errors.clear();
                    self.timings.clear();
                    self.offset += line.bytes;
                    self.anchor = line.tail;
                    continue;
                }
            };
            if row.kind == "event_msg" {
                if row.payload.kind.as_deref() == Some("token_count") {
                    if let Some(info) = row.payload.info {
                        self.usage = Some(serde_json::json!({
                            "last":{"totalTokens":info.last_token_usage.and_then(|u|u.total_tokens)},
                            "modelContextWindow":info.model_context_window
                        }));
                    }
                }
                let error = row.payload.error.and_then(|e| {
                    crate::codex_error::thin(&serde_json::json!({
                        "message":e.message,"codexErrorInfo":e.code,"additionalDetails":e.details
                    }))
                });
                let status = match row.payload.kind.as_deref() {
                    Some("task_started") => Some("inProgress"),
                    Some("task_complete") if error.is_some() => Some("failed"),
                    Some("task_complete") => Some("completed"),
                    Some("turn_aborted") => Some("interrupted"),
                    _ => None,
                };
                if let Some(status) = status {
                    let id = row
                        .payload
                        .turn_id
                        .as_deref()
                        .filter(|s| !s.is_empty() && s.len() <= 256)
                        .ok_or(ERROR)?;
                    if let Some(error) = error {
                        self.errors.insert(id.to_owned(), error);
                    } else if status == "inProgress" {
                        self.errors.remove(id);
                    }
                    let timestamp = row.timestamp.as_deref().and_then(|value| {
                        time::OffsetDateTime::parse(
                            value,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .ok()
                        .map(|at| at.unix_timestamp())
                        .filter(|at| *at > 0)
                    });
                    let timing = self.timings.entry(id.to_owned()).or_default();
                    if status == "inProgress" {
                        *timing = (timestamp, None);
                    } else {
                        timing.1 = timestamp;
                    }
                    if let Some(at) = self.turns.iter().position(|(t, _)| t == id) {
                        self.turns.remove(at);
                    }
                    self.turns.push_back((id.into(), status.into()));
                    if self.turns.len() > 256 {
                        if let Some((old, _)) = self.turns.pop_front() {
                            self.errors.remove(&old);
                            self.timings.remove(&old);
                        }
                    }
                    self.latest = Some((id.into(), status.into()));
                }
            }
            self.offset += line.bytes;
            self.anchor = line.tail;
        }
    }
}
// Serde skips unknown fields without materializing their strings/arrays.
// Only native lifecycle tags and the last context measurement are retained.
#[derive(Deserialize)]
struct LifecycleRecord {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: LifecyclePayload,
}
#[derive(Default, Deserialize)]
struct LifecyclePayload {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    info: Option<TokenUsageInfo>,
    #[serde(default)]
    error: Option<TerminalError>,
}
#[derive(Deserialize)]
struct TerminalError {
    message: Option<String>,
    #[serde(default, rename = "codex_error_info", alias = "codexErrorInfo")]
    code: Option<Value>,
    #[serde(default, rename = "additionalDetails")]
    details: Option<String>,
}
// Ignore cumulative usage and unrelated account/tool data without allocating it.
#[derive(Deserialize)]
struct TokenUsageInfo {
    last_token_usage: Option<LastTokenUsage>,
    model_context_window: Option<i64>,
}
#[derive(Deserialize)]
struct LastTokenUsage {
    total_tokens: Option<i64>,
}
struct RecordReader<'a, R> {
    input: &'a mut R,
    bytes: u64,
    complete: bool,
    tail: Vec<u8>,
}
impl<'a, R: BufRead> RecordReader<'a, R> {
    fn new(input: &'a mut R) -> Self {
        Self {
            input,
            bytes: 0,
            complete: false,
            tail: Vec::with_capacity(256),
        }
    }
}
impl<R: BufRead> Read for RecordReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.complete || out.is_empty() {
            return Ok(0);
        }
        let available = self.input.fill_buf()?;
        // serde may ask for a single byte. Searching the entire remaining
        // buffer on every such read makes large saved records quadratic.
        let available = &available[..available.len().min(out.len())];
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1)
            .min(out.len());
        if n == 0 {
            return Ok(0);
        }
        out[..n].copy_from_slice(&available[..n]);
        self.complete = out[n - 1] == b'\n';
        self.bytes += n as u64;
        let keep = n.min(256);
        let old = self.tail.len().min(256 - keep);
        self.tail.drain(..self.tail.len() - old);
        self.tail.extend_from_slice(&out[n - keep..n]);
        self.input.consume(n);
        Ok(n)
    }
}
#[cfg(test)]
fn latest(profile: &Path, path: &Path, native: &str) -> Result<(String, String), String> {
    Tracker::default().read(profile, path, native)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_connections_reuse_unchanged_compressed_state_but_revalidate_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let path = root.path().join("sessions/reconnect.jsonl");
        let compressed = path.with_extension("jsonl.zst");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let event = |kind: &str, turn: &str| {
            format!(
                "{}\n",
                json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn}})
            )
        };
        let large = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"text":"x".repeat(1024*1024)}})
        );
        let original = format!("{head}{large}{}", event("task_complete", "first"));
        let encode = |s: &str| zstd::stream::encode_all(s.as_bytes(), 1).unwrap();
        std::fs::write(&compressed, encode(&original)).unwrap();
        let before = SCANNED_BYTES.with(|n| n.get());
        assert_eq!(
            Tracker::default()
                .read(root.path(), &path, "native")
                .unwrap()
                .1,
            "completed"
        );
        let first = SCANNED_BYTES.with(|n| n.get());
        assert!(first - before > 1024 * 1024);
        // Each Tracker represents a new, independently closed history reader.
        for _ in 0..4 {
            assert_eq!(
                Tracker::default()
                    .read(root.path(), &path, "native")
                    .unwrap(),
                ("first".into(), "completed".into())
            );
        }
        assert_eq!(
            SCANNED_BYTES.with(|n| n.get()),
            first,
            "Reconnecting must not rescan unchanged compressed records"
        );
        assert!(Tracker::default()
            .read(root.path(), &path, "wrong")
            .is_err());
        std::fs::write(
            &compressed,
            encode(&format!("{head}{}", event("turn_aborted", "replacement"))),
        )
        .unwrap();
        assert_eq!(
            Tracker::default()
                .read(root.path(), &path, "native")
                .unwrap(),
            ("replacement".into(), "interrupted".into())
        );
        std::fs::write(&path, format!("{head}{}", event("task_started", "live"))).unwrap();
        assert_eq!(
            Tracker::default()
                .read(root.path(), &path, "native")
                .unwrap(),
            ("live".into(), "inProgress".into())
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&compressed, encode(&format!("{head}{{\"type\":"))).unwrap();
        assert!(Tracker::default()
            .read(root.path(), &path, "native")
            .is_err());
    }

    #[test]
    fn compressed_lifecycle_cache_is_bounded_and_separates_file_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, "one").unwrap();
        let file = crate::storage_stage::open(&path, false).unwrap();
        let identity = crate::storage_stage::identity(&file).unwrap();
        let stamp = (3, file.metadata().unwrap().modified().unwrap());
        let mut cache = CompressedCache::default();
        for n in 0..40 {
            cache.insert(&Tracker {
                path: path.clone(),
                native: format!("n{n}"),
                identity: Some(identity.clone()),
                compressed_stamp: Some(stamp),
                complete: true,
                ..Default::default()
            });
        }
        assert_eq!(cache.entries.len(), 32);
        assert!(cache.bytes <= CompressedCache::BYTES);
        assert!(cache.get(&path, &identity, stamp, "n0").is_none());
        assert!(cache.get(&path, &identity, stamp, "n39").is_some());
        assert!(cache
            .get(&path.with_extension("other"), &identity, stamp, "n39")
            .is_none());
        let other = root.path().join("other");
        std::fs::write(&other, "two").unwrap();
        let other =
            crate::storage_stage::identity(&crate::storage_stage::open(&other, false).unwrap())
                .unwrap();
        assert!(cache.get(&path, &other, stamp, "n39").is_none());
        assert!(cache.get(&path, &identity, (4, stamp.1), "n39").is_none());
    }

    #[test]
    fn compressed_task_state_survives_storage_changes_without_rewriting_history() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let plain = root.path().join("sessions/saved.jsonl");
        let compressed = plain.with_extension("jsonl.zst");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let event = |kind: &str| {
            format!(
                "{}\n",
                json!({"type":"event_msg","payload":{"type":kind,"turn_id":"turn"}})
            )
        };
        let saved = format!("{head}{}", event("task_complete"));
        let bytes = zstd::stream::encode_all(saved.as_bytes(), 1).unwrap();
        std::fs::write(&compressed, &bytes).unwrap();
        let mut tracker = Tracker::default();
        for _ in 0..2 {
            assert_eq!(
                tracker.read(root.path(), &plain, "native").unwrap().1,
                "completed"
            );
            assert_eq!(tracker.state("turn"), Some("completed"));
        }
        assert!(!plain.exists());
        assert_eq!(std::fs::read(&compressed).unwrap(), bytes);
        assert!(Tracker::default()
            .read(root.path(), &plain, "wrong-id")
            .is_err());
        std::fs::write(&plain, format!("{head}{}", event("task_started"))).unwrap();
        assert_eq!(
            tracker.read(root.path(), &plain, "native").unwrap().1,
            "inProgress"
        );
        std::fs::remove_file(&plain).unwrap();
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(format!("{head}{}", event("turn_aborted")).as_bytes(), 1)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            tracker.read(root.path(), &plain, "native").unwrap().1,
            "interrupted"
        );
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(format!("{saved}{{\"type\":").as_bytes(), 1).unwrap(),
        )
        .unwrap();
        assert!(tracker.read(root.path(), &plain, "native").is_err());
        assert!(tracker.state("turn").is_none());
        std::fs::write(&compressed, b"broken compressed stream").unwrap();
        assert!(tracker.read(root.path(), &plain, "native").is_err());
    }

    #[test]
    fn task_complete_with_remote_compact_error_is_failed_and_scoped_to_its_turn() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let path = root.path().join("sessions/error.jsonl");
        let event = |payload: Value| format!("{}\n", json!({"type":"event_msg","payload":payload}));
        std::fs::write(
            &path,
            format!(
                "{}\n{}",
                json!({"type":"session_meta","payload":{"id":"native"}}),
                event(json!({"type":"task_started","turn_id":"failed-turn"}))
            ),
        )
        .unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        let message = "Error running remote compact task: {\"detail\":\"Bad Request\"}";
        let complete = event(
            json!({"type":"task_complete","turn_id":"failed-turn","last_agent_message":"Still checking","error":{"message":message,"codex_error_info":"other","private":"must not retain"}}),
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&complete.as_bytes()[..complete.len() - 1])
            .unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        assert!(tracker.error("failed-turn").is_none());
        file.write_all(b"\n").unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "failed"
        );
        assert_eq!(
            tracker.error("failed-turn").unwrap(),
            json!({"message":message,"codexErrorInfo":"other"})
        );
        file.write_all(event(json!({"type":"task_started","turn_id":"next"})).as_bytes())
            .unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        assert!(tracker.error("next").is_none());
        assert_eq!(tracker.state("failed-turn"), Some("failed"));
        assert_eq!(tracker.error("failed-turn").unwrap()["message"], message);
        // Keep only the same bounded number of error records as lifecycle rows.
        for n in 0..260 {
            file.write_all(event(json!({"type":"task_complete","turn_id":format!("t{n}"),"error":{"message":"Bad Request"}})).as_bytes()).unwrap();
        }
        tracker.read(root.path(), &path, "native").unwrap();
        assert!(tracker.error("failed-turn").is_none());
        assert!(tracker.errors.len() <= 256);
    }

    #[test]
    fn saved_usage_survives_idle_partial_appends_and_resets_on_replacement() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let p = root.path().join("sessions/usage.jsonl");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let event = |payload: Value| format!("{}\n", json!({"type":"event_msg","payload":payload}));
        let usage = |used: i64| {
            event(
                json!({"type":"token_count","info":{"last_token_usage":{"total_tokens":used},"total_token_usage":{"total_tokens":99999999},"model_context_window":258400},"rate_limits":{"primary":{"used_percent":5,"window_minutes":10080,"resets_at":1793599688},"credits":{"secret":"PRIVATE"}}}),
            )
        };
        std::fs::write(
            &p,
            format!(
                "{head}{}{}{}",
                event(json!({"type":"task_started","turn_id":"turn"})),
                usage(42000),
                event(json!({"type":"task_complete","turn_id":"turn"}))
            ),
        )
        .unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(root.path(), &p, "native").unwrap().1,
            "completed"
        );
        assert_eq!(tracker.usage().unwrap()["last"]["totalTokens"], 42000);
        assert_eq!(tracker.usage().unwrap()["modelContextWindow"], 258400);
        let next = usage(12000);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&next.as_bytes()[..next.len() - 1]).unwrap();
        tracker.read(root.path(), &p, "native").unwrap();
        assert_eq!(tracker.usage().unwrap()["last"]["totalTokens"], 42000);
        f.write_all(b"\n").unwrap();
        tracker.read(root.path(), &p, "native").unwrap();
        assert_eq!(tracker.usage().unwrap()["last"]["totalTokens"], 12000);
        // A quota-only event must not erase the measured context.
        f.write_all(event(json!({"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":7,"window_minutes":10080}}})).as_bytes()).unwrap();
        tracker.read(root.path(), &p, "native").unwrap();
        assert_eq!(tracker.usage().unwrap()["last"]["totalTokens"], 12000);
        std::fs::write(
            &p,
            format!(
                "{head}{}",
                event(json!({"type":"task_started","turn_id":"replacement"}))
            ),
        )
        .unwrap();
        tracker.read(root.path(), &p, "native").unwrap();
        assert!(tracker.usage().is_none());
    }

    #[test]
    fn initial_status_uses_recent_events_and_falls_back_when_tool_output_fills_tail() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let p = root.path().join("sessions/large.jsonl");
        let event = |kind: &str, turn: &str| {
            format!(
                "{}\n",
                json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn}})
            )
        };
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let large = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"text":"x".repeat(2*1024*1024)}})
        );
        let prefix = format!("{head}{}{large}", event("task_started", "old"));
        std::fs::write(&p, format!("{prefix}{}", event("task_complete", "recent"))).unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(root.path(), &p, "native").unwrap(),
            ("recent".into(), "completed".into())
        );
        assert_eq!(
            tracker.state("old"),
            None,
            "initial read must skip the older prefix"
        );
        std::fs::write(&p, &prefix).unwrap();
        assert_eq!(
            Tracker::default().read(root.path(), &p, "native").unwrap(),
            ("old".into(), "inProgress".into())
        );
        std::fs::write(
            &p,
            format!("{prefix}{}broken\n", event("task_complete", "recent")),
        )
        .unwrap();
        assert!(
            Tracker::default().read(root.path(), &p, "native").is_err(),
            "a malformed newer record cannot prove completion"
        );
    }

    #[test]
    fn damaged_old_record_does_not_hide_new_lifecycle_but_damaged_tail_is_unknown() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("sessions")).unwrap();
        let p = t.path().join("sessions/test.jsonl");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let done = format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"latest"}})
        );
        let mut bytes = head.as_bytes().to_vec();
        bytes.extend_from_slice(b"{\"type\":\"response_item\",\"payload\":\"broken\xff");
        bytes.extend(vec![b'x'; 40000]);
        bytes.extend_from_slice(b"\n");
        bytes.extend(done.as_bytes());
        std::fs::write(&p, &bytes).unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(t.path(), &p, "native").unwrap(),
            ("latest".into(), "completed".into())
        );
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"invalid record\n").unwrap();
        assert!(tracker.read(t.path(), &p, "native").is_err());
        f.write_all(done.as_bytes()).unwrap();
        assert_eq!(tracker.read(t.path(), &p, "native").unwrap().1, "completed");
    }
    #[test]
    fn active_owner_is_not_interrupted_by_saved_history_and_terminals_need_native_events() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("sessions")).unwrap();
        let p = t.path().join("sessions/test.jsonl");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let start = format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}})
        );
        std::fs::write(&p, format!("{head}{start}")).unwrap();
        assert_eq!(
            latest(t.path(), &p, "native").unwrap(),
            ("turn".into(), "inProgress".into())
        );
        for (event, state) in [
            ("task_complete", "completed"),
            ("turn_aborted", "interrupted"),
        ] {
            std::fs::write(
                &p,
                format!(
                    "{head}{start}{}\n",
                    json!({"type":"event_msg","payload":{"type":event,"turn_id":"turn"}})
                ),
            )
            .unwrap();
            assert_eq!(latest(t.path(), &p, "native").unwrap().1, state);
        }
        assert!(latest(t.path(), &p, "foreign").is_err());
        let padding = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"text":"x".repeat(300000)}})
        );
        std::fs::write(&p, format!("{head}{start}{padding}")).unwrap();
        assert_eq!(latest(t.path(), &p, "native").unwrap().1, "inProgress");
    }
    #[test]
    fn incremental_reader_preserves_old_terminal_across_new_turn_and_partial_append() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let path = root.path().join("sessions/events.jsonl");
        let head = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"native"}})
        );
        let event = |kind: &str, id: &str| {
            format!(
                "{}\n",
                json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}})
            )
        };
        std::fs::write(&path, format!("{head}{}", event("task_started", "first"))).unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        let first_offset = tracker.offset;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            format!(
                "{}\n",
                json!({"type":"response_item","payload":{"text":"x".repeat(700000)}})
            )
            .as_bytes(),
        )
        .unwrap();
        let end = event("task_complete", "first");
        file.write_all(&end.as_bytes()[..10]).unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        assert!(tracker.offset > first_offset);
        let offset = tracker.offset;
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "inProgress"
        );
        assert_eq!(tracker.offset, offset);
        file.write_all(&end.as_bytes()[10..]).unwrap();
        file.write_all(event("task_started", "second").as_bytes())
            .unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap(),
            ("second".into(), "inProgress".into())
        );
        assert_eq!(tracker.state("first"), Some("completed"));
        drop(file);
        std::fs::write(
            &path,
            format!("{head}{}", event("turn_aborted", "replacement")),
        )
        .unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap(),
            ("replacement".into(), "interrupted".into())
        );
        assert_eq!(tracker.state("first"), None);
    }
    #[test]
    fn large_image_records_do_not_hide_later_lifecycle_events() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let path = root.path().join("sessions/images.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":"native"}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"image-turn"}})
        )
        .unwrap();
        file.write_all(br#"{"type":"response_item","payload":{"type":"message","content":[{"type":"input_image","image_url":"data:image/png;base64,"#).unwrap();
        for _ in 0..(12 * 1024) {
            file.write_all(&[b'A'; 1024]).unwrap();
        }
        file.write_all(b"\"}]}}\n").unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"image-turn"}})
        )
        .unwrap();
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap(),
            ("image-turn".into(), "completed".into())
        );
        assert!(tracker.anchor.len() <= 256);
        let offset = tracker.offset;
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "completed"
        );
        assert_eq!(tracker.offset, offset);
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"next\"}}").unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap().1,
            "completed"
        );
        assert_eq!(tracker.offset, offset);
        file.write_all(b"\n").unwrap();
        assert_eq!(
            tracker.read(root.path(), &path, "native").unwrap(),
            ("next".into(), "inProgress".into())
        );
    }
}
