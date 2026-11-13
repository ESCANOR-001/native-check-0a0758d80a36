//! Provider events are projected without exposing system prompts, private
//! thinking blocks, or raw tool output. Full command/input details remain usable.
use crate::{
    auth,
    chat::{AgentKind, Chat},
};
use serde_json::{json, Value};
pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
pub fn now() -> String {
    auth::timestamp(auth::now())
}
/// Native item clocks are authoritative. Receipt time is only a live fallback.
pub(crate) fn native_time(row: &mut Value, start: Option<i64>, end: Option<i64>) {
    if let Some(at) = start.filter(|v| *v > 0) {
        row["createdAt"] = json!(auth::timestamp(at));
        row.as_object_mut().unwrap().remove("sequence");
        if row.get("updatedAt").is_some() {
            row["updatedAt"] = json!(auth::timestamp(end.filter(|v| *v >= at).unwrap_or(at)));
        }
    }
}

fn codex_command(chat: &mut Chat, item: &Value, status: &str) {
    let id = s(&item["id"]);
    let actions = item["commandActions"].as_array();
    // Do not guess from shell text. Codex already classifies read/search/list
    // actions; retaining that classification keeps them out of generic-command
    // filtering in completed Work. Mixed arbitrary commands remain commands.
    let semantic = actions.is_some_and(|a| {
        !a.is_empty()
            && a.len() <= 128
            && a.iter()
                .all(|v| matches!(s(&v["type"]), "read" | "search" | "listFiles"))
    });
    if !semantic {
        tool(
            chat,
            id,
            "Bash",
            &json!({"command":item["command"],"cwd":item["cwd"]}),
            status,
        );
        return;
    }
    let actions = actions.unwrap();
    let reads_only = actions.iter().all(|v| v["type"] == "read");
    let mut names = Vec::new();
    let mut paths = Vec::new();
    for action in actions {
        let path = s(&action["path"]);
        if action["type"] == "read" && !path.is_empty() && !paths.iter().any(|p| p == path) {
            paths.push(bounded(path, 4096));
        }
        let name = match s(&action["type"]) {
            "read" => format!(
                "Read {}",
                if s(&action["name"]).is_empty() {
                    basename(path)
                } else {
                    s(&action["name"])
                }
            ),
            "search" => format!("Search {}", s(&action["query"])),
            _ => format!("List {}", path),
        };
        if !names.contains(&name) {
            names.push(bounded(&name, 240));
        }
    }
    let title = bounded(&names.join("; "), 240);
    activity(
        chat,
        id,
        if status == "running" {
            "tool.started"
        } else {
            "tool.completed"
        },
        &title,
        if status == "failed" { "error" } else { "tool" },
        json!({"itemId":id,"itemType":if reads_only{"file-read"}else{"codebase-search"},
            "requestKind":if reads_only{"file-read"}else{"codebase-search"},"title":title,"status":status,
            "readFiles":paths,"fileNames":paths,
            "data":{"providerName":"codex","toolCallId":id,"inputDetail":bounded(s(&item["command"]),64_000)}}),
    );
}
pub fn message(chat: &mut Chat, id: &str, text: &str, role: &str, delta: bool, streaming: bool) {
    let t = &mut chat.thread;
    let turn = t["latestTurn"]["turnId"].clone();
    let rows = t["messages"].as_array_mut().unwrap();
    if let Some(row) = rows.iter_mut().find(|r| r["id"] == id) {
        let next = if delta {
            format!("{}{}", s(&row["text"]), text)
        } else {
            text.to_owned()
        };
        row["text"] = json!(next);
        row["streaming"] = json!(streaming);
        row["updatedAt"] = json!(now());
    } else {
        rows.push(json!({"id":id,"role":role,"text":text,"turnId":turn,"streaming":streaming,"phase":if role=="assistant" {"final_answer"} else {"user"},"createdAt":now(),"updatedAt":now()}));
    }
}
pub fn activity(chat: &mut Chat, id: &str, kind: &str, title: &str, tone: &str, payload: Value) {
    let turn = chat.thread["latestTurn"]["turnId"].clone();
    let rows = chat.thread["activities"].as_array_mut().unwrap();
    let mut row = json!({"id":id,"kind":kind,"tone":tone,"summary":title,"payload":payload,"turnId":turn,"createdAt":now()});
    if let Some(old) = rows.iter_mut().find(|r| r["id"] == id) {
        row["createdAt"] = old["createdAt"].clone();
        if let Some(sequence) = old.get("sequence") {
            row["sequence"] = sequence.clone();
        }
        // Native replay may include only tool metadata. Keep verified image
        // references until a new authoritative image result replaces them.
        if row["payload"]["data"]["toolCallId"] == id
            && old["payload"]["data"]["toolCallId"] == id
            && row["payload"]["data"]["toolImages"].is_null()
        {
            if let Some(paths) = old["payload"]["data"]["toolImages"].as_array() {
                row["payload"]["data"]["toolImages"] = json!(paths);
            }
        }
        *old = row;
    } else {
        rows.push(row);
    }
}

/// Adapt copied tool images to Android's existing Codex gallery contract.
/// Keep the real tool row and details; this companion is not another tool call.
pub fn tool_image_gallery(chat: &mut Chat, tool_id: &str) {
    use sha2::{Digest, Sha256};
    if chat.agent != AgentKind::Codex {
        return;
    }
    let Some(tool) = chat.thread["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == tool_id)
        .cloned()
    else {
        return;
    };
    let Some(paths) = tool["payload"]["data"]["toolImages"]
        .as_array()
        .filter(|a| !a.is_empty())
    else {
        return;
    };
    // Image generation is already a gallery row. Keep its native ID, status
    // and position instead of adding a second synthetic generation activity.
    if tool["payload"]["itemType"] == "image-generation" {
        let row = chat.thread["activities"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == tool_id)
            .unwrap();
        row["payload"]["data"]["generatedImages"] = json!(paths);
        return;
    }
    let id = format!("tool-images-{:x}", Sha256::digest(tool_id.as_bytes()));
    activity(
        chat,
        &id,
        "tool.completed",
        "Tool images",
        "tool",
        json!({"itemId":id,"itemType":"image-generation","status":"completed","title":"Tool images","data":{"providerName":"codex","sourceToolCallId":tool_id,"generatedImages":paths}}),
    );
    let row = chat.thread["activities"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == id)
        .unwrap();
    row["createdAt"] = tool["createdAt"].clone();
    row["turnId"] = tool["turnId"].clone();
}

pub fn bounded(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
/// Show what the user is approving, not the CLI's routing envelope.
pub fn approval_detail(request: &Value, codex: bool) -> String {
    let input = if codex {
        &request["params"]
    } else {
        &request["request"]["input"]
    };
    let mut parts = Vec::new();
    for key in ["reason", "command", "file_path", "path", "grantRoot"] {
        if let Some(value) = input[key].as_str().filter(|s| !s.is_empty()) {
            parts.push(bounded(value, 16_000));
        }
    }
    if let Some(cwd) = input["cwd"].as_str().filter(|s| !s.is_empty()) {
        parts.push(format!("Folder: {}", bounded(cwd, 4096)));
    }
    if parts.is_empty() {
        if codex {
            parts.push("Review the requested action before approving.".into());
        } else {
            parts.push(safe_input(input, 0, &mut 16_000).to_string());
        }
    }
    bounded(&parts.join("\n\n"), 16_000)
}
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
fn safe_input(v: &Value, depth: usize, budget: &mut usize) -> Value {
    if depth > 4 || *budget < 8 {
        return Value::Null;
    }
    *budget -= 8;
    match v {
        Value::String(text) => {
            let text = bounded(text, (*budget).min(16_000));
            *budget = budget.saturating_sub(text.len());
            json!(text)
        }
        Value::Array(rows) => {
            let mut result = vec![];
            for row in rows.iter().take(32) {
                if *budget < 8 {
                    break;
                }
                result.push(safe_input(row, depth + 1, budget));
            }
            json!(result)
        }
        Value::Object(rows) => {
            let mut result = serde_json::Map::new();
            for (key, value) in rows
                .iter()
                .filter(|(k, _)| {
                    !["content", "source", "data", "base64", "blob", "signature"]
                        .contains(&k.as_str())
                })
                .take(32)
            {
                if *budget < 16 {
                    break;
                }
                let key = bounded(key, (*budget).min(128));
                *budget = budget.saturating_sub(key.len());
                result.insert(key, safe_input(value, depth + 1, budget));
            }
            Value::Object(result)
        }
        _ => v.clone(),
    }
}
pub fn tool(chat: &mut Chat, id: &str, name: &str, input: &Value, status: &str) {
    if id.is_empty() {
        return;
    }
    let command = s(&input["command"]);
    let path = s(&input["file_path"]);
    let clean = safe_input(input, 0, &mut 32_000);
    let detail = match name {
        "Bash" => command.to_owned(),
        "Read" | "Write" | "Edit" | "MultiEdit" => path.to_owned(),
        "Grep" | "Glob" => [s(&input["pattern"]), s(&input["path"])].join("\n"),
        "ToolSearch" | "WebSearch" => s(&input["query"]).to_owned(),
        "WebFetch" => s(&input["url"]).to_owned(),
        "AskUserQuestion" => input["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .map(|q| bounded(s(&q["question"]), 1000))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => clean.to_string(),
    };
    let title = if name == "AskUserQuestion" {
        bounded(
            detail
                .lines()
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("Answer a question"),
            100,
        )
    } else if name == "Bash" {
        bounded(command.lines().next().unwrap_or("Command"), 100)
    } else if !path.is_empty() {
        format!("{name} {}", basename(path))
    } else {
        bounded(name, 100)
    };
    let kind = match name {
        "Bash" => "command_execution",
        "Read" => "file-read",
        "Grep" | "Glob" => "codebase-search",
        "WebSearch" => "web_search",
        _ => "dynamic_tool_call",
    };
    let mut payload = json!({"itemId":id,"itemType":kind,"title":title,"status":status,"detail":bounded(&detail,16_000),
        "data":{"providerName":chat.agent.instance(),"tool":name,"command":bounded(command,64_000),"input":clean,"inputDetail":bounded(&detail,16_000),"toolCallId":id}});
    if name == "Read" {
        payload["readFiles"] = json!([path]);
        payload["fileNames"] = json!([path]);
        payload["requestKind"] = json!("file-read");
    }
    if detail.len() > 16_000 {
        payload["inputTruncated"] = json!(true);
    }
    activity(
        chat,
        id,
        if status == "running" {
            "tool.started"
        } else {
            "tool.completed"
        },
        &title,
        if status == "failed" { "error" } else { "tool" },
        payload,
    );
}
fn public_summary_model(model: &str) -> bool {
    ["claude-opus-", "claude-sonnet-", "claude-haiku-"]
        .iter()
        .any(|p| {
            model
                .strip_prefix(p)
                .and_then(|v| v.chars().next())
                .is_some_and(|v| ('4'..='9').contains(&v))
        })
}
fn thinking(chat: &mut Chat, id: &str, text: &str, status: &str, delta: bool) {
    let old = chat.thread["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .map(|v| s(&v["payload"]["detail"]))
        .unwrap_or("");
    let text = if delta {
        bounded(&format!("{old}{text}"), 16_000)
    } else {
        bounded(text, 16_000)
    };
    if text.trim().is_empty() {
        return;
    }
    activity(
        chat,
        id,
        "claude.thinking-summary",
        "Thinking",
        "thinking",
        json!({"status":status,"detail":text,"data":{"providerName":"claude-code"}}),
    );
}
fn file_change(result: &Value) -> Option<Value> {
    if result["staged"] == true || s(&result["filePath"]).is_empty() {
        return None;
    }
    let path = s(&result["filePath"]);
    let mut hunks = result["structuredPatch"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if hunks.is_empty() && result["type"] == "create" {
        if let Some(content) = result["content"].as_str() {
            let lines: Vec<_> = content.lines().map(|l| format!("+{l}")).collect();
            hunks.push(json!({"oldStart":0,"oldLines":0,"newStart":if lines.is_empty(){0}else{1},"newLines":lines.len(),"lines":lines}));
        }
    }
    let mut diff = format!(
        "--- {}\n+++ {}\n",
        if result["type"] == "create" {
            "/dev/null".to_owned()
        } else {
            format!("a/{path}")
        },
        format_args!("b/{path}")
    );
    let (mut added, mut removed, mut valid, mut truncated) = (0, 0, 0, false);
    for h in hunks.iter().take(256) {
        if !["oldStart", "oldLines", "newStart", "newLines"]
            .iter()
            .all(|k| h[k].as_u64().is_some())
        {
            continue;
        }
        let Some(lines) = h["lines"].as_array() else {
            continue;
        };
        if !lines.iter().all(|l| {
            l.as_str()
                .is_some_and(|s| s.starts_with(['+', '-', ' ']) || s.starts_with("\\ No newline"))
        }) {
            continue;
        }
        valid += 1;
        let header = format!(
            "@@ -{},{} +{},{} @@\n",
            h["oldStart"], h["oldLines"], h["newStart"], h["newLines"]
        );
        if diff.len() + header.len() <= 64_000 {
            diff.push_str(&header);
        } else {
            truncated = true;
        }
        for l in lines {
            let line = s(l);
            if line.starts_with('+') {
                added += 1;
            }
            if line.starts_with('-') {
                removed += 1;
            }
            if !truncated && diff.len() + line.len() < 64_000 {
                diff.push_str(line);
                diff.push('\n');
            } else {
                truncated = true;
            }
        }
    }
    let mut change = json!({"path":path,"additions":added,"deletions":removed});
    if valid > 0 {
        change["diff"] = json!(diff);
        change["diffTruncated"] = json!(truncated || hunks.len() > 256);
    }
    Some(change)
}
fn proposed_plan(chat: &mut Chat, id: &str, text: &str, delta: bool, completed: bool) {
    if id.is_empty() || text.is_empty() {
        return;
    }
    let turn = chat.thread["latestTurn"]["turnId"].clone();
    let rows = chat.thread["proposedPlans"].as_array_mut().unwrap();
    if let Some(row) = rows.iter_mut().find(|p| p["id"] == id) {
        row["planMarkdown"] = json!(if delta {
            bounded(&format!("{}{}", s(&row["planMarkdown"]), text), 64_000)
        } else {
            bounded(text, 64_000)
        });
        row["updatedAt"] = json!(now());
    } else {
        rows.push(json!({"id":id,"turnId":turn,"planMarkdown":bounded(text,64_000),"implementedAt":null,"implementationThreadId":null,"createdAt":now(),"updatedAt":now()}));
    }
    if completed {
        if let Some(row) = rows.iter_mut().find(|p| p["id"] == id) {
            row["completed"] = json!(true);
        }
        chat.thread["hasActionableProposedPlan"] = json!(true);
    }
}
fn compaction(chat: &mut Chat, id: &str, running: bool) {
    if running {
        chat.compaction_turn = chat.thread["latestTurn"]["turnId"]
            .as_str()
            .map(str::to_owned);
    } else {
        chat.compaction_turn = None;
    }
    activity(
        chat,
        id,
        "context-compaction",
        if running {
            "Compacting context"
        } else {
            "Context compacted"
        },
        "info",
        json!({"itemId":id,"itemType":"context-compaction","status":if running {"inProgress"}else{"completed"}}),
    );
}
pub fn project(chat: &mut Chat, v: &Value) {
    match chat.agent {
        AgentKind::Codex => {
            let p = &v["params"];
            match s(&v["method"]) {
                "error" => {
                    crate::codex_error::project(chat, &p["error"], p["willRetry"] == true, None)
                }
                "turn/completed" => {
                    let error = crate::codex_error::terminal_error(&p["turn"]);
                    crate::codex_error::project(chat, &error, false, None);
                }
                "item/plan/delta" => {
                    proposed_plan(chat, s(&p["itemId"]), s(&p["delta"]), true, false)
                }
                "item/agentMessage/delta" => message(
                    chat,
                    s(&p["itemId"]),
                    s(&p["delta"]),
                    "assistant",
                    true,
                    true,
                ),
                "item/reasoning/summaryTextDelta" => {
                    let id = s(&p["itemId"]);
                    let previous = chat.thread["activities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|a| a["id"] == id)
                        .map(|a| s(&a["payload"]["detail"]))
                        .unwrap_or("");
                    let text = bounded(&format!("{previous}{}", s(&p["delta"])), 16_000);
                    activity(
                        chat,
                        id,
                        "task.progress",
                        &bounded(&text, 240),
                        "info",
                        json!({"itemId":id,"detail":text,"summaryParts":[text],"summaryAvailable":true,"status":"running"}),
                    );
                }
                "item/started" | "item/completed" => {
                    let i = &p["item"];
                    if crate::desktop_questions::project(chat, i) {
                        return;
                    }
                    let id = s(&i["id"]);
                    let completed = v["method"] == "item/completed";
                    let status = if i["status"] == "failed" {
                        "failed"
                    } else if completed {
                        "completed"
                    } else {
                        "running"
                    };
                    match s(&i["type"]) {
                        "contextCompaction" => compaction(chat, id, !completed),
                        "plan" => proposed_plan(chat, id, s(&i["text"]), false, completed),
                        "agentMessage" => {
                            message(chat, id, s(&i["text"]), "assistant", false, !completed);
                            if let Some(phase) = i["phase"]
                                .as_str()
                                .filter(|p| matches!(*p, "commentary" | "final_answer"))
                            {
                                if let Some(row) = chat.thread["messages"]
                                    .as_array_mut()
                                    .unwrap()
                                    .iter_mut()
                                    .find(|r| r["id"] == id)
                                {
                                    row["phase"] = json!(phase);
                                }
                            }
                        }
                        "reasoning" => {
                            let text = i["summary"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default();
                            if !text.is_empty() {
                                activity(
                                    chat,
                                    id,
                                    "task.progress",
                                    &bounded(&text, 240),
                                    "info",
                                    json!({"itemId":id,"detail":bounded(&text,16_000),"summaryParts":[bounded(&text,16_000)],"summaryAvailable":true,"status":status}),
                                );
                            }
                        }
                        "commandExecution" => codex_command(chat, i, status),
                        "fileChange" => {
                            let mut remaining = 96_000usize;
                            let changes:Vec<_>=i["changes"].as_array().into_iter().flatten().take(64).map(|c|{
                                let original=s(&c["diff"]);
                                let diff=bounded(original,remaining.min(64_000));remaining=remaining.saturating_sub(diff.len());
                                let additions=original.lines().filter(|l|l.starts_with('+')&&!l.starts_with("+++")).count();
                                let deletions=original.lines().filter(|l|l.starts_with('-')&&!l.starts_with("---")).count();
                                json!({"path":bounded(s(&c["path"]),4096),"diff":diff,"additions":additions,"deletions":deletions,"diffTruncated":original.len()>diff.len()})
                            }).collect();
                            activity(
                                chat,
                                id,
                                "fileChange",
                                "File Change",
                                if status == "failed" { "error" } else { "tool" },
                                json!({"itemId":id,"itemType":"file_change","status":status,"fileChanges":changes,"data":{"providerName":"codex","toolCallId":id}}),
                            );
                        }
                        "imageView" => {
                            tool(chat, id, "Read", &json!({"file_path":i["path"]}), status);
                        }
                        "imageGeneration" => {
                            activity(
                                chat,
                                id,
                                if completed {
                                    "tool.completed"
                                } else {
                                    "tool.started"
                                },
                                "Image generation",
                                if status == "failed" { "error" } else { "tool" },
                                json!({"itemId":id,"itemType":"image-generation","status":status,
                                    "title":"Image generation","data":{"providerName":"codex","toolCallId":id}}),
                            );
                        }
                        "sleep" => {
                            let duration = i["durationMs"].as_u64();
                            activity(
                                chat,
                                id,
                                if completed {
                                    "tool.completed"
                                } else {
                                    "tool.started"
                                },
                                "Wait",
                                "info",
                                json!({"itemId":id,"itemType":"sleep","title":"Wait","status":status,
                                    "detail":duration.map(|n|format!("{n} ms")),
                                    "data":{"type":"sleep","durationMs":duration,"providerName":"codex","toolCallId":id}}),
                            );
                        }
                        "mcpToolCall" | "dynamicToolCall" => {
                            tool(chat, id, s(&i["tool"]), &i["arguments"], status)
                        }
                        "webSearch" => {
                            tool(chat, id, "WebSearch", &json!({"query":i["query"]}), status)
                        }
                        _ => {}
                    }
                    for kind in ["messages", "activities", "proposedPlans"] {
                        if let Some(row) = chat.thread[kind]
                            .as_array_mut()
                            .unwrap()
                            .iter_mut()
                            .find(|r| r["id"] == id)
                        {
                            native_time(
                                row,
                                p["startedAtMs"].as_i64(),
                                p["completedAtMs"].as_i64(),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        AgentKind::Claude => {
            // Parent output excludes nested agents and internal/user prompt echoes.
            if !v["parent_tool_use_id"].is_null() || crate::claude_turns::no_response_placeholder(v)
            {
                return;
            }
            if v["type"] == "system" && v["subtype"] == "status" && v["status"] == "compacting" {
                let id = format!("compaction-{}", s(&chat.thread["latestTurn"]["turnId"]));
                compaction(chat, &id, true);
            }
            if v["type"] == "system"
                && (v["subtype"] == "compact_boundary"
                    || v["subtype"] == "status"
                        && v.get("status") == Some(&Value::Null)
                        && chat.compaction_turn.is_some())
            {
                let id = format!("compaction-{}", s(&chat.thread["latestTurn"]["turnId"]));
                compaction(chat, &id, false);
            }
            if v["type"] == "stream_event" {
                let e = &v["event"];
                if e["type"] == "message_start" {
                    chat.stream_id = s(&e["message"]["id"]).into();
                    chat.stream_model = s(&e["message"]["model"]).into();
                }
                let tid = format!(
                    "claude-thinking-{}-{}",
                    chat.stream_id,
                    e["index"].as_u64().unwrap_or(0)
                );
                if e["type"] == "content_block_delta" && !chat.stream_id.is_empty() {
                    if e["delta"]["type"] == "text_delta" {
                        let id = chat.stream_id.clone();
                        message(chat, &id, s(&e["delta"]["text"]), "assistant", true, true);
                    }
                    if e["delta"]["type"] == "thinking_delta"
                        && public_summary_model(&chat.stream_model)
                    {
                        thinking(chat, &tid, s(&e["delta"]["thinking"]), "running", true);
                    }
                }
                if e["type"] == "content_block_stop" {
                    if let Some(a) = chat.thread["activities"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|a| a["id"] == tid)
                    {
                        a["payload"]["status"] = json!("completed");
                    }
                }
            }
            if v["type"] == "assistant" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    let text = blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .map(|b| s(&b["text"]))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        message(
                            chat,
                            s(&v["message"]["id"]),
                            &text,
                            "assistant",
                            false,
                            false,
                        );
                    }
                    for (index, b) in blocks.iter().enumerate() {
                        if b["type"] == "tool_use" {
                            tool(chat, s(&b["id"]), s(&b["name"]), &b["input"], "running");
                        }
                        if b["type"] == "thinking"
                            && public_summary_model(s(&v["message"]["model"]))
                        {
                            thinking(
                                chat,
                                &format!("claude-thinking-{}-{index}", s(&v["message"]["id"])),
                                s(&b["thinking"]),
                                "completed",
                                false,
                            );
                        }
                    }
                }
            }
            if v["type"] == "user" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    let result_count = blocks.iter().filter(|b| b["type"] == "tool_result").count();
                    for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                        if let Some(row) = chat.thread["activities"]
                            .as_array_mut()
                            .unwrap()
                            .iter_mut()
                            .find(|r| r["id"] == b["tool_use_id"])
                        {
                            if result_count == 1 && v["tool_use_result"]["detachedToolCall"] == true
                            {
                                continue;
                            }
                            row["kind"] = json!("tool.completed");
                            row["payload"]["status"] = json!(if b["is_error"] == true {
                                "failed"
                            } else {
                                "completed"
                            });
                            if b["is_error"] == true {
                                row["tone"] = json!("error");
                            }
                            if b["is_error"] != true
                                && result_count == 1
                                && ["Write", "Edit", "MultiEdit"]
                                    .contains(&s(&row["payload"]["data"]["tool"]))
                            {
                                if let Some(change) = file_change(&v["tool_use_result"]) {
                                    row["payload"]["itemType"] = json!("file_change");
                                    row["payload"]["fileChanges"] = json!([change]);
                                }
                            }
                            // No stdout, file contents or image bytes enter tool detail.
                        }
                    }
                }
            }
            if v["type"] == "tool_progress" {
                if let Some(row) = chat.thread["activities"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|r| r["id"] == v["tool_use_id"])
                {
                    row["payload"]["elapsedSeconds"] = v["elapsed_time_seconds"].clone();
                }
            }
            if v["type"] == "tool_use_summary" && !s(&v["summary"]).is_empty() {
                activity(
                    chat,
                    s(&v["uuid"]),
                    "claude.tool-summary",
                    &bounded(s(&v["summary"]), 240),
                    "thinking",
                    json!({"status":"completed","detail":bounded(s(&v["summary"]),2000),"data":{"providerName":"claude-code"}}),
                );
            }
        }
    }
}

/// Claude represents a user-canceled tool with an error result. Only relabel
/// that explicit result when this worker was stopped; unrelated failures stay.
pub fn interrupted_tools(chat: &mut Chat, event: &Value, stopping: bool) {
    if !stopping || chat.agent != AgentKind::Claude || event["type"] != "user" {
        return;
    }
    for result in event["message"]["content"].as_array().into_iter().flatten() {
        if result["type"] != "tool_result" || result["is_error"] != true {
            continue;
        }
        let interrupted = |text: &str| {
            text.trim() == "[Request interrupted by user for tool use]"
                || (text.starts_with("The user doesn't want to proceed with this tool use. The tool use was rejected")
                    && text.contains("STOP what you are doing and wait for the user to tell you how to proceed."))
        };
        let content = &result["content"];
        let canceled = content.as_str().is_some_and(interrupted)
            || content.as_array().is_some_and(|blocks| {
                blocks.len() == 1
                    && blocks[0]["type"] == "text"
                    && blocks[0]["text"].as_str().is_some_and(interrupted)
            });
        if canceled {
            if let Some(row) = chat.thread["activities"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|row| row["id"] == result["tool_use_id"])
            {
                row["payload"]["status"] = json!("stopped");
                row["tone"] = json!("info");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_retries_and_terminal_errors_keep_exact_text_and_commentary_phase() {
        let mut c = chat(AgentKind::Codex);
        c.thread["session"] = json!({"status":"running"});
        let error = json!({"message":"Error running remote compact task: {\"detail\":\"Bad Request\"}","codexErrorInfo":"badRequest"});
        project(
            &mut c,
            &json!({"method":"item/started","params":{"item":{"id":"progress","type":"agentMessage","phase":"commentary","text":"Checking"}}}),
        );
        project(
            &mut c,
            &json!({"method":"item/agentMessage/delta","params":{"itemId":"progress","delta":" more"}}),
        );
        assert_eq!(c.thread["messages"][0]["phase"], "commentary");
        for _ in 0..2 {
            project(
                &mut c,
                &json!({"method":"error","params":{"error":error,"willRetry":true}}),
            );
        }
        assert_eq!(c.thread["session"]["status"], "running");
        assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
        assert_eq!(c.thread["activities"][0]["payload"]["willRetry"], true);
        project(
            &mut c,
            &json!({"method":"turn/completed","params":{"turn":{"status":"failed","error":error}}}),
        );
        assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
        assert_eq!(c.thread["activities"][0]["payload"]["willRetry"], false);
        assert_eq!(
            c.thread["activities"][0]["payload"]["detail"],
            error["message"]
        );
        assert_eq!(
            c.thread["messages"].as_array().unwrap().len(),
            1,
            "never fabricate a final answer"
        );
    }
    fn chat(agent: AgentKind) -> Chat {
        Chat {
            agent,
            thread: json!({"id":"projection","latestTurn":{"turnId":"turn"},"messages":[],"activities":[],"proposedPlans":[]}),
            native_id: String::new(),
            resumed: false,
            claude_plan_access: None,
            claude_permission_rules: vec![],
            remote_identity: None,
            external_claude: false,
            local_claude: None,
            claude_parked: false,
            external_codex: false,
            discovered_codex_title: None,
            desktop_read_revision: None,
            external_stop: None,
            native_control: None,
            fork_from: None,
            sync_replacement: None,
            queue: vec![],
            paused: false,
            compaction_turn: None,
            disabled_mcp: vec![],
            stream_id: String::new(),
            stream_model: String::new(),
            pending_message: Value::Null,
        }
    }
    #[test]
    fn claude_synthetic_resume_placeholder_is_hidden_but_real_replies_and_errors_remain() {
        let mut c = chat(AgentKind::Claude);
        let mut frame = json!({"type":"assistant","message":{"id":"filler","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}});
        project(&mut c, &frame);
        assert!(c.thread["messages"].as_array().unwrap().is_empty());
        frame["message"]["id"] = json!("real-reply");
        frame["message"]["model"] = json!("claude-opus-5-5");
        project(&mut c, &frame);
        frame["message"]["id"] = json!("api-error");
        frame["message"]["model"] = json!("<synthetic>");
        frame["isApiErrorMessage"] = json!(true);
        frame["message"]["content"][0]["text"] = json!("Bad Request");
        project(&mut c, &frame);
        assert_eq!(c.thread["messages"].as_array().unwrap().len(), 2);
        assert_eq!(c.thread["messages"][0]["text"], "No response requested.");
        assert_eq!(c.thread["messages"][1]["text"], "Bad Request");
    }
    #[test]
    fn native_reads_and_searches_keep_their_semantics_and_identity() {
        let mut c = chat(AgentKind::Codex);
        let read = json!({"id":"read","type":"commandExecution","command":"cat a.txt; head b.txt","commandActions":[{"type":"read","name":"a.txt","path":"/tmp/a.txt"},{"type":"read","name":"b.txt","path":"/tmp/b.txt"}],"aggregatedOutput":"PRIVATE_OUTPUT"});
        for method in ["item/started", "item/completed"] {
            project(
                &mut c,
                &json!({"method":method,"params":{"item":read,"startedAtMs":1700000001000_i64,"completedAtMs":1700000002000_i64}}),
            );
        }
        let row = &c.thread["activities"][0];
        assert_eq!(row["id"], "read");
        assert_eq!(row["summary"], "Read a.txt; Read b.txt");
        assert_eq!(row["payload"]["itemType"], "file-read");
        assert_eq!(
            row["payload"]["readFiles"],
            json!(["/tmp/a.txt", "/tmp/b.txt"])
        );
        assert!(row["payload"]["data"]["command"].is_null());
        assert_eq!(row["createdAt"], auth::timestamp(1700000001000));
        assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
        assert!(!c.thread.to_string().contains("PRIVATE_OUTPUT"));
        let search = json!({"id":"search","type":"commandExecution","command":"rg needle src; cat a.txt","commandActions":[{"type":"search","query":"needle","path":"src"},{"type":"read","name":"a.txt","path":"/tmp/a.txt"}]});
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":search}}),
        );
        assert_eq!(
            c.thread["activities"][1]["payload"]["itemType"],
            "codebase-search"
        );
        let mixed = json!({"id":"mixed","type":"commandExecution","command":"cat a.txt; npm test","commandActions":[{"type":"read","path":"a.txt"},{"type":"unknown","command":"npm test"}]});
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":mixed}}),
        );
        assert_eq!(
            c.thread["activities"][2]["payload"]["itemType"],
            "command_execution"
        );
    }

    #[test]
    fn native_item_clock_corrects_late_live_receipts_without_moving_on_completion() {
        let mut c = chat(AgentKind::Codex);
        project(
            &mut c,
            &json!({"method":"item/started","params":{"item":{"id":"tool","type":"commandExecution","command":"pwd"}}}),
        );
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"tool","type":"commandExecution","command":"pwd"},"startedAtMs":1700000001000_i64,"completedAtMs":1700000010000_i64}}),
        );
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"progress","type":"agentMessage","text":"Public progress"},"startedAtMs":1700000002000_i64,"completedAtMs":1700000003000_i64}}),
        );
        assert_eq!(
            c.thread["activities"][0]["createdAt"],
            auth::timestamp(1700000001000)
        );
        assert_eq!(
            c.thread["messages"][0]["createdAt"],
            auth::timestamp(1700000002000)
        );
        assert_eq!(
            c.thread["messages"][0]["updatedAt"],
            auth::timestamp(1700000003000)
        );
        let mut row = c.thread["messages"][0].clone();
        native_time(&mut row, None, None);
        assert_eq!(row, c.thread["messages"][0]);
    }

    #[test]
    fn sparse_codex_tool_replay_keeps_verified_preview_references() {
        let mut c = chat(AgentKind::Codex);
        let event = json!({"method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"image-tool","tool":"screenshot","arguments":{}}}});
        project(&mut c, &event);
        c.thread["activities"][0]["payload"]["data"]["toolImages"] =
            json!(["/private/verified.png"]);
        project(&mut c, &event);
        assert_eq!(
            c.thread["activities"][0]["payload"]["data"]["toolImages"],
            json!(["/private/verified.png"])
        );
        assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn invocation_details_survive_completion_and_errors_without_raw_output() {
        let mut c = chat(AgentKind::Claude);
        project(
            &mut c,
            &json!({"type":"assistant","message":{"id":"a","content":[{"type":"tool_use","id":"bash","name":"Bash","input":{"command":"rg hello src\nprintf done","description":"Search"}},{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/test.txt"}}]}}),
        );
        project(
            &mut c,
            &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"bash","content":"DO_NOT_RENDER_STDOUT","is_error":true}]}}),
        );
        let rows = c.thread["activities"].as_array().unwrap();
        assert_eq!(rows[0]["summary"], "rg hello src");
        assert_eq!(
            rows[0]["payload"]["data"]["inputDetail"],
            "rg hello src\nprintf done"
        );
        assert_eq!(rows[0]["payload"]["status"], "failed");
        assert_eq!(rows[1]["payload"]["itemType"], "file-read");
        assert_eq!(rows[1]["summary"], "Read test.txt");
        assert!(!c.thread.to_string().contains("DO_NOT_RENDER_STDOUT"));
    }
    #[test]
    fn stopping_only_relabels_explicit_user_interruption_results() {
        let mut c = chat(AgentKind::Claude);
        for id in ["failed", "stopped", "other-error"] {
            tool(&mut c, id, "AskUserQuestion", &json!({}), "running");
        }
        let failed = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"failed","is_error":true,"content":"failure"}]}});
        project(&mut c, &failed);
        let canceled = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"stopped","is_error":true,"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},{"type":"tool_result","tool_use_id":"other-error","is_error":true,"content":"unrelated failure"}]}});
        project(&mut c, &canceled);
        interrupted_tools(&mut c, &canceled, false);
        assert_eq!(c.thread["activities"][1]["payload"]["status"], "failed");
        interrupted_tools(&mut c, &canceled, true);
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "failed");
        assert_eq!(c.thread["activities"][1]["payload"]["status"], "stopped");
        assert_eq!(c.thread["activities"][2]["payload"]["status"], "failed");
        let current_cli = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"other-error","is_error":true,"content":"The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.\n\nNote: A later message may explain the correction."}]}});
        interrupted_tools(&mut c, &current_cli, true);
        assert_eq!(c.thread["activities"][2]["payload"]["status"], "stopped");
    }
    #[test]
    fn only_public_model_summaries_are_projected_and_reconciled() {
        let mut c = chat(AgentKind::Claude);
        project(
            &mut c,
            &json!({"type":"assistant","message":{"id":"old","model":"unknown","content":[{"type":"thinking","thinking":"PRIVATE"}]}}),
        );
        assert!(c.thread["activities"].as_array().unwrap().is_empty());
        project(
            &mut c,
            &json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5"}}}),
        );
        project(
            &mut c,
            &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I will inspect"}}}),
        );
        project(
            &mut c,
            &json!({"type":"assistant","message":{"id":"m","model":"claude-sonnet-4-5","content":[{"type":"thinking","thinking":"I will inspect the files."},{"type":"text","text":"Done"}]}}),
        );
        assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
        assert_eq!(
            c.thread["activities"][0]["payload"]["detail"],
            "I will inspect the files."
        );
        assert_eq!(c.thread["activities"][0]["payload"]["status"], "completed");
        let before = c.thread.clone();
        project(
            &mut c,
            &json!({"type":"assistant","parent_tool_use_id":"subagent","message":{"id":"nested","content":[{"type":"text","text":"Nested private response"}]}}),
        );
        assert_eq!(c.thread, before);
        assert!(!c.thread.to_string().contains("PRIVATE"));
    }
    #[test]
    fn file_diff_requires_successful_authoritative_result() {
        let mut c = chat(AgentKind::Claude);
        let start = json!({"type":"assistant","message":{"id":"a","content":[{"type":"tool_use","id":"write","name":"Write","input":{"file_path":"test.txt","content":"not yet written"}}]}});
        project(&mut c, &start);
        assert_ne!(
            c.thread["activities"][0]["payload"]["itemType"],
            "file_change"
        );
        assert!(!c.thread.to_string().contains("not yet written"));
        project(
            &mut c,
            &json!({"type":"user","tool_use_result":{"type":"create","filePath":"test.txt","content":"one\ntwo\n"},"message":{"content":[{"type":"tool_result","tool_use_id":"write","content":"RAW_OUTPUT"}]}}),
        );
        let change = &c.thread["activities"][0]["payload"]["fileChanges"][0];
        assert_eq!(change["additions"], 2);
        assert!(s(&change["diff"]).contains("+one\n+two\n"));
        assert!(!c.thread.to_string().contains("RAW_OUTPUT"));
    }
    #[test]
    fn codex_summaries_and_failures_do_not_publish_private_content() {
        let mut c = chat(AgentKind::Codex);
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"reason","type":"reasoning","summary":["Checking files"],"content":["PRIVATE"]}}}),
        );
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"cmd","type":"commandExecution","command":"false","status":"failed","aggregatedOutput":"RAW_OUTPUT"}}}),
        );
        assert_eq!(
            c.thread["activities"][0]["payload"]["summaryParts"][0],
            "Checking files"
        );
        assert_eq!(c.thread["activities"][1]["payload"]["status"], "failed");
        assert!(!c.thread.to_string().contains("PRIVATE"));
        assert!(!c.thread.to_string().contains("RAW_OUTPUT"));
    }
    #[test]
    fn codex_wait_activity_survives_completion_and_replay() {
        let mut c = chat(AgentKind::Codex);
        for method in ["item/started", "item/completed", "item/completed"] {
            project(
                &mut c,
                &json!({"method":method,"params":{"item":{
                    "id":"wait-id","type":"sleep","durationMs":20000,"content":"PRIVATE"
                }}}),
            );
        }
        let rows = c.thread["activities"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "tool.completed");
        assert_eq!(rows[0]["payload"]["itemType"], "sleep");
        assert_eq!(rows[0]["payload"]["data"]["durationMs"], 20000);
        assert!(!c.thread.to_string().contains("PRIVATE"));
    }
    #[test]
    fn large_tool_inputs_and_many_file_diffs_have_a_shared_budget() {
        let mut c = chat(AgentKind::Codex);
        let fields: serde_json::Map<_, _> = (0..32)
            .map(|i| (format!("field{i}"), json!("x".repeat(16_000))))
            .collect();
        tool(
            &mut c,
            "large",
            "fixture",
            &Value::Object(fields),
            "completed",
        );
        assert!(c.thread["activities"][0].to_string().len() < 120_000);
        let changes: Vec<_> = (0..64)
            .map(|i| json!({"path":format!("file-{i}"),"diff":format!("+{}", "x".repeat(60_000))}))
            .collect();
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"diff","type":"fileChange","changes":changes}}}),
        );
        assert!(c.thread["activities"][1].to_string().len() < 160_000);
        assert_eq!(
            c.thread["activities"][1]["payload"]["fileChanges"][2]["diffTruncated"],
            true
        );
    }
    #[test]
    fn approval_displays_the_action_without_protocol_metadata_or_file_contents() {
        let request = json!({"id":32,"params":{"threadId":"private-thread","turnId":"private-turn","command":"printf ok","cwd":"/test/work","reason":"Run the check?","availableDecisions":["accept"],"aggregatedOutput":"PRIVATE_OUTPUT"}});
        assert_eq!(
            approval_detail(&request, true),
            "Run the check?\n\nprintf ok\n\nFolder: /test/work"
        );
        assert_eq!(
            approval_detail(
                &json!({"request":{"input":{"file_path":"/test/a.txt","content":"PRIVATE_CONTENT"}}}),
                false
            ),
            "/test/a.txt"
        );
        let large = approval_detail(&json!({"params":{"command":"🙂".repeat(20000)}}), true);
        assert!(large.len() <= 16000);
    }
    #[test]
    fn completed_plan_reconciles_streamed_text_and_is_actionable() {
        let mut c = chat(AgentKind::Codex);
        project(
            &mut c,
            &json!({"method":"item/plan/delta","params":{"itemId":"plan","delta":"Inspect"}}),
        );
        project(
            &mut c,
            &json!({"method":"item/completed","params":{"item":{"id":"plan","type":"plan","text":"Inspect files, then test."}}}),
        );
        assert_eq!(c.thread["proposedPlans"].as_array().unwrap().len(), 1);
        assert_eq!(
            c.thread["proposedPlans"][0]["planMarkdown"],
            "Inspect files, then test."
        );
        assert_eq!(c.thread["hasActionableProposedPlan"], true);
    }
    #[test]
    fn only_native_top_level_compaction_events_set_and_clear_the_phase() {
        for agent in [AgentKind::Codex, AgentKind::Claude] {
            let mut c = chat(agent);
            c.thread["latestTurn"] = json!({"turnId":"t","state":"running"});
            let start = if agent == AgentKind::Codex {
                json!({"method":"item/started","params":{"item":{"type":"contextCompaction","id":"c"}}})
            } else {
                json!({"type":"system","subtype":"status","status":"compacting"})
            };
            project(&mut c, &start);
            assert_eq!(c.compaction_turn.as_deref(), Some("t"));
            let end = if agent == AgentKind::Codex {
                json!({"method":"item/completed","params":{"item":{"type":"contextCompaction","id":"c"}}})
            } else {
                json!({"type":"system","subtype":"compact_boundary"})
            };
            project(&mut c, &end);
            assert_eq!(c.compaction_turn, None);
            assert_eq!(c.thread["activities"].as_array().unwrap().len(), 1);
            assert_eq!(c.thread["activities"][0]["payload"]["status"], "completed");
            if agent == AgentKind::Claude {
                let mut nested = start;
                nested["parent_tool_use_id"] = json!("nested");
                project(&mut c, &nested);
                assert_eq!(c.compaction_turn, None);
            }
        }
    }
}
