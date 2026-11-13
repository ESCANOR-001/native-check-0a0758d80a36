//! Local executable/profile discovery for first setup. Never installs CLIs,
//! copies credentials, starts a model turn or modifies Desktop applications.
use crate::chat::{AgentConfig, AgentKind, Config};
use serde_json::json;
use std::{
    net::{IpAddr, Ipv4Addr, UdpSocket},
    path::{Path, PathBuf},
};

pub(crate) fn local_ip() -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            // UDP connect selects a route; no packet is sent.
            socket.connect("192.0.2.1:9")?;
            Ok(socket.local_addr()?.ip())
        })
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn executable(name: &str) -> Option<PathBuf> {
    let explicit = std::env::var_os(if name == "codex" {
        "REMODEX_CODEX_EXECUTABLE"
    } else {
        "REMODEX_CLAUDE_EXECUTABLE"
    });
    let mut candidates = if let Some(path) = explicit.as_ref() {
        vec![PathBuf::from(path)]
    } else {
        std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|p| p.is_absolute())
                    .map(|p| {
                        p.join(if cfg!(windows) {
                            format!("{name}.exe")
                        } else {
                            name.to_owned()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    // npm places Claude's native executable beneath node_modules.
    if explicit.is_none() && cfg!(windows) && name == "claude" {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(
                PathBuf::from(appdata)
                    .join("npm/node_modules/@anthropic-ai/claude-code/bin/claude.exe"),
            );
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            candidates.push(PathBuf::from(home).join(".local/bin/claude.exe"));
        }
    }
    candidates.into_iter().find_map(|path| {
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path).ok()?.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        // Preserve an npm CLI's symlink entry: its relative module resolution
        // belongs to the CLI, not to a shell wrapper invented by this bridge.
        Some(path)
    })
}

async fn agent(
    kind: AgentKind,
    home: &Path,
    workspace: &Path,
    retained_profile: Option<&Path>,
) -> Result<Option<AgentConfig>, String> {
    let (name, variable, folder) = match kind {
        AgentKind::Codex => ("codex", "CODEX_HOME", ".codex"),
        AgentKind::Claude => ("claude", "CLAUDE_CONFIG_DIR", ".claude"),
    };
    let explicit = retained_profile
        .map(|p| p.as_os_str().to_owned())
        .or_else(|| std::env::var_os(variable));
    let profile = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(folder));
    let binary = executable(name);
    let explicit_binary = std::env::var_os(format!("REMODEX_{}_EXECUTABLE", name.to_uppercase()));
    if binary.is_none() && explicit.is_none() && explicit_binary.is_none() {
        return Ok(None);
    }
    let executable = binary.ok_or_else(||format!("{name} was configured but its executable is missing. Install it or set REMODEX_{}_EXECUTABLE",name.to_uppercase()))?;
    if !profile.is_absolute() || !profile.is_dir() {
        return Err(format!("Open {name} and finish its local setup first, or supply an existing absolute {variable}"));
    }
    let mut config = AgentConfig {
        executable,
        profile: profile
            .canonicalize()
            .map_err(|_| "Cannot resolve agent profile")?,
        model: if kind == AgentKind::Claude {
            "default"
        } else {
            "__native_setup__"
        }
        .into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    if kind == AgentKind::Codex {
        // Reuse the existing metadata-only CLI transport; no thread/start or
        // turn/start is sent. Select an actual CLI-reported model, never a
        // hardcoded model version that could be unavailable for this account.
        let catalog = crate::discovery::discover(kind,config.clone(),workspace).await
            .map_err(|_|"Codex could not report its models. Complete CLI setup, then retry onboarding or supply --config")?;
        config.model = catalog["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["slug"].as_str())
            .find(|s| *s != "__native_setup__")
            .ok_or("Codex reported no usable models")?
            .to_owned();
    }
    Ok(Some(config))
}

pub(crate) async fn configuration(port: u16) -> Result<Config, String> {
    configuration_with_codex_profile(port, None).await
}
pub(crate) async fn configuration_with_codex_profile(
    port: u16,
    retained_profile: Option<&Path>,
) -> Result<Config, String> {
    let home = PathBuf::from(
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .ok_or("User home directory is unavailable")?,
    );
    if !home.is_absolute() || !home.is_dir() {
        return Err("An existing absolute user home directory is required".into());
    }
    let workspace = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .map_err(|_| "The working directory is unavailable")?;
    let codex = agent(AgentKind::Codex, &home, &workspace, retained_profile).await?;
    let claude = agent(AgentKind::Claude, &home, &workspace, None).await?;
    if codex.is_none() && claude.is_none() {
        return Err("Install and sign in to Codex CLI or Claude Code first, then run rmx onboard again. Custom installations can use --config".into());
    }
    let mut config: Config = serde_json::from_value(json!({"workspace":workspace,
        "connection":{"dashboard_bind":format!("127.0.0.1:{port}"),"label":"Remodex"}}))
    .map_err(|_| "Invalid setup configuration")?;
    config.codex = codex;
    config.claude = claude;
    Ok(config)
}
