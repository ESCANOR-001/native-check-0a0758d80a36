//! Saved app-server projections can synthesize `interrupted` for another
//! process's live turn. Accept terminal state only from the native lifecycle.
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
/// Per connection, bounded native lifecycle index. Initial scan is sequential;
/// subsequent polls read only new complete lines, not the full transcript.
#[derive(Default)]
pub(crate) struct Tracker {
    identity: Option<crate::storage_stage::Identity>,
    path: PathBuf,
    native: String,
    offset: u64,
    anchor: Vec<u8>,
    latest: Option<(String, String)>,
    turns: std::collections::VecDeque<(String, String)>,
}
impl Tracker {
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
        for (i, part) in parts.iter().enumerate() {
            checked.push(part);
            crate::storage_stage::open(&checked, i + 1 < parts.len()).map_err(|_| ERROR)?;
        }
        let mut file = crate::storage_stage::open(&checked, false).map_err(|_| ERROR)?;
        let identity = crate::storage_stage::identity(&file).map_err(|_| ERROR)?;
        let size = file.metadata().map_err(|_| ERROR)?.len();
        if size == 0
            || size > 512 * 1024 * 1024
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
        let same = self.identity.as_ref() == Some(&identity)
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
        file.seek(SeekFrom::Start(self.offset)).map_err(|_| ERROR)?;
        let mut reader = std::io::BufReader::new((&mut file).take(size - self.offset));
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut reader)
                .take(8 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
                .map_err(|_| ERROR)?;
            if n == 0 {
                break;
            }
            if n > 8 * 1024 * 1024 {
                *self = Self::default();
                return Err(ERROR.into());
            }
            if !line.ends_with(b"\n") {
                break;
            } // An append in progress is not a terminal event.
            let row: Value = serde_json::from_slice(&line).map_err(|_| ERROR)?;
            if row["type"] == "event_msg" {
                let p = &row["payload"];
                let status = match p["type"].as_str() {
                    Some("task_started") => Some("inProgress"),
                    Some("task_complete") => Some("completed"),
                    Some("turn_aborted") => Some("interrupted"),
                    _ => None,
                };
                if let Some(status) = status {
                    let id = p["turn_id"]
                        .as_str()
                        .filter(|s| !s.is_empty() && s.len() <= 256)
                        .ok_or(ERROR)?;
                    if let Some(at) = self.turns.iter().position(|(t, _)| t == id) {
                        self.turns.remove(at);
                    }
                    self.turns.push_back((id.into(), status.into()));
                    if self.turns.len() > 256 {
                        self.turns.pop_front();
                    }
                    self.latest = Some((id.into(), status.into()));
                }
            }
            self.offset += n as u64;
            self.anchor = line[line.len().saturating_sub(256)..].to_vec();
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
}
