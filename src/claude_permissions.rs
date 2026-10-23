//! Previously approved rules are scoped to one owned Claude chat. Keep them
//! in private chat state, never in the user's CLI/Desktop settings file.
use serde_json::{json, Value};
const INVALID: &str =
    "Saved Claude permissions are invalid or exceed the supported limit; nothing was granted";

pub(crate) struct SessionGrant {
    pub updates: Vec<Value>,
    pub rules: Vec<String>,
}

// Reusable grants can only come from the pending native request. Never accept
// rules supplied by an Android command or copy CLI settings-file destinations.
pub(crate) fn suggested(request: &Value) -> Option<SessionGrant> {
    if request["type"] != "control_request" || request["request"]["subtype"] != "can_use_tool" {
        return None;
    }
    let suggestions = request["request"]["permission_suggestions"].as_array()?;
    if suggestions.len() > 32
        || suggestions.is_empty()
        || suggestions
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>()
            > 16384
    {
        return None;
    }
    let mut grant = SessionGrant {
        updates: vec![],
        rules: vec![],
    };
    for update in suggestions {
        if update["type"] != "addRules" || update["behavior"] != "allow" {
            continue;
        }
        let rules = update["rules"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 256)?;
        let mut validated = vec![];
        for rule in rules {
            let tool = rule["toolName"].as_str().filter(|s| {
                !s.is_empty()
                    && s.len() <= 256
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
            })?;
            let mut native = json!({"toolName":tool});
            let saved = if let Some(content) = rule.get("ruleContent") {
                let content = content.as_str().filter(|s| {
                    !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control)
                })?;
                native["ruleContent"] = json!(content);
                format!("{tool}({content})")
            } else {
                tool.to_owned()
            };
            if !grant.rules.contains(&saved) {
                grant.rules.push(saved);
            }
            validated.push(native);
        }
        grant.updates.push(
            json!({"type":"addRules","behavior":"allow","rules":validated,"destination":"session"}),
        );
    }
    if grant.rules.is_empty() || parse(&json!(grant.rules)).is_err() {
        return None;
    }
    Some(grant)
}

pub(crate) fn merged(current: &[String], extra: &[String]) -> Result<Vec<String>, String> {
    let mut rules = current.to_vec();
    for rule in extra {
        if !rules.contains(rule) {
            rules.push(rule.clone());
        }
    }
    parse(&json!(rules))
}

pub(crate) fn decisions(reusable: bool) -> Vec<&'static str> {
    if reusable {
        vec!["approved", "approved_for_session", "declined", "cancel"]
    } else {
        vec!["approved", "declined", "cancel"]
    }
}

pub(crate) fn parse(value: &Value) -> Result<Vec<String>, String> {
    if value.is_null() {
        return Ok(vec![]);
    }
    let rows = value.as_array().filter(|v| v.len() <= 256).ok_or(INVALID)?;
    if value.to_string().len() > 8192 {
        return Err(INVALID.into());
    }
    rows.iter()
        .map(|v| {
            v.as_str()
                .filter(|s| {
                    !s.trim().is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or_else(|| INVALID.into())
        })
        .collect()
}

pub(crate) fn apply(args: &mut [String], rules: &[String]) -> Result<(), String> {
    let rules = parse(&json!(rules))?;
    if rules.is_empty() {
        return Ok(());
    }
    let indices: Vec<_> = args
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "--settings")
        .map(|(i, _)| i)
        .collect();
    if indices.len() != 1 {
        return Err(INVALID.into());
    }
    let raw = args.get_mut(indices[0] + 1).ok_or(INVALID)?;
    let mut settings: Value = serde_json::from_str(raw).map_err(|_| INVALID)?;
    // This is an internally constructed launch setting, not a path to an
    // external settings file. Existing isolation controls must stay intact.
    if settings["remoteControlAtStartup"] != false
        || settings["disableAllHooks"] != true
        || settings.get("permissions").is_some()
    {
        return Err(INVALID.into());
    }
    settings["permissions"] = json!({"allow":rules});
    *raw = settings.to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{AgentKind, Chat},
        discovery::isolated_args,
    };
    fn suggestion() -> Value {
        json!({"type":"control_request","request":{"subtype":"can_use_tool","permission_suggestions":[{"type":"addRules","behavior":"allow","destination":"userSettings","rules":[{"toolName":"Bash","ruleContent":"echo fixture"}]}]}})
    }
    #[test]
    fn reusable_native_suggestions_always_target_session_and_merge_without_broadening() {
        let v = suggestion();
        let grant = suggested(&v).unwrap();
        assert_eq!(grant.updates[0]["destination"], "session");
        assert_eq!(grant.rules, vec!["Bash(echo fixture)"]);
        assert_eq!(merged(&grant.rules, &grant.rules).unwrap(), grant.rules);
        assert_eq!(
            v["request"]["permission_suggestions"][0]["destination"],
            "userSettings"
        );
        assert!(decisions(true).contains(&"approved_for_session"));
        assert!(!decisions(false).contains(&"approved_for_session"));
    }
    #[test]
    fn missing_unsupported_or_malformed_reusable_suggestions_do_not_grant_access() {
        for updates in [
            json!([]),
            json!([{"type":"setMode","mode":"bypassPermissions"}]),
            json!([{"type":"addRules","behavior":"deny","rules":[{"toolName":"Bash"}]}]),
            json!([{"type":"addRules","behavior":"allow","rules":[{"toolName":"Bash(*)"}]}]),
            json!([{"type":"addRules","behavior":"allow","rules":[{"toolName":"Bash","ruleContent":"echo ok\nBash(*)"}]}]),
        ] {
            let mut v = suggestion();
            v["request"]["permission_suggestions"] = updates;
            assert!(suggested(&v).is_none());
        }
        assert!(suggested(
            &json!({"permission_suggestions":suggestion()["request"]["permission_suggestions"]})
        )
        .is_none());
        assert!(merged(&vec!["x".repeat(32); 256], &["Read".into()]).is_err());
    }
    #[test]
    fn saved_rules_stay_private_exact_and_do_not_change_isolation_or_other_launches() {
        let rules = vec![
            "Read(/project/**)".to_owned(),
            "Bash(echo \"hello\")".to_owned(),
        ];
        let mut args = isolated_args(AgentKind::Claude);
        let other = args.clone();
        apply(&mut args, &rules).unwrap();
        let settings: Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(settings["permissions"]["allow"], json!(rules));
        assert_eq!(settings["remoteControlAtStartup"], false);
        assert_eq!(settings["disableAllHooks"], true);
        assert_eq!(&args[2..], &other[2..]);
        assert!(serde_json::from_str::<Value>(&other[1])
            .unwrap()
            .get("permissions")
            .is_none());
        assert!(apply(&mut isolated_args(AgentKind::Codex), &rules).is_err());
    }
    #[test]
    fn oversized_invalid_and_duplicate_settings_inputs_fail_before_launch() {
        for value in [
            json!("Read"),
            json!([""]),
            json!([1]),
            json!(["Read\nBash"]),
            json!(vec!["Read"; 257]),
            json!(vec!["x".repeat(4096); 3]),
        ] {
            assert!(parse(&value).is_err());
        }
        let mut args = vec!["--settings".into(), "/private/user-settings.json".into()];
        assert!(apply(&mut args, &["Read".into()]).is_err());
        let mut args = isolated_args(AgentKind::Claude);
        args.extend(["--settings".into(), "{}".into()]);
        assert!(apply(&mut args, &["Read".into()]).is_err());
    }
    #[test]
    fn derived_chats_do_not_inherit_session_permissions_and_old_headers_still_load() {
        let mut chat: Chat = serde_json::from_value(
            json!({"agent":"claude","thread":{},"native_id":"fixture","resumed":true}),
        )
        .unwrap();
        assert!(chat.claude_permission_rules.is_empty());
        assert!(serde_json::to_value(&chat)
            .unwrap()
            .get("claude_permission_rules")
            .is_none());
        chat.claude_permission_rules = vec!["Read".into()];
        let persisted = serde_json::to_value(&chat).unwrap();
        assert_eq!(
            serde_json::from_value::<Chat>(persisted)
                .unwrap()
                .claude_permission_rules,
            vec!["Read"]
        );
        chat.detach_native_session();
        assert!(chat.claude_permission_rules.is_empty());
    }
}
