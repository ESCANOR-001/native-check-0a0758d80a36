//! Immutable configuration generations; a single atomic receipt selects one.
//! Rollback changes settings only, never restores an older chat database.
use super::*;

fn backup(root: &Path, revision: u64) -> PathBuf {
    root.join(format!("installation-revision-{revision}.json"))
}

pub fn inspect(root: &Path) -> Result<Value, String> {
    if !exists(root)? {
        return crate::migrated_configuration::inspect(root);
    }
    let saved = read(root)?;
    Ok(json!({"revision":saved.revision,"configuration":saved.configuration}))
}

fn same_profiles(old: &Config, new: &Config) -> Result<(), String> {
    for (a, b) in [(&old.codex, &new.codex), (&old.claude, &new.claude)] {
        if let Some(a) = a {
            if b.as_ref()
                .is_none_or(|b| a.profile.canonicalize().ok() != b.profile.canonicalize().ok())
            {
                return Err("Changing or removing an existing agent profile would disconnect its saved chats; use a separate installation instead".into());
            }
        }
    }
    Ok(())
}

pub(super) fn candidate(saved: &Installed, mut config: Config) -> Result<Installed, String> {
    if config.native_profile_handoff.is_some()
        || config
            .native_installation
            .as_ref()
            .is_some_and(|p| p != &saved.root)
    {
        return Err("Configuration belongs to another installation or migration".into());
    }
    config.validate_export()?;
    if saved.network_mode.as_deref() == Some("named")
        && config
            .connection
            .as_ref()
            .is_none_or(|c| c.named_tunnel.is_none())
    {
        return Err("Use remote local or quick to change the named connection mode".into());
    }
    let old: Config = serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
    same_profiles(&old, &config)?;
    let connection = config
        .connection
        .as_ref()
        .ok_or("A loopback dashboard configuration is required")?;
    let port = connection
        .dashboard_bind
        .port()
        .checked_add(5)
        .ok_or("Dashboard port must not exceed 65530")?;
    let bind = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
    if connection
        .named_tunnel
        .as_ref()
        .is_some_and(|t| t.origin.port() != port || !t.origin.ip().is_ipv4())
    {
        return Err(
            "The named tunnel origin must be 127.0.0.1 at the dashboard port plus five".into(),
        );
    }
    config.native_installation = Some(saved.root.clone());
    Ok(Installed {
        version: 1,
        revision: saved.revision.checked_add(1).ok_or(ERROR)?,
        network_revision: saved.network_revision,
        network_mode: saved.network_mode.clone(),
        environment: saved.environment.clone(),
        root: saved.root.clone(),
        bind,
        public_url: format!("http://{}:{port}", defaults::local_ip()),
        configuration: serde_json::to_value(config).map_err(|_| ERROR)?,
    })
}

pub(super) fn commit(saved: &Installed, next: &Installed) -> Result<Value, String> {
    let config: Config = serde_json::from_value(next.configuration.clone()).map_err(|_| ERROR)?;
    let _ports = (
        crate::listener::reserve(config.connection.as_ref().ok_or(ERROR)?.dashboard_bind)
            .map_err(|_| "The requested dashboard port is occupied; configuration was retained")?,
        crate::listener::reserve(next.bind)
            .map_err(|_| "The requested phone port is occupied; configuration was retained")?,
    );
    let bytes = serde_json::to_vec(next).map_err(|_| ERROR)?;
    if bytes.len() > 262144 {
        return Err("Configuration exceeds the supported record size".into());
    }
    // Retain every previous receipt and generation, including on failed writes.
    write_once(
        &backup(&saved.root, saved.revision),
        &serde_json::to_vec(saved).map_err(|_| ERROR)?,
    )?;
    write_once(
        &configuration_path(next),
        &serde_json::to_vec(&next.configuration).map_err(|_| ERROR)?,
    )?;
    // Preflight the existing selector; never replace a link or a changed record.
    let current = read(&saved.root)?;
    if serde_json::to_value(&current).map_err(|_| ERROR)?
        != serde_json::to_value(saved).map_err(|_| ERROR)?
    {
        return Err(ERROR.into());
    }
    let file = storage_stage::open(&saved.root.join(RECORD), false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    drop(file);
    let mut temporary = tempfile::NamedTempFile::new_in(&saved.root).map_err(|_| ERROR)?;
    temporary.write_all(&bytes).map_err(|_| ERROR)?;
    temporary.as_file().sync_all().map_err(|_| ERROR)?;
    temporary
        .persist(saved.root.join(RECORD))
        .map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(&saved.root, true)
        .and_then(|f| f.sync_all())
        .map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"configured","revision":next.revision,"previousRevision":saved.revision,
        "restartRequired":true,"historyRetained":true,"pairingRetained":true}),
    )
}

/// check is read-only. apply/rollback require both the supervisor and child to
/// be stopped, including a low-level child started without the supervisor.
pub fn configure(
    root: &Path,
    operation: &str,
    input: &str,
    expected: Option<u64>,
) -> Result<Value, String> {
    if !exists(root)? {
        return crate::migrated_configuration::configure(root, operation, input, expected);
    }
    let root = directory(root)?;
    if !matches!(operation, "check" | "apply" | "rollback") {
        return Err("Unknown configuration operation".into());
    }
    let _owner = if operation == "check" {
        None
    } else {
        Some(profile_lock::acquire(&root)?)
    };
    let saved = read(&root)?;
    if expected.is_some_and(|r| r != saved.revision) {
        return Err("Configuration changed since it was reviewed; inspect and check the current revision again".into());
    }
    if operation != "check" && expected.is_none() {
        return Err(
            "Supply --revision from config show to prevent overwriting a newer configuration"
                .into(),
        );
    }
    let _state = if operation == "check" {
        None
    } else {
        Some(profile_lock::acquire(&root.join("state"))?)
    };
    let mut source = saved.clone();
    let config = if operation == "rollback" {
        let revision: u64 = input.parse().map_err(|_| "Invalid rollback revision")?;
        if revision >= saved.revision {
            return Err("Choose an earlier configuration revision".into());
        }
        let previous: Installed = serde_json::from_slice(
            &profile_snapshot::bytes(&backup(&root, revision), 262144).map_err(|_| ERROR)?,
        )
        .map_err(|_| ERROR)?;
        if previous.root != saved.root
            || previous.environment != saved.environment
            || previous.revision != revision
            || previous.version != 1
        {
            return Err(ERROR.into());
        }
        source.network_mode = previous.network_mode.or_else(|| Some("local".into()));
        source.network_revision = saved.network_revision.checked_add(1).ok_or(ERROR)?;
        serde_json::from_value(previous.configuration).map_err(|_| ERROR)?
    } else {
        serde_json::from_slice(
            &profile_snapshot::bytes(Path::new(input), 262144)
                .map_err(|_| "Cannot read configuration")?,
        )
        .map_err(|_| "Invalid configuration")?
    };
    let next = candidate(&source, config)?;
    if operation == "check" {
        return Ok(
            json!({"valid":true,"revision":saved.revision,"nextRevision":next.revision,"requiresStoppedBridge":true,"pairingRetained":true}),
        );
    }
    // Preserve the old bytes even when repairing an unavailable executable or
    // workspace; only the replacement configuration needs to be launchable.
    write_once(
        &configuration_path(&saved),
        &serde_json::to_vec(&saved.configuration).map_err(|_| ERROR)?,
    )?;
    commit(&saved, &next)
}
