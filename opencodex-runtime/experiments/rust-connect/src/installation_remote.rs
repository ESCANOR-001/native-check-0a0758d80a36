//! Explicit offline connection setup. Runtime changes are applied once, on the
//! next start, without revoking paired clients or overriding later UI choices.
use super::*;
use zeroize::Zeroizing;

pub async fn remote(
    root: &Path,
    mode: &str,
    hostname: Option<&str>,
    token: Option<Zeroizing<Vec<u8>>>,
    revision: u64,
) -> Result<Value, String> {
    if !exists(root)? {
        return crate::migrated_configuration::remote(root, mode, hostname, token, revision).await;
    }
    if !matches!(mode, "local" | "quick" | "named") {
        return Err("Choose local, quick or named remote mode".into());
    }
    let root = directory(root)?;
    let _owner = profile_lock::acquire(&root)?;
    let saved = read(&root)?;
    let _state = profile_lock::acquire(&root.join("state"))?;
    if revision != saved.revision {
        return Err(
            "Configuration changed; inspect the current revision before setting up remote access"
                .into(),
        );
    }
    let mut config: Config =
        serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
    let c = config.connection.as_mut().ok_or(ERROR)?;
    c.inherited_quick_tunnel = None;
    if mode == "named" {
        let hostname = hostname
            .ok_or("A named tunnel requires a hostname")?
            .to_ascii_lowercase();
        let account = crate::auth::digest(&uuid::Uuid::new_v4().to_string());
        let named = crate::named_tunnel::Config {
            hostname,
            account,
            origin: format!("127.0.0.1:{}", saved.bind.port())
                .parse()
                .map_err(|_| ERROR)?,
        };
        named.validate()?;
        let token = token.ok_or("Supply the connector token through standard input")?;
        crate::named_tunnel::save(&named.account, token).await?;
        c.named_tunnel = Some(named);
    } else {
        if hostname.is_some() || token.is_some() {
            return Err("Only named mode accepts a hostname and token".into());
        }
        c.named_tunnel = None;
    }
    let mut source = saved.clone();
    source.network_mode = Some(mode.into());
    let mut next = configuration::candidate(&source, config)?;
    next.network_revision = saved.network_revision.checked_add(1).ok_or(ERROR)?;
    next.network_mode = Some(mode.into());
    materialize(&saved)?;
    let mut result = configuration::commit(&saved, &next)?;
    result["connectionMode"] = json!(mode);
    result["reconnectPhone"] = json!(true);
    result["note"] = json!(if mode == "quick" {
        "Quick-tunnel URLs change when restarted; open the dashboard to reconnect the phone once the new endpoint is verified"
    } else {
        "Open the dashboard after starting the bridge to verify reachability and update the phone connection"
    });
    Ok(result)
}

/// Creates a remotely managed tunnel and DNS record using a scoped API token.
/// Existing routes must match exactly. API credentials are not retained.
pub async fn provision(
    root: &Path,
    account: &str,
    zone: &str,
    hostname: &str,
    api_token: Zeroizing<Vec<u8>>,
    revision: u64,
) -> Result<Value, String> {
    if !exists(root)? {
        return crate::migrated_configuration::provision(
            root, account, zone, hostname, api_token, revision,
        )
        .await;
    }
    let root = directory(root)?;
    // Keep the settings generation fixed during remote provisioning. Drop the
    // lease before the shared named-import path reacquires and revalidates it.
    let token = {
        let _owner = profile_lock::acquire(&root)?;
        let saved = read(&root)?;
        let _state = profile_lock::acquire(&root.join("state"))?;
        if saved.revision != revision {
            return Err("Configuration changed; inspect it again before provisioning".into());
        }
        let config: Config =
            serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
        let _ports = (
            crate::listener::reserve(saved.bind)
                .map_err(|_| "Stop the bridge before remote setup")?,
            crate::listener::reserve(config.connection.ok_or(ERROR)?.dashboard_bind)
                .map_err(|_| "Stop the bridge before remote setup")?,
        );
        crate::tunnel_provisioning::Provisioner::new()?
            .provision(
                api_token,
                account,
                zone,
                hostname,
                saved.bind.port(),
                &saved.environment,
            )
            .await?
    };
    remote(&root, "named", Some(hostname), Some(token), revision).await
}
