//! Read-only local Claude catalog and incremental JSONL reader. History is
//! copied into the existing paged store; it is never all retained in phone RAM.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(crate) struct Entry {
    pub native: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
    pub created: String,
    pub modified: u128,
    /// Last observed transcript outcome, never authorization for live controls.
    pub saved_turn: Option<Value>,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub(crate) struct Cursor {
    #[serde(default)]
    pub projection_version: u32,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub turn: String,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub unreadable: u64,
}
pub(crate) struct Batch {
    pub rows: Vec<Value>,
    pub next: Cursor,
    pub caught_up: bool,
}

fn open(path: &Path) -> Result<fs::File, String> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o
        .open(path)
        .map_err(|_| "Local Claude history is unavailable")?;
    if !f
        .metadata()
        .map_err(|_| "Cannot inspect Claude history")?
        .is_file()
    {
        return Err("Claude history must be a regular file".into());
    }
    Ok(f)
}
fn identity(m: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", m.dev(), m.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", m.created().ok())
    }
}

pub(crate) fn catalog(profile: &Path) -> Result<Vec<Entry>, String> {
    let root = profile.join("projects");
    let dirs = match fs::read_dir(&root) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err("Claude's saved projects cannot be read".into()),
    };
    let mut entries = vec![];
    for dir in dirs {
        let dir = dir.map_err(|_| "Cannot enumerate Claude projects")?;
        if !dir.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let files =
            fs::read_dir(dir.path()).map_err(|_| "Cannot enumerate saved Claude sessions")?;
        for file in files {
            let file = file.map_err(|_| "Cannot enumerate saved Claude sessions")?;
            let path = file.path();
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl")
                || !file.file_type().is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let Some(native) = path
                .file_stem()
                .and_then(|v| v.to_str())
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            else {
                continue;
            };
            if let Ok(entry) = metadata(&path, native) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    Ok(entries)
}
fn metadata(path: &Path, native: &str) -> Result<Entry, String> {
    let mut file = open(path)?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect Claude history")?;
    // Catalog reads are bounded head/tail reads, independent of session size.
    let mut head = Vec::new();
    (&mut file)
        .take(256 * 1024)
        .read_to_end(&mut head)
        .map_err(|_| "Cannot read Claude history")?;
    let mut tail = Vec::new();
    if meta.len() > head.len() as u64 {
        file.seek(SeekFrom::Start(meta.len().saturating_sub(256 * 1024)))
            .map_err(|_| "Cannot read Claude history")?;
        file.read_to_end(&mut tail)
            .map_err(|_| "Cannot read Claude history")?;
    }
    let mut cwd = None;
    let mut title = None;
    let mut custom = None;
    let mut created = None;
    let mut has_history = false;
    for bytes in head
        .split(|b| *b == b'\n')
        .chain(tail.split(|b| *b == b'\n'))
    {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        if v["isSidechain"] == true {
            continue;
        }
        if matches!(v["type"].as_str(), Some("user" | "assistant"))
            && !v["message"]["content"].is_null()
        {
            has_history = true;
        }
        if let Some(dir) = v["cwd"].as_str().filter(|s| Path::new(s).is_absolute()) {
            cwd = Some(PathBuf::from(dir));
        }
        if created.is_none() {
            created = v["timestamp"].as_str().map(str::to_owned);
        }
        if let Some(name) = v["customTitle"]
            .as_str()
            .or(v["name"].as_str().filter(|_| v["type"] == "session-name"))
        {
            custom = Some(name.to_owned());
        }
        if title.is_none() {
            if let Ok(Some((_, text))) = crate::claude_turns::user_input(&v) {
                title = Some(text);
            }
        }
    }
    if !has_history {
        return Err("Claude session has no saved conversation yet".into());
    }
    // When the bounded reads overlap, they cover the whole file. Join them
    // without duplicating rows so a large diagnostic between a prompt and its
    // interruption cannot hide the prompt identity. Never join across a gap.
    let saved_turn = if !tail.is_empty() && meta.len() <= (head.len() + tail.len()) as u64 {
        let overlap = head.len() + tail.len() - meta.len() as usize;
        head.extend_from_slice(&tail[overlap..]);
        saved_turn(&head, native)
    } else {
        saved_turn(if tail.is_empty() { &head } else { &tail }, native)
    };
    Ok(Entry {
        native: native.into(),
        path: path.into(),
        cwd: cwd.ok_or("Claude history has no working directory")?,
        title: custom
            .or(title)
            .unwrap_or_else(|| "Claude session".into())
            .chars()
            .take(120)
            .collect(),
        created: created.unwrap_or_else(crate::chat_projection::now),
        saved_turn,
        modified: meta
            .modified()
            .ok()
            .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|v| v.as_millis())
            .unwrap_or(0),
    })
}

// Use only the contiguous tail, never an old head outcome across an unread gap.
// A partial or malformed newer record invalidates earlier completion evidence.
fn saved_turn(bytes: &[u8], native: &str) -> Option<Value> {
    let mut outcome = None;
    let mut prompt = None;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if line.iter().all(|b| *b == 0 || b.is_ascii_whitespace()) {
            continue;
        }
        let row: Value = match serde_json::from_slice(line) {
            Ok(row) if line.last() == Some(&b'\n') => row,
            _ => {
                outcome = None;
                prompt = None;
                continue;
            }
        };
        if row["sessionId"].as_str().is_some_and(|id| id != native) {
            return None;
        }
        if row["isSidechain"] == true || !row["parent_tool_use_id"].is_null() {
            continue;
        }
        let state = if crate::claude_turns::interruption_notice(&row, prompt.as_deref()) {
            "interrupted"
        } else if crate::claude_turns::no_response_placeholder(&row) {
            continue;
        } else if row["type"] == "user"
            && row["message"]["content"].as_array().is_some_and(|blocks| {
                !blocks.is_empty() && blocks.iter().all(|b| b["type"] == "tool_result")
            })
        {
            // The bounded tail can start after the original prompt. A native
            // tool result still identifies that prompt for a following Esc
            // notice; a new human prompt has its own ID/provenance.
            prompt = row["promptId"].as_str().map(str::to_owned);
            "incomplete"
        } else if !matches!(crate::claude_turns::input_identity(&row), Ok(None)) {
            prompt = row["promptId"].as_str().map(str::to_owned);
            "incomplete"
        } else if row["type"] == "assistant" || row["type"] == "result" {
            if row["is_error"] == true || row["isApiErrorMessage"] == true {
                "error"
            } else if row["type"] == "result"
                || matches!(
                    row["message"]["stop_reason"].as_str(),
                    Some("end_turn" | "stop_sequence")
                )
            {
                "completed"
            } else {
                "incomplete"
            }
        } else {
            continue;
        };
        outcome = row["timestamp"]
            .as_str()
            .filter(|at| {
                time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                    .is_ok()
            })
            .map(|at| serde_json::json!({"state":state,"updatedAt":at}));
    }
    outcome
}

pub(crate) fn read(path: &Path, session: &str, before: &Cursor) -> Result<Batch, String> {
    let mut file = open(path)?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect Claude history")?;
    let mut next = before.clone();
    let identity = identity(&meta);
    if next.projection_version != 1 || next.identity != identity || next.offset > meta.len() {
        next = Cursor {
            projection_version: 1,
            identity,
            ..Default::default()
        };
    }
    file.seek(SeekFrom::Start(next.offset))
        .map_err(|_| "Cannot seek Claude history")?;
    let mut reader = BufReader::new(file);
    let mut rows = vec![];
    let mut consumed = 0usize;
    loop {
        let mut line = Vec::new();
        // A batch budget limits work, not the age/total length of a chat.
        let n = reader
            .by_ref()
            .take(16 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| "Cannot read Claude history")?;
        if n == 0 {
            break;
        }
        if n > 16 * 1024 * 1024 {
            return Err(
                "A Claude transcript record exceeds 16 MiB; its history is retained".into(),
            );
        }
        let padding = line.iter().all(|b| *b == 0 || b.is_ascii_whitespace());
        if line.last() != Some(&b'\n') && !padding {
            break;
        } // writer has not committed this row yet
        next.offset += n as u64;
        consumed += n;
        // Claude can leave all-NUL padding records after interrupted writes.
        // Ignore padding and blank lines; preserve cached history and report
        // other complete unreadable records instead of blocking every later row.
        if padding {
            if consumed >= 2 * 1024 * 1024 {
                break;
            }
            continue;
        }
        let mut v: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                next.unreadable = next.unreadable.saturating_add(1);
                if consumed >= 2 * 1024 * 1024 {
                    break;
                }
                continue;
            }
        };
        if v["sessionId"].as_str().is_some_and(|id| id != session) {
            return Err("Claude transcript identity changed; history was not imported".into());
        }
        if v["isSidechain"] != true
            && matches!(
                v["type"].as_str(),
                Some("user" | "assistant" | "system" | "result")
            )
        {
            if crate::claude_turns::interruption_notice(&v, next.prompt_id.as_deref()) {
                v["isSynthetic"] = Value::Bool(true);
                v["shouldQuery"] = Value::Bool(false);
            } else if crate::claude_turns::user_input(&v).is_ok_and(|v| v.is_some()) {
                next.prompt_id = v["promptId"]
                    .as_str()
                    .filter(|s| s.len() <= 256)
                    .map(str::to_owned);
            }
            rows.push(v);
        }
        if consumed >= 2 * 1024 * 1024 || rows.len() >= 160 {
            break;
        }
    }
    Ok(Batch {
        rows,
        caught_up: next.offset >= meta.len(),
        next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn outcome(rows: &[Value]) -> Option<Value> {
        saved_turn(
            &rows
                .iter()
                .map(|r| format!("{r}\n"))
                .collect::<String>()
                .into_bytes(),
            "session",
        )
    }
    #[test]
    fn saved_outcomes_follow_native_completion_error_and_interruption() {
        let final_row = serde_json::json!({"type":"assistant","sessionId":"session","timestamp":"2026-11-07T10:00:00Z","message":{"stop_reason":"end_turn","content":"done"}});
        assert_eq!(
            outcome(std::slice::from_ref(&final_row)).unwrap()["state"],
            "completed"
        );
        let mut error = final_row.clone();
        error["isApiErrorMessage"] = true.into();
        assert_eq!(outcome(&[error]).unwrap()["state"], "error");
        let result = serde_json::json!({"type":"user","promptId":"original","timestamp":"2026-11-07T10:01:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"result"}]}});
        let stop = serde_json::json!({"type":"user","promptId":"original","timestamp":"2026-11-07T10:02:00Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        assert_eq!(
            outcome(&[result.clone(), stop.clone()]).unwrap()["state"],
            "interrupted"
        );
        let mut literal = stop.clone();
        literal["origin"] = serde_json::json!({"kind":"human"});
        literal["uuid"] = "input".into();
        assert_eq!(outcome(&[result, literal]).unwrap()["state"], "incomplete");
        let placeholder = serde_json::json!({"type":"assistant","message":{"model":"<synthetic>","content":"No response requested."}});
        let mut stop = stop;
        stop["isSynthetic"] = true.into();
        assert_eq!(
            outcome(&[stop, placeholder]).unwrap()["state"],
            "interrupted"
        );
    }
    #[test]
    fn saved_outcome_does_not_bridge_new_input_partial_records_or_sidechains() {
        let final_row = serde_json::json!({"type":"assistant","timestamp":"2026-11-07T10:00:00Z","message":{"stop_reason":"end_turn"}});
        let input = serde_json::json!({"type":"user","uuid":"new","timestamp":"2026-11-07T10:01:00Z","message":{"role":"user","content":"continue"}});
        assert_eq!(
            outcome(&[final_row.clone(), input.clone()]).unwrap()["state"],
            "incomplete"
        );
        let mut sidechain = final_row.clone();
        sidechain["isSidechain"] = true.into();
        assert_eq!(
            outcome(&[input.clone(), sidechain]).unwrap()["state"],
            "incomplete"
        );
        for suffix in ["{\"type\":", "{broken}\n", "{\"type\":\"assistant\"}"] {
            assert!(saved_turn(format!("{final_row}\n{suffix}").as_bytes(), "session").is_none());
        }
        let mut foreign = final_row.clone();
        foreign["sessionId"] = "other".into();
        assert!(outcome(&[final_row.clone(), foreign]).is_none());
        let mut undated = input;
        undated["timestamp"] = Value::Null;
        assert!(outcome(&[final_row, undated]).is_none());
    }
    #[test]
    fn catalog_never_uses_a_head_completion_across_an_unread_tail_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let row = serde_json::json!({"type":"assistant","cwd":dir.path(),"timestamp":"2026-11-07T10:00:00Z","message":{"stop_reason":"end_turn","content":"done"}});
        let large_progress = serde_json::json!({"type":"system","data":"x".repeat(600*1024)});
        std::fs::write(&path, format!("{row}\n{large_progress}\n")).unwrap();
        assert!(metadata(&path, "session").unwrap().saved_turn.is_none());
    }
    #[test]
    fn catalog_preserves_interruption_proof_across_overlapping_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let input = serde_json::json!({"type":"user","uuid":"00000000-0000-4000-8000-000000000001","promptId":"original","cwd":dir.path(),"timestamp":"2026-11-07T10:00:00Z","message":{"role":"user","content":"continue"}});
        let diagnostic = serde_json::json!({"type":"api-request-blob","data":"x".repeat(330*1024)});
        let stop = serde_json::json!({"type":"user","uuid":"00000000-0000-4000-8000-000000000002","promptId":"original","timestamp":"2026-11-07T10:01:00Z","message":{"role":"user","content":"[Request interrupted by user]"}});
        std::fs::write(&path, format!("{input}\n{diagnostic}\n{stop}\n")).unwrap();
        assert_eq!(
            metadata(&path, "session").unwrap().saved_turn.unwrap()["state"],
            "interrupted"
        );
        let mut literal = stop;
        literal["origin"] = serde_json::json!({"kind":"human"});
        std::fs::write(&path, format!("{input}\n{diagnostic}\n{literal}\n")).unwrap();
        assert_eq!(
            metadata(&path, "session").unwrap().saved_turn.unwrap()["state"],
            "incomplete"
        );
    }
    #[test]
    fn stop_notice_proof_survives_paging_restart_and_old_cursor_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let sid = uuid::Uuid::new_v4().to_string();
        let prompt = uuid::Uuid::new_v4().to_string();
        let user = serde_json::json!({"type":"user","uuid":uuid::Uuid::new_v4().to_string(),"promptId":prompt,"origin":{"kind":"human"},"message":{"role":"user","content":"original"}});
        let padding = serde_json::json!({"type":"system","subtype":"progress"});
        let notice = serde_json::json!({"type":"user","uuid":uuid::Uuid::new_v4().to_string(),"promptId":prompt,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        let lines = format!("{user}\n{}{notice}\n", format!("{padding}\n").repeat(159));
        fs::write(&path, &lines).unwrap();
        // An old EOF checkpoint must be reconciled, including old display rows.
        let old = Cursor {
            offset: lines.len() as u64,
            identity: identity(&fs::metadata(&path).unwrap()),
            ..Default::default()
        };
        let first = read(&path, &sid, &old).unwrap();
        assert_eq!(first.rows.len(), 160);
        assert!(!first.caught_up);
        let restored: Cursor =
            serde_json::from_value(serde_json::to_value(first.next).unwrap()).unwrap();
        let second = read(&path, &sid, &restored).unwrap();
        assert!(second.caught_up);
        assert_eq!(second.rows.len(), 1);
        assert!(crate::claude_turns::interruption_notice(
            &second.rows[0],
            None
        ));
        assert!(crate::claude_turns::user_input(&second.rows[0])
            .unwrap()
            .is_none());
        assert!(read(&path, &sid, &second.next).unwrap().rows.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), lines);
    }
    #[test]
    fn paged_reader_retains_complete_rows_and_waits_for_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        let id = uuid::Uuid::new_v4().to_string();
        let row=serde_json::json!({"type":"user","sessionId":id,"uuid":uuid::Uuid::new_v4().to_string(),"message":{"role":"user","content":"hello"}}).to_string();
        fs::write(&p, format!("{row}\n{{\"type\":")).unwrap();
        let batch = read(&p, &id, &Cursor::default()).unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert!(!batch.caught_up);
        let again = read(&p, &id, &batch.next).unwrap();
        assert!(again.rows.is_empty());
        assert_eq!(again.next.offset, batch.next.offset);
        fs::write(&p, format!("{row}\n{row}\n")).unwrap();
        assert_eq!(read(&p, &id, &batch.next).unwrap().rows.len(), 1);
    }
    #[test]
    fn unreadable_records_do_not_hide_later_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let id = uuid::Uuid::new_v4().to_string();
        let row=serde_json::json!({"type":"assistant","sessionId":id,"uuid":"answer","message":{"content":[{"type":"text","text":"still here"}]}}).to_string();
        fs::write(
            &path,
            format!("\0\0\0\n   \ninvalid JSON\n{row}\n{}", "\0".repeat(21)),
        )
        .unwrap();
        let batch = read(&path, &id, &Cursor::default()).unwrap();
        assert!(batch.caught_up);
        assert_eq!(batch.next.unreadable, 1);
        assert_eq!(batch.rows.len(), 1);
        assert!(read(&path, &id, &batch.next).unwrap().rows.is_empty());
    }
    #[test]
    fn reader_reaches_history_beyond_one_batch_and_rejects_foreign_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let id = uuid::Uuid::new_v4().to_string();
        let lines=(0..350).map(|i|serde_json::json!({"type":"user","sessionId":id,"uuid":format!("user-{i}"),"message":{"content":"history"}}).to_string()+"\n").collect::<String>();
        fs::write(&path, lines).unwrap();
        let mut cursor = Cursor::default();
        let mut count = 0;
        for _ in 0..4 {
            let batch = read(&path, &id, &cursor).unwrap();
            count += batch.rows.len();
            cursor = batch.next;
            if batch.caught_up {
                break;
            }
        }
        assert_eq!(count, 350);
        assert!(read(&path, "different-session", &Cursor::default()).is_err());
    }
}
