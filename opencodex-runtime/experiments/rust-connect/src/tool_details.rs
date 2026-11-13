//! On-demand public tool input/output. Never fetch a whole turn or resume its owner.
use crate::{auth_service::AuthService, chat::AgentConfig, codex_history_reader::Reader};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

const PAGE: usize = 32_768;
const ERROR: &str = "This activity is not saved by Codex yet. Try again shortly.";

pub(crate) async fn read(
    mut config: AgentConfig,
    db: AuthService,
    chat: String,
    native: String,
    activity: String,
    offset: usize,
    cwd: PathBuf,
) -> Result<Value, String> {
    if activity.is_empty() || activity.len() > 1024 || native.is_empty() {
        return Err(ERROR.into());
    }
    // Resolve identifiers from this chat's saved row, not caller-supplied native IDs.
    let (turn, item, pending) = db
        .call(
            move |a| -> Result<(String, String, Option<Value>), String> {
                let row = a
                    .store
                    .history_row(&chat, "activities", &activity)
                    .map_err(|_| ERROR)?
                    .ok_or(ERROR)?;
                let public = row["turnId"].as_str().ok_or(ERROR)?;
                let turn: Option<String> = a
                    .store
                    .db
                    .query_row(
                        "SELECT native FROM native_turns WHERE chat=?1 AND turn=?2",
                        params![chat, public],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|_| ERROR)?;
                let item = row["payload"]["itemId"]
                    .as_str()
                    .or_else(|| row["payload"]["data"]["toolCallId"].as_str())
                    .ok_or(ERROR)?;
                Ok((
                    turn.unwrap_or_else(|| public.to_owned()),
                    item.to_owned(),
                    pending_details(&row, offset),
                ))
            },
        )
        .await??;
    let cwd = if let Some(crate::agent::SharedServer::Desktop {
        reader_executable,
        reader_workspace,
        ..
    }) = &config.shared_server
    {
        let cwd = reader_workspace.clone();
        config.executable = reader_executable.clone();
        cwd
    } else {
        cwd
    };
    let mut reader = Reader::open(&config, &cwd).await?;
    let result = tokio::time::timeout(Duration::from_secs(18), async {
        // Native anchors are exclusive. Read the predecessor, then the next item.
        // This also handles the first item without scanning a large response.
        let before = reader
            .rpc(
                "thread/items/list",
                json!({"threadId":native,"turnId":turn,
            "cursor":{"type":"item","itemId":item},"limit":1,"sortDirection":"desc"}),
            )
            .await?;
        let previous = before["data"].as_array().and_then(|rows| rows.first());
        if previous.is_some_and(|v| v["turnId"] != turn) {
            return Err(ERROR.into());
        }
        let anchor = previous
            .and_then(|v| v["item"]["id"].as_str())
            .map(|id| json!({"type":"item","itemId":id}))
            .unwrap_or(Value::Null);
        let page = reader
            .rpc(
                "thread/items/list",
                json!({"threadId":native,"turnId":turn,
            "cursor":anchor,"limit":1,"sortDirection":"asc"}),
            )
            .await?;
        let entry = page["data"]
            .as_array()
            .and_then(|rows| rows.first())
            .ok_or(ERROR)?;
        if entry["turnId"] != turn || entry["item"]["id"] != item {
            return Err(ERROR.into());
        }
        project(&entry["item"], offset)
    })
    .await
    .unwrap_or_else(|_| Err("Tool details timed out. Try again.".into()));
    reader.close().await;
    result.or_else(|error| {
        if error == crate::codex_history_reader::ITEM_NOT_SAVED {
            pending.ok_or(error)
        } else {
            Err(error)
        }
    })
}

// The live event already supplies public input, even when the CLI has not
// persisted the running tool. Do not fabricate output, scan another turn, or
// weaken saved-history identity checks. Refresh fetches the finished native item.
fn pending_details(row: &Value, offset: usize) -> Option<Value> {
    let payload = &row["payload"];
    if offset != 0 || row["kind"] != "tool.started" || payload["status"] != "running" {
        return None;
    }
    let data = &payload["data"];
    if data["providerName"] != "codex" {
        return None;
    }
    let input = data["inputDetail"]
        .as_str()
        .or_else(|| data["command"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| pretty(&data["input"]));
    let label = if data["command"].is_string()
        || matches!(
            payload["itemType"].as_str(),
            Some("file-read" | "codebase-search")
        ) {
        "Command"
    } else {
        "Input"
    };
    let mut end = input.len().min(64_000);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    Some(
        json!({"inputLabel":label,"input":&input[..end],"inputTruncated":end<input.len(),
        "output":"","outputAvailable":false,"offset":0,"nextOffset":null,
        "status":"inProgress","exitCode":null,"cwd":data["input"]["cwd"],"error":null}),
    )
}

// Only explicit public tool fields; no user/system messages or reasoning payloads.
fn project(item: &Value, offset: usize) -> Result<Value, String> {
    let kind = item["type"].as_str().unwrap_or_default();
    let (label, input, output) = match kind {
        "commandExecution" => (
            "Command",
            string(&item["command"]),
            string(&item["aggregatedOutput"]),
        ),
        "mcpToolCall" => (
            "Input",
            pretty(&item["arguments"]),
            public_result(&item["result"]),
        ),
        "dynamicToolCall" => (
            "Input",
            pretty(&item["arguments"]),
            public_result(&item["contentItems"]),
        ),
        "fileChange" => (
            "Files",
            item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["path"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["diff"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        "webSearch" => ("Search", string(&item["query"]), pretty(&item["action"])),
        "imageView" => ("Image", string(&item["path"]), String::new()),
        "imageGeneration" => ("Input", string(&item["revisedPrompt"]), String::new()),
        "sleep" => (
            "Duration",
            item["durationMs"]
                .as_u64()
                .map(|n| format!("{n} ms"))
                .unwrap_or_default(),
            String::new(),
        ),
        "collabAgentToolCall" => (
            "Input",
            string(&item["prompt"]),
            pretty(&item["agentsStates"]),
        ),
        _ => return Err("This activity has no command output.".into()),
    };
    if offset > output.len() || !output.is_char_boundary(offset) {
        return Err("Invalid output page".into());
    }
    let mut end = output.len().min(offset + PAGE);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < output.len()).then_some(end);
    let mut input_end = input.len().min(64_000);
    while !input.is_char_boundary(input_end) {
        input_end -= 1;
    }
    Ok(
        json!({"inputLabel":label,"input":&input[..input_end],"inputTruncated":input_end<input.len(),
        "output":&output[offset..end],"nextOffset":next,"offset":offset,
        "status":item["status"],"exitCode":item["exitCode"],"cwd":item["cwd"],
        "error":item["error"]["message"],"outputAvailable":!output.is_empty()}),
    )
}
fn string(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}
fn pretty(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else if v.is_string() {
        string(v)
    } else {
        serde_json::to_string_pretty(v).unwrap_or_default()
    }
}
fn public_result(v: &Value) -> String {
    let blocks = v.as_array().or_else(|| v["content"].as_array());
    if let Some(blocks) = blocks {
        blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text" | "inputText") => b["text"].as_str().map(str::to_owned),
                Some("image" | "inputImage") => {
                    Some("[Image — open the image preview in the chat]".into())
                }
                Some("resource") => b["resource"]["text"].as_str().map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        pretty(&v["structuredContent"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsaved_running_tool_keeps_public_input_without_inventing_output() {
        let row = json!({"kind":"tool.started","payload":{"status":"running",
            "data":{"providerName":"codex","command":"sleep 30", "input":{"cwd":"/test"},
            "aggregatedOutput":"NOT_SAVED","private":"SECRET"}}});
        let details = pending_details(&row, 0).unwrap();
        assert_eq!(details["input"], "sleep 30");
        assert_eq!(details["inputLabel"], "Command");
        assert_eq!(details["status"], "inProgress");
        assert_eq!(details["outputAvailable"], false);
        assert_eq!(details["output"], "");
        assert!(!details.to_string().contains("SECRET"));
        assert!(!details.to_string().contains("NOT_SAVED"));
        assert!(pending_details(&row, 1).is_none());
        for (field, value) in [("kind", "tool.completed"), ("kind", "task.progress")] {
            let mut changed = row.clone();
            changed[field] = json!(value);
            assert!(pending_details(&changed, 0).is_none());
        }
        for status in ["completed", "failed", "interrupted"] {
            let mut changed = row.clone();
            changed["payload"]["status"] = json!(status);
            assert!(pending_details(&changed, 0).is_none());
        }
        let mut changed = row.clone();
        changed["payload"]["data"]["providerName"] = json!("claude-code");
        assert!(pending_details(&changed, 0).is_none());
    }

    #[test]
    fn command_and_output_are_separate_and_unicode_pages_are_lossless() {
        let output = "λ".repeat(PAGE);
        let item = json!({"type":"commandExecution","command":"printf λ","aggregatedOutput":output,"status":"completed","exitCode":0});
        let a = project(&item, 0).unwrap();
        let b = project(&item, a["nextOffset"].as_u64().unwrap() as usize).unwrap();
        assert_eq!(a["input"], "printf λ");
        assert_eq!(
            format!(
                "{}{}",
                a["output"].as_str().unwrap(),
                b["output"].as_str().unwrap()
            ),
            output
        );
        assert!(b["nextOffset"].is_null());
        assert!(project(&item, 1).is_err());
    }
    #[test]
    fn public_text_only_not_image_bytes_or_private_messages() {
        let v = json!({"type":"mcpToolCall","arguments":{"path":"a.png"},"result":{"content":[{"type":"image","data":"SECRET_BYTES"},{"type":"text","text":"Screenshot saved"}]}});
        let r = project(&v, 0).unwrap();
        assert!(r["output"].as_str().unwrap().contains("Screenshot saved"));
        assert!(!r.to_string().contains("SECRET_BYTES"));
        for kind in ["reasoning", "userMessage", "agentMessage", "hookPrompt"] {
            assert!(project(&json!({"type":kind,"text":"private"}), 0).is_err());
        }
    }
    #[test]
    fn empty_output_and_failure_are_not_fabricated() {
        let r = project(&json!({"type":"commandExecution","command":"false","status":"completed","exitCode":1,"aggregatedOutput":""}),0).unwrap();
        assert_eq!(r["exitCode"], 1);
        assert_eq!(r["outputAvailable"], false);
    }
}
