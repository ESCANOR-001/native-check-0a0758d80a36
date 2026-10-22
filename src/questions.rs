//! Convert the two CLI question protocols without treating answers as approvals.
use crate::chat_projection::s;
use serde_json::{json, Map, Value};

pub fn is_input(v: &Value) -> bool {
    is_question(v) || crate::elicitation::is_elicitation(v)
}
pub fn is_question(v: &Value) -> bool {
    v["method"] == "item/tool/requestUserInput"
        || (v["type"] == "control_request"
            && v["request"]["subtype"] == "can_use_tool"
            && v["request"]["tool_name"] == "AskUserQuestion")
}
pub fn questions(v: &Value) -> Result<Value, String> {
    if crate::elicitation::is_elicitation(v) {
        return crate::elicitation::questions(v).map_err(str::to_owned);
    }
    let codex = v["method"] == "item/tool/requestUserInput";
    let rows = if codex {
        &v["params"]["questions"]
    } else {
        &v["request"]["input"]["questions"]
    };
    let rows = rows
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 16)
        .ok_or("Invalid agent questions")?;
    let mut result = vec![];
    for (i, q) in rows.iter().enumerate() {
        let id = if codex {
            s(&q["id"]).to_owned()
        } else {
            format!("question-{i}")
        };
        if id.is_empty()
            || id.len() > 256
            || result.iter().any(|q: &Value| q["id"] == id)
            || (!codex
                && result
                    .iter()
                    .any(|previous| previous["question"] == q["question"]))
            || s(&q["question"]).trim().is_empty()
            || q.to_string().len() > 8192
        {
            return Err("Invalid agent question details".into());
        }
        let options: Vec<_> = q["options"].as_array().into_iter().flatten().take(32)
            .filter(|o| !s(&o["label"]).is_empty())
            .map(|o| json!({"label":o["label"],"description":o["description"].as_str().unwrap_or("")})).collect();
        result.push(
            json!({"id":id,"header":q["header"].as_str().unwrap_or("Question"),
            "question":q["question"],"options":options,"multiSelect":q["multiSelect"]==true,
            "isOther":!codex || q["isOther"]==true,"isSecret":q["isSecret"]==true}),
        );
    }
    Ok(json!(result))
}
pub fn answer(v: &Value, answers: &Value) -> Result<Value, String> {
    if crate::elicitation::is_elicitation(v) {
        return crate::elicitation::answer(v, answers).map_err(str::to_owned);
    }
    if !is_question(v) {
        return Err("This request is not a question".into());
    }
    let qs = questions(v)?;
    let qs = qs.as_array().unwrap();
    let submitted = answers
        .as_object()
        .filter(|a| a.len() == qs.len())
        .ok_or("Answer every question")?;
    if answers.to_string().len() > 32000 {
        return Err("Answers exceed 32,000 bytes".into());
    }
    let codex = v["method"] == "item/tool/requestUserInput";
    let mut mapped = Map::new();
    for q in qs {
        let raw = submitted.get(s(&q["id"])).ok_or("Answer every question")?;
        let values = if raw.is_string() {
            vec![raw.clone()]
        } else {
            raw.as_array()
                .or_else(|| raw["answers"].as_array())
                .cloned()
                .ok_or("Invalid answer")?
        };
        if values.is_empty()
            || values.len() > 32
            || values
                .iter()
                .any(|v| v.as_str().is_none_or(|s| s.trim().is_empty()))
        {
            return Err("Enter a non-empty answer".into());
        }
        if codex {
            mapped.insert(s(&q["id"]).into(), json!({"answers":values}));
        } else {
            mapped.insert(
                s(&q["question"]).into(),
                json!(values
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(", ")),
            );
        }
    }
    Ok(if codex {
        json!({"id":v["id"],"result":{"answers":mapped}})
    } else {
        let mut input = v["request"]["input"].clone();
        input["answers"] = json!(mapped);
        json!({"type":"control_response","response":{"subtype":"success","request_id":v["request_id"],"response":{"behavior":"allow","updatedInput":input}}})
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_exact_ids_and_requires_all_answers() {
        let c = json!({"id":42,"method":"item/tool/requestUserInput","params":{"questions":[{"id":"q","question":"Pick a color","options":[{"label":"Blue"}]}]}});
        let a = answer(&c, &json!({"q":{"answers":["Blue"]}})).unwrap();
        assert_eq!(a["id"], 42);
        assert_eq!(a["result"]["answers"]["q"]["answers"][0], "Blue");
        assert!(answer(&c, &json!({})).is_err());
        assert!(answer(&c, &json!({"q":[""]})).is_err());
        let c = json!({"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Pick a color"}]}}});
        let a = answer(&c, &json!({"question-0":["Blue"]})).unwrap();
        assert_eq!(
            a["response"]["response"]["updatedInput"]["answers"]["Pick a color"],
            "Blue"
        );
        assert!(answer(&json!({"request":{"tool_name":"Write"}}), &json!({})).is_err());
        let duplicate = json!({"type":"control_request","request_id":"duplicate","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Same question"},{"question":"Same question"}]}}});
        assert!(questions(&duplicate).is_err());
    }
}
