//! Native task lifetimes, without retaining descriptions, prompts or outputs.
//! A late notification for an older run must not finish a resumed task.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StopTarget {
    pub request: String,
    task: String,
    run: Option<String>,
}
/// Ephemeral delivery sequence. All targets are durably reserved by the caller
/// before the first write; reconnect deliberately never reconstructs delivery.
pub(crate) struct StopSequence {
    targets: VecDeque<StopTarget>,
    interrupt: Value,
}
impl StopSequence {
    pub fn new(targets: Vec<StopTarget>, interrupt: Value) -> Self {
        Self {
            targets: targets.into(),
            interrupt,
        }
    }
    pub fn first(&self, tasks: &Tasks) -> Result<Value, String> {
        if let Some(target) = self.targets.front() {
            if tasks
                .active
                .get(&target.task)
                .is_some_and(|run| run != &target.run)
            {
                return Err("A Claude background task changed while stopping; the replacement was not stopped".into());
            }
            Ok(
                json!({"type":"control_request","request_id":target.request,"request":{"subtype":"stop_task","task_id":target.task}}),
            )
        } else {
            Ok(self.interrupt.clone())
        }
    }
    pub fn reply(&mut self, frame: &Value, tasks: &Tasks) -> Result<Option<Value>, String> {
        let Some(target) = self.targets.front() else {
            return Ok(None);
        };
        if frame["type"] != "control_response" || frame["response"]["request_id"] != target.request
        {
            return Ok(None);
        }
        if frame["response"]["subtype"] != "success" {
            return Err("Claude did not confirm stopping a background task. It may still be running; Stop was not replayed.".into());
        }
        self.targets.pop_front();
        self.first(tasks).map(Some)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Tasks {
    active: BTreeMap<String, Option<String>>,
}
impl Tasks {
    pub fn busy(&self) -> bool {
        !self.active.is_empty()
    }
    pub fn stop_targets(&self) -> Vec<StopTarget> {
        self.active
            .iter()
            .map(|(task, run)| StopTarget {
                task: task.clone(),
                run: run.clone(),
                request: format!("remodex-task-stop-{}", uuid::Uuid::new_v4()),
            })
            .collect()
    }
    /// A successful stop_task response is authoritative even when the CLI
    /// suppresses task_notification. Never clear a later run of the same ID.
    pub fn acknowledge_stop(&mut self, frame: &Value, targets: &[StopTarget]) {
        if frame["type"] != "control_response" || frame["response"]["subtype"] != "success" {
            return;
        }
        if let Some(target) = targets
            .iter()
            .find(|t| frame["response"]["request_id"] == t.request)
        {
            if self.active.get(&target.task) == Some(&target.run) {
                self.active.remove(&target.task);
            }
        }
    }
    pub fn observe(&mut self, frame: &Value) -> Result<(), String> {
        if frame["type"] != "system" || !frame["parent_tool_use_id"].is_null() {
            return Ok(());
        }
        let started = frame["subtype"] == "task_started";
        let ended = frame["subtype"] == "task_notification" || frame["subtype"] == "task_updated";
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
        let run_value = frame
            .get("run_id")
            .or_else(|| frame["patch"].get("run_id"))
            .unwrap_or(&Value::Null);
        let run = run_value.as_str();
        if !run_value.is_null() && run.is_none()
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
        } else if ["completed", "failed", "stopped", "killed"].contains(
            &frame["status"]
                .as_str()
                .or_else(|| frame["patch"]["status"].as_str())
                .unwrap_or(""),
        ) && self
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
    fn stop_targets_are_scoped_sequential_and_require_exact_success() {
        let mut tasks = Tasks::default();
        for task in ["one", "two"] {
            tasks.observe(&json!({"type":"system","subtype":"task_started","task_id":task,"run_id":"001"})).unwrap();
        }
        let targets = tasks.stop_targets();
        let targets: Vec<StopTarget> =
            serde_json::from_value(serde_json::to_value(targets).unwrap()).unwrap();
        let mut sequence =
            StopSequence::new(targets.clone(), json!({"request":{"subtype":"interrupt"}}));
        let first = sequence.first(&tasks).unwrap();
        assert_eq!(first["request"]["task_id"], "one");
        let wrong = json!({"type":"control_response","response":{"subtype":"success","request_id":"unrelated"}});
        tasks.acknowledge_stop(&wrong, &targets);
        assert_eq!(tasks.active.len(), 2);
        assert!(sequence.reply(&wrong, &tasks).unwrap().is_none());
        let reject = json!({"type":"control_response","response":{"subtype":"error","request_id":targets[0].request}});
        assert!(sequence.reply(&reject, &tasks).is_err());
        tasks.acknowledge_stop(&reject, &targets);
        assert_eq!(tasks.active.len(), 2);
        let success = json!({"type":"control_response","response":{"subtype":"success","request_id":targets[0].request}});
        tasks.acknowledge_stop(&success, &targets);
        assert_eq!(
            sequence.reply(&success, &tasks).unwrap().unwrap()["request"]["task_id"],
            "two"
        );
        assert!(sequence.reply(&success, &tasks).unwrap().is_none());
        tasks
            .observe(
                &json!({"type":"system","subtype":"task_started","task_id":"two","run_id":"002"}),
            )
            .unwrap();
        assert!(sequence.first(&tasks).is_err());
        let late = json!({"type":"control_response","response":{"subtype":"success","request_id":targets[1].request}});
        tasks.acknowledge_stop(&late, &targets);
        assert!(tasks.busy());
        tasks.observe(&json!({"type":"system","subtype":"task_updated","task_id":"two","patch":{"run_id":"001","status":"killed"}})).unwrap();
        assert!(tasks.busy());
        tasks.observe(&json!({"type":"system","subtype":"task_updated","task_id":"two","patch":{"run_id":"002","status":"killed"}})).unwrap();
        assert!(!tasks.busy());
    }
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
