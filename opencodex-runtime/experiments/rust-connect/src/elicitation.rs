//! Bounded MCP forms shared by the two native protocols. Unsupported schemas
//! are canceled explicitly; constraints are never silently dropped.
use crate::chat_projection::s;
use serde_json::{json, Map, Value};

const UNSUPPORTED: &str = "This MCP request needs a form the Rust backend cannot validate. The request was canceled; retry it in a supported client.";
const BROWSER: &str = "This MCP request needs a browser or device-verification step. Complete it on the PC, then retry the tool.";
type Result<T> = std::result::Result<T, &'static str>;
struct Field {
    key: String,
    definition: Value,
    choices: Vec<(String, Value)>,
    optional: bool,
    skip: String,
    question: Value,
}
struct Form {
    fields: Vec<Field>,
    context: String,
}

pub fn is_elicitation(v: &Value) -> bool {
    matches!(
        s(&v["method"]),
        "mcpServer/elicitation/request" | "item/tool/requestMcpServerElicitation"
    ) || (v["type"] == "control_request" && v["request"]["subtype"] == "elicitation")
}
pub fn response(v: &Value, content: Value) -> Value {
    if v["method"].is_string() {
        json!({"id":v["id"],"result":content})
    } else {
        json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":content}})
    }
}
fn text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(2000)
        .collect()
}
fn keys(v: &Value, allowed: &[&str]) -> Result<()> {
    if v.as_object()
        .is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(UNSUPPORTED);
    }
    Ok(())
}
fn range(f: &Value, low: &str, high: &str, integral: bool) -> Result<()> {
    for key in [low, high] {
        if let Some(n) = f.get(key) {
            if n.as_f64().is_none_or(|v| !v.is_finite()) || (integral && n.as_u64().is_none()) {
                return Err(UNSUPPORTED);
            }
        }
    }
    if let (Some(a), Some(b)) = (f[low].as_f64(), f[high].as_f64()) {
        if a > b {
            return Err(UNSUPPORTED);
        }
    }
    Ok(())
}
fn choices(v: &Value, named_key: &str, names: &Value) -> Result<Vec<(String, Value)>> {
    let mut out = vec![];
    if let Some(values) = v.get(named_key) {
        if v.get("enum").is_some() || !names.is_null() {
            return Err(UNSUPPORTED);
        }
        for option in values.as_array().ok_or(UNSUPPORTED)? {
            keys(option, &["const", "title"])?;
            let value = option["const"].as_str().ok_or(UNSUPPORTED)?;
            let label = option["title"].as_str().ok_or(UNSUPPORTED)?;
            out.push((text(label), json!(value)));
        }
    } else if let Some(values) = v.get("enum") {
        let values = values.as_array().ok_or(UNSUPPORTED)?;
        if !names.is_null() && names.as_array().is_none_or(|n| n.len() != values.len()) {
            return Err(UNSUPPORTED);
        }
        for (i, value) in values.iter().enumerate() {
            let value = value.as_str().ok_or(UNSUPPORTED)?;
            let label = if names.is_null() {
                value
            } else {
                names[i].as_str().ok_or(UNSUPPORTED)?
            };
            out.push((text(label), json!(value)));
        }
    } else if !names.is_null() {
        return Err(UNSUPPORTED);
    }
    if out.len() > 32
        || ((v.get(named_key).is_some() || v.get("enum").is_some()) && out.is_empty())
        || out.iter().enumerate().any(|(i, (label, value))| {
            label.trim().is_empty() || out[..i].iter().any(|(l, v)| l == label || v == value)
        })
    {
        return Err(UNSUPPORTED);
    }
    Ok(out)
}
fn form(v: &Value) -> Result<Form> {
    if !is_elicitation(v) {
        return Err(UNSUPPORTED);
    }
    let codex = v["method"].is_string();
    let r = if codex { &v["params"] } else { &v["request"] };
    if !r["mode"].is_null() && r["mode"] != "form" {
        return Err(BROWSER);
    }
    let schema = if codex {
        &r["requestedSchema"]
    } else {
        &r["requested_schema"]
    };
    if schema.to_string().len() > 32_000 {
        return Err(UNSUPPORTED);
    }
    keys(
        schema,
        &[
            "type",
            "properties",
            "required",
            "title",
            "description",
            "$schema",
            "additionalProperties",
        ],
    )?;
    if schema["type"] != "object"
        || schema
            .get("additionalProperties")
            .is_some_and(|v| !v.is_boolean())
    {
        return Err(UNSUPPORTED);
    }
    let properties = schema["properties"].as_object().ok_or(UNSUPPORTED)?;
    if properties.len() > 16 {
        return Err(UNSUPPORTED);
    }
    let required = match schema.get("required") {
        None => vec![],
        Some(v) => v
            .as_array()
            .ok_or(UNSUPPORTED)?
            .iter()
            .map(|v| v.as_str().ok_or(UNSUPPORTED))
            .collect::<Result<Vec<_>>>()?,
    };
    if required
        .iter()
        .enumerate()
        .any(|(i, k)| !properties.contains_key(*k) || required[..i].contains(k))
    {
        return Err(UNSUPPORTED);
    }
    let server = if codex {
        s(&r["serverName"])
    } else {
        s(&r["mcp_server_name"])
    };
    let context = format!("{}: {}", text(server), text(s(&r["message"])));
    let mut fields = vec![];
    for (i, (key, f)) in properties.iter().enumerate() {
        if key.is_empty() || key.len() > 256 {
            return Err(UNSUPPORTED);
        }
        for label in ["title", "description"] {
            if f.get(label).is_some_and(|v| !v.is_string()) {
                return Err(UNSUPPORTED);
            }
        }
        let mut allowed = vec!["type", "title", "description", "default"];
        let choices = match s(&f["type"]) {
            "string" => {
                allowed.extend(["enum", "enumNames", "oneOf", "minLength", "maxLength"]);
                range(f, "minLength", "maxLength", true)?;
                choices(f, "oneOf", &f["enumNames"])?
            }
            "boolean" => vec![("Yes".into(), json!(true)), ("No".into(), json!(false))],
            "number" | "integer" => {
                allowed.extend(["minimum", "maximum"]);
                range(f, "minimum", "maximum", false)?;
                vec![]
            }
            "array" => {
                allowed.extend(["items", "minItems", "maxItems"]);
                range(f, "minItems", "maxItems", true)?;
                keys(&f["items"], &["type", "enum", "anyOf"])?;
                if f["items"]["type"] != "string" {
                    return Err(UNSUPPORTED);
                }
                let c = choices(&f["items"], "anyOf", &Value::Null)?;
                if c.is_empty() {
                    return Err(UNSUPPORTED);
                }
                c
            }
            _ => return Err(UNSUPPORTED),
        };
        keys(f, &allowed)?;
        let optional = !required.contains(&key.as_str());
        if choices.len() + usize::from(optional) > 32 {
            return Err(UNSUPPORTED);
        }
        let mut skip = "Leave empty".to_owned();
        while choices.iter().any(|(l, _)| l == &skip) {
            skip.push_str(" (optional)");
        }
        let label = text(f["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(key));
        let mut options: Vec<_> = choices
            .iter()
            .map(|(l, _)| json!({"label":l,"description":""}))
            .collect();
        if optional {
            options.push(json!({"label":skip,"description":""}));
        }
        let question = json!({"id":format!("mcp-field-{i}"),"header":label.chars().take(80).collect::<String>(),
            "question":format!("{}{}{}{}",if i==0 {format!("{context}\n\n")}else{String::new()},label,if optional {" (optional)"}else{""},if f["description"].is_string(){format!("\n{}",text(s(&f["description"])))}else{String::new()}),
            "options":options,"multiSelect":f["type"]=="array","isOther":choices.is_empty(),"isSecret":false});
        fields.push(Field {
            key: key.clone(),
            definition: f.clone(),
            choices,
            optional,
            skip,
            question,
        });
    }
    Ok(Form { fields, context })
}
pub fn questions(v: &Value) -> Result<Value> {
    let f = form(v)?;
    if f.fields.is_empty() {
        Ok(
            json!([{"id":"mcp-confirm","header":"MCP request","question":f.context,"options":[{"label":"Continue","description":""},{"label":"Decline","description":""}],"multiSelect":false,"isOther":false,"isSecret":false}]),
        )
    } else {
        Ok(json!(f
            .fields
            .iter()
            .map(|f| &f.question)
            .collect::<Vec<_>>()))
    }
}
fn selected(v: &Value) -> Result<Vec<&str>> {
    let values = if let Some(s) = v.as_str() {
        vec![s]
    } else {
        v.as_array()
            .or_else(|| v["answers"].as_array())
            .ok_or("Answer every MCP question")?
            .iter()
            .map(|v| v.as_str().ok_or("Invalid MCP answer"))
            .collect::<Result<Vec<_>>>()?
    };
    if values.is_empty()
        || values.len() > 32
        || values.iter().any(|s| s.trim().is_empty() || s.len() > 4000)
    {
        return Err("Enter a non-empty MCP answer of at most 4,000 bytes");
    }
    Ok(values)
}
fn within(n: f64, f: &Value, low: &str, high: &str) -> Result<()> {
    if f[low].as_f64().is_some_and(|v| n < v) || f[high].as_f64().is_some_and(|v| n > v) {
        return Err("An MCP answer is outside the permitted range");
    }
    Ok(())
}
pub fn answer(v: &Value, answers: &Value) -> Result<Value> {
    let form = form(v)?;
    let submitted = answers.as_object().ok_or("Answer every MCP question")?;
    if answers.to_string().len() > 32_000 || submitted.len() != form.fields.len().max(1) {
        return Err("Answer every MCP question without extra fields");
    }
    if form.fields.is_empty() {
        let values = selected(&answers["mcp-confirm"])?;
        return Ok(response(
            v,
            match values.as_slice() {
                ["Continue"] => json!({"action":"accept","content":{}}),
                ["Decline"] => json!({"action":"decline"}),
                _ => return Err("Choose Continue or Decline"),
            },
        ));
    }
    let mut content = Map::new();
    for field in form.fields {
        let values = selected(&answers[s(&field.question["id"])])?;
        if field.optional && values.contains(&field.skip.as_str()) {
            if values.len() != 1 {
                return Err("Choose a value or Leave empty");
            }
            continue;
        }
        let f = &field.definition;
        if f["type"] != "array" && values.len() != 1 {
            return Err("Choose one value for this MCP question");
        }
        let mut converted = vec![];
        for value in values {
            let value = if !field.choices.is_empty() {
                field
                    .choices
                    .iter()
                    .find(|(l, _)| l == value)
                    .ok_or("Choose a listed MCP value")?
                    .1
                    .clone()
            } else if f["type"] == "number" || f["type"] == "integer" {
                let n: Value =
                    serde_json::from_str(value.trim()).map_err(|_| "Enter a valid number")?;
                let num = n
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or("Enter a valid number")?;
                if f["type"] == "integer"
                    && (num.fract() != 0.0 || num.abs() > 9_007_199_254_740_991.0)
                {
                    return Err("Enter an integer within the supported numeric range");
                }
                within(num, f, "minimum", "maximum")?;
                n
            } else {
                json!(value)
            };
            if f["type"] == "string" {
                within(
                    value.as_str().unwrap().chars().count() as f64,
                    f,
                    "minLength",
                    "maxLength",
                )?;
            }
            converted.push(value);
        }
        if f["type"] == "array" {
            within(converted.len() as f64, f, "minItems", "maxItems")?;
            content.insert(field.key, json!(converted));
        } else {
            content.insert(field.key, converted.remove(0));
        }
    }
    Ok(response(v, json!({"action":"accept","content":content})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(codex: bool, schema: Value) -> Value {
        if codex {
            json!({"id":44,"method":"mcpServer/elicitation/request","params":{"serverName":"fixture","mode":"form","message":"Provide test input","requestedSchema":schema}})
        } else {
            json!({"type":"control_request","request_id":"form-1","request":{"subtype":"elicitation","mcp_server_name":"fixture","mode":"form","message":"Provide test input","requested_schema":schema}})
        }
    }
    #[test]
    fn typed_forms_preserve_values_and_validate_every_answer() {
        for codex in [true, false] {
            let v = request(
                codex,
                json!({"type":"object","properties":{
                "a":{"type":"string","enum":["blue","green"],"enumNames":["Blue","Green"]},
                "b":{"type":"boolean"},"c":{"type":"integer","minimum":1,"maximum":4},
                "d":{"type":"array","items":{"type":"string","anyOf":[{"const":"alpha","title":"A"},{"const":"beta","title":"B"}]},"minItems":1,"maxItems":2},
                "e":{"type":"string","minLength":2,"maxLength":3},"f":{"type":"number","minimum":-2.0,"maximum":2.0}
            },"required":["a","b","c","d","e","f"]}),
            );
            assert_eq!(questions(&v).unwrap().as_array().unwrap().len(), 6);
            let answers = json!({"mcp-field-0":{"answers":["Blue"]},"mcp-field-1":["No"],"mcp-field-2":"3","mcp-field-3":["A","B"],"mcp-field-4":["é好"],"mcp-field-5":["1.5"]});
            let r = answer(&v, &answers).unwrap();
            let r = if codex {
                &r["result"]
            } else {
                &r["response"]["response"]
            };
            assert_eq!(
                *r,
                json!({"action":"accept","content":{"a":"blue","b":false,"c":3,"d":["alpha","beta"],"e":"é好","f":1.5}})
            );
            for (key, bad) in [
                ("mcp-field-0", json!(["unlisted"])),
                ("mcp-field-1", json!(["Yes", "No"])),
                ("mcp-field-2", json!(["3.5"])),
                ("mcp-field-2", json!(["5"])),
                ("mcp-field-3", json!(["A", "B", "A"])),
                ("mcp-field-4", json!(["a"])),
                ("mcp-field-5", json!(["NaN"])),
                ("mcp-field-5", json!(["1e309"])),
            ] {
                let mut a = answers.clone();
                a[key] = bad;
                assert!(answer(&v, &a).is_err(), "{key}");
            }
            let mut a = answers.clone();
            a["extra"] = json!(["secret"]);
            assert!(answer(&v, &a).is_err());
            let mut a = answers.clone();
            a.as_object_mut().unwrap().remove("mcp-field-0");
            assert!(answer(&v, &a).is_err());
        }
    }
    #[test]
    fn optional_values_and_empty_forms_require_explicit_choices() {
        let v = request(
            false,
            json!({"type":"object","properties":{"color":{"type":"string","enum":["Leave empty","Blue"]}}}),
        );
        let q = questions(&v).unwrap();
        assert_eq!(q[0]["options"][2]["label"], "Leave empty (optional)");
        assert_eq!(
            answer(&v, &json!({"mcp-field-0":["Leave empty (optional)"]})).unwrap()["response"]
                ["response"]["content"],
            json!({})
        );
        assert!(answer(
            &v,
            &json!({"mcp-field-0":["Blue","Leave empty (optional)"]})
        )
        .is_err());
        assert_eq!(
            answer(&v, &json!({"mcp-field-0":["Leave empty"]})).unwrap()["response"]["response"]
                ["content"]["color"],
            "Leave empty"
        );
        let v = request(true, json!({"type":"object","properties":{}}));
        assert_eq!(questions(&v).unwrap()[0]["id"], "mcp-confirm");
        assert_eq!(
            answer(&v, &json!({"mcp-confirm":["Decline"]})).unwrap()["result"]["action"],
            "decline"
        );
        assert!(answer(&v, &json!({})).is_err());
    }
    #[test]
    fn unsupported_constraints_modes_and_ambiguous_choices_are_not_ignored() {
        for definition in [
            json!({"type":"string","pattern":".*"}),
            json!({"type":"string","format":"email"}),
            json!({"type":"integer","minimum":5,"maximum":1}),
            json!({"type":"string","minLength":-1}),
            json!({"type":"object","properties":{}}),
            json!({"type":"string","enum":["a","b"],"enumNames":["Same","Same"]}),
            json!({"type":"array","items":{"type":"string"}}),
            json!({"type":"number","multipleOf":2}),
        ] {
            let v = request(
                true,
                json!({"type":"object","properties":{"field":definition},"required":["field"]}),
            );
            assert!(questions(&v).is_err());
        }
        for codex in [true, false] {
            let mut v = request(codex, json!({"type":"object","properties":{}}));
            let r = if codex {
                &mut v["params"]
            } else {
                &mut v["request"]
            };
            r["mode"] = json!("url");
            r["url"] = json!("https://example.invalid/private");
            assert_eq!(questions(&v).unwrap_err(), BROWSER);
            let (reply, notice) = crate::controls::automatic(&v, codex).unwrap();
            assert!(notice.is_some());
            assert!(!reply.to_string().contains("private"));
            assert_eq!(
                if codex {
                    &reply["result"]["action"]
                } else {
                    &reply["response"]["response"]["action"]
                },
                "cancel"
            );
        }
    }
}
