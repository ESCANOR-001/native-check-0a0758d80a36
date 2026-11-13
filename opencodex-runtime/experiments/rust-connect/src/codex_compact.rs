//! Manual compaction is an app-server operation, never a synthetic user prompt.
use crate::{
    agent::{Connection, Event},
    chat::AgentConfig,
};
use serde_json::{json, Value};
use std::time::Duration;

async fn rpc(connection: &mut Connection, method: &str, params: Value) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    connection
        .send(&json!({"id":id,"method":method,"params":params}))
        .await
        .map_err(|_| "Codex connection lost; check the chat before retrying")?;
    for _ in 0..256 {
        match connection.events.recv().await {
            Some(Event::Message(v)) if v["id"] == id && v.get("method").is_none() => {
                if let Some(e) = v.get("error") {
                    return Err(e["message"]
                        .as_str()
                        .unwrap_or("Codex rejected compaction")
                        .chars()
                        .take(512)
                        .collect());
                }
                return v
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "Codex did not acknowledge compaction".into());
            }
            Some(Event::Message(_)) => {}
            _ => return Err("Codex disconnected; check the chat before retrying".into()),
        }
    }
    Err("Codex did not acknowledge compaction; check the chat before retrying".into())
}

pub(crate) fn validate(params: &Value) -> Result<(), String> {
    if params["threadId"]
        .as_str()
        .is_none_or(|s| s.is_empty() || s.len() > 128)
        || params["commandId"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 128)
        || params.as_object().is_none_or(|o| {
            o.keys()
                .any(|k| !["threadId", "commandId"].contains(&k.as_str()))
        })
    {
        return Err("Use /compact without arguments in an existing Codex chat.".into());
    }
    Ok(())
}

pub(crate) async fn run(config: AgentConfig, native: String) -> Result<Value, String> {
    let shared = config
        .shared_server
        .as_ref()
        .ok_or("This PC does not expose a shared Codex compaction control")?;
    let mut connection = shared.connect(&config.profile).await?;
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        rpc(&mut connection,"initialize",json!({"clientInfo":{"name":"remodex_compact","version":"1"},"capabilities":{"experimentalApi":true}})).await?;
        connection.send(&json!({"method":"initialized"})).await.map_err(|_| "Codex disconnected")?;
        let mut read = json!({"threadId":native,"includeTurns":false});
        if matches!(shared, crate::agent::SharedServer::Desktop {..}) {
            // Explicit control may reopen this same Desktop route, just like
            // Send. Ordinary history reads never receive activation authority.
            read["remodexAllowActivation"] = json!(true);
        }
        let state = rpc(&mut connection,"thread/read",read).await?;
        if state["thread"]["status"]["type"] == "active" || state["thread"]["status"] == "active" {
            return Err("Wait for the current task to finish before using /compact.".into());
        }
        // Desktop's adapter binds directly to its owner. A daemon connection
        // resumes the same ID without hydrating its full transcript.
        if !matches!(shared, crate::agent::SharedServer::Desktop {..}) {
            rpc(&mut connection,"thread/resume",json!({"threadId":native,"excludeTurns":true})).await?;
        }
        rpc(&mut connection,"thread/compact/start",json!({"threadId":native})).await?;
        Ok(json!({"accepted":true}))
    }).await.unwrap_or_else(|_| Err("Compaction acknowledgement timed out. Check the chat before retrying; it was not resent.".into()));
    connection.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_requires_one_thread_and_no_invented_numeric_argument() {
        assert!(validate(&json!({"threadId":"a","commandId":"b"})).is_ok());
        for value in [
            json!({"threadId":"","commandId":"b"}),
            json!({"threadId":"a"}),
            json!({"threadId":"a","commandId":"b","amount":1}),
        ] {
            assert!(validate(&value).is_err());
        }
    }
}
