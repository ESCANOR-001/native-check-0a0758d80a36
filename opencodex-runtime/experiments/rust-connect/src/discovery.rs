//! Read CLI capabilities without starting a chat or sending a model prompt.
use crate::{
    agent::{Agent, Connection, Event, Launch},
    chat::{AgentConfig, AgentKind},
    models,
};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, time::Duration};

pub fn environment(agent: AgentKind, profile: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "TMPDIR",
        "TEMP",
        "SystemRoot",
        "USERPROFILE",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_owned(), value);
        }
    }
    env.insert(
        if agent == AgentKind::Codex {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        }
        .into(),
        profile.to_string_lossy().into_owned(),
    );
    if agent == AgentKind::Claude {
        // Forward provider/network configuration only to Claude, never to the
        // phone catalog. Do not inherit injection/runtime/debug variables.
        for (key, value) in std::env::vars() {
            if key.starts_with("ANTHROPIC_")
                || key.starts_with("CLAUDE_CODE_USE_")
                || matches!(
                    key.as_str(),
                    "HTTP_PROXY"
                        | "HTTPS_PROXY"
                        | "ALL_PROXY"
                        | "NO_PROXY"
                        | "http_proxy"
                        | "https_proxy"
                        | "all_proxy"
                        | "no_proxy"
                        | "NODE_EXTRA_CA_CERTS"
                        | "SSL_CERT_FILE"
                        | "SSL_CERT_DIR"
                        | "AWS_PROFILE"
                        | "AWS_REGION"
                        | "AWS_DEFAULT_REGION"
                        | "AWS_ACCESS_KEY_ID"
                        | "AWS_SECRET_ACCESS_KEY"
                        | "AWS_SESSION_TOKEN"
                        | "AWS_CONFIG_FILE"
                        | "AWS_SHARED_CREDENTIALS_FILE"
                        | "GOOGLE_APPLICATION_CREDENTIALS"
                        | "CLOUD_ML_REGION"
                        | "ANTHROPIC_VERTEX_PROJECT_ID"
                        | "AZURE_CLIENT_ID"
                        | "AZURE_TENANT_ID"
                        | "AZURE_CLIENT_SECRET"
                )
            {
                env.insert(key, value);
            }
        }
    }
    env
}

pub fn isolated_args(agent: AgentKind) -> Vec<String> {
    if agent == AgentKind::Codex {
        [
            "-c",
            "features.plugins=false",
            "-c",
            "features.remote_plugin=false",
            "-c",
            "features.apps=false",
        ]
        .map(str::to_owned)
        .to_vec()
    } else {
        [
            "--settings",
            "{\"remoteControlAtStartup\":false,\"disableAllHooks\":true}",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

pub async fn discover(agent: AgentKind, config: AgentConfig, cwd: &Path) -> Result<Value, String> {
    let mut args = if agent == AgentKind::Codex {
        Vec::new()
    } else {
        isolated_args(agent)
    };
    if agent == AgentKind::Claude {
        args.extend([
            "--tools".into(),
            "".into(),
            "--no-session-persistence".into(),
        ]);
    }
    let mut connection = config
        .connect(Launch {
            agent: if agent == AgentKind::Codex {
                Agent::Codex
            } else {
                Agent::Claude
            },
            executable: config.executable.clone(),
            cwd: cwd.to_owned(),
            env: environment(agent, &config.profile),
            extra_args: args,
        })
        .await
        .map_err(|_| "Could not start the isolated CLI capability check")?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        connection.send(&if agent == AgentKind::Codex {
            json!({"id":"cap-init","method":"initialize","params":{"clientInfo":{"name":"remodex_native_capabilities","version":"0.0.0"},"capabilities":{"experimentalApi":true}}})
        } else {
            json!({"type":"control_request","request_id":"cap-init","request":{"subtype":"initialize"}})
        }).await.map_err(|_| "Capability check disconnected")?;
        while let Some(event) = connection.events.recv().await {
            let Event::Message(v) = event else { return Err("CLI exited before reporting capabilities".into()); };
            if agent == AgentKind::Codex && v["id"] == "cap-init" {
                if v.get("error").is_some() || !v["result"].is_object() { return Err("CLI initialization was rejected".into()); }
                connection.send(&json!({"method":"initialized","params":{}})).await.map_err(|_| "Capability check disconnected")?;
                connection.send(&json!({"id":"cap-models","method":"model/list","params":{"limit":100}})).await.map_err(|_| "Capability check disconnected")?;
            }
            let rows = if agent == AgentKind::Codex && v["id"] == "cap-models" {
                Some(&v["result"]["data"])
            } else if agent == AgentKind::Claude && v["response"]["request_id"] == "cap-init" {
                Some(&v["response"]["response"]["models"])
            } else { None };
            if let Some(rows) = rows {
                if !rows.is_array() || rows.as_array().is_some_and(|r|r.is_empty()) {return Err("CLI did not report a model catalog".into());}
                return Ok(models::catalog(rows, agent == AgentKind::Codex, &config.model));
            }
        }
        Err("Capability check disconnected".into())
    }).await.unwrap_or_else(|_|Err("CLI capability check timed out".into()));
    // Inventory is optional: an older CLI must still provide its model list.
    let result = match result {
        Ok(models) => {
            let catalog = if agent == AgentKind::Codex {
                crate::codex_catalog::discover(&mut connection, cwd).await
            } else {
                json!({})
            };
            Ok(json!({"models":models,"catalog":catalog}))
        }
        Err(error) => Err(error),
    };
    connection.close().await;
    result
}

pub async fn claude_usage(config: AgentConfig, cwd: &Path) -> Option<Value> {
    let agent = AgentKind::Claude;
    let mut args = isolated_args(agent);
    args.extend([
        "--tools".into(),
        "".into(),
        "--no-session-persistence".into(),
    ]);
    let mut c = Connection::start(Launch {
        agent: Agent::Claude,
        executable: config.executable,
        cwd: cwd.to_owned(),
        env: environment(agent, &config.profile),
        extra_args: args,
    })
    .ok()?;
    let result=tokio::time::timeout(Duration::from_secs(8),async {
        c.send(&json!({"type":"control_request","request_id":"usage-init","request":{"subtype":"initialize"}})).await.ok()?;
        while let Some(Event::Message(v))=c.events.recv().await {
            if v["response"]["request_id"]=="usage-init" {
                if v["response"]["subtype"]!="success"{return None;}
                c.send(&json!({"type":"control_request","request_id":"usage-read","request":{"subtype":"get_usage","skip_behaviors":true}})).await.ok()?;
            } else if v["response"]["request_id"]=="usage-read" {
                return crate::usage::limits(&v["response"]["response"]);
            }
        }
        None
    }).await.ok().flatten();
    c.close().await;
    result
}
