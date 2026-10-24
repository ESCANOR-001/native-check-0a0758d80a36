//! Durable npm-launcher handoff from an exactly closed legacy owner. Package
//! installation never executes this module. The next explicit bridge launch
//! prepares and selects one generation, without moving the agent profiles.
use crate::{legacy_export, profile_lock, profile_snapshot, service_cutover, storage_stage};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const ERROR: &str =
    "The npm upgrade could not be verified. Existing chat data and recovery files were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    closed_owner: PathBuf,
    configuration: PathBuf,
    bind: String,
    public_url: String,
    gui: PathBuf,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn location(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(ERROR.into());
    }
    let parent = path.parent().ok_or(ERROR)?;
    storage_stage::open(parent, true).map_err(|_| ERROR)?;
    Ok(parent
        .canonicalize()
        .map_err(|_| ERROR)?
        .join(path.file_name().ok_or(ERROR)?))
}
fn write_once(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if fs::symlink_metadata(path).is_ok() {
        let previous = read(path, 262144)?;
        if previous == bytes {
            return Ok(());
        }
        // Only the same journal-bound intent can complete an interrupted write.
        if !bytes.starts_with(&previous) {
            return Err(ERROR.into());
        }
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options.open(path).map_err(|_| ERROR)?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && profile_snapshot::regular(&m))
        || storage_stage::links(&file).map_err(|_| ERROR)? != 1
    {
        return Err(ERROR.into());
    }
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(path.parent().ok_or(ERROR)?, true)
        .map_err(|_| ERROR)?
        .sync_all()
        .map_err(|_| ERROR)?;
    Ok(())
}
struct Journal {
    db: Connection,
    _lease: profile_lock::Lease,
    root: PathBuf,
}
impl Journal {
    fn open(root: &Path, create: bool) -> Result<Self, String> {
        let root = location(root)?;
        if !create
            && !root
                .join("npm-upgrade.sqlite")
                .try_exists()
                .map_err(|_| ERROR)?
        {
            return Err(ERROR.into());
        }
        if root.try_exists().map_err(|_| ERROR)? {
            storage_stage::open(&root, true).map_err(|_| ERROR)?;
            for entry in fs::read_dir(&root).map_err(|_| ERROR)? {
                let entry = entry.map_err(|_| ERROR)?;
                let name = entry.file_name();
                let name = name.to_str().ok_or(ERROR)?;
                if ![
                    "server.lock",
                    "npm-upgrade.sqlite",
                    "npm-upgrade.sqlite-wal",
                    "npm-upgrade.sqlite-shm",
                    "npm-upgrade.sqlite-journal",
                    "cutover.json",
                    "service",
                ]
                .contains(&name)
                    && !matches!(name, "attempt-1" | "attempt-2" | "attempt-3")
                {
                    return Err(ERROR.into());
                }
            }
        }
        let lease = profile_lock::acquire(&root)?;
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let path = root.join(format!("npm-upgrade.sqlite{suffix}"));
            if fs::symlink_metadata(&path).is_ok() {
                let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
                if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
                    return Err(ERROR.into());
                }
            }
        }
        let db = Connection::open(root.join("npm-upgrade.sqlite")).map_err(|_| ERROR)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.join("npm-upgrade.sqlite"),
                fs::Permissions::from_mode(0o600),
            )
            .map_err(|_| ERROR)?;
        }
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| ERROR)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 262144)
            .map_err(|_| ERROR)?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; PRAGMA cache_size=-256; PRAGMA mmap_size=0; CREATE TABLE IF NOT EXISTS upgrade(id INTEGER PRIMARY KEY CHECK(id=1),plan BLOB NOT NULL,receipt_hash TEXT NOT NULL,config_hash TEXT NOT NULL,attempt INTEGER NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('preparing','selected','cancelled')));").map_err(|_| ERROR)?;
        Ok(Self {
            db,
            _lease: lease,
            root,
        })
    }
}
/// Closed-owner proof is mandatory: this command never guesses a process from a
/// PID, stops Desktop, executes npm hooks, or resumes a native session itself.
pub fn prepare(root: &Path, plan_path: &Path) -> Result<Value, String> {
    let raw = read(plan_path, 32768)?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    let receipt = read(&plan.closed_owner, 8 * 1024 * 1024)?;
    let owner: Value = serde_json::from_slice(&receipt).map_err(|_| ERROR)?;
    let config = read(&plan.configuration, 262144)?;
    let parsed: crate::chat::Config = serde_json::from_slice(&config).map_err(|_| ERROR)?;
    parsed.validate()?;
    let address: std::net::SocketAddr = plan.bind.parse().map_err(|_| ERROR)?;
    let url = reqwest::Url::parse(&plan.public_url).map_err(|_| ERROR)?;
    if plan.version != 1
        || owner["profile_mode"] != "existing"
        || owner["source_hold"].as_str().is_none()
        || address.port() == 0
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ERROR.into());
    }
    storage_stage::open(&plan.gui, true).map_err(|_| ERROR)?;
    let root_path = location(root)?;
    for input in [
        &plan.closed_owner,
        &plan.configuration,
        &parsed.codex.profile,
        &parsed.claude.profile,
        &plan.gui,
        plan_path,
    ] {
        let input = input.canonicalize().map_err(|_| ERROR)?;
        if root_path.starts_with(&input) || input.starts_with(&root_path) {
            return Err(ERROR.into());
        }
    }
    let journal = Journal::open(root, true)?;
    let old: Option<(Vec<u8>, String, String, u32, String)> = journal
        .db
        .query_row(
            "SELECT plan,receipt_hash,config_hash,attempt,phase FROM upgrade WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|_| ERROR)?;
    let mut attempt = 1;
    if let Some((saved, rh, ch, n, phase)) = old {
        if saved != raw
            || rh != hash(&receipt)
            || ch != hash(&config)
            || phase == "cancelled"
            || !(1..=3).contains(&n)
        {
            return Err(ERROR.into());
        }
        attempt = n;
    } else {
        if journal.root.join("service").exists()
            || journal.root.join("attempt-1").exists()
            || journal.root.join("cutover.json").exists()
        {
            return Err(ERROR.into());
        }
        journal
            .db
            .execute(
                "INSERT INTO upgrade VALUES(1,?1,?2,?3,1,'preparing')",
                params![raw, hash(&receipt), hash(&config)],
            )
            .map_err(|_| ERROR)?;
    }
    let mut export = journal.root.join(format!("attempt-{attempt}"));
    if export.try_exists().map_err(|_| ERROR)?
        && !export
            .join("prepared.json")
            .try_exists()
            .map_err(|_| ERROR)?
    {
        // An unpublished partial export cannot be reused or deleted. Allocate a
        // bounded fresh attempt only before any cutover was selected.
        if journal.root.join("service").exists()
            || journal.root.join("cutover.json").exists()
            || attempt == 3
        {
            return Err(ERROR.into());
        }
        attempt += 1;
        journal
            .db
            .execute(
                "UPDATE upgrade SET attempt=?1 WHERE id=1 AND phase='preparing'",
                [attempt],
            )
            .map_err(|_| ERROR)?;
        export = journal.root.join(format!("attempt-{attempt}"));
        if fs::symlink_metadata(&export).is_ok() {
            return Err(ERROR.into());
        }
    }
    if !export.try_exists().map_err(|_| ERROR)? {
        legacy_export::run(&export, &plan.closed_owner, &plan.configuration)?;
    }
    if read(plan_path, 32768)? != raw
        || read(&plan.closed_owner, 8 * 1024 * 1024)? != receipt
        || read(&plan.configuration, 262144)? != config
    {
        return Err(ERROR.into());
    }
    let cutover = json!({"version":1,"export_root":export,"bind":plan.bind,"public_url":plan.public_url,"gui":plan.gui});
    let cutover_path = journal.root.join("cutover.json");
    write_once(
        &cutover_path,
        &serde_json::to_vec(&cutover).map_err(|_| ERROR)?,
    )?;
    let selected = service_cutover::select(&journal.root.join("service"), &cutover_path)?;
    journal
        .db
        .execute("UPDATE upgrade SET phase='selected' WHERE id=1", [])
        .map_err(|_| ERROR)?;
    Ok(
        json!({"phase":selected["phase"],"service":journal.root.join("service"),"attempt":attempt,"profilesKeptInPlace":true,"migrationComplete":false}),
    )
}

/// Same immutable plan is required on every invocation; a repeated npm launch
/// selects the existing generation instead of replaying an old export.
pub async fn run<F: std::future::Future<Output = ()>>(
    root: &Path,
    plan: &Path,
    stop: F,
) -> Result<i32, String> {
    prepare(root, plan)?;
    // The journal lease covers the service lifetime, preventing a second npm
    // launcher from concurrently changing selection or cancelling preparation.
    let journal = Journal::open(root, false)?;
    service_cutover::run(&journal.root.join("service"), stop).await
}
pub fn cancel(root: &Path) -> Result<Value, String> {
    let journal = Journal::open(root, false)?;
    let phase: String = journal
        .db
        .query_row("SELECT phase FROM upgrade WHERE id=1", [], |r| r.get(0))
        .map_err(|_| ERROR)?;
    if phase == "preparing" {
        return Err("Preparation has not selected a service. Keep the partial export and use source recovery; no data was restored".into());
    }
    let report = service_cutover::cancel(&journal.root.join("service"))?;
    journal
        .db
        .execute("UPDATE upgrade SET phase='cancelled' WHERE id=1", [])
        .map_err(|_| ERROR)?;
    Ok(report)
}
