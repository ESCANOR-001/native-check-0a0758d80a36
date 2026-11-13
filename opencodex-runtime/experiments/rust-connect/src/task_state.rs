//! Presentation state is derived from confirmed execution, independently of
//! socket connectivity. A lost observer must not call a task paused or done.
use serde_json::{json, Value};
pub(crate) fn refresh(state: &mut Value) {
    claude_connection(state);
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
    } else if session["localClaudeWaiting"] == true
        || thread["hasPendingApprovals"] == true
        || (thread["hasPendingUserInput"] == true
            && thread["hasPendingNonBlockingUserInput"] != true)
    {
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

/// Transport availability must never erase a saved task result or authorize
/// Send/Stop. Keep it alongside execution confidence, including in shell rows.
fn claude_connection(state: &mut Value) {
    if state["agent"] != "claude" || !state["thread"]["session"].is_object() {
        return;
    }
    let payload = state["thread"]["activities"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .rev()
                .find(|r| r["kind"] == "claude.desktop-sync.updated")
        })
        .map(|r| r["payload"].clone());
    let Some(payload) = payload else { return };
    let detail = payload["detail"].as_str().unwrap_or("");
    let issue = if let Some(issue) = crate::agent_setup::verification_issue(detail) {
        Some(issue)
    } else if detail.contains("no active Remote Control link") {
        Some("not-shared")
    } else if detail.contains("original Claude worker is disconnected") {
        Some("owner-offline")
    } else if detail.contains("workspace") || detail.contains("folder") {
        Some("workspace")
    } else if detail.contains("free slot") || detail.contains("available worker") {
        Some("capacity")
    } else {
        None
    };
    let status = match payload["status"].as_str() {
        Some("connected") => "connected",
        Some("connecting") => "connecting",
        Some("history" | "parked") => "history",
        _ if issue == Some("not-shared") => "history",
        _ => "disconnected",
    };
    let session = &mut state["thread"]["session"];
    session["connectionState"] = json!(status);
    session["connectionIssue"] = if status == "connected" {
        Value::Null
    } else {
        json!(issue)
    };
    session["connectionDetail"] = if detail.is_empty() {
        Value::Null
    } else {
        json!(detail.chars().take(2048).collect::<String>())
    };
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_connection_errors_preserve_saved_results_and_uncertain_controls() {
        for (detail, issue, connection) in [
            (
                "This chat has no active Remote Control link; its history is retained",
                "not-shared",
                "history",
            ),
            (
                "The original Claude worker is disconnected; history is retained",
                "owner-offline",
                "disconnected",
            ),
            (
                "The native chat is outside this isolated workspace",
                "workspace",
                "disconnected",
            ),
        ] {
            let mut chat = json!({"agent":"claude","thread":{
                "session":{"status":"disconnected","statusConfidence":"unknown","activeTurnId":null},
                "latestTurn":{"turnId":"original","state":"completed"},
                "activities":[{"kind":"claude.desktop-sync.updated","payload":{"status":"disconnected","detail":detail}}]
            }});
            refresh(&mut chat);
            assert_eq!(chat["thread"]["latestTurn"]["state"], "completed");
            assert_eq!(chat["thread"]["session"]["statusConfidence"], "unknown");
            assert_eq!(chat["thread"]["session"]["executionState"], "unknown");
            assert_eq!(chat["thread"]["session"]["connectionState"], connection);
            assert_eq!(chat["thread"]["session"]["connectionIssue"], issue);
            chat["thread"]["activities"][0]["payload"] = json!({"status":"connected"});
            refresh(&mut chat);
            assert!(chat["thread"]["session"]["connectionIssue"].is_null());
            // A transport notification alone still cannot authorize controls.
            assert_eq!(chat["thread"]["session"]["statusConfidence"], "unknown");
        }
    }
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
