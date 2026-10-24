//! Durable launcher selection for an explicitly exported installation. Selection
//! never stops another process. Before the first spawn, fallback to the old
//! service is permanently disarmed; subsequent runs retain the current data.
use crate::{legacy_upgrade, profile_lock, profile_snapshot, storage_stage};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
const ERROR: &str =
    "Service cutover is not ready or has changed; the old service was not started or stopped";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    export_root: PathBuf,
    bind: String,
    public_url: String,
    gui: PathBuf,
}
struct Selection {
    db: Connection,
    _lease: profile_lock::Lease,
}
fn bytes(path: &Path) -> Result<Vec<u8>, String> {
    profile_snapshot::bytes(path, 262144).map_err(|_| ERROR.into())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn inputs(raw: &[u8]) -> Result<(Plan, Vec<OsString>, String), String> {
    let mut plan: Plan = serde_json::from_slice(raw).map_err(|_| ERROR)?;
    let address: std::net::SocketAddr = plan.bind.parse().map_err(|_| ERROR)?;
    let url = reqwest::Url::parse(&plan.public_url).map_err(|_| ERROR)?;
    if plan.version != 1
        || address.port() == 0
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !plan.export_root.is_absolute()
        || !plan.gui.is_absolute()
    {
        return Err(ERROR.into());
    }
    storage_stage::open(&plan.export_root, true).map_err(|_| ERROR)?;
    storage_stage::open(&plan.gui, true).map_err(|_| ERROR)?;
    plan.export_root = plan.export_root.canonicalize().map_err(|_| ERROR)?;
    plan.gui = plan.gui.canonicalize().map_err(|_| ERROR)?;
    let prepared = bytes(&plan.export_root.join("prepared.json"))?;
    let prepared: Value = serde_json::from_slice(&prepared).map_err(|_| ERROR)?;
    let generation = plan.export_root.join("generation");
    if prepared["phase"] != "prepared"
        || prepared["generation"] != json!(generation)
        || prepared["plan"] != json!(plan.export_root.join("upgrade-plan.json"))
    {
        return Err(ERROR.into());
    }
    let config_path = generation.join("configuration.json");
    let config = bytes(&config_path)?;
    let parsed: crate::chat::Config = serde_json::from_slice(&config).map_err(|_| ERROR)?;
    parsed.validate()?;
    if parsed.native_profile_handoff.as_ref() != Some(&generation.join("native")) {
        return Err(ERROR.into());
    }
    let arguments = vec![
        generation.join("bridge/state").into_os_string(),
        plan.bind.clone().into(),
        plan.public_url.clone().into(),
        config_path.into_os_string(),
        plan.gui.clone().into_os_string(),
    ];
    Ok((plan, arguments, hash(&config)))
}
impl Selection {
    fn open(directory: &Path, create: bool) -> Result<Self, String> {
        let path = directory.join("service.sqlite");
        if !create && !path.try_exists().map_err(|_| ERROR)? {
            return Err(ERROR.into());
        }
        // Refuse foreign directories rather than quietly treating normal state
        // as a new service-selection root.
        if directory.try_exists().map_err(|_| ERROR)? {
            for entry in fs::read_dir(directory).map_err(|_| ERROR)? {
                let name = entry.map_err(|_| ERROR)?.file_name();
                if ![
                    "server.lock",
                    "service.sqlite",
                    "service.sqlite-wal",
                    "service.sqlite-shm",
                    "service.sqlite-journal",
                ]
                .iter()
                .any(|n| name == *n)
                {
                    return Err(ERROR.into());
                }
            }
        }
        let lease = profile_lock::acquire(directory)?;
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let p = directory.join(format!("service.sqlite{suffix}"));
            if fs::symlink_metadata(&p).is_ok() {
                let file = storage_stage::open(&p, false).map_err(|_| ERROR)?;
                if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
                    return Err(ERROR.into());
                }
            }
        }
        let db = Connection::open(&path).map_err(|_| ERROR)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|_| ERROR)?;
        }
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| ERROR)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 262144)
            .map_err(|_| ERROR)?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; PRAGMA cache_size=-256; PRAGMA mmap_size=0; CREATE TABLE IF NOT EXISTS selection(id INTEGER PRIMARY KEY CHECK(id=1),plan BLOB NOT NULL,config_hash TEXT NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('selected','started','cancelled')));").map_err(|_| ERROR)?;
        Ok(Self { db, _lease: lease })
    }
    fn saved(&self) -> Result<Option<(Vec<u8>, String, String)>, String> {
        self.db
            .query_row(
                "SELECT plan,config_hash,phase FROM selection WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| ERROR.into())
    }
}
pub fn select(directory: &Path, plan_file: &Path) -> Result<Value, String> {
    let raw = bytes(plan_file)?;
    let (plan, _, digest) = inputs(&raw)?;
    let selection = Selection::open(directory, true)?;
    if let Some((old, config, phase)) = selection.saved()? {
        if old != raw || config != digest || phase == "cancelled" {
            return Err(ERROR.into());
        }
        return Ok(
            json!({"phase":phase,"sourceServiceTouched":false,"legacyFallbackAllowed":phase=="selected","migrationComplete":false}),
        );
    }
    legacy_upgrade::run(
        "check",
        &plan.export_root.join("generation"),
        &plan.export_root.join("upgrade-plan.json"),
    )?;
    selection
        .db
        .execute(
            "INSERT INTO selection VALUES(1,?1,?2,'selected')",
            rusqlite::params![raw, digest],
        )
        .map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"selected","sourceServiceTouched":false,"legacyFallbackAllowed":true,"migrationComplete":false}),
    )
}
pub fn cancel(directory: &Path) -> Result<Value, String> {
    let selection = Selection::open(directory, false)?;
    let (raw, digest, phase) = selection.saved()?.ok_or(ERROR)?;
    if phase == "started" {
        return Err("The native service may have written new work. Keep current data and use a compatible native executable; automatic legacy fallback is disabled".into());
    }
    let (plan, _, current) = inputs(&raw)?;
    if digest != current {
        return Err(ERROR.into());
    }
    // Also catches a manual launch outside this selector.
    legacy_upgrade::run(
        "check",
        &plan.export_root.join("generation"),
        &plan.export_root.join("upgrade-plan.json"),
    )?;
    selection
        .db
        .execute("UPDATE selection SET phase='cancelled' WHERE id=1", [])
        .map_err(|_| ERROR)?;
    Ok(json!({"phase":"cancelled","sourceServiceTouched":false,"migrationComplete":false}))
}
pub async fn run(
    directory: &Path,
    stop: impl std::future::Future<Output = ()>,
) -> Result<i32, String> {
    let selection = Selection::open(directory, false)?;
    let (raw, expected, phase) = selection.saved()?.ok_or(ERROR)?;
    if !matches!(phase.as_str(), "selected" | "started") {
        return Err(ERROR.into());
    }
    let (_, arguments, actual) = inputs(&raw)?;
    if expected != actual {
        return Err(ERROR.into());
    }
    // Durable BEFORE spawn: a crash or failed launch must not restore old data.
    selection
        .db
        .execute("UPDATE selection SET phase='started' WHERE id=1", [])
        .map_err(|_| ERROR)?;
    crate::supervisor::run(arguments, stop).await.map_err(|_| "Native service stopped unexpectedly. Current history was retained; legacy fallback remains disabled".into())
}
