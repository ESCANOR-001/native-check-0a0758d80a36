//! Opt-in live check, never run by cargo test. Requires an explicit isolated
//! config and a previously recorded disposable native/bridge identity pair.
use remodex_connect_native::{
    agent::{Agent, Connection, Launch},
    chat::Config,
    claude_host::{HostedSession, Identity},
};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "Provide isolated config, prior probe receipt, and output receipt paths".into(),
        );
    }
    let read = |path: &str| -> Result<Value, String> {
        let bytes = std::fs::read(path).map_err(|_| "Cannot read test input")?;
        if bytes.len() > 128 * 1024 {
            return Err("Test input exceeds its limit".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "Invalid test input".into())
    };
    let config: Config =
        serde_json::from_value(read(&args[1])?).map_err(|_| "Invalid isolated config")?;
    config.validate()?;
    for p in [&config.claude.profile, &config.workspace] {
        if !p.components().any(|c| c.as_os_str() == ".tmp") {
            return Err("This live check requires disposable .tmp paths".into());
        }
    }
    let previous = read(&args[2])?;
    let identity = Identity {
        native_session: previous[0]["nativeId"]
            .as_str()
            .ok_or("Native test identity missing")?
            .into(),
        bridge_session: previous[0]["bridgeId"]
            .as_str()
            .ok_or("Shared test identity missing")?
            .into(),
    };
    identity.validate()?;
    let output = PathBuf::from(&args[3]);
    if output.exists()
        || !output
            .parent()
            .is_some_and(|p| p.components().any(|c| c.as_os_str() == ".tmp"))
    {
        return Err("Choose a new receipt in a disposable .tmp directory".into());
    }
    let mut evidence = json!({"identity":identity,"checks":[]});
    for attempt in 0..2 {
        let mut env: HashMap<String, String> = ["PATH", "HOME", "USER", "TMPDIR", "TEMP"]
            .into_iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.into(), v)))
            .collect();
        env.insert(
            "CLAUDE_CONFIG_DIR".into(),
            config.claude.profile.to_string_lossy().into_owned(),
        );
        let connection = Connection::start(Launch {
            agent: Agent::Claude,
            executable: config.claude.executable.clone(),
            cwd: config.workspace.clone(),
            env,
            extra_args: vec![
                "--settings".into(),
                r#"{"remoteControlAtStartup":false,"disableAllHooks":true}"#.into(),
                "--setting-sources".into(),
                "".into(),
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                r#"{"mcpServers":{}}"#.into(),
                "--tools".into(),
                "".into(),
                "--resume".into(),
                identity.native_session.clone(),
                "--model".into(),
                config.claude.model.clone(),
                "--replay-user-messages".into(),
            ],
        })
        .map_err(|_| "Cannot start disposable Claude CLI")?;
        let mut host = HostedSession::attach(
            connection,
            identity.native_session.clone(),
            Some(identity.clone()),
            "Remodex Rust shared verification",
        )
        .await?;
        // This exact identity is already durable in the supplied probe receipt.
        host.confirm_persisted(&identity)?;
        println!(
            "{}",
            json!({"attempt":attempt,"sameSession":host.identity()==&identity})
        );
        if attempt == 0 {
            for (index, marker) in ["RUST_PERSISTENT_FIRST_OK", "RUST_PERSISTENT_SECOND_OK"]
                .into_iter()
                .enumerate()
            {
                let id = uuid::Uuid::new_v4().to_string();
                host.send_prompt(&id,json!([{"type":"text","text":format!("Reply exactly {marker}. Do not use tools.")}]),false).await?;
                let checked = tokio::time::timeout(Duration::from_secs(60), async {
                    let mut visible = false;
                    loop {
                        let frame = host
                            .next()
                            .await?
                            .ok_or("Claude disconnected during test turn")?;
                        if frame.value["type"] == "assistant" {
                            visible |= frame.value["message"]["content"].as_array().is_some_and(
                                |blocks| {
                                    blocks.iter().any(|b| {
                                        b["type"] == "text"
                                            && b["text"]
                                                .as_str()
                                                .is_some_and(|s| s.trim() == marker)
                                    })
                                },
                            );
                        }
                        if frame.value["type"] == "result"
                            && frame.receipt.completed.iter().any(|s| s == &id)
                        {
                            if !visible || frame.value["is_error"] == true {
                                return Err("Claude test reply did not match".to_string());
                            }
                            return Ok(());
                        }
                    }
                })
                .await
                .map_err(|_| "Claude live turn exceeded its deadline")?;
                checked?;
                evidence["checks"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"turn":index,"reply":marker,"consumptionConfirmed":true}));
                println!("{}", json!({"turn":index,"consumptionConfirmed":true}));
                if index == 0 {
                    let until = tokio::time::Instant::now() + Duration::from_secs(12);
                    loop {
                        match tokio::time::timeout_at(until, host.next()).await {
                            Err(_) => break,
                            Ok(Ok(Some(_))) => {}
                            _ => return Err("Claude disconnected between prompts".into()),
                        }
                    }
                }
            }
        }
        host.close().await;
    }
    evidence["reattachedWithoutReplay"] = json!(true);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(output)
        .map_err(|_| "Cannot save live test receipt")?;
    serde_json::to_writer_pretty(file, &evidence).map_err(|_| "Cannot write live test receipt")?;
    Ok(())
}
