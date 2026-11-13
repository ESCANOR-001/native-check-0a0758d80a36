//! Read-only recovery when native paginated history has no index rows. Read only
//! canonical item_completed records, never model context or raw response items.
//! The index contains offsets, not transcripts; each response remains bounded.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

const ERROR: &str =
    "Saved Codex messages could not be verified. The original chat has not been changed.";
const CHANGED: &str = "Saved Codex history changed. Reopen the chat to load its latest messages.";
const LIMIT: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ITEMS: usize = 200_000;
const PAGE: usize = crate::agent::MAX_FRAME / 2;
const PREFIX: &str = "remodex-rollout-v1:";
pub(crate) fn is_cursor(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.starts_with(PREFIX))
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    ordinal: Option<u64>,
    #[serde(default)]
    payload: Payload,
}
#[derive(Default, Deserialize)]
struct Payload {
    #[serde(rename = "type", default)]
    kind: String,
    turn_id: Option<String>,
    thread_id: Option<String>,
    root_turn_id: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    error: Option<Value>,
    item: Option<Item>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    phase: Option<String>,
}
#[derive(Clone)]
struct ItemRef {
    id: String,
    kind: String,
    phase: Option<String>,
    offset: u64,
    length: u64,
    source: usize,
}
#[derive(Clone)]
struct Turn {
    metadata: Value,
    items: Vec<ItemRef>,
    positions: BTreeMap<String, usize>,
}
struct Index {
    path: PathBuf,
    native: String,
    sources: Vec<Segment>,
    revision: String,
    turns: Vec<Turn>,
    count: usize,
    prefix: Option<Arc<PrefixIndex>>,
}
// A fork/reverted chat appends to its own rollout while its inherited history
// stays unchanged. Keep that verified prefix's small offset index and hash
// state, so each new output does not decompress/parse the entire ancestor.
struct PrefixIndex {
    sources: Vec<Segment>,
    hash: Sha256,
    turns: Vec<Turn>,
    count: usize,
    total: u64,
}
#[derive(Clone, PartialEq, Eq)]
struct Segment {
    path: PathBuf,
    owner: String,
    identity: crate::storage_stage::Identity,
    size: u64,
    modified: SystemTime,
    end: Option<(u64, u64)>,
}
static CACHE: OnceLock<Mutex<VecDeque<Arc<Index>>>> = OnceLock::new();

// One JSONL record at a time, including records with huge ignored tool/media
// fields. serde can skip those fields without allocating the whole record.
struct Line<'a, R> {
    input: &'a mut R,
    hash: &'a mut Sha256,
    bytes: u64,
    complete: bool,
}
impl<R: BufRead> Read for Line<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.complete || out.is_empty() {
            return Ok(0);
        }
        let available = self.input.fill_buf()?;
        let available = &available[..available.len().min(out.len())];
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if n == 0 {
            return Ok(0);
        }
        out[..n].copy_from_slice(&available[..n]);
        self.hash.update(&out[..n]);
        self.complete = out[n - 1] == b'\n';
        self.bytes += n as u64;
        self.input.consume(n);
        Ok(n)
    }
}
fn record(
    input: &mut impl BufRead,
    hash: &mut Sha256,
    scratch: &mut Vec<u8>,
) -> Result<(Record, u64), String> {
    // Most records already fit in the bounded file buffer. Parsing that slice
    // avoids serde's byte-at-a-time Read adapter while still ignoring large
    // context/tool fields. A reusable 1 MiB buffer also covers records that
    // cross a file-buffer boundary; larger media records remain streaming.
    let available = input.fill_buf().map_err(|_| ERROR)?;
    if let Some(end) = available.iter().position(|b| *b == b'\n') {
        let length = end + 1;
        let row = serde_json::from_slice(&available[..length]).map_err(|_| ERROR)?;
        hash.update(&available[..length]);
        input.consume(length);
        return Ok((row, length as u64));
    }
    scratch.clear();
    while scratch.len() < 1024 * 1024 {
        let available = input.fill_buf().map_err(|_| ERROR)?;
        if available.is_empty() {
            return Err(ERROR.into());
        }
        let length = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1)
            .min(1024 * 1024 - scratch.len());
        let complete = available[length - 1] == b'\n';
        scratch.extend_from_slice(&available[..length]);
        input.consume(length);
        if complete {
            let row = serde_json::from_slice(scratch).map_err(|_| ERROR)?;
            hash.update(scratch.as_slice());
            return Ok((row, scratch.len() as u64));
        }
    }
    let mut replay = std::io::Cursor::new(scratch.as_slice()).chain(input);
    let mut line = Line {
        input: &mut replay,
        hash,
        bytes: 0,
        complete: false,
    };
    let row = serde_json::from_reader(BufReader::new(&mut line)).map_err(|_| ERROR)?;
    if !line.complete {
        return Err(ERROR.into());
    }
    Ok((row, line.bytes))
}
fn id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ERROR.into());
    }
    Ok(())
}
fn spelling(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(s) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{s}"));
        }
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }
    #[cfg(not(windows))]
    {
        path.into()
    }
}
fn source(profile: &Path, path: &Path) -> Result<crate::codex_rollout::Rollout, String> {
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
        || !matches!(parts.first(), Some(Component::Normal(p)) if *p=="sessions" || *p=="archived_sessions")
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
    crate::codex_rollout::open(&checked, LIMIT).map_err(|_| ERROR.into())
}
fn matches(segment: &Segment, rollout: &crate::codex_rollout::Rollout) -> bool {
    segment.path == rollout.physical
        && segment.identity == rollout.identity
        && segment.size == rollout.metadata.len()
        && rollout.metadata.modified().ok() == Some(segment.modified)
}
fn header(input: &mut impl BufRead) -> Result<(Vec<u8>, Value), String> {
    let mut bytes = Vec::new();
    input
        .take(2 * 1024 * 1024)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ERROR)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
    if bytes.last() != Some(&b'\n') || value["type"] != "session_meta" {
        return Err(ERROR.into());
    }
    Ok((bytes, value))
}
// history_base points to a rollout ID, which can differ from its owner's chat
// ID after a revert. Resolve that exact filename, not the owner's current file.
fn ancestor(profile: &Path, native: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(native).map_err(|_| ERROR)?;
    let suffix = format!("-{native}.jsonl");
    let reverted_suffix = format!("_{native}.jsonl");
    let mut pending = vec![
        (profile.join("sessions"), 0),
        (profile.join("archived_sessions"), 0),
    ];
    let mut found = std::collections::BTreeSet::new();
    let mut visited = 0;
    while let Some((directory, depth)) = pending.pop() {
        if !directory.exists() {
            continue;
        }
        crate::storage_stage::open(&directory, true).map_err(|_| ERROR)?;
        for entry in std::fs::read_dir(&directory).map_err(|_| ERROR)? {
            let entry = entry.map_err(|_| ERROR)?;
            visited += 1;
            if visited > 100_000 || depth > 16 {
                return Err(ERROR.into());
            }
            let kind = entry.file_type().map_err(|_| ERROR)?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push((entry.path(), depth + 1));
                continue;
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or(ERROR)?;
            let plain = name.strip_suffix(".zst").unwrap_or(name);
            if kind.is_file() && (plain.ends_with(&suffix) || plain.ends_with(&reverted_suffix)) {
                found.insert(directory.join(plain));
            }
        }
    }
    if found.len() != 1 {
        return Err("The original source for this saved chat could not be verified.".into());
    }
    Ok(found.into_iter().next().ok_or(ERROR)?)
}
fn segments(profile: &Path, path: &Path, native: &str) -> Result<Option<Vec<Segment>>, String> {
    let mut path = path.to_owned();
    let mut end = None;
    let mut rows = vec![];
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if rows.len() >= 32 {
            return Err(ERROR.into());
        }
        let mut rollout = source(profile, &path)?;
        if !seen.insert(rollout.physical.clone()) {
            return Err(ERROR.into());
        }
        let (_, first) = header(&mut BufReader::new(&mut rollout.reader))?;
        let owner = first["payload"]["id"].as_str().ok_or(ERROR)?;
        uuid::Uuid::parse_str(owner).map_err(|_| ERROR)?;
        if rows.is_empty() && owner != native {
            return Err(ERROR.into());
        }
        if first["payload"]["history_mode"] != "paginated" {
            if rows.is_empty() {
                return Ok(None);
            }
            return Err(ERROR.into());
        }
        // Attached subagents intentionally hide inherited context in native
        // history. Do not expose that context as an ordinary fork transcript.
        if first["payload"]["attached_thread"] == true {
            return Err("Attached-agent history requires Codex's native index.".into());
        }
        let base = &first["payload"]["history_base"];
        let start = if base.is_null() {
            0
        } else {
            base["end_ordinal_exclusive"]
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or(ERROR)?
        };
        if first["ordinal"].as_u64() != Some(start)
            || end.is_some_and(|(ordinal, bytes)| ordinal <= start || bytes == 0)
        {
            return Err(ERROR.into());
        }
        rollout.verify(false).map_err(|_| CHANGED)?;
        rows.push(Segment {
            path: rollout.physical,
            owner: owner.into(),
            identity: rollout.identity,
            size: rollout.metadata.len(),
            modified: rollout.metadata.modified().map_err(|_| ERROR)?,
            end,
        });
        if base.is_null() {
            break;
        }
        end = Some((
            start,
            base["end_byte_offset"]
                .as_u64()
                .filter(|v| *v > 0 && *v <= LIMIT)
                .ok_or(ERROR)?,
        ));
        path = ancestor(profile, base["thread_id"].as_str().ok_or(ERROR)?)?;
    }
    rows.reverse();
    Ok(Some(rows))
}
fn index(profile: &Path, path: &Path, native: &str) -> Result<Option<Arc<Index>>, String> {
    let rollout = source(profile, path)?;
    let cache = CACHE.get_or_init(Default::default);
    let previous = cache
        .lock()
        .map_err(|_| ERROR)?
        .iter()
        .find(|v| v.native == native && v.path == rollout.physical)
        .cloned();
    if let Some(found) = &previous {
        if found
            .sources
            .iter()
            .all(|s| source(profile, &s.path).is_ok_and(|r| matches(s, &r)))
        {
            return Ok(Some(found.clone()));
        }
    }
    let Some(sources) = segments(profile, path, native)? else {
        return Ok(None);
    };
    let mut prefix = previous.and_then(|v| v.prefix.clone()).filter(|p| {
        !p.sources.is_empty() && p.sources.len() < sources.len() && sources.starts_with(&p.sources)
    });
    let start = prefix.as_ref().map_or(0, |p| p.sources.len());
    let mut hash = prefix.as_ref().map_or_else(Sha256::new, |p| p.hash.clone());
    let mut turns: Vec<Turn> = prefix.as_ref().map_or_else(Vec::new, |p| p.turns.clone());
    let mut positions: BTreeMap<String, usize> = turns
        .iter()
        .enumerate()
        .map(|(n, t)| (t.metadata["id"].as_str().unwrap().to_owned(), n))
        .collect();
    let mut count = prefix.as_ref().map_or(0, |p| p.count);
    let mut total = prefix.as_ref().map_or(0, |p| p.total);
    let mut scratch = Vec::new();
    for (source_number, segment) in sources.iter().enumerate().skip(start) {
        if source_number > 0 && source_number + 1 == sources.len() && prefix.is_none() {
            prefix = Some(Arc::new(PrefixIndex {
                sources: sources[..source_number].to_vec(),
                hash: hash.clone(),
                turns: turns.clone(),
                count,
                total,
            }));
        }
        let mut rollout = source(profile, &segment.path)?;
        if !matches(segment, &rollout) {
            return Err(CHANGED.into());
        }
        let mut bounded =
            (&mut rollout.reader).take(segment.end.map_or(LIMIT + 1, |(_, bytes)| bytes));
        let mut input = BufReader::with_capacity(65536, &mut bounded);
        let (bytes, first) = header(&mut input)?;
        let mut ordinal = first["ordinal"].as_u64().ok_or(ERROR)?;
        let mut offset = bytes.len() as u64;
        hash.update(&bytes);
        while !input.fill_buf().map_err(|_| ERROR)?.is_empty() {
            let (row, length) = record(&mut input, &mut hash, &mut scratch)?;
            if offset + length > LIMIT || row.ordinal != ordinal.checked_add(1) {
                return Err(ERROR.into());
            }
            ordinal += 1;
            if row.kind == "event_msg"
                && matches!(
                    row.payload.kind.as_str(),
                    "task_started" | "task_complete" | "turn_aborted" | "item_completed"
                )
            {
                let p = row.payload;
                if p.thread_id.as_deref().is_some_and(|s| s != segment.owner) {
                    return Err(ERROR.into());
                }
                if let Some(turn_id) = p.turn_id {
                    id(&turn_id)?;
                    let position = if let Some(position) = positions.get(&turn_id) {
                        *position
                    } else {
                        if turns.len() >= 100_000 {
                            return Err(ERROR.into());
                        }
                        let position = turns.len();
                        positions.insert(turn_id.clone(), position);
                        turns.push(Turn { metadata: json!({"id":turn_id,"rootTurnId":null,"status":"inProgress","error":null,"startedAt":null,"completedAt":null,"durationMs":null}), items: vec![], positions: BTreeMap::new() });
                        position
                    };
                    let turn = &mut turns[position];
                    if p.kind == "item_completed" {
                        let item = p.item.ok_or(ERROR)?;
                        id(&item.id)?;
                        let item = ItemRef {
                            id: item.id,
                            kind: item.kind,
                            phase: item.phase,
                            offset,
                            length,
                            source: source_number,
                        };
                        if let Some(position) = turn.positions.get(&item.id) {
                            turn.items[*position] = item;
                        } else {
                            count += 1;
                            if count > MAX_ITEMS {
                                return Err(ERROR.into());
                            }
                            turn.positions.insert(item.id.clone(), turn.items.len());
                            turn.items.push(item);
                        }
                    } else if turn.metadata["status"] == "inProgress" {
                        let status = match p.kind.as_str() {
                            "task_complete" if p.error.is_some() => "failed",
                            "task_complete" => "completed",
                            "turn_aborted" => "interrupted",
                            _ => "inProgress",
                        };
                        if p.error
                            .as_ref()
                            .is_some_and(|v| v.to_string().len() > 65536)
                        {
                            return Err(ERROR.into());
                        }
                        turn.metadata = json!({"id":turn_id,"rootTurnId":p.root_turn_id,"status":status,"error":p.error.map(|v|json!({"message":v["message"],"codexErrorInfo":v["codex_error_info"],"additionalDetails":null})),"startedAt":p.started_at.or_else(||turn.metadata["startedAt"].as_i64()),"completedAt":p.completed_at,"durationMs":p.duration_ms});
                    }
                } else if p.kind != "turn_aborted" {
                    return Err(ERROR.into());
                }
            }
            offset += length;
        }
        total += offset;
        if total > LIMIT {
            return Err(ERROR.into());
        }
        if let Some((end_ordinal, end_bytes)) = segment.end {
            if offset != end_bytes || ordinal.checked_add(1) != Some(end_ordinal) {
                return Err(ERROR.into());
            }
            if let Some(turn) = turns
                .last_mut()
                .filter(|t| t.metadata["status"] == "inProgress")
            {
                turn.metadata["status"] = json!("interrupted");
            }
        }
        rollout.verify(false).map_err(|_| CHANGED)?;
    }
    let index = Arc::new(Index {
        path: rollout.physical,
        native: native.into(),
        sources,
        revision: format!("{:x}", hash.finalize()),
        turns,
        count,
        prefix,
    });
    let retained = |v: &Index| v.count + v.prefix.as_ref().map_or(0, |p| p.count);
    if retained(&index) <= 100_000 {
        let mut cache = cache.lock().map_err(|_| ERROR)?;
        cache.retain(|v| !(v.native == native && v.path == index.path));
        while cache.len() >= 16
            || cache.iter().map(|v| retained(v)).sum::<usize>() + retained(&index) > 100_000
        {
            cache.pop_front();
        }
        cache.push_back(index.clone());
    }
    Ok(Some(index))
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    native: String,
    turn: Option<String>,
    anchor: String,
    inclusive: bool,
    revision: String,
}
fn cursor(index: &Index, turn: Option<&str>, anchor: &str, inclusive: bool) -> String {
    let value = Cursor {
        native: index.native.clone(),
        turn: turn.map(str::to_owned),
        anchor: anchor.into(),
        inclusive,
        revision: index.revision.clone(),
    };
    format!(
        "{PREFIX}{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).expect("serializable cursor"))
    )
}
fn selection(
    index: &Index,
    params: &Value,
    turn: Option<&str>,
    ids: &[&str],
) -> Result<Vec<usize>, String> {
    let desc = match params["sortDirection"].as_str() {
        Some("desc") => true,
        None | Some("asc") => false,
        _ => return Err(ERROR.into()),
    };
    let mut rows: Vec<_> = (0..ids.len()).collect();
    if desc {
        rows.reverse();
    }
    if !params["cursor"].is_null() {
        let (anchor, inclusive) =
            if params["cursor"].is_object() && turn.is_some() && params["cursor"]["type"] == "item"
            {
                (
                    params["cursor"]["itemId"].as_str().ok_or(ERROR)?.to_owned(),
                    false,
                )
            } else {
                let encoded = params["cursor"]
                    .as_str()
                    .filter(|v| v.len() <= 8192)
                    .and_then(|s| s.strip_prefix(PREFIX))
                    .ok_or(ERROR)?;
                let c: Cursor =
                    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).map_err(|_| ERROR)?)
                        .map_err(|_| ERROR)?;
                if c.native != index.native || c.turn.as_deref() != turn {
                    return Err(ERROR.into());
                }
                if c.revision != index.revision {
                    return Err(CHANGED.into());
                }
                (c.anchor, c.inclusive)
            };
        let position = rows.iter().position(|i| ids[*i] == anchor).ok_or(ERROR)?;
        rows.drain(..position + usize::from(!inclusive));
    }
    Ok(rows)
}
fn read_items(
    profile: &Path,
    index: &Index,
    selected: &[&ItemRef],
) -> Result<BTreeMap<String, Value>, String> {
    let mut ordered = selected.to_vec();
    ordered.sort_by_key(|v| (v.source, v.offset));
    let mut result = BTreeMap::new();
    for (number, segment) in index.sources.iter().enumerate() {
        let mut rollout = source(profile, &segment.path)?;
        if !matches(segment, &rollout) {
            return Err(CHANGED.into());
        }
        let mut offset = 0;
        for item in ordered.iter().filter(|i| i.source == number) {
            if item.length > crate::agent::MAX_FRAME as u64 {
                return Err("A saved Codex activity exceeds the message size limit.".into());
            }
            // Plain logs can seek directly to indexed records. Compressed
            // history still makes one forward decoding pass for this page.
            rollout
                .reader
                .skip_exact(item.offset - offset)
                .map_err(|_| ERROR)?;
            let mut bytes = vec![0; item.length as usize];
            rollout.reader.read_exact(&mut bytes).map_err(|_| ERROR)?;
            let row: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
            let p = &row["payload"];
            if p["type"] != "item_completed" || p["item"]["id"] != item.id {
                return Err(ERROR.into());
            }
            let value = crate::codex_saved_items::convert(&p["item"])?;
            result.insert(item.id.clone(), json!({"turnId":p["turn_id"],"item":value,"startedAtMs":p["started_at_ms"],"completedAtMs":p["completed_at_ms"].as_i64().filter(|t|*t!=0)}));
            offset = item.offset + item.length;
        }
        rollout.verify(false).map_err(|_| CHANGED)?;
    }
    Ok(result)
}
pub(crate) async fn page(
    profile: PathBuf,
    path: PathBuf,
    native: String,
    method: String,
    params: Value,
) -> Result<Option<Value>, String> {
    tokio::task::spawn_blocking(move || page_sync(&profile, &path, &native, &method, &params))
        .await
        .map_err(|_| ERROR)?
}
fn page_sync(
    profile: &Path,
    path: &Path,
    native: &str,
    method: &str,
    params: &Value,
) -> Result<Option<Value>, String> {
    let Some(index) = index(profile, path, native)? else {
        return Ok(None);
    };
    let limit = params["limit"].as_u64().unwrap_or(8);
    if limit == 0 || limit > 64 {
        return Err(ERROR.into());
    }
    let mut data = vec![];
    let mut bytes = 0;
    let mut anchors = vec![];
    let (rows, turn_id) = if method == "thread/turns/list" {
        (
            selection(
                &index,
                params,
                None,
                &index
                    .turns
                    .iter()
                    .map(|t| t.metadata["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
            )?,
            None,
        )
    } else {
        let turn_id = params["turnId"].as_str().ok_or(ERROR)?;
        let turn = index
            .turns
            .iter()
            .find(|t| t.metadata["id"] == turn_id)
            .ok_or(ERROR)?;
        (
            selection(
                &index,
                params,
                Some(turn_id),
                &turn.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            )?,
            Some(turn_id),
        )
    };
    let mut wanted = vec![];
    let mut raw_bytes = 0;
    if let Some(turn_id) = turn_id {
        let turn = index
            .turns
            .iter()
            .find(|t| t.metadata["id"] == turn_id)
            .ok_or(ERROR)?;
        for position in rows.iter().take(limit as usize) {
            let item = &turn.items[*position];
            if !wanted.is_empty() && raw_bytes + item.length > crate::agent::MAX_FRAME as u64 {
                break;
            }
            raw_bytes += item.length;
            wanted.push(item);
        }
    }
    // A single forward decompression pass per work page, not one pass per tool.
    let mut work_items = if turn_id.is_some() {
        read_items(profile, &index, &wanted)?
    } else {
        BTreeMap::new()
    };
    let take = if turn_id.is_some() {
        wanted.len()
    } else {
        limit as usize
    };
    for position in rows.iter().take(take) {
        let (value, anchor) = if let Some(turn_id) = turn_id {
            let turn = index
                .turns
                .iter()
                .find(|t| t.metadata["id"] == turn_id)
                .ok_or(ERROR)?;
            let item = &turn.items[*position];
            (work_items.remove(&item.id).ok_or(ERROR)?, item.id.clone())
        } else {
            let turn = &index.turns[*position];
            let mut value = turn.metadata.clone();
            let view = params["itemsView"].as_str().unwrap_or("notLoaded");
            let selected = match view {
                "notLoaded" => vec![],
                "summary" => turn
                    .items
                    .iter()
                    .find(|i| i.kind == "UserMessage")
                    .into_iter()
                    .chain(turn.items.iter().rev().find(|i| {
                        i.kind == "AgentMessage"
                            && i.phase.as_deref().is_none_or(|p| p == "final_answer")
                    }))
                    .collect(),
                _ => return Err(ERROR.into()),
            };
            let read = read_items(profile, &index, &selected)?;
            value["items"] = json!(selected
                .iter()
                .map(|i| read[&i.id]["item"].clone())
                .collect::<Vec<_>>());
            value["itemsView"] = json!(view);
            (value, turn.metadata["id"].as_str().ok_or(ERROR)?.to_owned())
        };
        let size = serde_json::to_vec(&value).map_err(|_| ERROR)?.len();
        if size > PAGE {
            return Err("A saved Codex message exceeds the page size limit.".into());
        }
        if bytes + size > PAGE {
            break;
        }
        bytes += size;
        data.push(value);
        anchors.push(anchor);
    }
    let next = if data.len() < rows.len() {
        anchors.last().map(|a| cursor(&index, turn_id, a, false))
    } else {
        None
    };
    let backwards = anchors.first().map(|a| cursor(&index, turn_id, a, true));
    Ok(Some(
        json!({"data":data,"nextCursor":next,"backwardsCursor":backwards}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_record_reader_keeps_split_large_and_partial_record_rules() {
        for size in [0, 100, 70_000, 1024 * 1024 + 100] {
            let mut bytes = serde_json::to_vec(
                &json!({"type":"response_item","ordinal":7,"ignored":"x".repeat(size)}),
            )
            .unwrap();
            bytes.push(b'\n');
            let following = b"{\"type\":\"event_msg\",\"ordinal\":8,\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn\"}}\n";
            let mut all = bytes.clone();
            all.extend_from_slice(following);
            for capacity in [7, 65536] {
                let mut input = BufReader::with_capacity(capacity, all.as_slice());
                let mut hash = Sha256::new();
                let mut scratch = Vec::new();
                let (first, length) = record(&mut input, &mut hash, &mut scratch).unwrap();
                assert_eq!(first.ordinal, Some(7));
                assert_eq!(length, bytes.len() as u64);
                assert_eq!(hash.clone().finalize(), Sha256::digest(&bytes));
                assert!(scratch.len() <= 1024 * 1024);
                let (second, length) = record(&mut input, &mut hash, &mut scratch).unwrap();
                assert_eq!(second.payload.kind, "task_complete");
                assert_eq!(length, following.len() as u64);
                assert_eq!(hash.finalize(), Sha256::digest(&all));
                let partial = &bytes[..bytes.len() - 1];
                assert!(record(
                    &mut BufReader::with_capacity(capacity, partial),
                    &mut Sha256::new(),
                    &mut scratch
                )
                .is_err());
            }
        }
    }
    #[test]
    #[ignore = "Read-only timing against an explicitly supplied private fixture"]
    fn saved_history_read_benchmark() {
        let profile = PathBuf::from(std::env::var_os("REMODEX_HISTORY_BENCH_PROFILE").unwrap());
        let path = PathBuf::from(std::env::var_os("REMODEX_HISTORY_BENCH_PATH").unwrap());
        let native = std::env::var("REMODEX_HISTORY_BENCH_NATIVE").unwrap();
        let start = std::time::Instant::now();
        let result = index(&profile, &path, &native).unwrap().unwrap();
        println!(
            "saved-history-index: {} ms, {} turns, {} items",
            start.elapsed().as_millis(),
            result.turns.len(),
            result.count
        );
    }
    fn fixture(compressed: bool) -> (tempfile::TempDir, PathBuf, PathBuf, String, Vec<u8>) {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile space 测试");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let path = profile.join("sessions/history.jsonl");
        let native = uuid::Uuid::new_v4().to_string();
        let mut rows =
            vec![json!({"type":"session_meta","payload":{"id":native,"history_mode":"paginated"}})];
        for n in 0..7 {
            let turn = format!("turn-{n}");
            rows.push(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn,"started_at":100+n*10}}));
            for (suffix, item) in [
                (
                    "u",
                    json!({"type":"UserMessage","content":[{"type":"text","text":format!("question {n}")}]}),
                ),
                (
                    "a",
                    json!({"type":"AgentMessage","content":[{"type":"Text","text":format!("answer {n}")}],"phase":"final_answer"}),
                ),
            ] {
                let mut item = item;
                item["id"] = json!(format!("{suffix}-{n}"));
                rows.push(json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":native,"turn_id":turn,"item":item,"started_at_ms":(100+n*10)*1000,"completed_at_ms":(101+n*10)*1000}}));
            }
            rows.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"started_at":100+n*10,"completed_at":102+n*10}}));
        }
        // Later snapshots update an existing item without creating a duplicate.
        rows.push(json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":native,"turn_id":"turn-0","item":{"type":"AgentMessage","id":"a-0","content":[{"type":"Text","text":"updated answer"}],"phase":"final_answer"},"started_at_ms":100000,"completed_at_ms":102000}}));
        // Model input must never be shown as a user's message.
        rows.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"private context"}]}}));
        let mut bytes = vec![];
        for (ordinal, mut row) in rows.into_iter().enumerate() {
            row["ordinal"] = json!(ordinal);
            bytes.extend(serde_json::to_vec(&row).unwrap());
            bytes.push(b'\n');
        }
        let physical = if compressed {
            path.with_extension("jsonl.zst")
        } else {
            path.clone()
        };
        let written = if compressed {
            zstd::stream::encode_all(bytes.as_slice(), 1).unwrap()
        } else {
            bytes
        };
        std::fs::write(physical, &written).unwrap();
        (root, profile, path, native, written)
    }
    #[test]
    fn pages_original_ids_in_both_directions_without_duplicates_or_source_writes() {
        for compressed in [false, true] {
            let (_root, profile, path, native, written) = fixture(compressed);
            let mut params = json!({"limit":2,"sortDirection":"desc","itemsView":"summary"});
            let mut ids = vec![];
            loop {
                let page = page_sync(&profile, &path, &native, "thread/turns/list", &params)
                    .unwrap()
                    .unwrap();
                for turn in page["data"].as_array().unwrap() {
                    ids.push(turn["id"].as_str().unwrap().to_owned());
                    assert_eq!(turn["items"].as_array().unwrap().len(), 2);
                }
                if page["nextCursor"].is_null() {
                    break;
                }
                params["cursor"] = page["nextCursor"].clone();
            }
            assert_eq!(
                ids,
                (0..7)
                    .rev()
                    .map(|i| format!("turn-{i}"))
                    .collect::<Vec<_>>()
            );
            let page = page_sync(
                &profile,
                &path,
                &native,
                "thread/items/list",
                &json!({"turnId":"turn-0","limit":8}),
            )
            .unwrap()
            .unwrap();
            assert_eq!(page["data"].as_array().unwrap().len(), 2);
            assert_eq!(page["data"][1]["item"]["text"], "updated answer");
            assert_eq!(page["data"][0]["item"]["id"], "u-0");
            assert_eq!(
                std::fs::read(if compressed {
                    path.with_extension("jsonl.zst")
                } else {
                    path.clone()
                })
                .unwrap(),
                written
            );
        }
    }

    #[test]
    fn appended_fork_reuses_verified_parent_index_and_preserves_revision_and_pages() {
        use std::io::Write;
        let (_root, profile, original, native, bytes) = fixture(false);
        let parent = profile
            .join("sessions")
            .join(format!("rollout-parent-{native}.jsonl"));
        let compressed = parent.with_extension("jsonl.zst");
        std::fs::remove_file(&original).unwrap();
        let encode = |bytes: &[u8]| zstd::stream::encode_all(bytes, 1).unwrap();
        std::fs::write(&compressed, encode(&bytes)).unwrap();
        let end = bytes.iter().filter(|b| **b == b'\n').count() as u64;
        let child_id = uuid::Uuid::new_v4().to_string();
        let child = profile.join("sessions/child.jsonl");
        let row = |ordinal: u64, payload: Value| {
            format!(
                "{}\n",
                json!({"ordinal":ordinal,"type":"event_msg","payload":payload})
            )
        };
        let header = format!(
            "{}\n",
            json!({"ordinal":end,"type":"session_meta","payload":{"id":child_id,"history_mode":"paginated","history_base":{"thread_id":native,"end_ordinal_exclusive":end,"end_byte_offset":bytes.len()}}})
        );
        std::fs::write(&child,format!("{header}{}{}",
            row(end+1,json!({"type":"task_started","turn_id":"live","started_at":200})),
            row(end+2,json!({"type":"item_completed","thread_id":child_id,"turn_id":"live","item":{"id":"live-u","type":"UserMessage","content":[{"type":"text","text":"new question"}]}})))).unwrap();
        let first = index(&profile, &child, &child_id).unwrap().unwrap();
        let prefix = first.prefix.as_ref().unwrap().clone();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&child)
            .unwrap();
        file.write_all(format!("{}{}",row(end+3,json!({"type":"item_completed","thread_id":child_id,"turn_id":"live","item":{"id":"live-a","type":"AgentMessage","phase":"final_answer","content":[{"type":"Text","text":"new answer"}]}})),
            row(end+4,json!({"type":"task_complete","turn_id":"live","started_at":200,"completed_at":205}))).as_bytes()).unwrap();
        drop(file);
        let second = index(&profile, &child, &child_id).unwrap().unwrap();
        assert!(
            Arc::ptr_eq(second.prefix.as_ref().unwrap(), &prefix),
            "Unchanged inherited history must not be reindexed for an append"
        );
        assert_ne!(first.revision, second.revision);
        let params = json!({"limit":1,"sortDirection":"desc","itemsView":"summary"});
        let page = page_sync(&profile, &child, &child_id, "thread/turns/list", &params)
            .unwrap()
            .unwrap();
        assert_eq!(page["data"][0]["status"], "completed");
        assert_eq!(page["data"][0]["items"][1]["text"], "new answer");
        CACHE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .retain(|v| v.native != child_id);
        let rebuilt = index(&profile, &child, &child_id).unwrap().unwrap();
        assert_eq!(
            rebuilt.revision, second.revision,
            "Reusing a prefix must produce the exact same cursor revision"
        );
        assert_eq!(
            page_sync(&profile, &child, &child_id, "thread/turns/list", &params)
                .unwrap()
                .unwrap(),
            page
        );
        assert_eq!(std::fs::read(&compressed).unwrap(), encode(&bytes));
        let changed = String::from_utf8(bytes)
            .unwrap()
            .replace("updated answer", "changed answer");
        std::fs::write(&compressed, encode(changed.as_bytes())).unwrap();
        let updated = index(&profile, &child, &child_id).unwrap().unwrap();
        assert!(!Arc::ptr_eq(
            updated.prefix.as_ref().unwrap(),
            rebuilt.prefix.as_ref().unwrap()
        ));
        assert_ne!(updated.revision, rebuilt.revision);
        let old = page_sync(
            &profile,
            &child,
            &child_id,
            "thread/items/list",
            &json!({"turnId":"turn-0","limit":8}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(old["data"][1]["item"]["text"], "changed answer");
    }
    #[test]
    fn cursors_are_bound_to_thread_turn_and_source_revision() {
        let (_root, profile, path, native, _) = fixture(false);
        let first = page_sync(
            &profile,
            &path,
            &native,
            "thread/items/list",
            &json!({"turnId":"turn-0","limit":1}),
        )
        .unwrap()
        .unwrap();
        let next = first["nextCursor"].clone();
        assert!(page_sync(
            &profile,
            &path,
            &native,
            "thread/items/list",
            &json!({"turnId":"turn-1","cursor":next})
        )
        .is_err());
        let second = page_sync(
            &profile,
            &path,
            &native,
            "thread/items/list",
            &json!({"turnId":"turn-0","cursor":next}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(second["data"][0]["item"]["id"], "a-0");
        let previous = page_sync(
            &profile,
            &path,
            &native,
            "thread/items/list",
            &json!({"turnId":"turn-0","cursor":second["backwardsCursor"],"sortDirection":"desc"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(previous["data"][0]["item"]["id"], "a-0");
        assert_eq!(previous["data"][1]["item"]["id"], "u-0");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"ordinal\":31,\"type\":\"response_item\",\"payload\":{}}\n");
        std::fs::write(&path, bytes).unwrap();
        assert!(page_sync(
            &profile,
            &path,
            &native,
            "thread/items/list",
            &json!({"turnId":"turn-0","cursor":next})
        )
        .is_err());
    }
    #[test]
    fn rejects_partial_wrong_identity_redirected_and_inherited_records() {
        for mode in ["partial", "id", "outside", "base", "ordinal"] {
            let (root, profile, path, native, _) = fixture(false);
            let mut bytes = std::fs::read(&path).unwrap();
            match mode {
                "partial" => {
                    bytes.pop();
                }
                "id" => {
                    bytes = String::from_utf8(bytes)
                        .unwrap()
                        .replace(&native, &uuid::Uuid::new_v4().to_string())
                        .into_bytes();
                }
                "base" => {
                    bytes=String::from_utf8(bytes).unwrap().replace("\"history_mode\":\"paginated\"","\"history_mode\":\"paginated\",\"history_base\":{\"parent\":\"other\"}").into_bytes();
                }
                "ordinal" => {
                    bytes = String::from_utf8(bytes)
                        .unwrap()
                        .replace("\"ordinal\":2,", "\"ordinal\":1,")
                        .into_bytes();
                }
                _ => {}
            }
            let actual = if mode == "outside" {
                root.path().join("outside.jsonl")
            } else {
                path
            };
            std::fs::write(&actual, bytes).unwrap();
            assert!(
                page_sync(
                    &profile,
                    &actual,
                    &native,
                    "thread/turns/list",
                    &json!({"limit":1})
                )
                .is_err(),
                "{mode}"
            );
        }
    }
    #[test]
    fn fork_and_revert_read_only_the_verified_parent_prefix() {
        for reverted in [false, true] {
            let (_root, profile, original, native, bytes) = fixture(false);
            let parent = profile.join("sessions").join(if reverted {
                format!("rollout-original-owner_{native}.jsonl")
            } else {
                format!("rollout-original-{native}.jsonl")
            });
            std::fs::rename(&original, &parent).unwrap();
            let cutoff = bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .nth(3)
                .unwrap()
                .0
                + 1;
            let rollout_id = uuid::Uuid::new_v4().to_string();
            let child_id = if reverted {
                native.clone()
            } else {
                rollout_id.clone()
            };
            let child = profile
                .join("sessions")
                .join(format!("rollout-child-{rollout_id}.jsonl"));
            let mut rows = vec![
                json!({"type":"session_meta","payload":{"id":child_id,"history_mode":"paginated","history_base":{"thread_id":native,"end_ordinal_exclusive":4,"end_byte_offset":cutoff}}}),
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"child-turn","started_at":200}}),
                json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":child_id,"turn_id":"child-turn","item":{"type":"UserMessage","id":"child-user","content":[{"type":"text","text":"child question"}]}}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"child-turn","started_at":200,"completed_at":201}}),
            ];
            let revision = std::cell::Cell::new(0u64);
            let write = |rows: &mut Vec<Value>| {
                let mut out = vec![];
                for (n, row) in rows.iter_mut().enumerate() {
                    row["ordinal"] = json!(n + 4);
                    out.extend(serde_json::to_vec(row).unwrap());
                    out.push(b'\n');
                }
                std::fs::write(
                    child.with_extension("jsonl.zst"),
                    zstd::stream::encode_all(out.as_slice(), 1).unwrap(),
                )
                .unwrap();
                // Each fixture rewrite is a distinct history revision. Rapid
                // same-size writes can share a filesystem timestamp on a VM;
                // do not make cache invalidation depend on clock resolution.
                revision.set(revision.get() + 1);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(child.with_extension("jsonl.zst"))
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH
                            + std::time::Duration::from_secs(1_700_000_000 + revision.get()),
                    ))
                    .unwrap();
            };
            write(&mut rows);
            let page = page_sync(
                &profile,
                &child,
                &child_id,
                "thread/turns/list",
                &json!({"limit":8,"itemsView":"summary"}),
            )
            .unwrap()
            .unwrap();
            assert_eq!(page["data"].as_array().unwrap().len(), 2);
            assert_eq!(page["data"][0]["id"], "turn-0");
            assert_eq!(page["data"][0]["status"], "interrupted");
            assert_eq!(page["data"][0]["items"][1]["text"], "answer 0");
            assert_eq!(
                page["data"][1]["items"][0]["content"][0]["text"],
                "child question"
            );
            let work = page_sync(
                &profile,
                &child,
                &child_id,
                "thread/items/list",
                &json!({"turnId":"turn-0"}),
            )
            .unwrap()
            .unwrap();
            assert_eq!(work["data"].as_array().unwrap().len(), 2);
            assert_eq!(std::fs::read(&parent).unwrap(), bytes);
            rows[0]["payload"]["history_base"]["end_byte_offset"] = json!(cutoff - 1);
            write(&mut rows);
            assert!(page_sync(
                &profile,
                &child,
                &child_id,
                "thread/turns/list",
                &json!({"limit":1})
            )
            .is_err());
            rows[0]["payload"]["history_base"]["thread_id"] = json!(rollout_id);
            write(&mut rows);
            assert!(page_sync(
                &profile,
                &child,
                &child_id,
                "thread/turns/list",
                &json!({"limit":1})
            )
            .is_err());
        }
    }
}
