//! Explicit isolated CLI protocol probe. Does not send a model prompt.
use remodex_connect_native::agent::{Agent, Connection, Event, Launch};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Duration};
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        return Err("probe codex|claude ABSOLUTE_EXECUTABLE WORKSPACE ISOLATED_PROFILE".into());
    }
    let agent = match args[1].as_str() {
        "codex" => Agent::Codex,
        "claude" => Agent::Claude,
        _ => return Err("Unsupported agent".into()),
    };
    let profile = PathBuf::from(&args[4]);
    if !profile.is_absolute() {
        return Err("Absolute isolated profile required".into());
    }
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
        if matches!(agent, Agent::Codex) {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        }
        .to_owned(),
        profile.to_string_lossy().into_owned(),
    );
    let extra_args = if matches!(agent, Agent::Claude) {
        vec![
            "--settings".into(),
            "{\"remoteControlAtStartup\":false,\"disableAllHooks\":true}".into(),
            "--setting-sources".into(),
            "".into(),
            "--tools".into(),
            "".into(),
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            "{\"mcpServers\":{}}".into(),
        ]
    } else {
        vec![]
    };
    let mut connection = Connection::start(Launch {
        agent,
        executable: PathBuf::from(&args[2]),
        cwd: PathBuf::from(&args[3]),
        env,
        extra_args,
    })?;
    let request = match agent {
        Agent::Codex => {
            json!({"id":"native-initialize","method":"initialize","params":{"clientInfo":{"name":"remodex_native_probe","version":"0"},"capabilities":{"experimentalApi":true}}})
        }
        Agent::Claude => {
            json!({"type":"control_request","request_id":"native-initialize","request":{"subtype":"initialize"}})
        }
    };
    connection.send(&request).await?;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = connection.events.recv().await {
            match event {
                Event::Message(value) => {
                    let matches = match agent {
                        Agent::Codex => value["id"] == "native-initialize",
                        Agent::Claude => value["response"]["request_id"] == "native-initialize",
                    };
                    if matches {
                        let success = match agent {
                            Agent::Codex => value.get("result").is_some(),
                            Agent::Claude => value["response"]["subtype"] == "success",
                        };
                        return success;
                    }
                }
                Event::Closed { .. } | Event::ClaudeRemote(_) => return false,
            }
        }
        false
    })
    .await;
    connection.close().await;
    println!(
        "{}",
        json!({"agent":args[1],"nativeInitializePassed":matches!(result,Ok(true)),"javascriptBackend":false,"modelPromptSent":false})
    );
    if !matches!(result, Ok(true)) {
        return Err("Native initialization failed".into());
    }
    Ok(())
}
