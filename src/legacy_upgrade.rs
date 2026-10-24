//! Offline owned-install coordinator. Every step delegates to the existing
//! verified import/profile/activation receipts. No service or agent is launched,
//! stopped, discovered, or pointed at an original profile by this command.
use crate::{
    legacy_activation, legacy_rollback, legacy_stage, profile_handoff, profile_lock, storage_stage,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const ERROR: &str =
    "Upgrade preparation is incomplete or changed; source files and recovery copies were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    environment: String,
    export_snapshot: PathBuf,
    native_plan: PathBuf,
    configuration: PathBuf,
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    crate::profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn location(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|p| matches!(p, Component::CurDir | Component::ParentDir))
    {
        return Err(ERROR.into());
    }
    let mut parent = path;
    let mut leaves = Vec::new();
    while !parent.try_exists().map_err(|_| ERROR)? {
        leaves.push(parent.file_name().ok_or(ERROR)?);
        parent = parent.parent().ok_or(ERROR)?;
    }
    let mut result = parent.canonicalize().map_err(|_| ERROR)?;
    for leaf in leaves.into_iter().rev() {
        result.push(leaf);
    }
    Ok(result)
}
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn regular(path: &Path) -> Result<(), String> {
    let f = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&f).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    Ok(())
}
fn only_empty_journal(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|_| ERROR)? {
        let name = entry.map_err(|_| ERROR)?.file_name();
        if ![
            "server.lock",
            "upgrade.sqlite",
            "upgrade.sqlite-wal",
            "upgrade.sqlite-shm",
            "upgrade.sqlite-journal",
        ]
        .iter()
        .any(|s| name == *s)
        {
            return Err(ERROR.into());
        }
    }
    Ok(())
}
// The input receipt is durable before this function is called. An interrupted
// private temporary file may be completed only when it is a prefix of the same
// recorded configuration. The final name is never overwritten.
fn configuration(root: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let target = root.join("configuration.json");
    let pending = root.join("configuration.pending");
    if fs::symlink_metadata(&target).is_ok() {
        // A crash after link-before-unlink leaves two names for this one file.
        // Remove only that exact verified pending alias, never another file.
        if fs::symlink_metadata(&pending).is_ok() {
            let a = storage_stage::open(&target, false).map_err(|_| ERROR)?;
            let b = storage_stage::open(&pending, false).map_err(|_| ERROR)?;
            if storage_stage::identity(&a).map_err(|_| ERROR)?
                != storage_stage::identity(&b).map_err(|_| ERROR)?
                || storage_stage::links(&a).map_err(|_| ERROR)? != 2
                || read(&target, 262144)? != bytes
            {
                return Err(ERROR.into());
            }
            drop(a);
            drop(b);
            fs::remove_file(&pending).map_err(|_| ERROR)?;
        }
        regular(&target)?;
        if read(&target, 262144)? != bytes {
            return Err(ERROR.into());
        }
        return Ok(target);
    }
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if fs::symlink_metadata(&pending).is_ok() {
        regular(&pending)?;
        if !bytes.starts_with(&read(&pending, 262144)?) {
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
    let mut file = options.open(&pending).map_err(|_| ERROR)?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && crate::profile_snapshot::regular(&m))
        || storage_stage::links(&file).map_err(|_| ERROR)? != 1
    {
        return Err(ERROR.into());
    }
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    drop(file);
    fs::hard_link(&pending, &target).map_err(|_| ERROR)?;
    fs::remove_file(pending).map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(root, true)
        .map_err(|_| ERROR)?
        .sync_all()
        .map_err(|_| ERROR)?;
    Ok(target)
}

/// `prepare` is resumable with exactly the same inputs; `check` verifies an
/// unused prepared generation; `rollback` removes only unchanged imported data.
/// After runtime use, the underlying receipt requires executable rollback with
/// current data. This coordinator never restores an old backup over new work.
pub fn run(operation: &str, root: &Path, plan_file: &Path) -> Result<Value, String> {
    if !matches!(operation, "prepare" | "check" | "rollback") {
        return Err(ERROR.into());
    }
    let raw = read(plan_file, 32768)?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    let native_raw = read(&plan.native_plan, 32768)?;
    let native: profile_handoff::Plan = serde_json::from_slice(&native_raw).map_err(|_| ERROR)?;
    let config_raw = read(&plan.configuration, 262144)?;
    let config: crate::chat::Config = serde_json::from_slice(&config_raw).map_err(|_| ERROR)?;
    config.validate()?;
    if plan.version != 1
        || native.version != 1
        || native.environment != plan.environment
        || config.codex.shared_server.is_some()
        || config.claude.shared_server.is_some()
    {
        return Err(ERROR.into());
    }
    // Resolve parent aliases, but reject the destination itself if it is a link.
    if fs::symlink_metadata(root).is_ok() {
        storage_stage::open(root, true).map_err(|_| ERROR)?;
    }
    let root = location(root)?;
    for key in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(key) {
            for name in [".codex", ".claude"] {
                if root.starts_with(location(&PathBuf::from(&home).join(name))?) {
                    return Err(ERROR.into());
                }
            }
        }
    }
    storage_stage::open(root.parent().ok_or(ERROR)?, true).map_err(|_| ERROR)?;
    if location(config.native_profile_handoff.as_deref().ok_or(ERROR)?)? != root.join("native")
        || location(&config.codex.profile)? != location(&native.codex.original_profile)?
        || location(&config.claude.profile)? != location(&native.claude.original_profile)?
    {
        return Err(ERROR.into());
    }
    for input in [
        plan_file,
        plan.export_snapshot.as_path(),
        plan.native_plan.as_path(),
        plan.configuration.as_path(),
        native.codex.original_profile.as_path(),
        native.claude.original_profile.as_path(),
        native.codex.snapshot.as_path(),
        native.claude.snapshot.as_path(),
    ] {
        if overlap(&root, &location(input)?) {
            return Err(ERROR.into());
        }
    }
    let journal_path = root.join("upgrade.sqlite");
    let existed = journal_path.try_exists().map_err(|_| ERROR)?;
    if operation != "prepare" && !existed {
        return Err(ERROR.into());
    }
    if root.try_exists().map_err(|_| ERROR)? {
        for entry in fs::read_dir(&root).map_err(|_| ERROR)? {
            let name = entry.map_err(|_| ERROR)?.file_name();
            if if existed {
                ![
                    "server.lock",
                    "upgrade.sqlite",
                    "upgrade.sqlite-wal",
                    "upgrade.sqlite-shm",
                    "upgrade.sqlite-journal",
                    "configuration.json",
                    "configuration.pending",
                    "bridge",
                    "native",
                ]
                .iter()
                .any(|s| name == *s)
            } else {
                name != "server.lock"
            } {
                return Err(ERROR.into());
            }
        }
    }
    let _lease = profile_lock::acquire(&root)?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = root.join(format!("upgrade.sqlite{suffix}"));
        if fs::symlink_metadata(&p).is_ok() {
            regular(&p)?;
        }
    }
    let db = Connection::open_with_flags(
        &journal_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | if existed {
                OpenFlags::empty()
            } else {
                OpenFlags::SQLITE_OPEN_CREATE
            },
    )
    .map_err(|_| ERROR)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).map_err(|_| ERROR)?;
    }
    db.busy_timeout(Duration::from_secs(2)).map_err(|_| ERROR)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA mmap_size=0;").map_err(|_| ERROR)?;
    let tables: Vec<String> = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 2").map_err(|_|ERROR)?.query_map([],|r|r.get(0)).map_err(|_|ERROR)?.collect::<rusqlite::Result<_>>().map_err(|_|ERROR)?;
    if tables.iter().any(|name| name != "upgrade") {
        return Err(ERROR.into());
    }
    if tables.is_empty() {
        if operation != "prepare" {
            return Err(ERROR.into());
        }
        only_empty_journal(&root)?;
        db.execute_batch("CREATE TABLE upgrade(id INTEGER PRIMARY KEY CHECK(id=1),binding TEXT NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('preparing','ready','rolling-back','rolled-back')));").map_err(|_| ERROR)?;
    }
    let mut hash = Sha256::new();
    for input in [&raw, &native_raw, &config_raw] {
        hash.update((input.len() as u64).to_le_bytes());
        hash.update(input);
    }
    let binding = format!("{:x}", hash.finalize());
    let prior: Option<(String, String)> = db
        .query_row("SELECT binding,phase FROM upgrade WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .map_err(|_| ERROR)?;
    let resumed = prior.is_some();
    let phase = if let Some((saved, phase)) = prior {
        if saved != binding {
            return Err(ERROR.into());
        }
        phase
    } else {
        if operation != "prepare" {
            return Err(ERROR.into());
        }
        only_empty_journal(&root)?;
        db.execute("INSERT INTO upgrade VALUES(1,?1,'preparing')", [&binding])
            .map_err(|_| ERROR)?;
        "preparing".into()
    };
    let state = root.join("bridge/state");
    match operation {
        "prepare" => {
            if !matches!(phase.as_str(), "preparing" | "ready") {
                return Err(ERROR.into());
            }
            let cfg = configuration(&root, &config_raw)?;
            legacy_stage::run(
                &plan.export_snapshot,
                &root.join("bridge"),
                &plan.environment,
            )?;
            profile_handoff::prepare(&root.join("native"), &native)?;
            legacy_activation::run(&state, &cfg)?;
            legacy_rollback::run(&state, false)?;
            db.execute("UPDATE upgrade SET phase='ready' WHERE id=1", [])
                .map_err(|_| ERROR)?;
        }
        "check" => {
            if phase != "ready" {
                return Err(ERROR.into());
            }
            if read(&root.join("configuration.json"), 262144)? != config_raw {
                return Err(ERROR.into());
            }
            legacy_rollback::run(&state, false)?;
        }
        "rollback" => {
            if !matches!(phase.as_str(), "ready" | "rolling-back" | "rolled-back") {
                return Err(ERROR.into());
            }
            // Verify before changing our journal. A used or running generation
            // refuses here without changing either coordinator or chat state.
            legacy_rollback::run(&state, false)?;
            db.execute("UPDATE upgrade SET phase='rolling-back' WHERE id=1", [])
                .map_err(|_| ERROR)?;
            legacy_rollback::run(&state, true)?;
            // The bridge's import and the copied CLI profile generation must
            // both be revoked before a coordinator releases the old owner.
            // This step is idempotent after an interrupted rollback commit.
            profile_handoff::rollback(&root.join("native"), &plan.environment)?;
            db.execute("UPDATE upgrade SET phase='rolled-back' WHERE id=1", [])
                .map_err(|_| ERROR)?;
        }
        _ => unreachable!(),
    }
    Ok(
        json!({"phase":match operation {"prepare"=>"owned-upgrade-prepared","check"=>"owned-upgrade-verified",_=>"rolled-back"},"resumed":resumed,"sourceUnchanged":true,"backupRetained":true,"runtimeStarted":false,"migrationComplete":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_configuration_copy_and_link_are_resumable_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bytes = b"{\"configuration\":\"private\"}";
        fs::write(root.join("configuration.pending"), &bytes[..8]).unwrap();
        let p = configuration(root, bytes).unwrap();
        assert_eq!(fs::read(&p).unwrap(), bytes);
        fs::hard_link(&p, root.join("configuration.pending")).unwrap();
        assert_eq!(configuration(root, bytes).unwrap(), p);
        assert!(!root.join("configuration.pending").exists());
        assert!(configuration(root, b"changed").is_err());
        assert_eq!(fs::read(&p).unwrap(), bytes);
    }
    #[test]
    fn unknown_pending_configuration_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let pending = temp.path().join("configuration.pending");
        fs::write(&pending, b"DO NOT REPLACE").unwrap();
        assert!(configuration(temp.path(), b"{}").is_err());
        assert_eq!(fs::read(&pending).unwrap(), b"DO NOT REPLACE");
        assert!(!temp.path().join("configuration.json").exists());
    }
}
