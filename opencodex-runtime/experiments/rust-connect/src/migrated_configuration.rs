//! Configuration revisions for an already-running legacy migration. Original
//! cutover evidence stays immutable; the current database selects a new file and
//! activation binding in one transaction. Rollback never restores chat data.
use crate::{chat::Config, profile_lock, profile_snapshot, storage_stage};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
const ERROR: &str = "Migrated configuration could not be verified; current history and original upgrade records were retained";
// A Desktop update can remove its previous reader executable. That program
// location is replaceable; the profile, Desktop endpoint and activation remain
// the same ownership boundary. The candidate still passes validate_export().
fn ownership(config: &Config) -> Result<Value, String> {
    let mut codex = config.codex.clone();
    if let Some(crate::agent::SharedServer::Desktop {
        reader_executable, ..
    }) = codex.as_mut().and_then(|c| c.shared_server.as_mut())
    {
        *reader_executable = PathBuf::new();
    }
    serde_json::to_value((
        codex.as_ref().map(|c| (&c.profile, &c.shared_server)),
        config
            .claude
            .as_ref()
            .map(|c| (&c.profile, &c.shared_server)),
    ))
    .map_err(|_| ERROR.into())
}
fn raw(path: &Path) -> Result<Vec<u8>, String> {
    profile_snapshot::bytes(path, 262144).map_err(|_| ERROR.into())
}
fn digest(bytes: &[u8]) -> String {
    crate::auth::digest(std::str::from_utf8(bytes).unwrap_or(""))
}
fn database(generation: &Path) -> Result<Connection, String> {
    let path = generation.join("bridge/state/native-connect.sqlite");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        if suffix.is_empty() || std::fs::symlink_metadata(&p).is_ok() {
            let f = storage_stage::open(&p, false).map_err(|_| ERROR)?;
            if storage_stage::links(&f).map_err(|_| ERROR)? != 1 {
                return Err(ERROR.into());
            }
        }
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| ERROR)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 262144)
        .map_err(|_| ERROR)?;
    db.busy_timeout(std::time::Duration::from_millis(500))
        .map_err(|_| ERROR)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA cache_size=-256; PRAGMA mmap_size=0;").map_err(|_|ERROR)?;
    Ok(db)
}
fn path(generation: &Path, revision: u64, hash: &str) -> PathBuf {
    generation.join(format!("configuration-revision-{revision}-{hash}.json"))
}
/// Original configuration remains the immutable migration proof. None means no
/// explicit configuration edit has ever been committed to this generation.
pub(crate) fn current(generation: &Path) -> Result<Option<(u64, PathBuf, Config)>, String> {
    let db = database(generation)?;
    if !crate::legacy_codex_headers::table(&db, "native_configuration")? {
        return Ok(None);
    }
    let row:Option<(u64,String,String,String)>=db.query_row("SELECT revision,base,config,digest FROM native_configuration ORDER BY revision DESC LIMIT 1",[],|r|Ok((u64::from(r.get::<_,u32>(0)?),r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_|ERROR)?;
    let Some((revision, base, config, hash)) = row else {
        return Err(ERROR.into());
    };
    if revision == 0
        || base != digest(&raw(&generation.join("configuration.json"))?)
        || hash != digest(config.as_bytes())
    {
        return Err(ERROR.into());
    }
    let p = path(generation, revision, &hash);
    if raw(&p)? != config.as_bytes() {
        return Err(ERROR.into());
    }
    let c: Config = serde_json::from_str(&config).map_err(|_| ERROR)?;
    let binding:String=db.query_row("SELECT configuration FROM legacy_activation WHERE id=1 AND phase IN ('started','running')",[],|r|r.get(0)).map_err(|_|ERROR)?;
    if binding != crate::legacy_activation::retained_binding(&c)? {
        return Err(ERROR.into());
    }
    Ok(Some((revision, p, c)))
}
fn selected(root: &Path) -> Result<(PathBuf, u64, Config), String> {
    let generation = crate::npm_upgrade::managed_generation(root)?;
    let (revision, c) = match current(&generation)? {
        Some((r, _, c)) => (r, c),
        None => (
            0,
            serde_json::from_slice(&raw(&generation.join("configuration.json"))?)
                .map_err(|_| ERROR)?,
        ),
    };
    Ok((generation, revision, c))
}
pub fn inspect(root: &Path) -> Result<Value, String> {
    let (_, r, c) = selected(root)?;
    Ok(json!({"revision":r,"configuration":c,"migrated":true}))
}
pub fn configure(
    root: &Path,
    op: &str,
    input: &str,
    expected: Option<u64>,
) -> Result<Value, String> {
    configure_mode(root, op, input, expected, None)
}
fn configure_mode(
    root: &Path,
    op: &str,
    input: &str,
    expected: Option<u64>,
    mode: Option<&str>,
) -> Result<Value, String> {
    if !matches!(op, "check" | "apply" | "rollback") {
        return Err(ERROR.into());
    }
    let mutation = op != "check";
    let _root = if mutation {
        Some(profile_lock::acquire(root)?)
    } else {
        None
    };
    let (generation, revision, old) = selected(root)?;
    let _selection = if mutation {
        Some(crate::service_cutover::configuration_lease(
            &root.join("service"),
        )?)
    } else {
        None
    };
    let _state = if mutation {
        Some(profile_lock::acquire(&generation.join("bridge/state"))?)
    } else {
        None
    };
    if expected.is_some_and(|r| r != revision) || (mutation && expected.is_none()) {
        return Err(
            "Configuration changed or --revision is missing; inspect the current revision again"
                .into(),
        );
    }
    let value = if op == "rollback" {
        let r: u64 = input.parse().map_err(|_| ERROR)?;
        if r >= revision {
            return Err("Choose an earlier configuration revision".into());
        }
        if r == 0 {
            raw(&generation.join("configuration.json"))?
        } else {
            database(&generation)?
                .query_row(
                    "SELECT config FROM native_configuration WHERE revision=?1",
                    [u32::try_from(r).map_err(|_| ERROR)?],
                    |r| r.get::<_, String>(0),
                )
                .map_err(|_| ERROR)?
                .into_bytes()
        }
    } else {
        raw(Path::new(input))?
    };
    let config: Config = serde_json::from_slice(&value).map_err(|_| "Invalid configuration")?;
    config.validate_export()?;
    // Profiles and their ownership must never be switched by a settings edit.
    if config.native_installation.is_some()
        || config.native_profile_handoff != old.native_profile_handoff
        || ownership(&config)? != ownership(&old)?
    {
        return Err("Keep the migration's existing profiles and Desktop ownership; these cannot be replaced by configuration editing".into());
    }
    let c = config
        .connection
        .as_ref()
        .ok_or("A loopback dashboard connection is required")?;
    let port = c.dashboard_bind.port().checked_add(5).ok_or(ERROR)?;
    if c.named_tunnel
        .as_ref()
        .is_some_and(|t| t.origin.port() != port || !t.origin.ip().is_ipv4())
    {
        return Err("Named tunnel origin must match the new phone port".into());
    }
    let binding = crate::legacy_activation::binding(&config)?;
    if !mutation {
        return Ok(json!({"valid":true,"revision":revision,"requiresStoppedBridge":true}));
    }
    let _ports = (
        crate::listener::reserve(c.dashboard_bind).map_err(|_| "Dashboard port is occupied")?,
        crate::listener::reserve((std::net::Ipv4Addr::UNSPECIFIED, port))
            .map_err(|_| "Phone port is occupied")?,
    );
    let mut store = crate::legacy_stage::existing_store(&generation.join("bridge/state"))?;
    store.assert_retained_migration_configuration(&old)?;
    let phase: String = store
        .db
        .query_row("SELECT phase FROM legacy_activation WHERE id=1", [], |r| {
            r.get(0)
        })
        .map_err(|_| ERROR)?;
    if !matches!(phase.as_str(), "started" | "running") {
        return Err("Finish the first native startup before editing migrated settings; an unused migration remains eligible for its original rollback".into());
    }
    let next = u32::try_from(revision.checked_add(1).ok_or(ERROR)?).map_err(|_| ERROR)?;
    let encoded = serde_json::to_vec(&config).map_err(|_| ERROR)?;
    let hash = digest(&encoded);
    let destination = path(&generation, next.into(), &hash);
    if std::fs::symlink_metadata(&destination).is_ok() {
        if raw(&destination)? != encoded {
            return Err(ERROR.into());
        }
    } else {
        let mut f = tempfile::NamedTempFile::new_in(&generation).map_err(|_| ERROR)?;
        f.write_all(&encoded).map_err(|_| ERROR)?;
        f.as_file().sync_all().map_err(|_| ERROR)?;
        f.persist_noclobber(&destination).map_err(|_| ERROR)?;
        #[cfg(unix)]
        storage_stage::open(&generation, true)
            .and_then(|f| f.sync_all())
            .map_err(|_| ERROR)?;
    }
    let tx = store.db.transaction().map_err(|_| ERROR)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS native_configuration(revision INTEGER PRIMARY KEY,base TEXT NOT NULL,config TEXT NOT NULL,digest TEXT NOT NULL);").map_err(|_|ERROR)?;
    let last: u32 = tx
        .query_row(
            "SELECT coalesce(max(revision),0) FROM native_configuration",
            [],
            |r| r.get(0),
        )
        .map_err(|_| ERROR)?;
    if u64::from(last) != revision {
        return Err(ERROR.into());
    }
    let encoded = String::from_utf8(encoded).map_err(|_| ERROR)?;
    tx.execute(
        "INSERT INTO native_configuration VALUES(?1,?2,?3,?4)",
        params![
            next,
            digest(&raw(&generation.join("configuration.json"))?),
            encoded,
            hash
        ],
    )
    .map_err(|_| ERROR)?;
    tx.execute(
        "UPDATE legacy_activation SET configuration=?1,config=?2 WHERE id=1",
        params![binding, encoded],
    )
    .map_err(|_| ERROR)?;
    if let Some(mode) = mode {
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
            tx.execute("INSERT INTO metadata VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,value]).map_err(|_|ERROR)?;
        }
    }
    tx.commit().map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"configured","revision":next,"previousRevision":revision,"restartRequired":true,"historyRetained":true,"pairingRetained":true}),
    )
}

pub async fn remote(
    root: &Path,
    mode: &str,
    host: Option<&str>,
    token: Option<zeroize::Zeroizing<Vec<u8>>>,
    revision: u64,
) -> Result<Value, String> {
    if !matches!(mode, "local" | "quick" | "named") {
        return Err("Choose local, quick or named".into());
    }
    let (_, r, mut config) = selected(root)?;
    if r != revision {
        return Err("Configuration changed; inspect the current revision again".into());
    }
    let c = config
        .connection
        .as_mut()
        .ok_or("Add a loopback dashboard connection using config apply first")?;
    let port = c.dashboard_bind.port().checked_add(5).ok_or(ERROR)?;
    if mode == "named" {
        let named = crate::named_tunnel::Config {
            hostname: host.ok_or("A hostname is required")?.to_ascii_lowercase(),
            account: crate::auth::digest(&uuid::Uuid::new_v4().to_string()),
            origin: ([127, 0, 0, 1], port).into(),
        };
        named.validate()?;
        crate::named_tunnel::save(
            &named.account,
            token.ok_or("Supply a connector token on stdin")?,
        )
        .await?;
        c.inherited_quick_tunnel = None;
        c.named_tunnel = Some(named);
    } else {
        if host.is_some() || token.is_some() {
            return Err(ERROR.into());
        }
        c.inherited_quick_tunnel = None;
        c.named_tunnel = None;
    }
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(|_| ERROR)?;
    file.write_all(&serde_json::to_vec(&config).map_err(|_| ERROR)?)
        .map_err(|_| ERROR)?;
    let mut result = configure_mode(
        root,
        "apply",
        file.path().to_str().ok_or(ERROR)?,
        Some(revision),
        Some(mode),
    )?;
    result["connectionMode"] = json!(mode);
    result["reconnectPhone"] = json!(true);
    Ok(result)
}
pub async fn provision(
    root: &Path,
    account: &str,
    zone: &str,
    host: &str,
    token: zeroize::Zeroizing<Vec<u8>>,
    revision: u64,
) -> Result<Value, String> {
    let connector = {
        let _root = profile_lock::acquire(root)?;
        let (generation, r, config) = selected(root)?;
        if r != revision {
            return Err("Configuration changed; inspect the current revision again".into());
        }
        let _selection = crate::service_cutover::configuration_lease(&root.join("service"))?;
        let _state = profile_lock::acquire(&generation.join("bridge/state"))?;
        let c = config
            .connection
            .as_ref()
            .ok_or("Add a loopback dashboard connection first")?;
        let port = c.dashboard_bind.port().checked_add(5).ok_or(ERROR)?;
        let db = database(&generation)?;
        let environment: String = db
            .query_row(
                "SELECT value FROM metadata WHERE key='environment_id'",
                [],
                |r| r.get(0),
            )
            .map_err(|_| ERROR)?;
        crate::tunnel_provisioning::Provisioner::new()?
            .provision(token, account, zone, host, port, &environment)
            .await?
    };
    remote(root, "named", Some(host), Some(connector), revision).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> Value {
        json!({"workspace":"workspace", "codex":{
            "executable":"old-reader", "profile":"saved-profile", "model":"default",
            "shared_server":{"desktop_endpoint":{"path":"original-owner"},
                "reader_executable":"old-reader", "reader_workspace":"reader-workspace",
                "activation":{"kind":"system"}}
        }})
    }
    fn key(value: Value) -> Value {
        ownership(&serde_json::from_value::<Config>(value).unwrap()).unwrap()
    }
    #[test]
    fn migrated_reader_program_update_keeps_ownership() {
        let original = desktop();
        let mut repaired = original.clone();
        repaired["codex"]["executable"] = json!("new-reader");
        repaired["codex"]["shared_server"]["reader_executable"] = json!("new-reader");
        assert_eq!(key(original), key(repaired));
    }
    #[test]
    fn migrated_reader_update_cannot_replace_profile_or_desktop_owner() {
        let original = desktop();
        for (pointer, replacement) in [
            ("/codex/profile", json!("other-profile")),
            (
                "/codex/shared_server/desktop_endpoint/path",
                json!("other-owner"),
            ),
            (
                "/codex/shared_server/reader_workspace",
                json!("other-workspace"),
            ),
            ("/codex/shared_server/activation", Value::Null),
            ("/codex/shared_server", Value::Null),
            ("/codex", Value::Null),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert_ne!(key(original.clone()), key(changed), "{pointer}");
        }
    }
}
