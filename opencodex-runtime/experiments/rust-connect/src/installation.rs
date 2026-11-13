//! First installation, entirely native. Legacy installations always use the
//! export/upgrade coordinator; a damaged migration can never become a new one.
use crate::{chat::Config, private_directory, profile_lock, profile_snapshot, storage_stage};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[path = "installation_config.rs"]
mod configuration;
#[path = "installation_defaults.rs"]
pub(crate) mod defaults;
pub use configuration::{configure, inspect};
#[path = "installation_remote.rs"]
mod remote;
pub use remote::{provision, remote};
const ERROR: &str = "Native installation could not be verified. Existing files were retained; do not delete migration or recovery records";
const RECORD: &str = "native-install.json";
const CONFIG: &str = "configuration.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    version: u32,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    network_revision: u64,
    #[serde(default)]
    network_mode: Option<String>,
    environment: String,
    root: PathBuf,
    bind: std::net::SocketAddr,
    public_url: String,
    configuration: Value,
}

fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(ERROR.into()),
    }
}
pub fn exists(root: &Path) -> Result<bool, String> {
    present(&root.join(RECORD))
}

fn empty_setup_root(root: &Path) -> Result<(), String> {
    if present(root)? {
        directory(root)?;
        for entry in fs::read_dir(root).map_err(|_| ERROR)? {
            if entry.map_err(|_| ERROR)?.file_name() != "server.lock" {
                return Err(ERROR.into());
            }
        }
    }
    Ok(())
}

fn legacy_guard(root: &Path) -> Result<(), String> {
    let parent = root.parent().ok_or(ERROR)?;
    for path in [
        parent.join("android-remote.json"),
        parent.join("native-upgrade-source-hold.json"),
        root.join("npm-source.json"),
        root.join("npm-upgrade.sqlite"),
    ] {
        if present(&path)? {
            return Err("An existing or interrupted legacy installation must use its upgrade/recovery path; fresh setup was refused".into());
        }
    }
    Ok(())
}

fn directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ERROR.into());
    }
    storage_stage::open(path, true).map_err(|_| ERROR)?;
    path.canonicalize().map_err(|_| ERROR.into())
}

fn read(root: &Path) -> Result<Installed, String> {
    let root = directory(root)?;
    legacy_guard(&root)?;
    let bytes = profile_snapshot::bytes(&root.join(RECORD), 262144).map_err(|_| ERROR)?;
    let saved: Installed = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
    let config: Config = serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
    let url = reqwest::Url::parse(&saved.public_url).map_err(|_| ERROR)?;
    if saved
        .network_mode
        .as_deref()
        .is_some_and(|m| !matches!(m, "local" | "quick" | "named"))
        || saved.network_mode.as_deref() == Some("named")
            && config
                .connection
                .as_ref()
                .is_none_or(|c| c.named_tunnel.is_none())
        || saved.version != 1
        || saved.root != root
        || uuid::Uuid::parse_str(&saved.environment).is_err()
        || config.native_installation.as_ref() != Some(&root)
        || config.native_profile_handoff.is_some()
        || saved.bind.port() == 0
        || config
            .connection
            .as_ref()
            .is_none_or(|c| c.dashboard_bind.port() == saved.bind.port())
        || url.scheme() != "http"
        || url.port_or_known_default() != Some(saved.bind.port())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ERROR.into());
    }
    Ok(saved)
}

/// Authorize normal CLI profiles only when they exactly match this installation's
/// retained configuration. No copied login, guessed profile or fallback path.
pub(crate) fn validate_config(config: &Config) -> Result<(), String> {
    let saved = read(config.native_installation.as_deref().ok_or(ERROR)?)?;
    let expected: Config = serde_json::from_value(saved.configuration).map_err(|_| ERROR)?;
    if serde_json::to_value(config).map_err(|_| ERROR)?
        != serde_json::to_value(expected).map_err(|_| ERROR)?
    {
        return Err("Native installation configuration changed. Restore its saved configuration before starting; chat data was retained".into());
    }
    Ok(())
}

fn write_once(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > 262144 {
        return Err("Native setup configuration exceeds the supported record size; existing files were retained".into());
    }
    if present(path)? {
        if profile_snapshot::bytes(path, 262144).map_err(|_| ERROR)? == bytes {
            return Ok(());
        }
        return Err(ERROR.into());
    }
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or(ERROR)?).map_err(|_| ERROR)?;
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.as_file().sync_all().map_err(|_| ERROR)?;
    file.persist_noclobber(path).map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(path.parent().ok_or(ERROR)?, true)
        .and_then(|f| f.sync_all())
        .map_err(|_| ERROR)?;
    Ok(())
}

fn configuration_path(saved: &Installed) -> PathBuf {
    if saved.revision == 0 {
        saved.root.join(CONFIG)
    } else {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(serde_json::to_vec(&saved.configuration).expect("JSON value"));
        saved
            .root
            .join(format!("configuration-{}-{hash:x}.json", saved.revision))
    }
}

fn materialize(saved: &Installed) -> Result<Config, String> {
    let config: Config = serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
    config.validate()?;
    write_once(
        &configuration_path(saved),
        &serde_json::to_vec(&saved.configuration).map_err(|_| ERROR)?,
    )?;
    private_directory::prepare(&saved.root.join("state")).map_err(|_| ERROR)?;
    Ok(config)
}

fn summary(saved: &Installed) -> Value {
    json!({"runtime":"rust","installed":true,"environmentId":saved.environment,
        "dashboard":format!("http://{}",saved.configuration["connection"]["dashboard_bind"].as_str().unwrap_or("")),
        "configurationRevision":saved.revision,"profilesKeptInPlace":true,"migrationComplete":false})
}

/// Idempotent preparation. No model prompt, service registration or process
/// replacement happens here. The GUI path belongs to the current npm package.
pub async fn prepare(
    root: &Path,
    gui: &Path,
    port: Option<u16>,
    config_file: Option<&Path>,
) -> Result<Value, String> {
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(
            "Choose an absolute native installation directory without parent traversal".into(),
        );
    }
    directory(gui)?;
    if !gui.join("index.html").is_file() {
        return Err(
            "The packaged dashboard is missing. Reinstall the complete native package".into(),
        );
    }
    legacy_guard(root)?;
    if let Some(port) = port {
        if port == 0 || port > 65530 {
            return Err("Choose a dashboard port between 1 and 65530".into());
        }
    }
    if exists(root)? {
        let saved = read(root)?;
        if port.is_some_and(|p| {
            saved.configuration["connection"]["dashboard_bind"]
                .as_str()
                .and_then(|a| a.parse::<std::net::SocketAddr>().ok())
                .is_none_or(|a| a.port() != p)
        }) {
            return Err("The requested port differs from this installation's saved port; no runtime was started".into());
        }
        if config_file.is_some() {
            return Err("This installation is already configured; onboarding cannot replace its existing configuration".into());
        }
        let _lease = profile_lock::acquire(root)?;
        materialize(&saved)?;
        return Ok(summary(&saved));
    }
    // Refuse any unknown files before creating a lock or changing permissions.
    empty_setup_root(root)?;
    let mut config = if let Some(path) = config_file {
        let raw =
            profile_snapshot::bytes(path, 262144).map_err(|_| "Cannot read setup configuration")?;
        serde_json::from_slice::<Config>(&raw).map_err(|_| "Invalid native setup configuration")?
    } else {
        defaults::configuration(port.unwrap_or(10100)).await?
    };
    if config.native_installation.is_some() || config.native_profile_handoff.is_some() {
        return Err("Setup needs an unbound configuration; existing installations must use their own start command".into());
    }
    config.validate_export()?;
    let connection = config
        .connection
        .as_ref()
        .ok_or("Setup needs a loopback dashboard address in connection.dashboard_bind")?;
    if port.is_some_and(|p| p != connection.dashboard_bind.port()) {
        return Err("The setup port conflicts with the configuration".into());
    }
    let phone_port = connection
        .dashboard_bind
        .port()
        .checked_add(5)
        .filter(|p| *p > 0)
        .ok_or("Dashboard port is too high")?;
    if let Some(named) = &connection.named_tunnel {
        if named.origin.port() != phone_port || !named.origin.ip().is_ipv4() {
            return Err("Named tunnel must target the phone listener at dashboard port + 5".into());
        }
        crate::named_tunnel::preflight(named).await?;
    }
    // Check both listeners before committing an immutable setup. A busy port
    // must let the user retry with --port, not leave a configured dead end.
    let _ports = (
        crate::listener::reserve(connection.dashboard_bind)
            .map_err(|_| "The dashboard port is unavailable. Choose another --port before setup")?,
        crate::listener::reserve((std::net::Ipv4Addr::UNSPECIFIED, phone_port)).map_err(|_| {
            "The phone port (dashboard port + 5) is unavailable. Choose another --port before setup"
        })?,
    );
    let public_url = format!("http://{}:{phone_port}", defaults::local_ip());
    // The parent may be new, but must not be a link into somebody else's state.
    let parent = root.parent().ok_or(ERROR)?;
    if !present(parent)? {
        private_directory::prepare(parent)
            .map_err(|_| "Create the parent of OPENCODEX_HOME first")?;
    }
    directory(parent)?;
    private_directory::prepare(root).map_err(|_| ERROR)?;
    let _lease = profile_lock::acquire(root)?;
    legacy_guard(root)?;
    // A second setup may have finished while CLI discovery was in flight.
    if exists(root)? {
        return Err(
            "Another setup completed. Run start again to use its saved installation".into(),
        );
    }
    empty_setup_root(root)?;
    let root = directory(root)?;
    config.native_installation = Some(root.clone());
    let saved = Installed {
        version: 1,
        revision: 0,
        network_revision: 0,
        network_mode: config
            .connection
            .as_ref()
            .and_then(|c| c.named_tunnel.as_ref())
            .map(|_| "named".into()),
        environment: uuid::Uuid::new_v4().to_string(),
        root: root.clone(),
        bind: format!("0.0.0.0:{phone_port}").parse().map_err(|_| ERROR)?,
        public_url,
        configuration: serde_json::to_value(config).map_err(|_| ERROR)?,
    };
    write_once(
        &root.join(RECORD),
        &serde_json::to_vec(&saved).map_err(|_| ERROR)?,
    )?;
    materialize(&saved)?;
    Ok(summary(&saved))
}

pub async fn start(
    root: &Path,
    gui: &Path,
    port: Option<u16>,
    stop: impl std::future::Future<Output = ()>,
) -> Result<i32, String> {
    prepare(root, gui, port, None).await?;
    let _lease = profile_lock::acquire(root)?;
    let saved = read(root)?;
    materialize(&saved)?;
    let args: Vec<OsString> = vec![
        saved.root.join("state").into(),
        saved.bind.to_string().into(),
        format!("http://{}:{}", defaults::local_ip(), saved.bind.port()).into(),
        configuration_path(&saved).into(),
        directory(gui)?.into(),
    ];
    crate::supervisor::run(args, stop).await.map_err(|_| {
        "Native bridge stopped unexpectedly. Its installation and current history are retained"
            .into()
    })
}

pub(crate) fn control_target(root: &Path) -> Result<(String, String), String> {
    let saved = read(root)?;
    let config: Config = serde_json::from_value(saved.configuration).map_err(|_| ERROR)?;
    config.validate()?;
    Ok((
        format!("http://{}", config.connection.ok_or(ERROR)?.dashboard_bind),
        saved.environment,
    ))
}

pub fn before_start(state: &Path, config: Option<&Config>) -> Result<(), String> {
    // Check persisted ownership before acquiring a lock, clearing invitations or
    // opening the store for writes. Removing a receipt must not unbind its data.
    let database = state.join("native-connect.sqlite");
    if present(&database)? {
        storage_stage::open(&database, false).map_err(|_| ERROR)?;
        let db = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| ERROR)?;
        db.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| ERROR)?;
        let bound: Option<String> = db
            .query_row(
                "SELECT value FROM metadata WHERE key='native.installation'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| ERROR)?;
        if let Some(bound) = bound {
            if config.and_then(|c| c.native_installation.as_deref()) != Some(Path::new(&bound)) {
                return Err("Restore this bridge's native installation configuration".into());
            }
        }
    }
    if let Some(root) = config.and_then(|c| c.native_installation.as_deref()) {
        let saved = read(root)?;
        if directory(state)? != saved.root.join("state") {
            return Err(
                "Native configuration belongs to a different bridge state directory".into(),
            );
        }
        validate_config(config.ok_or(ERROR)?)?;
    } else if state.file_name().is_some_and(|n| n == "state")
        && state.parent().map(exists).transpose()?.unwrap_or(false)
    {
        return Err("This state requires its saved native installation configuration".into());
    }
    Ok(())
}

pub fn bind_store(store: &crate::store::Store, config: Option<&Config>) -> Result<(), String> {
    let bound: Option<String> = store
        .db
        .query_row(
            "SELECT value FROM metadata WHERE key='native.installation'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| ERROR)?;
    let root = config.and_then(|c| c.native_installation.as_deref());
    if root.is_none() {
        return if bound.is_some() {
            Err("Restore this bridge's native installation configuration".into())
        } else {
            Ok(())
        };
    }
    before_start(&store.directory, config)?;
    let saved = read(root.ok_or(ERROR)?)?;
    let path = saved.root.to_str().ok_or(ERROR)?;
    if bound.as_deref().is_some_and(|s| s != path) {
        return Err(ERROR.into());
    }
    let tx = store.db.unchecked_transaction().map_err(|_| ERROR)?;
    let old: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| ERROR)?;
    if old.is_some_and(|s| s != saved.environment) {
        return Err("This bridge state belongs to a different installation; no pairing or history was replaced".into());
    }
    tx.execute(
        "INSERT OR IGNORE INTO metadata VALUES ('environment_id',?1)",
        [&saved.environment],
    )
    .map_err(|_| ERROR)?;
    tx.execute(
        "INSERT OR IGNORE INTO metadata VALUES ('native.installation',?1)",
        [path],
    )
    .map_err(|_| ERROR)?;
    if let Some(mode) = &saved.network_mode {
        if !matches!(mode.as_str(), "local" | "quick" | "named") {
            return Err(ERROR.into());
        }
        let applied: Option<String> = tx
            .query_row(
                "SELECT value FROM metadata WHERE key='native.network-revision'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| ERROR)?;
        if applied.as_deref() != Some(saved.network_revision.to_string().as_str()) {
            for (key, value) in [
                (
                    "quick_tunnel_enabled",
                    if mode == "local" { "false" } else { "true" },
                ),
                (
                    "pairing_remote",
                    if mode == "local" { "false" } else { "true" },
                ),
                ("local_network_enabled", "true"),
            ] {
                tx.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,value]).map_err(|_|ERROR)?;
            }
            tx.execute("INSERT INTO metadata VALUES ('native.network-revision',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[saved.network_revision.to_string()]).map_err(|_|ERROR)?;
        }
    }
    tx.commit().map_err(|_| ERROR.into())
}

/// Verify the receipt and generated configuration before service registration.
pub fn service_configuration(root: &Path) -> Result<(), String> {
    if !exists(root)? {
        let generation = crate::npm_upgrade::managed_generation(root)?;
        if let Some((_, _, c)) = crate::migrated_configuration::current(&generation)? {
            return c.validate();
        }
        return crate::launch_config::load(&generation.join("configuration.json")).map(|_| ());
    }
    let saved = read(root)?;
    let config: Config = serde_json::from_value(saved.configuration.clone()).map_err(|_| ERROR)?;
    config.validate()?;
    let expected = serde_json::to_vec(&saved.configuration).map_err(|_| ERROR)?;
    if profile_snapshot::bytes(&configuration_path(&saved), 262144).map_err(|_| ERROR)? != expected
    {
        return Err(ERROR.into());
    }
    Ok(())
}
