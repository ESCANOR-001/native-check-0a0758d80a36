//! Keep Codex's user-facing error, including terminal errors in saved turns.
//! Do not turn an upstream error into a guessed capacity/authentication error.
use crate::{chat::Chat, chat_projection as projection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) const FALLBACK: &str =
    "Codex ended this task with an error but did not provide details.";

pub(crate) fn thin(error: &Value) -> Option<Value> {
    let message = error.as_str().or_else(|| error["message"].as_str())?;
    if message.trim().is_empty() {
        return None;
    }
    let mut out = json!({"message":projection::bounded(message, 8192)});
    if let Some(details) = error["additionalDetails"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        out["additionalDetails"] = json!(projection::bounded(details, 4096));
    }
    let code = error
        .get("codexErrorInfo")
        .or_else(|| error.get("codex_error_info"));
    if let Some(code) = code {
        // This open-ended protocol field is tiny. Do not retain other provider
        // fields (request bodies, tool contents, headers, or private metadata).
        if (code.is_string() || code.is_object()) && code.to_string().len() <= 512 {
            out["codexErrorInfo"] = code.clone();
        }
    }
    Some(out)
}

pub(crate) fn message(error: &Value) -> Option<String> {
    let error = thin(error)?;
    let mut text = error["message"].as_str()?.to_owned();
    if let Some(detail) = error["additionalDetails"].as_str() {
        if !text.contains(detail) {
            text.push_str("\n\n");
            text.push_str(detail);
        }
    }
    Some(text)
}

/// Terminal metadata is authoritative; a terminal error is never success.
pub(crate) fn state(turn: &Value) -> &str {
    match turn["status"].as_str() {
        Some("interrupted") => "interrupted",
        Some("failed") => "error",
        Some("completed") if thin(&turn["error"]).is_some() => "error",
        Some("completed") => "completed",
        _ => "running",
    }
}

pub(crate) fn terminal_error(turn: &Value) -> Value {
    thin(&turn["error"]).unwrap_or_else(|| {
        if turn["status"] == "failed" {
            json!({"message": FALLBACK})
        } else {
            Value::Null
        }
    })
}

pub(crate) fn project(chat: &mut Chat, error: &Value, retrying: bool, at: Option<&str>) {
    let Some(error) = thin(error) else {
        return;
    };
    let turn = projection::s(&chat.thread["latestTurn"]["turnId"]);
    if turn.is_empty() {
        return;
    }
    let text = message(&error).unwrap();
    let id = format!(
        "codex-error-{:x}",
        Sha256::digest(format!("{turn}\0{text}"))
    );
    let title = format!(
        "{}: {}",
        if retrying {
            "Codex is retrying"
        } else {
            "Codex error"
        },
        projection::bounded(&text, 512)
    );
    projection::activity(
        chat,
        &id,
        "provider.error",
        &title,
        "error",
        json!({"detail":text,"status":if retrying {"retrying"}else{"failed"},"willRetry":retrying,"data":{"providerName":"codex","error":error}}),
    );
    if let Some(at) = at {
        let row = chat.thread["activities"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == id)
            .unwrap();
        row["createdAt"] = json!(at);
        row.as_object_mut().unwrap().remove("sequence");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_compact_and_unknown_provider_errors_are_preserved_not_relabelled() {
        let text = "Error running remote compact task: {\"detail\":\"Bad Request\"}";
        assert_eq!(
            message(&json!({"message":text,"codex_error_info":"other"})).unwrap(),
            text
        );
        for code in [
            "serverOverloaded",
            "rateLimitExceeded",
            "unauthorized",
            "futureError",
        ] {
            let error = json!({"message":"Provider's actual explanation", "additionalDetails":"Request ID: abc", "codexErrorInfo":code,"private":"DO_NOT_KEEP"});
            let saved = thin(&error).unwrap();
            assert_eq!(saved["codexErrorInfo"], code);
            assert!(saved.get("private").is_none());
            assert_eq!(
                message(&saved).unwrap(),
                "Provider's actual explanation\n\nRequest ID: abc"
            );
        }
        assert_eq!(
            state(&json!({"status":"completed","error":{"message":text}})),
            "error"
        );
        assert_eq!(
            state(&json!({"status":"inProgress","error":{"message":text}})),
            "running"
        );
        assert_eq!(
            state(&json!({"status":"interrupted","error":{"message":text}})),
            "interrupted"
        );
        assert!(thin(&Value::Null).is_none());
        assert!(
            thin(&json!({"message":"🌻".repeat(10000)})).unwrap()["message"]
                .as_str()
                .unwrap()
                .len()
                <= 8195
        );
    }
}
