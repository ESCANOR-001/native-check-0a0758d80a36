//! Only provider-reported usage numbers cross the phone boundary.
use serde_json::{json, Value};
fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
pub fn codex_limits(v: &Value) -> Option<Value> {
    let by_id = v["rateLimitsByLimitId"]
        .as_object()
        .filter(|v| !v.is_empty());
    let buckets: Vec<(&str, &Value)> = if let Some(rows) = by_id {
        rows.iter().take(12).map(|(k, v)| (k.as_str(), v)).collect()
    } else {
        vec![("codex", &v["rateLimits"])]
    };
    let mut windows = Vec::new();
    for (id, bucket) in &buckets {
        let name = bucket["limitName"]
            .as_str()
            .unwrap_or(id)
            .chars()
            .filter(|c| c.is_alphanumeric() || " ._-".contains(*c))
            .take(40)
            .collect::<String>();
        for key in ["primary", "secondary"] {
            let w = &bucket[key];
            let Some(percent) = number(&w["usedPercent"]) else {
                continue;
            };
            if windows.len() >= 12 {
                break;
            }
            let label = match w["windowDurationMins"].as_u64() {
                Some(300) => "5-hour".into(),
                Some(1440) => "Daily".into(),
                Some(10080) => "Weekly".into(),
                Some(n) if n > 0 => format!("{n}-minute"),
                _ => if key == "primary" {
                    "Limit"
                } else {
                    "Secondary limit"
                }
                .into(),
            };
            windows.push(json!({"label":if buckets.len()>1{format!("{name} · {label}")}else{label},"usedPercent":percent.min(100.0),"resetAt":number(&w["resetsAt"]).filter(|v|*v>0.0)}));
        }
    }
    (!windows.is_empty()).then(|| json!(windows))
}
pub fn project_codex(chat: &mut crate::chat::Chat, v: &Value) -> bool {
    use crate::chat_projection::{activity, now, s};
    let id = s(&chat.thread["id"]).to_owned();
    if v["method"] == "thread/tokenUsage/updated" {
        let u = v["params"]
            .get("tokenUsage")
            .unwrap_or(&v["params"]["usage"]);
        let used = number(&u["last"]["totalTokens"]);
        let max = number(&u["modelContextWindow"]);
        if let (Some(used), Some(max)) = (used, max.filter(|v| *v > 0.0)) {
            activity(
                chat,
                &format!("context-window-{id}"),
                "context-window.updated",
                "Context window updated",
                "info",
                json!({"usedTokens":used,"maxTokens":max,"lastUsedTokens":used,"compactsAutomatically":true}),
            );
        }
        true
    } else if v["id"] == "quota" || v["method"] == "account/rateLimits/updated" {
        let data = if v["id"] == "quota" {
            &v["result"]
        } else {
            &v["params"]
        };
        let windows = codex_limits(data);
        activity(
            chat,
            &format!("provider-usage-{id}"),
            "provider.usage.updated",
            "Usage limits updated",
            "info",
            json!({"providerName":"codex","providerLabel":"ChatGPT account","status":if windows.is_some(){"ready"}else{"unavailable"},"checkedAt":now(),"windows":windows.unwrap_or(json!([]))}),
        );
        true
    } else {
        false
    }
}
pub fn context(v: &Value) -> Option<Value> {
    let used = number(&v["totalTokens"])?;
    let max = number(v.get("rawMaxTokens").unwrap_or(&v["maxTokens"]))?;
    (max > 0.0).then(|| json!({"usedTokens":used,"maxTokens":max,"estimated":true}))
}
pub fn limits(v: &Value) -> Option<Value> {
    if v["rate_limits_available"] != true {
        return None;
    }
    let mut windows = Vec::new();
    for (key, label) in [
        ("five_hour", "5-hour"),
        ("seven_day", "Weekly"),
        ("seven_day_opus", "Weekly Opus"),
        ("seven_day_sonnet", "Weekly Sonnet"),
        ("seven_day_oauth_apps", "Weekly apps"),
    ] {
        add_window(&mut windows, label, &v["rate_limits"][key]);
    }
    for model in v["rate_limits"]["model_scoped"]
        .as_array()
        .into_iter()
        .flatten()
        .take(20)
    {
        let Some(name) = model["display_name"].as_str().filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.chars().all(|c| c.is_alphanumeric() || " ._-".contains(c))
        }) else {
            continue;
        };
        let label = format!("Weekly {name}");
        if !windows.iter().any(|w| {
            w["label"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(&label))
        }) {
            add_window(&mut windows, &label, model);
        }
    }
    (!windows.is_empty()).then(|| json!(windows))
}
fn add_window(windows: &mut Vec<Value>, label: &str, v: &Value) {
    let Some(percent) = number(&v["utilization"]).filter(|n| *n <= 100.0) else {
        return;
    };
    let reset = v["resets_at"]
        .as_str()
        .and_then(|s| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
        })
        .and_then(|d| {
            d.format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
    windows.push(json!({"label":label,"usedPercent":percent,"resetsAt":reset}));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_never_guesses_missing_limits_or_exposes_raw_account_data() {
        assert!(context(&json!({"totalTokens":42})).is_none());
        let c = context(
            &json!({"totalTokens":42,"maxTokens":200,"rawMaxTokens":100,"systemPrompt":"PRIVATE"}),
        )
        .unwrap();
        assert_eq!(c["maxTokens"], 100.0);
        assert!(!c.to_string().contains("PRIVATE"));
        let l=limits(&json!({"rate_limits_available":true,"account":"PRIVATE","rate_limits":{"five_hour":{"utilization":8,"resets_at":"2026-01-01T12:00:00Z"},"seven_day":{"utilization":101},"model_scoped":[{"display_name":"New model","utilization":5},{"display_name":"<private>","utilization":5}]}})).unwrap();
        assert_eq!(l.as_array().unwrap().len(), 2);
        assert_eq!(l[0]["usedPercent"], 8.0);
        assert!(!l.to_string().contains("PRIVATE"));
        assert!(limits(&json!({"rate_limits_available":false,"rate_limits":{}})).is_none());
        let c=codex_limits(&json!({"account":"PRIVATE","rateLimits":{"primary":{"usedPercent":20,"windowDurationMins":300,"resetsAt":1760000000}}})).unwrap();
        assert_eq!(c[0]["label"], "5-hour");
        assert_eq!(c[0]["usedPercent"], 20.0);
        assert!(!c.to_string().contains("PRIVATE"));
    }
}
