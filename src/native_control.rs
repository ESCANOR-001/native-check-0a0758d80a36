//! Durable, explicitly requested controls of an already verified Claude worker.
//! Reservation is saved before delivery. Reconnect never repeats a control.
use crate::{capabilities::Control, chat::Chat};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const UNCERTAIN: &str = "Claude has not confirmed this control. Refresh its status before trying again; it was not replayed.";
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
            json!({"error":"Claude could not apply this control in the current session. The previous settings are retained."})
        }
    } else {
        let result = &value["response"]["response"];
        match &intent.action {
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
            _ => json!({"updated":true}),
        }
    };
    intent.result = Some(result);
    true
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
