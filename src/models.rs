//! Model choices come from CLI capability responses, never guessed model lists.
use crate::chat_projection::{bounded, s};
use serde_json::{json, Value};
pub fn fallback(model: &str) -> Value {
    json!([{"slug":model,"name":model,"isCustom":false,"isDefault":true,"capabilities":{"optionDescriptors":[]}}])
}
pub fn catalog(rows: &Value, codex: bool, default: &str) -> Value {
    let mut models = vec![];
    for row in rows.as_array().into_iter().flatten().take(128) {
        let slug = s(&row[if codex { "model" } else { "value" }]);
        if slug.is_empty() || slug.len() > 256 {
            continue;
        }
        let name = s(&row["displayName"]);
        let mut efforts: Vec<_> = row[if codex {
            "supportedReasoningEfforts"
        } else {
            "supportedEffortLevels"
        }]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            if codex {
                v["reasoningEffort"].as_str()
            } else {
                v.as_str()
            }
        })
        .filter(|v| {
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(v)
        })
        .take(12)
        .map(|v| json!({"id":v,"label":v}))
        .collect();
        let descriptors = if efforts.is_empty() {
            json!([])
        } else {
            {
                efforts.insert(0, json!({"id":"auto","label":"Auto"}));
                json!([{"id":"effort","label":"Reasoning effort","type":"select","currentValue":"auto","options":efforts}])
            }
        };
        let mut model = json!({"slug":slug,"name":if name.is_empty(){slug.to_owned()}else{bounded(name,256)},"isCustom":false,"isDefault":slug==default,"capabilities":{"optionDescriptors":descriptors}});
        if !codex && slug == "default" {
            let resolved = s(&row["resolvedModel"]);
            let matched = rows.as_array().into_iter().flatten().find(|r| {
                r["value"] != "default"
                    && !resolved.is_empty()
                    && (r["value"] == resolved || r["resolvedModel"] == resolved)
            });
            let actual = matched
                .and_then(|v| v["displayName"].as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or(if resolved.is_empty() || resolved == "default" {
                    "Default model"
                } else {
                    resolved
                });
            model["name"] = json!(format!("{} (default)", bounded(actual, 256)));
            model["shortName"] = json!(bounded(actual, 256));
        }
        models.push(model);
    }
    if !models.iter().any(|v| v["slug"] == default) {
        models.insert(0, fallback(default)[0].clone());
    }
    json!(models)
}
pub fn selection(
    catalog: &Value,
    requested: &Value,
    current: &Value,
    instance: &str,
) -> Result<Value, String> {
    if requested["instanceId"]
        .as_str()
        .is_some_and(|v| v != instance)
    {
        return Err("Model belongs to a different agent".into());
    }
    let model = requested["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| s(&current["model"]));
    let row = catalog
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["slug"] == model)
        .ok_or("Select a model reported by this agent")?;
    let raw = requested.get("options").or_else(|| {
        if model == s(&current["model"]) {
            current.get("options")
        } else {
            None
        }
    });
    let mut options = vec![];
    if let Some(raw) = raw {
        let requested = raw.as_array().ok_or("Model options must be an array")?;
        if requested.len() > 8 {
            return Err("Too many model options".into());
        }
        for option in requested {
            let id = s(&option["id"]);
            if options.iter().any(|v: &Value| v["id"] == id) {
                return Err("Duplicate model option".into());
            }
            if id != "effort" {
                return Err("Unsupported model option".into());
            }
            if option["value"] == "auto" {
                continue;
            }
            let allowed = row["capabilities"]["optionDescriptors"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["id"] == id)
                .is_some_and(|v| {
                    v["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|o| o["id"] == option["value"])
                });
            if !allowed {
                return Err("This model does not support that effort".into());
            }
            options.push(json!({"id":id,"value":option["value"]}));
        }
    }
    Ok(json!({"instanceId":instance,"model":model,"options":options}))
}
pub fn effort(selection: &Value) -> Option<&str> {
    selection["options"]
        .as_array()?
        .iter()
        .find(|v| v["id"] == "effort")?["value"]
        .as_str()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_actual_model_capabilities_and_rejects_cross_agent_settings() {
        let c = catalog(
            &json!([{"value":"sonnet","displayName":"Sonnet","supportedEffortLevels":["high","low"]}]),
            false,
            "sonnet",
        );
        let selected = selection(
            &c,
            &json!({"model":"sonnet","options":[{"id":"effort","value":"high"}]}),
            &json!({"model":"sonnet"}),
            "claude-code",
        )
        .unwrap();
        assert_eq!(effort(&selected), Some("high"));
        assert!(selection(&c, &json!({"model":"unknown"}), &selected, "claude-code").is_err());
        assert!(selection(&c, &json!({"instanceId":"codex"}), &selected, "claude-code").is_err());
        assert!(selection(
            &c,
            &json!({"options":[{"id":"effort","value":"ultra"}]}),
            &selected,
            "claude-code"
        )
        .is_err());
        assert_eq!(
            effort(
                &selection(
                    &c,
                    &json!({"options":[{"id":"effort","value":"auto"}]}),
                    &selected,
                    "claude-code"
                )
                .unwrap()
            ),
            None
        );
    }
    #[test]
    fn resolved_default_is_named_without_forcing_the_concrete_model() {
        let c = catalog(
            &json!([{"value":"default","displayName":"Default (recommended)","resolvedModel":"real-model"},{"value":"alias","resolvedModel":"real-model","displayName":"Actual model"}]),
            false,
            "default",
        );
        assert_eq!(c[0]["name"], "Actual model (default)");
        assert_eq!(c[0]["shortName"], "Actual model");
        assert_eq!(c[0]["slug"], "default");
    }
}
