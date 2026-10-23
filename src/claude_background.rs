//! Native task lifetimes, without retaining descriptions, prompts or outputs.
//! A late notification for an older run must not finish a resumed task.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Tasks {
    active: BTreeMap<String, Option<String>>,
}
impl Tasks {
    pub fn busy(&self) -> bool {
        !self.active.is_empty()
    }
    pub fn observe(&mut self, frame: &Value) -> Result<(), String> {
        if frame["type"] != "system" || !frame["parent_tool_use_id"].is_null() {
            return Ok(());
        }
        let started = frame["subtype"] == "task_started";
        let ended = frame["subtype"] == "task_notification";
        if !started && !ended {
            return Ok(());
        }
        if started && (frame["skip_transcript"] == true || frame["ambient"] == true) {
            return Ok(());
        }
        let id = frame["task_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or("Claude background task has an invalid identity")?;
        let run = frame["run_id"].as_str();
        if !frame["run_id"].is_null() && run.is_none()
            || run.is_some_and(|r| r.is_empty() || r.len() > 256 || r.chars().any(char::is_control))
        {
            return Err("Claude background task has an invalid run identity".into());
        }
        if started {
            if let Some(Some(current)) = self.active.get(id) {
                if run.is_none_or(|r| r < current.as_str()) {
                    return Ok(());
                }
            }
            if self.active.len() >= 128 && !self.active.contains_key(id) {
                return Err("Too many active Claude background tasks; history is retained".into());
            }
            self.active.insert(id.into(), run.map(str::to_owned));
        } else if ["completed", "failed", "stopped"]
            .contains(&frame["status"].as_str().unwrap_or(""))
            && self
                .active
                .get(id)
                .is_some_and(|current| current.as_deref() == run)
        {
            self.active.remove(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn task_lifetimes_ignore_watchers_and_late_runs_and_bound_memory() {
        let mut tasks = Tasks::default();
        tasks.observe(&json!({"type":"system","subtype":"task_started","task_id":"watcher","ambient":true})).unwrap();
        assert!(!tasks.busy());
        for run in ["001", "002", "001"] {
            tasks.observe(&json!({"type":"system","subtype":"task_started","task_id":"task","run_id":run,"description":"PRIVATE_OUTPUT"})).unwrap();
        }
        tasks.observe(&json!({"type":"system","subtype":"task_notification","task_id":"task","run_id":"001","status":"completed"})).unwrap();
        assert!(tasks.busy());
        let value = serde_json::to_value(&tasks).unwrap();
        assert!(!value.to_string().contains("PRIVATE_OUTPUT"));
        let mut tasks: Tasks = serde_json::from_value(value).unwrap();
        tasks.observe(&json!({"type":"system","subtype":"task_notification","task_id":"task","run_id":"002","status":"failed"})).unwrap();
        assert!(!tasks.busy());
        for i in 0..128 {
            tasks
                .observe(&json!({"type":"system","subtype":"task_started","task_id":i.to_string()}))
                .unwrap();
        }
        assert!(tasks
            .observe(&json!({"type":"system","subtype":"task_started","task_id":"overflow"}))
            .is_err());
        assert_eq!(tasks.active.len(), 128);
    }
}
