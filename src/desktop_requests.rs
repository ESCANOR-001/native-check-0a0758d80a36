//! Pending requests from one verified Desktop owner. Only this small control
//! inventory is retained, never the conversation or tool output.
use serde_json::{json, Value};
use std::collections::BTreeSet;

const INVALID: &str = "Desktop pending requests changed or could not be verified";
#[derive(Default)]
pub(super) struct Requests {
    rows: Vec<Value>,
}
fn key(value: &Value) -> Result<String, String> {
    if value
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 128)
        || value.as_i64().is_some()
    {
        Ok(value.to_string())
    } else {
        Err(INVALID.into())
    }
}
fn validate(rows: &Value, native: &str) -> Result<Vec<Value>, String> {
    let rows = rows.as_array().filter(|r| r.len() <= 16).ok_or(INVALID)?;
    let mut ids = BTreeSet::new();
    rows.iter()
        .map(|row| {
            let method = row["method"].as_str().ok_or(INVALID)?;
            if !matches!(
                method,
                "item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
                    | "item/tool/requestUserInput"
                    | "mcpServer/elicitation/request"
            ) || !row["params"].is_object()
                || row["params"]["threadId"] != native
                || !ids.insert(key(&row["id"])?)
                || row.to_string().len() > 65536
            {
                return Err(INVALID.into());
            }
            Ok(json!({"id":row["id"],"method":method,"params":row["params"]}))
        })
        .collect()
}
impl Requests {
    /// Phase validates the enclosing revision and owner before this is called.
    /// Stage every patch first: a malformed update cannot publish half a list.
    pub(super) fn apply(&mut self, change: &Value, native: &str) -> Result<(), String> {
        if change["type"] == "snapshot" {
            self.rows = validate(&change["conversationState"]["requests"], native)?;
        } else {
            let mut next = self.rows.clone();
            for patch in change["patches"].as_array().ok_or(INVALID)? {
                let path = patch["path"].as_array().ok_or(INVALID)?;
                if path.first().is_none_or(|v| v != "requests") {
                    continue;
                }
                let op = patch["op"].as_str().ok_or(INVALID)?;
                if path.len() == 1 && matches!(op, "add" | "replace") {
                    next = validate(&patch["value"], native)?;
                } else if path.len() == 2 {
                    let i = path[1].as_u64().filter(|n| *n <= 16).ok_or(INVALID)? as usize;
                    match op {
                        "add" if i <= next.len() && next.len() < 16 => {
                            next.insert(i, patch["value"].clone())
                        }
                        "replace" if i < next.len() => next[i] = patch["value"].clone(),
                        "remove" if i < next.len() => {
                            next.remove(i);
                        }
                        _ => return Err(INVALID.into()),
                    }
                } else {
                    return Err(INVALID.into());
                }
            }
            self.rows = validate(&json!(next), native)?;
        }
        Ok(())
    }
    pub(super) fn rows(&self) -> &[Value] {
        &self.rows
    }
    pub(super) fn response(&self, response: &Value) -> Result<(&'static str, Value), String> {
        key(&response["id"])?;
        if response.get("method").is_some()
            || response.get("error").is_some()
            || !response["result"].is_object()
        {
            return Err(INVALID.into());
        }
        let row = self
            .rows
            .iter()
            .find(|r| r["id"] == response["id"])
            .ok_or(INVALID)?;
        let id = &response["id"];
        let result = &response["result"];
        Ok(match row["method"].as_str().ok_or(INVALID)? {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                if result.as_object().is_none_or(|o| o.len() != 1)
                    || !matches!(
                        result["decision"].as_str(),
                        Some("accept" | "decline" | "cancel")
                    )
                {
                    return Err(INVALID.into());
                }
                (
                    if row["method"] == "item/commandExecution/requestApproval" {
                        "thread-follower-command-approval-decision"
                    } else {
                        "thread-follower-file-approval-decision"
                    },
                    json!({"requestId":id,"decision":result["decision"]}),
                )
            }
            "item/tool/requestUserInput" => (
                "thread-follower-submit-user-input",
                json!({"requestId":id,"response":result}),
            ),
            "mcpServer/elicitation/request" => (
                "thread-follower-submit-mcp-server-elicitation-response",
                json!({"requestId":id,"response":result}),
            ),
            _ => return Err(INVALID.into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: Value) -> Value {
        json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"native","turnId":"turn","command":"echo approved"}})
    }
    fn snapshot(rows: Value) -> Value {
        json!({"type":"snapshot","conversationState":{"requests":rows}})
    }
    #[test]
    fn exact_request_identity_and_expiration_are_required() {
        let mut r = Requests::default();
        r.apply(&snapshot(json!([row(json!(7)), row(json!("7"))])), "native")
            .unwrap();
        let (m, p) = r
            .response(&json!({"id":7,"result":{"decision":"decline"}}))
            .unwrap();
        assert_eq!(m, "thread-follower-command-approval-decision");
        assert_eq!(p["requestId"], 7);
        assert!(r
            .response(&json!({"id":8,"result":{"decision":"accept"}}))
            .is_err());
        assert!(r
            .response(&json!({"id":7,"result":{"decision":"acceptForSession"}}))
            .is_err());
        r.apply(
            &json!({"type":"patches","patches":[{"op":"remove","path":["requests",0]}]}),
            "native",
        )
        .unwrap();
        assert!(r
            .response(&json!({"id":7,"result":{"decision":"accept"}}))
            .is_err());
        assert!(r
            .response(&json!({"id":"7","result":{"decision":"accept"}}))
            .is_ok());
    }
    #[test]
    fn bad_inventories_and_partial_updates_do_not_replace_verified_requests() {
        let mut r = Requests::default();
        let good = row(json!(1));
        r.apply(&snapshot(json!([good])), "native").unwrap();
        assert!(r
            .apply(&snapshot(json!([row(json!(1)), row(json!(1))])), "native")
            .is_err());
        assert!(r
            .apply(&snapshot(json!([row(json!(1))])), "foreign")
            .is_err());
        assert!(r
            .apply(
                &snapshot(
                    json!([{"id":1,"method":"currentTime/read","params":{"threadId":"native"}}])
                ),
                "native"
            )
            .is_err());
        assert!(r.apply(&json!({"type":"patches","patches":[{"op":"remove","path":["requests",0]},{"op":"remove","path":["requests",4]}]}),"native").is_err());
        assert_eq!(r.rows(), &[row(json!(1))]);
        assert!(r
            .apply(&snapshot(json!(vec![row(json!(1)); 17])), "native")
            .is_err());
        let mut huge = row(json!(2));
        huge["params"]["command"] = json!("x".repeat(65536));
        assert!(r.apply(&snapshot(json!([huge])), "native").is_err());
    }
    #[test]
    fn input_and_elicitation_responses_use_their_own_routes() {
        for (method, route) in [
            (
                "item/tool/requestUserInput",
                "thread-follower-submit-user-input",
            ),
            (
                "mcpServer/elicitation/request",
                "thread-follower-submit-mcp-server-elicitation-response",
            ),
        ] {
            let mut r = Requests::default();
            let mut v = row(json!("question"));
            v["method"] = json!(method);
            r.apply(&snapshot(json!([v])), "native").unwrap();
            let answer = json!({"answers":{"q":{"answers":["yes"]}}});
            let (m, p) = r
                .response(&json!({"id":"question","result":answer}))
                .unwrap();
            assert_eq!(m, route);
            assert_eq!(p["response"], answer);
            assert!(r
                .response(&json!({"id":"question","error":{"code":-1}}))
                .is_err());
        }
    }
}
