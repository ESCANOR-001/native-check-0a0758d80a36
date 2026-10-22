//! Every server-to-client control request must receive a response or become a
//! visible, bounded pending input. Never infer an approval from a method suffix.
use crate::{chat_projection::s, elicitation, questions};
use serde_json::{json, Value};

pub fn is_approval(v: &Value, codex: bool) -> bool {
    if codex {
        v.get("id").is_some()
            && matches!(
                s(&v["method"]),
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
            )
    } else {
        v["type"] == "control_request" && v["request"]["subtype"] == "can_use_tool"
    }
}

pub fn automatic(v: &Value, codex: bool) -> Option<(Value, Option<&'static str>)> {
    let request = if codex {
        v.get("id").is_some() && v["method"].is_string()
    } else {
        v["type"] == "control_request"
    };
    if !request {
        return None;
    }
    if codex && v["method"] == "currentTime/read" {
        return Some((
            json!({"id":v["id"],"result":{"currentTimeAt":crate::auth::now()/1000}}),
            None,
        ));
    }
    if elicitation::is_elicitation(v) {
        return match elicitation::questions(v) {
            Ok(_) => None,
            Err(message) => Some((
                elicitation::response(v, json!({"action":"cancel"})),
                Some(message),
            )),
        };
    }
    if is_approval(v, codex) || questions::is_question(v) {
        return None;
    }
    let message = "This CLI control request is not supported by the Rust backend. It was declined; no action was approved.";
    let response = if codex {
        json!({"id":v["id"],"error":{"code":-32601,"message":message}})
    } else {
        json!({"type":"control_response","response":{"subtype":"error","request_id":v["request_id"],"error":message}})
    };
    Some((response, Some(message)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_requests_and_unknown_controls_are_answered_without_approval() {
        let (r, notice) = automatic(&json!({"id":9,"method":"currentTime/read"}), true).unwrap();
        assert_eq!(r["id"], 9);
        assert!(r["result"]["currentTimeAt"].as_u64().unwrap() > 1_000_000_000);
        assert!(notice.is_none());
        for method in [
            "item/newTool/requestApproval",
            "execCommandApproval",
            "unknown",
        ] {
            let v = json!({"id":17,"method":method});
            assert!(!is_approval(&v, true));
            let (r, n) = automatic(&v, true).unwrap();
            assert_eq!(r["error"]["code"], -32601);
            assert!(r["result"].is_null());
            assert!(n.is_some());
        }
        assert!(automatic(&json!({"method":"newNotification"}), true).is_none());
        assert!(automatic(&json!({"id":1,"result":{}}), true).is_none());
        let (r,_) = automatic(&json!({"type":"control_request","request_id":"q","request":{"subtype":"unknown","secret":"DO_NOT_PROJECT"}}),false).unwrap();
        assert_eq!(r["response"]["subtype"], "error");
        assert!(!r.to_string().contains("DO_NOT_PROJECT"));
    }
}
