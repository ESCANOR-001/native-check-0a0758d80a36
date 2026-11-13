//! Large Desktop snapshots contain tool output and images that observers do not
//! need. Deserialize only identity, turn phase and pending-control fields.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, io};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    method: String,
    source_client_id: String,
    params: Params,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    conversation_id: String,
    host_id: String,
    change: Change,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Change {
    #[serde(rename = "type")]
    kind: String,
    revision: u64,
    conversation_state: State,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct State {
    id: String,
    host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turns: Option<Vec<Turn>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_history: Option<TurnHistory>,
    requests: Vec<Value>,
}
#[derive(Deserialize, Serialize)]
struct TurnHistory {
    history: History,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct History {
    entities_by_key: BTreeMap<String, Turn>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Turn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    status: Option<String>,
    #[serde(
        default,
        deserialize_with = "turn_error",
        skip_serializing_if = "Option::is_none"
    )]
    error: Option<Value>,
    items: Option<Vec<Item>>,
}
fn turn_error<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(|value| crate::codex_error::thin(&value))
}
#[derive(Serialize)]
#[serde(transparent)]
struct Item(Value);
impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Borrow raw fields: multi-megabyte tool arguments/images are skipped,
        // not materialized just to preserve a small question card.
        #[derive(Deserialize)]
        struct Raw<'a> {
            #[serde(rename = "type")]
            kind: Option<String>,
            id: Option<String>,
            completed: Option<bool>,
            #[serde(borrow)]
            questions: Option<&'a serde_json::value::RawValue>,
            #[serde(borrow)]
            text: Option<&'a serde_json::value::RawValue>,
            #[serde(borrow)]
            content: Option<&'a serde_json::value::RawValue>,
            #[serde(borrow)]
            input: Option<&'a serde_json::value::RawValue>,
            #[serde(borrow)]
            arguments: Option<&'a serde_json::value::RawValue>,
            name: Option<String>,
            tool: Option<String>,
            call_id: Option<String>,
            #[serde(rename = "callId")]
            call_id_camel: Option<String>,
            role: Option<String>,
            status: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut v = serde_json::json!({"type":raw.kind,"id":raw.id,"completed":raw.completed});
        if let Some(q) = raw.questions.filter(|q| q.get().len() <= 65536) {
            v["questions"] = serde_json::from_str(q.get()).map_err(serde::de::Error::custom)?;
        }
        if raw.name.as_deref() == Some("request_user_input_async")
            || raw.tool.as_deref() == Some("request_user_input_async")
        {
            v["name"] = serde_json::json!("request_user_input_async");
            if let Some(id) = raw.call_id.or(raw.call_id_camel) {
                v["call_id"] = serde_json::json!(id);
            }
            if let Some(args) = raw.arguments.filter(|a| a.get().len() <= 65536) {
                v["arguments"] =
                    serde_json::from_str(args.get()).map_err(serde::de::Error::custom)?;
            }
        }
        let normalized = raw
            .kind
            .as_deref()
            .unwrap_or_default()
            .replace('_', "")
            .to_lowercase();
        if matches!(
            normalized.as_str(),
            "usermessage" | "steeringusermessage" | "message"
        ) {
            v["role"] = serde_json::json!(raw.role);
            v["status"] = serde_json::json!(raw.status);
            for (key, value) in [
                ("text", raw.text),
                ("content", raw.content),
                ("input", raw.input),
            ] {
                if let Some(raw) = value.filter(|r| {
                    r.get().len() <= 240_000 && r.get().contains("send_user_message_question_reply")
                }) {
                    v[key] = serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                }
            }
        }
        Ok(Item(crate::desktop_questions::thin(&v).unwrap_or_else(
            || serde_json::json!({"type":v["type"],"id":v["id"],"completed":v["completed"]}),
        )))
    }
}
/// Ordinary snapshots can contain megabytes of tool output too. Discard those
/// bodies before building a Value or measuring the transport queue, not only
/// after crossing the 8 MiB spooling threshold. Responses and patches retain
/// their original shape; validation of owner, revision and controls is unchanged.
pub(super) fn decode(bytes: &[u8]) -> io::Result<Value> {
    if bytes.len() >= 64 * 1024 {
        if let Ok(snapshot) = reduce(bytes) {
            return Ok(snapshot);
        }
    }
    serde_json::from_slice(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid IPC JSON"))
}

pub(super) fn reduce(bytes: &[u8]) -> io::Result<Value> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "Invalid large Desktop snapshot");
    let frame: Envelope = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let params = &frame.params;
    let state = &params.change.conversation_state;
    if frame.kind != "broadcast"
        || frame.method != "thread-stream-state-changed"
        || frame.source_client_id.len() > 256
        || params.host_id != "local"
        || state.host_id != "local"
        || params.conversation_id != state.id
        || uuid::Uuid::parse_str(&state.id).is_err()
        || params.change.kind != "snapshot"
        || state.requests.len() > 16
        || state.requests.iter().any(|r| r.to_string().len() > 65536)
    {
        return Err(invalid());
    }
    let turns: Vec<&Turn> = if let Some(h) = &state.turn_history {
        if h.history.entities_by_key.keys().any(|key| key.len() > 1024) {
            return Err(invalid());
        }
        h.history.entities_by_key.values().collect()
    } else {
        state.turns.as_ref().ok_or_else(invalid)?.iter().collect()
    };
    if turns.len() > 512
        || turns.iter().any(|t| {
            t.id.as_ref().is_some_and(|id| id.len() > 256)
                || t.turn_id.as_ref().is_some_and(|id| id.len() > 256)
                || t.items.as_ref().is_some_and(|items| items.len() > 32768)
        })
    {
        return Err(invalid());
    }
    serde_json::to_value(frame).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_snapshot_retains_questions_and_replies_without_tool_or_prompt_bodies() {
        use serde_json::json;
        let native = "11111111-1111-4111-8111-111111111111";
        let reply = format!(
            "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
            json!([{"questionItemId":"[\"request_user_input_async\",\"q\",0]","question":"Choose","answer":"One"}])
        );
        let raw = json!({"type":"broadcast","sourceClientId":"owner","method":"thread-stream-state-changed","params":{"conversationId":native,"hostId":"local","change":{"type":"snapshot","revision":1,"conversationState":{"id":native,"hostId":"local","requests":[],"turns":[{"id":native,"status":"inProgress","items":[
            {"id":"tool","type":"dynamicToolCall","arguments":{"private":"x".repeat(9*1024*1024)}},
            {"id":"q","type":"agentMessage","questions":[{"title":"Choose","options":["One","Two"]}]},
            {"id":"answer","type":"userMessage","content":[{"type":"text","text":reply}]},
            {"id":"ordinary","type":"userMessage","content":[{"type":"text","text":"PRIVATE_PROMPT"}]}
        ]}]}}}});
        let reduced = reduce(&serde_json::to_vec(&raw).unwrap()).unwrap();
        let serialized = reduced.to_string();
        assert!(serialized.len() < 4096);
        assert!(serialized.contains("Choose"));
        assert!(serialized.contains("questionItemId"));
        assert!(!serialized.contains("PRIVATE_PROMPT"));
        assert!(!serialized.contains("private"));
    }
    use serde_json::json;
    const THREAD: &str = "12345678-1234-4234-8234-123456789abc";
    fn snapshot() -> Value {
        json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner",
            "params":{"hostId":"local","conversationId":THREAD,"change":{"type":"snapshot","revision":3,
            "conversationState":{"id":THREAD,"hostId":"local","requests":[],"turns":[
                {"id":THREAD,"status":"inProgress","items":[{"id":"tool","type":"commandExecution","output":"x".repeat(9*1024*1024)},
                {"type":"contextCompaction","id":"compact","completed":false}]}]}}}})
    }
    #[test]
    fn oversized_snapshot_retains_phase_and_approvals_without_tool_content() {
        let mut raw = snapshot();
        let request = json!({"id":1,"method":"item/commandExecution/requestApproval","params":{"threadId":THREAD,"command":"echo safe"}});
        raw["params"]["change"]["conversationState"]["requests"] = json!([request]);
        let reduced = reduce(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(reduced.to_string().len() < 2048);
        assert_eq!(
            reduced["params"]["change"]["conversationState"]["requests"][0],
            request
        );
        let mut phase = super::super::desktop_phase::Phase::default();
        phase.apply(&reduced["params"]["change"], THREAD).unwrap();
        assert_eq!(phase.task_status(THREAD).as_deref(), Some("inProgress"));
        assert_eq!(
            phase.compacting(THREAD),
            Some((THREAD.into(), "compact".into()))
        );
    }
    #[test]
    fn accepts_normalized_turn_history_and_rejects_unrelated_large_frames() {
        let mut raw = snapshot();
        let state = &mut raw["params"]["change"]["conversationState"];
        let turns = state.as_object_mut().unwrap().remove("turns").unwrap();
        state["turnHistory"] = json!({"history":{"entitiesByKey":{"turn":turns[0]}}});
        let reduced = reduce(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(reduced.to_string().len() < 2048);
        raw["type"] = json!("response");
        assert!(reduce(&serde_json::to_vec(&raw).unwrap()).is_err());
        raw["type"] = json!("broadcast");
        raw["params"]["change"]["type"] = json!("patches");
        assert!(reduce(&serde_json::to_vec(&raw).unwrap()).is_err());
    }

    #[test]
    fn ordinary_snapshot_keeps_status_errors_questions_approvals_and_patch_positions() {
        use super::super::{desktop_phase::Phase, desktop_requests::Requests};
        let mut raw = snapshot();
        let question = json!({"id":"q","type":"agentMessage","questions":[{"title":"Choose","options":["One","Two"]}]});
        let reply = format!(
            "<send_user_message_question_reply>{}</send_user_message_question_reply>",
            json!([{"questionItemId":"[\"request_user_input_async\",\"q\",0]","question":"Choose","answer":"One"}])
        );
        let approval = json!({"id":"approval","method":"item/commandExecution/requestApproval","params":{"threadId":THREAD,"command":"echo check"}});
        let state = &mut raw["params"]["change"]["conversationState"];
        state["requests"] = json!([approval]);
        state["turns"][0]["items"][0]["output"] = json!("unneeded tool body".repeat(65536));
        state["turns"][0]["items"].as_array_mut().unwrap().extend([
            question,
            json!({"id":"reply","type":"user_message","content":[{"type":"text","text":reply}]}),
        ]);
        state["turns"][0]["error"] =
            json!({"message":"A visible error","codexErrorInfo":"badRequest","private":"discard"});
        let bytes = serde_json::to_vec(&raw).unwrap();
        assert!((64 * 1024..8 * 1024 * 1024).contains(&bytes.len()));
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.to_string().len() < 4096);
        let mut original = Phase::default();
        let mut reduced = Phase::default();
        let mut requests = Requests::default();
        original.apply(&raw["params"]["change"], THREAD).unwrap();
        reduced.apply(&decoded["params"]["change"], THREAD).unwrap();
        requests
            .apply(&decoded["params"]["change"], THREAD)
            .unwrap();
        assert_eq!(requests.rows(), &[approval]);
        assert_eq!(reduced.task_status(THREAD), original.task_status(THREAD));
        assert_eq!(reduced.task_error(THREAD), original.task_error(THREAD));
        assert_eq!(reduced.questions(THREAD), original.questions(THREAD));
        assert_eq!(reduced.question_replies(), original.question_replies());
        assert!(!reduced.question_replies().is_empty());
        assert_eq!(reduced.compacting(THREAD), original.compacting(THREAD));
        // Dropped bodies still occupy their original position. A later patch
        // must remove the question it names, not the neighboring compact item.
        let patch = json!({"type":"patches","baseRevision":3,"revision":4,"patches":[
            {"op":"remove","path":["turns",0,"items",2]},
            {"op":"replace","path":["turns",0,"items",1,"completed"],"value":true}
        ]});
        original.apply(&patch, THREAD).unwrap();
        reduced.apply(&patch, THREAD).unwrap();
        assert_eq!(reduced.questions(THREAD), original.questions(THREAD));
        assert_eq!(reduced.compacting(THREAD), None);
    }

    #[test]
    fn ordinary_responses_patches_and_invalid_json_keep_original_handling() {
        for frame in [
            json!({"type":"response","result":{"text":"history".repeat(65536)}}),
            json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner","params":{"conversationId":THREAD,"hostId":"local","change":{"type":"patches","baseRevision":3,"revision":4,"patches":[{"op":"replace","path":["turns",0,"items",0,"text"],"value":"update".repeat(65536)}]}}}),
            json!({"type":"client-discovery-request","requestId":"owner-probe","method":"thread-owner-discovery"}),
        ] {
            assert_eq!(decode(&serde_json::to_vec(&frame).unwrap()).unwrap(), frame);
        }
        assert!(decode(b"{invalid}").is_err());
    }

    #[test]
    #[ignore = "manual CPU comparison; prints timing without flaky timing assertions"]
    fn snapshot_decode_cpu_comparison() {
        let mut raw = snapshot();
        raw["params"]["change"]["conversationState"]["turns"][0]["items"][0]["output"] =
            json!("sample tool output\n".repeat(190000));
        let bytes = serde_json::to_vec(&raw).unwrap();
        let runs = 20;
        for thin in [false, true] {
            let start = std::time::Instant::now();
            let mut retained = 0;
            for _ in 0..runs {
                let value: Value = if thin {
                    decode(&bytes).unwrap()
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                // Transport measures each queued frame by serializing it. The
                // old path did this before dropping unneeded message bodies.
                let serialized = serde_json::to_vec(&value).unwrap();
                retained = std::hint::black_box(serialized.len());
            }
            println!("snapshot decode: thin={thin}, runs={runs}, input_bytes={}, retained_bytes={retained}, elapsed_ms={}", bytes.len(), start.elapsed().as_millis());
        }
    }
}
