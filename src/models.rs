//! Model choices come from CLI capability responses, never guessed model lists.
use crate::chat_projection::{bounded, s};
use serde_json::{json, Value};
fn tier_id(value: &str) -> &str {
    match value {
        "standard" => "default",
        "fast" => "priority",
        _ => value,
    }
}
fn tier_descriptor(row: &Value) -> Option<Value> {
    // An explicit modern list (including []) overrides deprecated metadata.
    let raw = row
        .get("serviceTiers")
        .or_else(|| row.get("additionalSpeedTiers"))?
        .as_array()?;
    let mut options = vec![json!({"id":"default","label":"Standard","semantic":"standard"})];
    for item in raw.iter().take(32) {
        let id = tier_id(item.as_str().unwrap_or_else(|| s(&item["id"])));
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || options.iter().any(|o| o["id"] == id)
        {
            continue;
        }
        let label = match id {
            "priority" => "Fast",
            "ultrafast" => "Ultrafast",
            _ => item["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(id),
        };
        let mut option = json!({"id":id,"label":bounded(label,128)});
        if matches!(id, "priority" | "ultrafast") {
            option["semantic"] = json!(if id == "priority" {
                "fast"
            } else {
                "ultrafast"
            });
        }
        if let Some(description) = item["description"].as_str() {
            option["description"] = json!(bounded(description, 512));
        }
        options.push(option);
    }
    if options.len() == 1 {
        return None;
    }
    let default = tier_id(s(&row["defaultServiceTier"]));
    let default = if options.iter().any(|o| o["id"] == default) {
        default
    } else {
        "default"
    };
    for option in &mut options {
        option["isDefault"] = json!(option["id"] == default);
    }
    let mut descriptor = json!({"id":"serviceTier","label":"Service Tier","type":"select","currentValue":default,"options":options});
    if options.iter().any(|o| {
        o.get("semantic")
            .is_some_and(|v| v == "fast" || v == "ultrafast")
    }) {
        descriptor["semantic"] = json!("responseSpeed");
    }
    Some(descriptor)
}
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
        let mut descriptors = if efforts.is_empty() {
            json!([])
        } else {
            {
                efforts.insert(0, json!({"id":"auto","label":"Auto"}));
                json!([{"id":"effort","label":"Reasoning effort","type":"select","currentValue":"auto","options":efforts}])
            }
        };
        if let Some(tier) = codex.then(|| tier_descriptor(row)).flatten() {
            descriptors.as_array_mut().unwrap().push(tier);
        }
        let mut model = json!({"slug":slug,"name":if name.is_empty(){slug.to_owned()}else{bounded(name,256)},"isCustom":false,"isDefault":slug==default,"capabilities":{"optionDescriptors":descriptors}});
        // A shared session reports its wire model ID, while the picker lists
        // aliases. Keep the CLI's public mapping to validate unchanged models.
        if let Some(resolved) = row["resolvedModel"]
            .as_str()
            .filter(|v| !codex && !v.is_empty() && v.len() <= 256)
        {
            model["resolvedModel"] = json!(resolved);
        }
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
            if id != "effort" && !(id == "serviceTier" && instance == "codex") {
                return Err("Unsupported model option".into());
            }
            if id == "effort" && option["value"] == "auto" {
                continue;
            }
            let value = if id == "serviceTier" {
                json!(tier_id(s(&option["value"])))
            } else {
                option["value"].clone()
            };
            // Standard is the protocol's explicit reset even when a model
            // offers no alternate tiers. Keep an owner's null acknowledgement
            // sendable without advertising an unsupported paid speed.
            let allowed = (id == "serviceTier" && value == "default")
                || row["capabilities"]["optionDescriptors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|v| v["id"] == id)
                    .is_some_and(|v| {
                        v["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|o| o["id"] == value)
                    });
            if !allowed {
                return Err(if id == "effort" {
                    "This model does not support that effort"
                } else {
                    "This model has not reported that service tier"
                }
                .into());
            }
            options.push(json!({"id":id,"value":value}));
        }
    }
    Ok(json!({"instanceId":instance,"model":model,"options":options}))
}
/// The existing folder picker emits this historical Codex default for both
/// agents, without offering a model choice. It is a creation placeholder, not
/// an explicit chat model. Never apply this compatibility rule to model edits.
pub fn project_selection(
    catalog: &Value,
    requested: &Value,
    instance: &str,
    creating: bool,
) -> Result<Value, String> {
    if creating
        && requested["instanceId"] == "codex"
        && requested["model"] == "gpt-5.4"
        && requested
            .get("options")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
    {
        let default = catalog
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["isDefault"] == true)
            .ok_or("This agent has not reported a default model")?;
        return selection(
            catalog,
            &json!({"instanceId":instance,"model":default["slug"]}),
            &Value::Null,
            instance,
        );
    }
    selection(catalog, requested, &Value::Null, instance)
}
/// The catalog row that reports `model`, by alias or by the wire model it
/// resolves to. Used only to validate options of an unchanged native model.
pub fn row<'a>(catalog: &'a Value, model: &str) -> Option<&'a Value> {
    let rows = catalog.as_array()?;
    rows.iter()
        .find(|v| v["slug"] == model)
        .or_else(|| rows.iter().find(|v| v["resolvedModel"] == model))
}
pub fn effort(selection: &Value) -> Option<&str> {
    selection["options"]
        .as_array()?
        .iter()
        .find(|v| v["id"] == "effort")?["value"]
        .as_str()
}
/// Omission preserves the CLI's default. An explicit Standard choice sends
/// `default`, so an earlier Fast setting cannot leak into the next turn.
pub fn codex_tier(params: &mut Value, selection: &Value) {
    if let Some(tier) = selection["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["id"] == "serviceTier")
        .and_then(|o| o["value"].as_str())
    {
        params["serviceTier"] = json!(tier_id(tier));
    }
}
/// Reflect the owning server's settings without erasing fields omitted by an
/// older response. Explicit null tier means Standard; it is not missing data.
pub fn codex_ack(current: &Value, response: &Value) -> Value {
    let model = response["model"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .unwrap_or_else(|| s(&current["model"]));
    let mut options = if current["model"] == model {
        current["options"].as_array().cloned().unwrap_or_default()
    } else {
        vec![]
    };
    for (field, id) in [
        ("reasoningEffort", "effort"),
        ("serviceTier", "serviceTier"),
    ] {
        if let Some(value) = response
            .get(field)
            .filter(|v| v.is_null() || v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 64))
        {
            options.retain(|o| o["id"] != id);
            if id == "serviceTier" {
                options.push(json!({"id":id,"value":tier_id(value.as_str().unwrap_or("default"))}));
            } else if !value.is_null() {
                options.push(json!({"id":id,"value":value}));
            }
        }
    }
    json!({"instanceId":"codex","model":model,"options":options})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_tiers_are_reported_only_by_codex_and_modern_empty_is_authoritative() {
        let rows = json!([{"model":"test","value":"test","serviceTiers":[{"id":"fast","name":"Fast","description":"Provider speed"},{"id":"priority"},{"id":"ultrafast"}],"defaultServiceTier":"fast","additionalSpeedTiers":["unreported"]}]);
        let c = catalog(&rows, true, "test");
        let d = &c[0]["capabilities"]["optionDescriptors"][0];
        assert_eq!(d["id"], "serviceTier");
        assert_eq!(d["semantic"], "responseSpeed");
        assert_eq!(d["currentValue"], "priority");
        assert_eq!(d["options"].as_array().unwrap().len(), 3);
        assert!(
            catalog(&rows, false, "test")[0]["capabilities"]["optionDescriptors"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        for modern in [json!([]), json!(null), json!("invalid")] {
            let mut rows = rows.clone();
            rows[0]["serviceTiers"] = modern;
            assert!(
                catalog(&rows, true, "test")[0]["capabilities"]["optionDescriptors"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let old = catalog(
            &json!([{"model":"test","additionalSpeedTiers":["fast"]}]),
            true,
            "test",
        );
        assert_eq!(
            old[0]["capabilities"]["optionDescriptors"][0]["options"][1]["id"],
            "priority"
        );
    }
    #[test]
    fn service_tier_selection_preserves_inherit_standard_and_validates_provider_choices() {
        let c = catalog(
            &json!([{"model":"test","serviceTiers":[{"id":"priority"}]}]),
            true,
            "test",
        );
        let current = json!({"model":"test"});
        let mut params = json!({});
        codex_tier(&mut params, &current);
        assert!(params.get("serviceTier").is_none());
        for (input, expected) in [
            ("fast", "priority"),
            ("priority", "priority"),
            ("standard", "default"),
            ("default", "default"),
        ] {
            let requested = json!({"model":"test","options":[{"id":"serviceTier","value":input}]});
            let selected = selection(&c, &requested, &current, "codex").unwrap();
            codex_tier(&mut params, &selected);
            assert_eq!(params["serviceTier"], expected);
            assert!(selection(&c, &requested, &current, "claude-code").is_err());
        }
        for value in [json!("unreported"), json!("auto"), json!(null), json!(42)] {
            assert!(selection(
                &c,
                &json!({"options":[{"id":"serviceTier","value":value}]}),
                &current,
                "codex"
            )
            .is_err());
        }
        assert!(selection(&c,&json!({"options":[{"id":"serviceTier","value":"fast"},{"id":"serviceTier","value":"priority"}]}),&json!({"model":"test"}),"codex").is_err());
    }
    #[test]
    fn owning_codex_ack_retains_omitted_settings_but_honors_explicit_null_and_model_changes() {
        let current = json!({"instanceId":"codex","model":"test","options":[{"id":"effort","value":"high"},{"id":"serviceTier","value":"priority"}]});
        assert_eq!(codex_ack(&current, &json!({"model":"test"})), current);
        let cleared = codex_ack(
            &current,
            &json!({"model":"test","serviceTier":null,"reasoningEffort":null}),
        );
        assert_eq!(
            cleared["options"],
            json!([{"id":"serviceTier","value":"default"}])
        );
        assert_eq!(
            codex_ack(&current, &json!({"model":"other"}))["options"],
            json!([])
        );
        assert_eq!(codex_ack(&current, &json!({"serviceTier":"fast"})), current);
    }
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
        let standard_only = catalog(&json!([{"model":"test","serviceTiers":[]}]), true, "test");
        assert!(selection(
            &standard_only,
            &json!({"options":[{"id":"serviceTier","value":"default"}]}),
            &json!({"model":"test"}),
            "codex"
        )
        .is_ok());
        assert!(selection(
            &standard_only,
            &json!({"options":[{"id":"serviceTier","value":"priority"}]}),
            &json!({"model":"test"}),
            "codex"
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
