//! Canonical TurnItem -> installed app-server display item. Keep tool arguments
//! opaque; do not recursively rename user JSON or expose model context records.
use serde_json::{json, Value};
const ERROR: &str = "This saved Codex activity format is not supported by the history reader.";
fn camel(s: &str) -> String {
    let mut result = String::new();
    let mut upper = false;
    for (i, c) in s.chars().enumerate() {
        if c == '_' {
            upper = true;
        } else if upper {
            result.extend(c.to_uppercase());
            upper = false;
        } else if i == 0 {
            result.extend(c.to_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}
fn fields(item: &Value, kind: &str, names: &[&str]) -> Value {
    let mut value = json!({"type":kind,"id":item["id"]});
    for name in names {
        value[camel(name)] = item[*name].clone();
    }
    value
}
fn duration(item: &Value) -> Value {
    item["duration"]["secs"]
        .as_u64()
        .and_then(|s| s.checked_mul(1000))
        .and_then(|s| s.checked_add(item["duration"]["nanos"].as_u64().unwrap_or(0) / 1_000_000))
        .and_then(|s| i64::try_from(s).ok())
        .map_or(Value::Null, |s| json!(s))
}
fn status(value: &Value) -> Result<Value, String> {
    let s = camel(value.as_str().ok_or(ERROR)?);
    if !matches!(
        s.as_str(),
        "inProgress" | "completed" | "failed" | "declined"
    ) {
        return Err(ERROR.into());
    }
    Ok(json!(s))
}
pub(crate) fn convert(item: &Value) -> Result<Value, String> {
    if !item["id"].is_string() {
        return Err(ERROR.into());
    }
    let value = match item["type"].as_str().ok_or(ERROR)? {
        "UserMessage" => {
            let mut content = item["content"].as_array().ok_or(ERROR)?.clone();
            for input in &mut content {
                let kind = camel(input["type"].as_str().ok_or(ERROR)?);
                if !matches!(
                    kind.as_str(),
                    "text" | "image" | "localImage" | "audio" | "localAudio" | "skill" | "mention"
                ) {
                    return Err(ERROR.into());
                }
                input["type"] = json!(kind);
                if let Some(file) = input.as_object_mut().and_then(|v| v.remove("file_id")) {
                    input["fileId"] = file;
                }
            }
            json!({"type":"userMessage","id":item["id"],"clientId":item["client_id"],"content":content})
        }
        "AgentMessage" => {
            let mut text = String::new();
            for part in item["content"].as_array().ok_or(ERROR)? {
                if part["type"] != "Text" {
                    return Err(ERROR.into());
                }
                text.push_str(part["text"].as_str().ok_or(ERROR)?);
            }
            let mut value = fields(
                item,
                "agentMessage",
                &[
                    "phase",
                    "memory_citation",
                    "delivery",
                    "questions",
                    "moderation",
                ],
            );
            value["text"] = json!(text);
            value
        }
        "Reasoning" => {
            json!({"type":"reasoning","id":item["id"],"summary":item["summary_text"],"content":item["raw_content"]})
        }
        "Plan" => fields(item, "plan", &["text"]),
        "FunctionCallOutput" => {
            fields(item, "functionCallOutput", &["name", "namespace", "output"])
        }
        "HookPrompt" => fields(item, "hookPrompt", &["fragments"]),
        "ImageView" => fields(item, "imageView", &["path"]),
        "ContextCompaction" => fields(item, "contextCompaction", &[]),
        "CommandExecution" => {
            let mut value = fields(
                item,
                "commandExecution",
                &[
                    "cwd",
                    "process_id",
                    "aggregated_output",
                    "exit_code",
                    "model_context",
                    "sandbox_type",
                    "plugin_id",
                    "script_path",
                ],
            );
            value["status"] = status(&item["status"])?;
            value["source"] = json!(camel(item["source"].as_str().ok_or(ERROR)?));
            let command = item["command"]
                .as_array()
                .ok_or(ERROR)?
                .iter()
                .map(|v| v.as_str().ok_or(ERROR))
                .collect::<Result<Vec<_>, _>>()?;
            // Keep each argument readable without executing or reinterpreting it.
            value["command"] = json!(command
                .iter()
                .map(|s| if s.chars().any(char::is_whitespace) {
                    format!("{s:?}")
                } else {
                    (*s).to_owned()
                })
                .collect::<Vec<_>>()
                .join(" "));
            let mut actions = vec![];
            for action in item["parsed_cmd"].as_array().ok_or(ERROR)? {
                let kind = camel(action["type"].as_str().ok_or(ERROR)?);
                let mut out = json!({"type":kind,"command":action["cmd"]});
                match kind.as_str() {
                    "read" => {
                        out["name"] = action["name"].clone();
                        let path = action["path"].as_str().ok_or(ERROR)?;
                        let cwd = item["cwd"].as_str().ok_or(ERROR)?;
                        out["path"] = json!(if path.starts_with('/')
                            || std::path::Path::new(path).is_absolute()
                        {
                            path.into()
                        } else if cwd.starts_with('/') {
                            format!("{}/{path}", cwd.trim_end_matches('/'))
                        } else {
                            std::path::Path::new(cwd)
                                .join(path)
                                .to_string_lossy()
                                .into_owned()
                        });
                    }
                    "search" => {
                        out["query"] = action["query"].clone();
                        out["path"] = action["path"].clone();
                    }
                    "listFiles" => {
                        out["path"] = action["path"].clone();
                    }
                    "unknown" => {}
                    _ => return Err(ERROR.into()),
                }
                actions.push(out);
            }
            value["commandActions"] = json!(actions);
            value["durationMs"] = duration(item);
            value
        }
        "FileChange" => {
            let mut changes = vec![];
            for (path, change) in item["changes"].as_object().ok_or(ERROR)? {
                let (kind, diff) = match change["type"].as_str() {
                    Some("add") => (json!({"type":"add"}), change["content"].clone()),
                    Some("delete") => (json!({"type":"delete"}), change["content"].clone()),
                    Some("update") => (
                        json!({"type":"update","move_path":change["move_path"]}),
                        change["unified_diff"].clone(),
                    ),
                    _ => return Err(ERROR.into()),
                };
                changes.push(json!({"path":path,"kind":kind,"diff":diff}));
            }
            json!({"type":"fileChange","id":item["id"],"changes":changes,"status":if item["status"].is_null(){json!("inProgress")}else{status(&item["status"])?}})
        }
        "McpToolCall" => {
            let mut value = fields(
                item,
                "mcpToolCall",
                &[
                    "server",
                    "tool",
                    "arguments",
                    "result",
                    "error",
                    "mcp_app_resource_uri",
                    "mcp_app_ui",
                    "plugin_id",
                    "read_only_hint",
                ],
            );
            value["status"] = status(&item["status"])?;
            value["durationMs"] = duration(item);
            value["appContext"] = if item["connector_id"].is_null() {
                Value::Null
            } else {
                json!({"connectorId":item["connector_id"],"linkId":item["link_id"],"resourceUri":item["mcp_app_resource_uri"],"appName":item["app_name"],"actionName":item["action_name"]})
            };
            value
        }
        "DynamicToolCall" => {
            let mut value = fields(
                item,
                "dynamicToolCall",
                &["namespace", "tool", "arguments", "content_items", "success"],
            );
            value["status"] = status(&item["status"])?;
            value["durationMs"] = duration(item);
            value
        }
        "WebSearch" => fields(item, "webSearch", &["query", "action", "results"]),
        "SubAgentActivity" => {
            let mut v = fields(
                item,
                "subAgentActivity",
                &["agent_thread_id", "agent_path", "model", "reasoning_effort"],
            );
            v["kind"] = json!(camel(item["kind"].as_str().ok_or(ERROR)?));
            v
        }
        "CollabAgentToolCall" => {
            let mut v = fields(
                item,
                "collabAgentToolCall",
                &[
                    "sender_thread_id",
                    "receiver_thread_ids",
                    "prompt",
                    "model",
                    "reasoning_effort",
                ],
            );
            v["tool"] = json!(camel(item["tool"].as_str().ok_or(ERROR)?));
            v["status"] = json!(camel(item["status"].as_str().ok_or(ERROR)?));
            let mut states = serde_json::Map::new();
            for (agent, state) in item["agents_states"].as_object().ok_or(ERROR)? {
                let (state, message) = if let Some(s) = state.as_str() {
                    (s, Value::Null)
                } else {
                    let entries = state.as_object().filter(|v| v.len() == 1).ok_or(ERROR)?;
                    let (s, m) = entries.iter().next().ok_or(ERROR)?;
                    (s.as_str(), m.clone())
                };
                states.insert(
                    agent.clone(),
                    json!({"status":camel(state),"message":message}),
                );
            }
            v["agentsStates"] = json!(states);
            v
        }
        "Extension" => match item["kind"].as_str() {
            Some("web.search") => fields(item, "webSearch", &["query", "action", "results"]),
            Some("clock.sleep") => {
                let mut v = fields(item, "sleep", &[]);
                v["durationMs"] = item
                    .get("durationMs")
                    .or_else(|| item.get("duration_ms"))
                    .cloned()
                    .ok_or(ERROR)?;
                v
            }
            Some("image_gen.generation") => image(item),
            _ => return Err(ERROR.into()),
        },
        "ImageGeneration" => image(item),
        "EnteredReviewMode" => {
            json!({"type":"enteredReviewMode","id":item["id"],"review":item["user_facing_hint"]})
        }
        "ExitedReviewMode" => {
            json!({"type":"exitedReviewMode","id":item["id"],"review":item["review_output"]["overall_explanation"].as_str().unwrap_or_default()})
        }
        // Unknown future variants are explicit errors, not made-up messages.
        _ => return Err(ERROR.into()),
    };
    Ok(value)
}
fn image(item: &Value) -> Value {
    let mut value = fields(
        item,
        "imageGeneration",
        &[
            "status",
            "result",
            "revised_prompt",
            "saved_path",
            "transparent_background",
            "failure",
        ],
    );
    for (internal, public) in [
        ("revised_prompt", "revisedPrompt"),
        ("saved_path", "savedPath"),
        ("transparent_background", "transparentBackground"),
    ] {
        if let Some(v) = item.get(public).or_else(|| item.get(internal)) {
            value[public] = v.clone();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_extension_names_and_opaque_tool_arguments_are_preserved() {
        let image=convert(&json!({"type":"Extension","kind":"image_gen.generation","id":"image","status":"completed","result":"image-bytes","savedPath":"C:\\saved.png","revisedPrompt":"blue"})).unwrap();
        assert_eq!(image["type"], "imageGeneration");
        assert_eq!(image["savedPath"], "C:\\saved.png");
        assert_eq!(image["revisedPrompt"], "blue");
        assert_eq!(
            convert(&json!({"type":"Extension","kind":"clock.sleep","id":"wait","durationMs":123}))
                .unwrap()["durationMs"],
            123
        );
        let arguments = json!({"unchanged_key":{"type":"user_defined","another_key":123}});
        let call=convert(&json!({"type":"McpToolCall","id":"tool","server":"server","tool":"lookup","status":"completed","arguments":arguments})).unwrap();
        assert_eq!(call["arguments"], arguments);
        assert!(convert(&json!({"type":"Extension","kind":"unknown","id":"unknown"})).is_err());
    }
}
