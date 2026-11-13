//! No-prompt fork capability probe, confined by the experiment's Config guard.
use remodex_connect_native::{
    agent::{Agent, Connection, Event, Launch},
    chat::Config,
};
use serde_json::json;
use std::{collections::HashMap, time::Duration};
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("Provide isolated config, source UUID, point UUID and new UUID".into());
    }
    for id in &args[2..] {
        uuid::Uuid::parse_str(id)?;
    }
    if args[2] == args[4] {
        return Err("A separate fork identity is required".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    config.validate()?;
    let claude = config
        .claude
        .as_ref()
        .ok_or("Configure Claude Code before using this probe")?;
    let mut env = HashMap::new();
    for k in [
        "HOME",
        "USER",
        "PATH",
        "TMPDIR",
        "TEMP",
        "SystemRoot",
        "USERPROFILE",
    ] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.into(), v);
        }
    }
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        claude.profile.to_string_lossy().into_owned(),
    );
    let extra_args = vec![
        "--settings".into(),
        "{\"remoteControlAtStartup\":false,\"disableAllHooks\":true}".into(),
        "--setting-sources".into(),
        "".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        "{\"mcpServers\":{}}".into(),
        "--tools".into(),
        "".into(),
        "--resume".into(),
        args[2].clone(),
        "--fork-session".into(),
        "--resume-session-at".into(),
        args[3].clone(),
        "--session-id".into(),
        args[4].clone(),
    ];
    let mut connection = Connection::start(Launch {
        agent: Agent::Claude,
        executable: claude.executable.clone(),
        cwd: config.workspace,
        env,
        extra_args,
    })?;
    let result=tokio::time::timeout(Duration::from_secs(20),async {
  connection.send(&json!({"type":"control_request","request_id":"fork-capability","request":{"subtype":"initialize"}})).await?;
  while let Some(event)=connection.events.recv().await {if let Event::Message(v)=event {
   if v["response"]["request_id"]=="fork-capability" {return Ok::<_,Box<dyn std::error::Error>>(json!({"initialized":v["response"]["subtype"]=="success","sessionId":v["response"]["response"]["session_id"],"responseKeys":v["response"]["response"].as_object().map(|v|v.keys().cloned().collect::<Vec<_>>()),"promptSent":false}));}
  }}Err("CLI exited before the fork initialized".into())
 }).await;
    connection.close().await;
    println!("{}", result??);
    Ok(())
}
