//! Presentation state is derived from confirmed execution, independently of
//! socket connectivity. A lost observer must not call a task paused or done.
use serde_json::{json, Value};
pub(crate) fn refresh(state: &mut Value) {
    let thread = &state["thread"];
    if !thread["session"].is_object() {
        return;
    }
    let session = &thread["session"];
    let confidence = session["statusConfidence"].as_str();
    let running = session["status"] == "running" || thread["latestTurn"]["state"] == "running";
    let uncertain = matches!(confidence, Some("unknown" | "unavailable"))
        || session["status"] == "error" && session["activeTurnId"].is_string();
    let phase = if uncertain {
        "unknown"
    } else if thread["hasPendingApprovals"] == true || thread["hasPendingUserInput"] == true {
        "paused"
    } else if session["status"] == "starting" {
        "starting"
    } else if running {
        if state["compaction_turn"].is_string()
            && state["compaction_turn"] == thread["latestTurn"]["turnId"]
        {
            "compacting"
        } else {
            "running"
        }
    } else if matches!(session["status"].as_str(), Some("interrupted" | "stopped"))
        || thread["latestTurn"]["state"] == "interrupted"
        || state["paused"] == true && state["queue"].as_array().is_some_and(|a| !a.is_empty())
    {
        "paused"
    } else {
        "idle"
    };
    let legacy_unknown = confidence == Some("unavailable");
    state["thread"]["session"]["executionState"] = json!(phase);
    if legacy_unknown {
        state["thread"]["session"]["statusConfidence"] = json!("unknown");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_distinguishes_compaction_pause_and_unknown_for_both_agents() {
        for agent in ["codex", "claude"] {
            let mut c = json!({"agent":agent,"compaction_turn":"t","thread":{"session":{"status":"running","statusConfidence":"confirmed","activeTurnId":"t"},"latestTurn":{"turnId":"t","state":"running"}}});
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "compacting");
            c["thread"]["session"]["statusConfidence"] = json!("unknown");
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "unknown");
            c["thread"]["session"]["statusConfidence"] = json!("confirmed");
            c["compaction_turn"] = json!("old");
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "running");
            c["thread"]["hasPendingApprovals"] = json!(true);
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "paused");
            c["thread"]["hasPendingApprovals"] = json!(false);
            c["thread"]["latestTurn"]["state"] = json!("interrupted");
            c["thread"]["session"]["status"] = json!("interrupted");
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "paused");
            c["thread"]["latestTurn"]["state"] = json!("completed");
            c["thread"]["session"]["status"] = json!("ready");
            refresh(&mut c);
            assert_eq!(c["thread"]["session"]["executionState"], "idle");
        }
    }
}
