//! Durable, explicitly requested controls of an already verified Claude worker.
//! Reservation is saved before delivery. Reconnect never repeats a control.
use crate::{capabilities::Control, chat::Chat, chat_projection::s};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const UNCERTAIN: &str = "Claude has not confirmed this control. Refresh its status before trying again; it was not replayed.";
const MODEL_PENDING: &str = "Claude has not confirmed the model change yet; it may still apply after Claude finishes an earlier model request. Check the model before sending again; nothing was retried.";
const INSTANCE: &str = "claude-code";

/// Validates a shared session's requested model and effort against the CLI
/// catalog. An unchanged selection returns the confirmed one exactly, even
/// when the session reports a wire model ID that the picker lists by alias.
pub(crate) fn shared_selection(
    catalog: &Value,
    requested: &Value,
    current: &Value,
) -> Result<Value, String> {
    if requested.is_null() {
        return Ok(current.clone());
    }
    if requested["instanceId"]
        .as_str()
        .is_some_and(|v| v != INSTANCE)
    {
        return Err("Model belongs to a different agent".into());
    }
    let model = requested["model"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| s(&current["model"]));
    // The worker reports the wire model an alias resolves to, so the alias
    // and that ID are the same confirmed model, never a change to apply.
    let resolves = |alias: &str, wire: &str| {
        crate::models::row(catalog, alias)
            .is_some_and(|r| r["slug"] == alias && r["resolvedModel"] == wire)
    };
    let confirmed = s(&current["model"]);
    let same = model == confirmed || resolves(model, confirmed) || resolves(confirmed, model);
    if same {
        let unchanged = match requested.get("options") {
            None => true,
            Some(options) if options == &current["options"] => true,
            Some(options) => options.as_array().is_some_and(|a| {
                a.iter().all(|v| v["id"] == "effort")
                    && a.iter()
                        .find_map(|v| v["value"].as_str().filter(|v| *v != "auto"))
                        == crate::models::effort(current)
            }),
        };
        if unchanged {
            return Ok(current.clone());
        }
    }
    // A listed alias, or the explicit wire ID that an alias resolves to. The
    // CLI accepts both; anything else was never reported by this agent.
    let mut row = crate::models::row(catalog, model)
        .ok_or(if same {
            "Select a model from the list before changing its effort"
        } else {
            "Select a model reported by this agent"
        })?
        .clone();
    row["slug"] = json!(model);
    let mut target = crate::models::selection(&json!([row]), requested, current, INSTANCE)?;
    if same {
        target["model"] = current["model"].clone();
    }
    if target["model"] == current["model"]
        && crate::models::effort(&target) == crate::models::effort(current)
    {
        return Ok(current.clone());
    }
    Ok(target)
}
/// Native steps that make `current` equal `target`: the model first, since
/// supported effort levels depend on it.
pub(crate) fn settings_steps(target: &Value, current: &Value) -> Vec<Control> {
    let mut steps = vec![];
    if target["model"] != current["model"] {
        steps.push(Control::Model(s(&target["model"]).into()));
    }
    let effort = crate::models::effort(target);
    if effort != crate::models::effort(current) {
        steps.push(Control::Effort(effort.map(str::to_owned)));
    }
    steps
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub key: String,
    pub digest: String,
    pub request: String,
    pub action: Control,
    pub deadline: i64,
    pub result: Option<Value>,
}
pub(crate) fn response(value: Value) -> Result<Value, String> {
    if value["nativePending"] == true {
        Err(UNCERTAIN.into())
    } else if let Some(message) = value["error"].as_str() {
        Err(message.into())
    } else {
        Ok(value)
    }
}
pub(crate) fn matches(chat: &Chat, value: &Value) -> bool {
    value["type"] == "control_response"
        && chat
            .native_control
            .as_ref()
            .is_some_and(|i| i.result.is_none() && value["response"]["request_id"] == i.request)
}
/// Only verified owner stdout or a durable worker event may reach this helper.
pub(crate) fn apply(chat: &mut Chat, value: &Value) -> bool {
    let Some(intent) = chat.native_control.as_mut().filter(|i| i.result.is_none()) else {
        return false;
    };
    if value["type"] != "control_response" || value["response"]["request_id"] != intent.request {
        return false;
    }
    let result = if value["response"]["subtype"] != "success" {
        // CLI errors can contain private configuration. Keep the user-facing
        // failure specific to this action but never persist its raw response.
        let message = value["response"]["error"].as_str().unwrap_or("");
        if chat.external_claude
            && (message.starts_with("REPL bridge does not handle control_request subtype:")
                || message.contains("is not supported in this context"))
        {
            json!({"error":"Claude's Remote Control connection does not support this action. Use the original Claude session for this control. This chat is unchanged."})
        } else {
            match &intent.action {
                // The CLI answers after its verdict deadline while the switch
                // can still complete later. That is neither success nor refusal.
                Control::Model(_) if message.starts_with("This model switch is still pending") => {
                    json!({"error":MODEL_PENDING})
                }
                Control::Model(_) => {
                    json!({"error":"Claude did not accept this model for the current session. The previous model is retained."})
                }
                Control::Effort(_) => {
                    json!({"error":"Claude did not accept this effort for the current session. The previous effort is retained."})
                }
                _ => {
                    json!({"error":"Claude could not apply this control in the current session. The previous settings are retained."})
                }
            }
        }
    } else {
        let result = &value["response"]["response"];
        match &intent.action {
            Control::Access {
                runtime_mode,
                interaction_mode,
            } => {
                chat.claude_plan_access =
                    (interaction_mode == "plan").then(|| runtime_mode.clone());
                chat.thread["runtimeMode"] = json!(runtime_mode);
                chat.thread["session"]["runtimeMode"] = json!(runtime_mode);
                chat.thread["interactionMode"] = json!(interaction_mode);
                json!({"updated":true})
            }
            Control::Toggle { name, enabled } => {
                chat.disabled_mcp.retain(|s| s != name);
                if !enabled {
                    chat.disabled_mcp.push(name.clone());
                    chat.disabled_mcp.sort();
                }
                json!({"updated":true})
            }
            Control::ReloadPlugins if result["held"] == true => {
                json!({"held":true,"detail":"Claude kept the current tools to protect the conversation cache. Start a new chat to use changed plugins."})
            }
            Control::ReloadPlugins if result["error_count"].as_u64().unwrap_or(0) > 0 => {
                json!({"warning":"Some plugins could not reload. Check their configuration on the PC."})
            }
            Control::Model(model) => {
                chat.thread["modelSelection"]["instanceId"] = json!(INSTANCE);
                chat.thread["modelSelection"]["model"] = json!(model);
                json!({"updated":true})
            }
            Control::Effort(level) => {
                chat.thread["modelSelection"]["options"] = json!(level
                    .iter()
                    .map(|v| json!({"id":"effort","value":v}))
                    .collect::<Vec<_>>());
                json!({"updated":true})
            }
            _ => json!({"updated":true}),
        }
    };
    intent.result = Some(result);
    true
}

/// Native Plan mode is a temporary execution restriction, not a replacement
/// for an explicitly confirmed access preference. An imported Plan session
/// with no such confirmation defaults to approval; never infer full access.
pub(crate) fn observe_permission(chat: &mut Chat, mode: &str) {
    let runtime = match mode {
        "plan" => chat
            .claude_plan_access
            .as_deref()
            .filter(|s| ["approval-required", "auto-accept-edits", "full-access"].contains(s))
            .unwrap_or("approval-required")
            .to_owned(),
        "default" => "approval-required".into(),
        "acceptEdits" => "auto-accept-edits".into(),
        "bypassPermissions" => "full-access".into(),
        _ => return,
    };
    if mode != "plan" {
        chat.claude_plan_access = None;
    }
    chat.thread["runtimeMode"] = json!(runtime);
    chat.thread["session"]["runtimeMode"] = json!(runtime);
    chat.thread["interactionMode"] = json!(if mode == "plan" { "plan" } else { "default" });
}
/// Called inside the chat/cursor transaction, so a committed event can never
/// leave a pending command behind or acknowledge changes whose save failed.
pub(crate) fn commit(tx: &rusqlite::Transaction<'_>, state: &Value) -> rusqlite::Result<()> {
    let Some(intent) = state.get("native_control").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let intent: Intent =
        serde_json::from_value(intent.clone()).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let Some(result) = intent.result else {
        return Ok(());
    };
    let changed=tx.execute("UPDATE native_tool_actions SET result=?3 WHERE id=?1 AND digest=?2 AND ((json_extract(result,'$.nativePending')=1 AND json_extract(result,'$.requestId')=?4) OR result=?3)", rusqlite::params![intent.key,intent.digest,result.to_string(),intent.request])?;
    if changed != 1 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        claude_cursor::Cursor, claude_host::Identity, claude_remote::Envelope, store::Store,
    };
    #[test]
    fn unsupported_shared_controls_explain_native_limit_without_exposing_raw_errors() {
        for (external, error, unsupported) in [
            (
                true,
                "REPL bridge does not handle control_request subtype: reload_skills",
                true,
            ),
            (
                true,
                "mcp_reconnect is not supported in this context (callback not registered)",
                true,
            ),
            (true, "PRIVATE_METADATA", false),
            (
                false,
                "mcp_reconnect is not supported in this context (callback not registered)",
                false,
            ),
        ] {
            let mut chat: Chat=serde_json::from_value(json!({"agent":"claude","native_id":"native","resumed":true,"external_claude":external,"thread":{},"native_control":{"key":"key","digest":"digest","request":"request","action":"ReloadSkills","deadline":0,"result":null}})).unwrap();
            assert!(apply(
                &mut chat,
                &json!({"type":"control_response","response":{"request_id":"request","subtype":"error","error":error}})
            ));
            let result = chat.native_control.unwrap().result.unwrap();
            assert_eq!(
                result.to_string().contains("does not support this action"),
                unsupported
            );
            assert!(!result.to_string().contains("PRIVATE_METADATA"));
            assert!(chat.disabled_mcp.is_empty());
        }
    }
    #[test]
    fn access_only_changes_on_the_matching_successful_acknowledgement() {
        let state = json!({"agent":"claude","native_id":"native","resumed":true,"external_claude":true,"thread":{"runtimeMode":"approval-required","interactionMode":"default","session":{"runtimeMode":"approval-required"}},"native_control":{"key":"key","digest":"digest","request":"request","action":{"Access":{"runtime_mode":"auto-accept-edits","interaction_mode":"plan"}},"deadline":0,"result":null}});
        for subtype in ["success", "error"] {
            let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
            assert!(!apply(
                &mut chat,
                &json!({"type":"control_response","response":{"request_id":"another","subtype":subtype}})
            ));
            assert_eq!(chat.thread, state["thread"]);
            assert!(apply(
                &mut chat,
                &json!({"type":"control_response","response":{"request_id":"request","subtype":subtype,"error":"PRIVATE_METADATA"}})
            ));
            if subtype == "success" {
                assert_eq!(chat.thread["runtimeMode"], "auto-accept-edits");
                assert_eq!(chat.thread["session"]["runtimeMode"], "auto-accept-edits");
                assert_eq!(chat.thread["interactionMode"], "plan");
            } else {
                assert_eq!(chat.thread, state["thread"]);
            }
            assert!(!chat
                .native_control
                .unwrap()
                .result
                .unwrap()
                .to_string()
                .contains("PRIVATE_METADATA"));
        }
    }
    fn shared_catalog() -> Value {
        crate::models::catalog(
            &json!([
                {"value":"default","displayName":"Default","resolvedModel":"claude-wire-sonnet","supportedEffortLevels":["low","high"]},
                {"value":"sonnet","displayName":"Sonnet","resolvedModel":"claude-wire-sonnet","supportedEffortLevels":["low","high"]},
                {"value":"opus","displayName":"Opus","resolvedModel":"claude-wire-opus","supportedEffortLevels":["low","high","max"]},
                {"value":"haiku","displayName":"Haiku","resolvedModel":"claude-wire-haiku"}
            ]),
            false,
            "default",
        )
    }
    fn opus_max() -> Value {
        json!({"model":"opus","options":[{"id":"effort","value":"max"}]})
    }
    #[test]
    fn shared_selection_validates_actual_capabilities_and_keeps_unchanged_wire_models() {
        let catalog = shared_catalog();
        let current = json!({"instanceId":"claude-code","model":"claude-wire-sonnet","options":[]});
        for unchanged in [
            Value::Null,
            current.clone(),
            json!({"instanceId":"claude-code","model":"claude-wire-sonnet"}),
            json!({"model":"claude-wire-sonnet","options":[{"id":"effort","value":"auto"}]}),
        ] {
            let selected = shared_selection(&catalog, &unchanged, &current).unwrap();
            assert_eq!(selected, current, "{unchanged}");
            assert!(settings_steps(&selected, &current).is_empty());
        }
        // An effort-only change on a reported wire model uses the alias row
        // that resolves to it; the model itself is not touched.
        let effort = shared_selection(
            &catalog,
            &json!({"model":"claude-wire-sonnet","options":[{"id":"effort","value":"high"}]}),
            &current,
        )
        .unwrap();
        assert_eq!(effort["model"], "claude-wire-sonnet");
        assert!(matches!(
            settings_steps(&effort, &current).as_slice(),
            [Control::Effort(Some(level))] if level == "high"
        ));
        let both = shared_selection(
            &catalog,
            &json!({"model":"opus","options":[{"id":"effort","value":"max"}]}),
            &current,
        )
        .unwrap();
        let steps = settings_steps(&both, &current);
        assert!(
            matches!(steps.as_slice(), [Control::Model(m), Control::Effort(Some(e))] if m == "opus" && e == "max"),
            "model must precede effort"
        );
        assert_eq!(
            steps[0].request(),
            json!({"subtype":"set_model","model":"opus"})
        );
        assert_eq!(
            steps[1].request(),
            json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"max"}})
        );
        let auto = shared_selection(&catalog, &json!({"model":"opus"}), &effort).unwrap();
        assert!(matches!(
            settings_steps(&auto, &effort).as_slice(),
            [Control::Model(_), Control::Effort(None)]
        ));
        assert_eq!(
            Control::Effort(None).request()["settings"],
            json!({"effortLevel":null})
        );
        for (requested, error) in [
            (json!({"model":"not-reported"}), "reported by this agent"),
            (
                json!({"instanceId":"codex","model":"opus"}),
                "different agent",
            ),
            (
                json!({"model":"sonnet","options":[{"id":"effort","value":"max"}]}),
                "does not support",
            ),
            (
                json!({"model":"haiku","options":[{"id":"effort","value":"low"}]}),
                "does not support",
            ),
            (
                json!({"model":"opus","options":[{"id":"thinking","value":true}]}),
                "Unsupported model option",
            ),
        ] {
            assert!(
                shared_selection(&catalog, &requested, &current)
                    .unwrap_err()
                    .contains(error),
                "{requested}"
            );
        }
        // After set_model("opus") the worker reports its wire ID. The alias
        // stays the same model: only the effort is a change.
        let reported = json!({"instanceId":"claude-code","model":"claude-wire-opus","options":[]});
        assert_eq!(
            shared_selection(&catalog, &json!({"model":"opus","options":[]}), &reported).unwrap(),
            reported
        );
        let alias_effort = shared_selection(&catalog, &opus_max(), &reported).unwrap();
        assert_eq!(alias_effort["model"], "claude-wire-opus");
        assert!(matches!(
            settings_steps(&alias_effort, &reported).as_slice(),
            [Control::Effort(Some(e))] if e == "max"
        ));
        let back = shared_selection(&catalog, &current, &both).unwrap();
        assert_eq!(
            back, current,
            "a reported wire ID is a valid explicit target"
        );
        assert!(matches!(
            settings_steps(&back, &both).as_slice(),
            [Control::Model(m), Control::Effort(None)] if m == "claude-wire-sonnet"
        ));
        let unknown = json!({"instanceId":"claude-code","model":"claude-unlisted","options":[]});
        assert_eq!(
            shared_selection(&catalog, &unknown, &unknown).unwrap(),
            unknown
        );
        assert!(shared_selection(
            &catalog,
            &json!({"model":"claude-unlisted","options":[{"id":"effort","value":"high"}]}),
            &unknown
        )
        .unwrap_err()
        .contains("before changing its effort"));
    }
    #[test]
    fn model_and_effort_change_only_on_their_matching_successful_acknowledgement() {
        let base = json!({"agent":"claude","native_id":"native","resumed":true,"external_claude":true,"thread":{"modelSelection":{"instanceId":"claude-code","model":"claude-wire-sonnet","options":[]}}});
        let ack = |subtype: &str, error: &str| json!({"type":"control_response","response":{"request_id":"request","subtype":subtype,"error":error}});
        let mut state = base.clone();
        state["native_control"] = json!({"key":"key","digest":"digest","request":"request","action":{"Model":"opus"},"deadline":0,"result":null});
        let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
        assert!(!apply(
            &mut chat,
            &json!({"type":"control_response","response":{"request_id":"other","subtype":"success"}})
        ));
        assert!(apply(&mut chat, &ack("success", "")));
        assert_eq!(chat.thread["modelSelection"]["model"], "opus");
        assert_eq!(chat.thread["modelSelection"]["options"], json!([]));
        assert!(
            !apply(&mut chat, &ack("success", "")),
            "one acknowledgement applies once"
        );
        for (error, expected) in [
            ("This model switch is still pending (an earlier model request or a PreModelSwitch hook has not finished); it will apply when that completes unless it is refused", "may still apply"),
            ("PRIVATE_METADATA refused", "previous model is retained"),
        ] {
            let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
            assert!(apply(&mut chat, &ack("error", error)));
            assert_eq!(chat.thread, base["thread"]);
            let result = chat.native_control.unwrap().result.unwrap();
            assert!(response(result.clone()).unwrap_err().contains(expected));
            assert!(!result.to_string().contains("PRIVATE_METADATA"));
        }
        state["native_control"]["action"] = json!({"Effort":"high"});
        let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
        assert!(apply(&mut chat, &ack("error", "PRIVATE_METADATA")));
        assert_eq!(chat.thread, base["thread"]);
        assert!(response(chat.native_control.unwrap().result.unwrap())
            .unwrap_err()
            .contains("previous effort is retained"));
        let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
        assert!(apply(&mut chat, &ack("success", "")));
        assert_eq!(
            chat.thread["modelSelection"],
            json!({"instanceId":"claude-code","model":"claude-wire-sonnet","options":[{"id":"effort","value":"high"}]})
        );
        // A durable reservation survives a restart in the same serialized form.
        let restored: Chat = serde_json::from_value(serde_json::to_value(&chat).unwrap()).unwrap();
        assert!(matches!(
            restored.native_control.unwrap().action,
            Control::Effort(Some(level)) if level == "high"
        ));
        state["native_control"]["action"] = json!({"Effort":null});
        let mut chat: Chat = serde_json::from_value(state).unwrap();
        chat.thread["modelSelection"]["options"] = json!([{"id":"effort","value":"high"}]);
        assert!(apply(&mut chat, &ack("success", "")));
        assert_eq!(chat.thread["modelSelection"]["options"], json!([]));
    }
    #[test]
    fn plan_status_keeps_only_confirmed_access_and_forgets_it_after_owner_changes() {
        let state = json!({"agent":"claude","native_id":"native","resumed":true,"external_claude":true,"thread":{"runtimeMode":"full-access","interactionMode":"default","session":{"runtimeMode":"full-access"}}});
        let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
        observe_permission(&mut chat, "plan");
        assert_eq!(
            chat.thread["runtimeMode"], "approval-required",
            "an imported plan cannot inherit an unconfirmed access preference"
        );
        chat.claude_plan_access = Some("auto-accept-edits".into());
        observe_permission(&mut chat, "plan");
        assert_eq!(chat.thread["runtimeMode"], "auto-accept-edits");
        assert_eq!(chat.thread["interactionMode"], "plan");
        let mut resumed: Chat =
            serde_json::from_value(serde_json::to_value(chat).unwrap()).unwrap();
        observe_permission(&mut resumed, "plan");
        assert_eq!(resumed.thread["runtimeMode"], "auto-accept-edits");
        let previous = resumed.thread.clone();
        observe_permission(&mut resumed, "");
        assert_eq!(resumed.thread, previous);
        observe_permission(&mut resumed, "default");
        assert!(resumed.claude_plan_access.is_none());
        observe_permission(&mut resumed, "plan");
        assert_eq!(resumed.thread["runtimeMode"], "approval-required");
    }
    #[test]
    fn acknowledgement_settings_cursor_and_receipt_commit_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open(dir.path()).unwrap();
        db.init_chats().unwrap();
        let identity = Identity {
            native_session: uuid::Uuid::new_v4().to_string(),
            bridge_session: "cse_controls_fixture".into(),
        };
        let mut state = json!({"agent":"claude","native_id":identity.native_session,"resumed":true,"remote_identity":identity,"thread":{"id":"control","messages":[{"id":"old","text":"retained"}],"activities":[],"proposedPlans":[]},"native_control":{"key":"key","digest":"digest","request":"request","action":{"Toggle":{"name":"fixture","enabled":false}},"deadline":0,"result":null}});
        db.save_tool_action(
            &state,
            "key",
            "digest",
            &json!({"nativePending":true,"requestId":"request"}),
        )
        .unwrap();
        let mut chat: Chat = serde_json::from_value(state.clone()).unwrap();
        assert!(!apply(
            &mut chat,
            &json!({"type":"control_response","response":{"request_id":"other","subtype":"success"}})
        ));
        let reply = json!({"type":"control_response","response":{"request_id":"request","subtype":"success","response":{}}});
        assert!(apply(&mut chat, &reply));
        state = serde_json::to_value(&chat).unwrap();
        let before = Cursor::new(identity.clone()).unwrap();
        let event = Envelope {
            sequence: Some(1),
            event_id: Some("ack".into()),
            created_at: None,
            source: "worker".into(),
            payload: Some(reply),
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
        };
        let after = before.advance(&event, Value::Null).unwrap();
        let mut bad = state.clone();
        bad["native_control"]["digest"] = json!("different");
        assert!(db.save_claude_remote(&bad, &before, &after).is_err());
        assert_eq!(
            db.claude_remote_cursor("control", identity.clone())
                .unwrap(),
            before
        );
        assert!(db.load_chat("control").unwrap()["disabled_mcp"].is_null());
        assert_eq!(
            db.tool_action("key").unwrap().unwrap().1["nativePending"],
            true
        );
        db.save_claude_remote(&state, &before, &after).unwrap();
        assert_eq!(db.claude_remote_cursor("control", identity).unwrap(), after);
        assert_eq!(
            db.load_chat("control").unwrap()["disabled_mcp"],
            json!(["fixture"])
        );
        assert_eq!(
            db.tool_action("key").unwrap().unwrap().1,
            json!({"updated":true})
        );
        assert!(
            !apply(&mut chat, &event.payload.unwrap()),
            "a repeated acknowledgement cannot apply twice"
        );
        db.save_chat(&state, None).unwrap();
    }
}
