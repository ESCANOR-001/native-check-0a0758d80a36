//! Completion metadata consumed by the existing phone timeline, including pages
//! that no longer contain latestTurn. Keep source timestamps, never tool output.
use serde_json::{json, Value};

pub(crate) struct Timing {
    pub turn: String,
    pub started: String,
    pub completed: String,
    pub millis: i64,
}
impl Timing {
    pub fn read(turn: &Value) -> Option<Self> {
        if !matches!(
            turn["state"].as_str(),
            Some("completed" | "interrupted" | "error")
        ) {
            return None;
        }
        let id = turn["turnId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)?;
        let started = turn["startedAt"].as_str().filter(|s| s.len() <= 64)?;
        let completed = turn["completedAt"].as_str().filter(|s| s.len() <= 64)?;
        let format = &time::format_description::well_known::Rfc3339;
        let begin = time::OffsetDateTime::parse(started, format).ok()?;
        let end = time::OffsetDateTime::parse(completed, format).ok()?;
        let millis = i64::try_from((end - begin).whole_milliseconds())
            .ok()
            .filter(|n| *n >= 0)?;
        Some(Self {
            turn: id.into(),
            started: started.into(),
            completed: completed.into(),
            millis,
        })
    }
    pub fn apply(&self, row: &mut Value) {
        if row["turnId"] != self.turn
            || (!row["payload"].is_object() && !row["payload"].is_null())
            || row.to_string().len() + 360 > crate::chat_store::ROW_BYTES
        {
            return;
        }
        row["payload"]["turnStartedAt"] = json!(self.started);
        row["payload"]["turnCompletedAt"] = json!(self.completed);
        row["payload"]["turnDurationMs"] = json!(self.millis);
    }
}
pub(crate) fn complete(thread: &mut Value) {
    let Some(timing) = Timing::read(&thread["latestTurn"]) else {
        return;
    };
    let Some(rows) = thread["activities"].as_array_mut() else {
        return;
    };
    for row in rows {
        timing.apply(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_requires_valid_completed_turn_and_preserves_other_content() {
        let mut turn = json!({"turnId":"t","state":"interrupted","startedAt":"2026-01-01T01:00:00Z","completedAt":"2026-01-01T01:00:57Z"});
        let t = Timing::read(&turn).unwrap();
        assert_eq!(t.millis, 57_000);
        let mut row = json!({"turnId":"t","payload":{"command":"keep","status":"stopped"}});
        t.apply(&mut row);
        assert_eq!(row["payload"]["command"], "keep");
        assert_eq!(row["payload"]["status"], "stopped");
        assert_eq!(row["payload"]["turnDurationMs"], 57_000);
        for state in ["running", "queued"] {
            turn["state"] = json!(state);
            assert!(Timing::read(&turn).is_none());
        }
        turn["state"] = json!("completed");
        turn["completedAt"] = json!("2025-01-01T00:00:00Z");
        assert!(Timing::read(&turn).is_none());
        turn["completedAt"] = json!("invalid");
        assert!(Timing::read(&turn).is_none());
    }
}
