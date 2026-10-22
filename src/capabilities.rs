//! Public names/descriptions only: never expose MCP credentials, environment,
//! executable paths, plugin paths or private initialization metadata.
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Optional, explicitly selected configuration for this isolated backend.
/// Normal Desktop configuration and implicit user/project settings stay excluded.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    #[serde(default)]
    pub mcp_config: Option<PathBuf>,
    #[serde(default)]
    pub plugin_dirs: Vec<PathBuf>,
}
impl ToolConfig {
    pub fn validate(&self, workspace: &Path, profile: &Path) -> Result<(), String> {
        if self.plugin_dirs.len() > 8 {
            return Err("At most eight explicit plugin folders are supported".into());
        }
        let roots = [workspace.canonicalize(), profile.canonicalize()];
        for path in self.mcp_config.iter().chain(self.plugin_dirs.iter()) {
            let canonical = path
                .canonicalize()
                .map_err(|_| "Tool configuration does not exist")?;
            if !path.is_absolute()
                || !roots
                    .iter()
                    .filter_map(|r| r.as_ref().ok())
                    .any(|r| canonical.starts_with(r))
            {
                return Err(
                    "Tool configuration must belong to the isolated workspace or profile".into(),
                );
            }
        }
        if self.mcp_config.as_ref().is_some_and(|p| !p.is_file())
            || self.plugin_dirs.iter().any(|p| !p.is_dir())
        {
            return Err("Select an MCP configuration file and plugin directories".into());
        }
        Ok(())
    }
    pub fn apply(&self, args: &mut Vec<String>) {
        if let Some(file) = &self.mcp_config {
            if let Some(index) = args.iter().position(|a| a == "--mcp-config") {
                args[index + 1] = file.to_string_lossy().into_owned();
            }
        }
        for folder in &self.plugin_dirs {
            args.extend(["--plugin-dir".into(), folder.to_string_lossy().into_owned()]);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.mcp_config.is_none() && self.plugin_dirs.is_empty()
    }
}
fn name(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| {
        !s.is_empty()
            && s.len() <= 200
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:@/-".contains(&b))
    })
}
fn text(v: &Value) -> String {
    v.as_str()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(256)
        .collect()
}
fn rows(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten().take(128)
}
pub fn seed(seed: &mut Value, event: &Value) {
    let init = &event["response"]["response"];
    if let Some(commands) = init["commands"].as_array() {
        seed["commands"] = json!(commands
            .iter()
            .take(128)
            .filter_map(|c| Some(
                json!({"name":name(&c["name"])?,"description":text(&c["description"])})
            ))
            .collect::<Vec<_>>());
    }
    if event["type"] == "system" && event["subtype"] == "init" {
        for k in ["skills", "terminal_slash_commands"] {
            if event[k].is_array() {
                seed[k] = json!(rows(&event[k]).filter_map(name).collect::<Vec<_>>());
            }
        }
        if event["plugins"].is_array() {
            seed["plugins"] = json!(rows(&event["plugins"])
                .filter_map(|p| Some(
                    json!({"name":name(&p["name"])?,"version":text(&p["version"])})
                ))
                .collect::<Vec<_>>());
        }
    }
}
pub fn project(seed: &Value, servers: &Value) -> Value {
    let descriptions: BTreeMap<_, _> = rows(&seed["commands"])
        .filter_map(|c| Some((name(&c["name"])?, text(&c["description"]))))
        .collect();
    // A metadata-only initialization does not replay system/init. Its supported
    // commands are prompt-backed skills; terminal commands are excluded by CLI.
    let names: Vec<_> = if seed["skills"].is_array() {
        rows(&seed["skills"]).filter_map(name).collect()
    } else {
        descriptions.keys().copied().collect()
    };
    let skills:Vec<_>=names.into_iter().filter(|s|!rows(&seed["terminal_slash_commands"]).any(|t|name(t)==Some(s))).map(|s|json!({"name":s,"description":descriptions.get(s),"path":format!("claude-skill:{s}"),"scope":"session","enabled":true})).collect();
    let plugins:Vec<_>=rows(&seed["plugins"]).filter_map(|p|Some(json!({"name":name(&p["name"])?,"version":text(&p["version"]),"skills":skills.iter().filter(|s|s["name"].as_str().unwrap().starts_with(&format!("{}:",p["name"].as_str().unwrap()))).map(|s|s["name"].clone()).collect::<Vec<_>>()}))).collect();
    let servers:Vec<_>=rows(servers).filter_map(|s|Some(json!({"name":name(&s["name"])?,"status":s["status"].as_str().filter(|s|["connected","failed","needs-auth","pending","disabled"].contains(s)).unwrap_or("unknown"),"tools":rows(&s["tools"]).filter_map(|t|name(&t["name"])).collect::<Vec<_>>()}))).collect();
    json!({"status":"ready","pluginsReported":seed["plugins"].is_array(),"slashCommands":skills.iter().map(|s|json!({"name":s["name"],"description":s["description"]})).collect::<Vec<_>>(),"skills":skills,"plugins":plugins,"mcpServers":servers})
}
pub fn unavailable() -> Value {
    let mut v = project(&json!({}), &json!([]));
    v["status"] = json!("unavailable");
    v
}

/// Remote Control omits the MCP status catalog in current CLI versions. Its
/// context report exposes names of real tools, not credentials or connections.
/// Do not turn loaded schemas into a claim that a server is still connected.
pub fn project_context(seed: &Value, context: &Value) -> Value {
    let mut servers: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for tool in rows(&context["mcpTools"]) {
        let (Some(server), Some(full)) = (name(&tool["serverName"]), name(&tool["name"])) else {
            continue;
        };
        let prefix = format!("mcp__{server}__");
        let Some(short) = full.strip_prefix(&prefix).filter(|v| !v.is_empty()) else {
            continue;
        };
        let names = servers.entry(server).or_default();
        let value = json!({"name":short});
        if !names.contains(&value) {
            names.push(value);
        }
    }
    let rows: Vec<_> = servers
        .into_iter()
        .map(|(name, tools)| json!({"name":name,"status":"unknown","tools":tools}))
        .collect();
    let mut result = project(seed, &json!(rows));
    result["mcpStatusReported"] = json!(false);
    result["pluginsReported"] = json!(false);
    result["scope"] = json!("session");
    result
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Control {
    Toggle {
        name: String,
        enabled: bool,
    },
    Reconnect(String),
    ReloadSkills,
    ReloadPlugins,
    Access {
        runtime_mode: String,
        interaction_mode: String,
    },
}
impl Control {
    pub fn parse(v: &Value) -> Result<Self, String> {
        Ok(match v["action"].as_str().unwrap_or("") {
            "mcpToggle" => Self::Toggle {
                name: name(&v["name"])
                    .ok_or("Select a configured MCP server")?
                    .into(),
                enabled: v["enabled"].as_bool().ok_or("Provide an enabled boolean")?,
            },
            "mcpReconnect" => Self::Reconnect(
                name(&v["name"])
                    .ok_or("Select a configured MCP server")?
                    .into(),
            ),
            "reloadSkills" => Self::ReloadSkills,
            "reloadPlugins" => Self::ReloadPlugins,
            _ => return Err("This session control is not migrated yet".into()),
        })
    }
    pub(crate) fn request(&self) -> Value {
        match self {
            Self::Toggle { name, enabled } => {
                json!({"subtype":"mcp_toggle","serverName":name,"enabled":enabled})
            }
            Self::Reconnect(name) => json!({"subtype":"mcp_reconnect","serverName":name}),
            Self::ReloadSkills => json!({"subtype":"reload_skills"}),
            Self::ReloadPlugins => json!({"subtype":"reload_plugins","hold_on_cache_impact":true}),
            Self::Access {
                runtime_mode,
                interaction_mode,
            } => {
                let mode = match (interaction_mode.as_str(), runtime_mode.as_str()) {
                    ("plan", _) => "plan",
                    (_, "full-access") => "bypassPermissions",
                    (_, "auto-accept-edits") => "acceptEdits",
                    _ => "default",
                };
                json!({"subtype":"set_permission_mode","mode":mode})
            }
        }
    }
}
/// Refresh public names only after a committed native control acknowledgement.
pub(crate) fn control_seed(metadata: &mut Value, control: &Control, event: &Value) {
    if event["response"]["subtype"] != "success" {
        return;
    }
    if !metadata.is_object() {
        *metadata = json!({});
    }
    let response = &event["response"]["response"];
    match control {
        Control::ReloadSkills if response["skills"].is_array() => {
            seed(
                metadata,
                &json!({"response":{"response":{"commands":response["skills"]}}}),
            );
            metadata["skills"] = json!(rows(&response["skills"])
                .filter_map(|v| name(&v["name"]))
                .collect::<Vec<_>>());
        }
        Control::ReloadPlugins if response["held"] != true => {
            seed(metadata, event);
            seed(
                metadata,
                &json!({"type":"system","subtype":"init","plugins":response["plugins"],"skills":rows(&response["commands"]).filter_map(|v|name(&v["name"])).collect::<Vec<_>>()}),
            );
        }
        _ => {}
    }
}
pub struct Probe {
    pub catalog: Value,
    pub result: Value,
}
async fn rpc(
    c: &crate::agent::Connection,
    events: &mut tokio::sync::mpsc::Receiver<crate::agent::Event>,
    metadata: &mut Value,
    request: Value,
) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"type":"control_request","request_id":id,"request":request}))
        .await
        .map_err(|_| "Claude tool check disconnected")?;
    while let Some(crate::agent::Event::Message(v)) = events.recv().await {
        seed(metadata, &v);
        if v["response"]["request_id"] == id {
            if v["response"]["subtype"] != "success" {
                return Err("Claude could not complete the tool control".into());
            }
            return Ok(v["response"]["response"].clone());
        }
    }
    Err("Claude tool check disconnected".into())
}
/// One metadata-only process per coalesced read/control, never a model prompt.
/// Changes are scoped to the disposable process; the caller persists successful
/// per-chat toggles and applies them before any future model prompt.
pub async fn inspect(
    config: crate::chat::AgentConfig,
    cwd: PathBuf,
    disabled: Vec<String>,
) -> Result<Value, String> {
    Ok(probe(config, cwd, disabled, None).await?.catalog)
}
pub async fn probe(
    config: crate::chat::AgentConfig,
    cwd: PathBuf,
    disabled: Vec<String>,
    control: Option<Control>,
) -> Result<Probe, String> {
    use crate::agent::{Agent, Connection, Launch};
    use crate::chat::AgentKind;
    use std::time::Duration;
    let mut args = crate::discovery::isolated_args(AgentKind::Claude);
    config.tools.apply(&mut args);
    args.push("--no-session-persistence".into());
    let mut c = Connection::start(Launch {
        agent: Agent::Claude,
        executable: config.executable,
        cwd,
        env: crate::discovery::environment(AgentKind::Claude, &config.profile),
        extra_args: args,
    })
    .map_err(|_| "Could not check Claude tools")?;
    // Move only the receiver to allow one bounded writer and one reader borrow.
    let (_, empty) = tokio::sync::mpsc::channel(1);
    let mut events = std::mem::replace(&mut c.events, empty);
    let result=tokio::time::timeout(Duration::from_secs(15),async {
        let mut metadata=json!({});
        rpc(&c,&mut events,&mut metadata,json!({"subtype":"initialize"})).await?;
        let servers=rpc(&c,&mut events,&mut metadata,json!({"subtype":"mcp_status"})).await?;
        let known: Vec<_>=rows(&servers["mcpServers"]).filter_map(|v|name(&v["name"])).collect();
        for server in &disabled {
            if known.contains(&server.as_str()) {rpc(&c,&mut events,&mut metadata,json!({"subtype":"mcp_toggle","serverName":server,"enabled":false})).await?;}
        }
        let mut result=json!({"updated":true});
        if let Some(control)=&control {
            let target=match control {Control::Toggle{name,..}|Control::Reconnect(name)=>Some(name),_=>None};
            if target.is_some_and(|name|!known.contains(&name.as_str())) {return Err("Select an MCP server configured in this chat".into());}
            let response=rpc(&c,&mut events,&mut metadata,control.request()).await?;
            match control {
                Control::ReloadSkills => {seed(&mut metadata,&json!({"response":{"response":{"commands":response["skills"]}}}));metadata["skills"]=json!(rows(&response["skills"]).filter_map(|s|name(&s["name"])).collect::<Vec<_>>());},
                Control::ReloadPlugins if response["held"]==true => {result=json!({"held":true,"detail":"Claude kept the current tools to protect the conversation cache. Start a new chat to use changed plugins."});},
                Control::ReloadPlugins => {
                    seed(&mut metadata,&json!({"response":{"response":{"commands":response["commands"]}}}));
                    seed(&mut metadata,&json!({"type":"system","subtype":"init","plugins":response["plugins"],"skills":rows(&response["commands"]).filter_map(|s|name(&s["name"])).collect::<Vec<_>>()}));
                    if response["error_count"].as_u64().unwrap_or(0)>0 {result=json!({"warning":"Some plugins could not reload. Check their configuration on the PC."});}
                },
                _=>{},
            }
        }
        let mut servers=rpc(&c,&mut events,&mut metadata,json!({"subtype":"mcp_status"})).await?;
        for _ in 0..6 {
            if !rows(&servers["mcpServers"]).any(|s|s["status"]=="pending") {break;}
            tokio::time::sleep(Duration::from_millis(300)).await;
            servers=rpc(&c,&mut events,&mut metadata,json!({"subtype":"mcp_status"})).await?;
        }
        let mut catalog=project(&metadata,&servers["mcpServers"]);catalog["scope"]=json!("configuration");
        Ok(Probe{catalog,result})
    }).await.unwrap_or_else(|_|Err("Claude tool control timed out; no model prompt was sent".into()));
    drop(events);
    c.close().await;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_context_catalog_preserves_names_without_inventing_status_or_leaking_context() {
        let value = project_context(
            &json!({"skills":["fixture:review"]}),
            &json!({"systemPrompt":"PRIVATE","mcpTools":[
                {"name":"mcp__fixture__read_marker","serverName":"fixture","isLoaded":true,"schema":"PRIVATE"},
                {"name":"mcp__fixture__read_marker","serverName":"fixture"},
                {"name":"mcp__fixture__ask_color","serverName":"fixture","isLoaded":false},
                {"name":"mcp__different__bad","serverName":"fixture"},
                {"name":"mcp__bad name__read","serverName":"bad name"}
            ]}),
        );
        assert_eq!(
            value["mcpServers"],
            json!([{"name":"fixture","status":"unknown","tools":["read_marker","ask_color"]}])
        );
        assert_eq!(value["skills"][0]["name"], "fixture:review");
        assert_eq!(value["mcpStatusReported"], false);
        assert_eq!(value["pluginsReported"], false);
        assert!(!value.to_string().contains("PRIVATE"));
        assert_eq!(
            project_context(&json!({}), &json!({"mcpTools":[]}))["mcpServers"],
            json!([])
        );
    }
    #[test]
    fn public_catalog_hides_private_configuration_and_terminal_commands() {
        let mut state = json!({});
        seed(
            &mut state,
            &json!({"response":{"response":{"commands":[{"name":"sample:read","description":"Read a file","prompt":"SECRET"}]}}}),
        );
        seed(
            &mut state,
            &json!({"type":"system","subtype":"init","skills":["sample:read","login"],"terminal_slash_commands":["login"],"plugins":[{"name":"sample","version":"1","path":"SECRET"}],"apiKey":"SECRET"}),
        );
        let v = project(
            &state,
            &json!([{ "name":"local","status":"connected","tools":[{"name":"inspect","inputSchema":{"secret":"SECRET"}}],"env":{"TOKEN":"SECRET"},"url":"SECRET"}]),
        );
        assert!(!v.to_string().contains("SECRET"));
        assert!(!state.to_string().contains("SECRET"));
        assert_eq!(v["skills"].as_array().unwrap().len(), 1);
        assert_eq!(v["mcpServers"][0]["tools"][0], "inspect");
        assert_eq!(v["plugins"][0]["skills"][0], "sample:read");
    }
}
