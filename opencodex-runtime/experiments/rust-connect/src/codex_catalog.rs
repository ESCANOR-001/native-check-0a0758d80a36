//! Composer inventory from Codex's read-only app-server APIs. Never expose config
//! values (credentials, commands, URLs) or start MCP servers just to build a menu.
use crate::agent::{Connection, Event};
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path, time::Duration};

async fn read(c: &mut Connection, id: &str, method: &str, params: Value) -> Option<Value> {
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .ok()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Event::Message(v)) = c.events.recv().await {
            if v["id"] == id {
                return v.get("result").cloned();
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

pub async fn discover(c: &mut Connection, cwd: &Path) -> Value {
    let skills = read(c, "catalog-skills", "skills/list", json!({"cwds":[cwd]})).await;
    let mut plugins = read(
        c,
        "catalog-plugins",
        "plugin/installed",
        json!({"cwds":[cwd]}),
    )
    .await;
    if plugins.is_none() {
        plugins = read(
            c,
            "catalog-plugins-fallback",
            "plugin/list",
            json!({"cwds":[cwd]}),
        )
        .await;
    }
    let config = read(
        c,
        "catalog-config",
        "config/read",
        json!({"cwd":cwd,"includeLayers":false}),
    )
    .await;
    project(
        &skills.unwrap_or_default(),
        &plugins.unwrap_or_default(),
        &config.unwrap_or_default(),
    )
}

fn text(v: &Value, limit: usize) -> Option<String> {
    v.as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.chars().take(limit).collect())
}
fn optional(out: &mut Value, key: &str, value: &Value) {
    if let Some(s) = text(value, 2000) {
        out[key] = json!(s);
    }
}
fn rows(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

pub fn project(skills: &Value, plugins: &Value, config: &Value) -> Value {
    let mut seen = HashSet::new();
    let skills: Vec<_> = rows(&skills["data"])
        .flat_map(|v| rows(&v["skills"]))
        .filter_map(|s| {
            let path = text(&s["path"], 4096)?;
            if !seen.insert(path.clone()) {
                return None;
            }
            let mut out =
                json!({"name":text(&s["name"],256)?,"path":path,"enabled":s["enabled"]!=false});
            for key in ["description", "scope", "displayName", "shortDescription"] {
                optional(
                    &mut out,
                    key,
                    if s[key].is_string() {
                        &s[key]
                    } else {
                        &s["interface"][key]
                    },
                );
            }
            Some(out)
        })
        .take(1024)
        .collect();
    seen.clear();
    let plugins: Vec<_> = rows(&plugins["marketplaces"]).flat_map(|m| rows(&m["plugins"]).map(move |p| (m,p)))
        .filter_map(|(m,p)| {
            if p["installed"]!=true { return None; }
            let id = text(&p["id"],512)?;
            if !seen.insert(id.clone()) { return None; }
            let mut out = json!({"id":id,"name":text(&p["name"],256)?,"enabled":p["enabled"]!=false,"installed":true,
                "version":text(&p["localVersion"],256),"skillsCount":rows(&p["skills"]).count(),"appsCount":rows(&p["apps"]).count(),
                "mcpServersCount":rows(&p["mcpServers"]).count(),"hooksCount":rows(&p["hooks"]).count()});
            optional(&mut out,"displayName",&p["interface"]["displayName"]);
            optional(&mut out,"description",&p["interface"]["shortDescription"]);
            optional(&mut out,"marketplace",&m["name"]);
            Some(out)
        }).take(512).collect();
    let servers: Vec<_> = config["config"]["mcp_servers"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !name.is_empty() && name.len() <= 256)
        .take(256)
        .map(|(name, c)| json!({"name":name,"enabled":c["enabled"]!=false}))
        .collect();
    json!({"skills":skills,"plugins":plugins,"mcpServers":servers})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_identities_disabled_entries_and_config_redaction() {
        let skill = json!({"name":"review","path":"/skills/review/SKILL.md","enabled":false,"interface":{"displayName":"Review"},"secret":"SECRET"});
        let result = project(
            &json!({"data":[{"skills":[skill.clone(),skill]}]}),
            &json!({"marketplaces":[{"name":"market","plugins":[{"id":"tool@market","name":"tool","installed":true,"enabled":false,"source":{"path":"SECRET"}},{"id":"uninstalled","name":"other","installed":false}]}]}),
            &json!({"config":{"mcp_servers":{"fixture":{"enabled":true,"env":{"TOKEN":"SECRET"},"url":"SECRET"},"off":{"enabled":false}}}}),
        );
        assert_eq!(result["skills"].as_array().unwrap().len(), 1);
        assert_eq!(result["skills"][0]["displayName"], "Review");
        assert_eq!(result["skills"][0]["enabled"], false);
        assert_eq!(result["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(result["plugins"][0]["id"], "tool@market");
        assert_eq!(result["mcpServers"][1]["enabled"], false);
        assert!(!result.to_string().contains("SECRET"));
    }
    #[test]
    fn unsupported_apis_leave_empty_catalog_without_fabricated_entries() {
        assert_eq!(
            project(&Value::Null, &Value::Null, &Value::Null),
            json!({"skills":[],"plugins":[],"mcpServers":[]})
        );
    }
}
