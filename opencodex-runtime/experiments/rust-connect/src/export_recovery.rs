//! Recover only an unpublished export whose owner durably confirmed shutdown
//! and subsequently exited. Never infer worker exit from a stale PID alone.
use crate::{profile_snapshot, source_hold, storage_stage};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const ERROR: &str =
    "Source recovery is not proven safe; the old startup hold and all history were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stopped {
    version: u32,
    phase: String,
    pid: u32,
    environment: String,
    export: PathBuf,
    source_hold: Value,
}
fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(ERROR.into()),
    }
}
fn absent(path: &Path) -> Result<(), String> {
    if exists(path)? {
        return Err(ERROR.into());
    }
    Ok(())
}
fn exited(pid: u32) -> Result<(), String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(ERROR.into());
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
            System::Threading::{OpenProcess, WaitForSingleObject},
        };
        // SYNCHRONIZE permits waiting without asking for termination rights.
        let handle = unsafe { OpenProcess(0x00100000, 0, pid) };
        if handle.is_null() {
            if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                return Ok(());
            }
        } else {
            let status = unsafe { WaitForSingleObject(handle, 0) };
            unsafe {
                CloseHandle(handle);
            }
            if status == WAIT_OBJECT_0 {
                return Ok(());
            }
        }
    }
    // Alive, reused PID, permission denial and unknown status all refuse.
    Err(ERROR.into())
}
fn sync(root: &Path) -> Result<(), String> {
    #[cfg(unix)]
    storage_stage::open(root, true)
        .map_err(|_| ERROR)?
        .sync_all()
        .map_err(|_| ERROR)?;
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}
pub fn run(proof: &Path) -> Result<Value, String> {
    if !proof.is_absolute()
        || proof.file_name().and_then(|s| s.to_str()) != Some("stopped-owner.json")
        || proof.components().any(|p| {
            matches!(
                p,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(ERROR.into());
    }
    let root = proof
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    for path in root.ancestors() {
        storage_stage::open(path, true).map_err(|_| ERROR)?;
    }
    let raw =
        profile_snapshot::bytes(&root.join("stopped-owner.json"), 16384).map_err(|_| ERROR)?;
    let stopped: Stopped = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    if stopped.version != 1
        || stopped.phase != "workers-stopped"
        || stopped.export != root
        || stopped.environment.is_empty()
        || stopped.environment.len() > 256
    {
        return Err(ERROR.into());
    }
    let _lease = source_hold::recovery_lease(&stopped.source_hold)?;
    exited(stopped.pid)?;
    let hold = Path::new(stopped.source_hold["path"].as_str().ok_or(ERROR)?);
    let source = hold.parent().ok_or(ERROR)?;
    if root.starts_with(source) || source.starts_with(&root) {
        return Err(ERROR.into());
    }
    absent(&root.join("closed-owner.json"))?;
    absent(&source.join("native-upgrade-selected-service.json"))?;
    let journal = root.join("source-recovery.json");
    let expected = serde_json::to_vec(&json!({"version":1,"phase":"cancelled-unpublished-export","proof":format!("{:x}", Sha256::digest(&raw)),"source_hold":stopped.source_hold})).map_err(|_| ERROR)?;
    let resumed = exists(&journal)?;
    if resumed {
        if profile_snapshot::bytes(&journal, 16384).map_err(|_| ERROR)? != expected {
            return Err(ERROR.into());
        }
    } else if source_hold::bind(hold, &root, &stopped.environment)? != stopped.source_hold {
        return Err(ERROR.into());
    }
    // Recheck exact material before committing the recovery intent. A complete
    // or partial published receipt is never discarded by this operation.
    if profile_snapshot::bytes(&root.join("stopped-owner.json"), 16384).map_err(|_| ERROR)? != raw {
        return Err(ERROR.into());
    }
    absent(&root.join("closed-owner.json"))?;
    if !resumed {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&journal).map_err(|_| ERROR)?;
        file.write_all(&expected).map_err(|_| ERROR)?;
        file.sync_all().map_err(|_| ERROR)?;
        sync(&root)?;
    }
    source_hold::release_unselected(&stopped.source_hold, &root, &stopped.environment)?;
    Ok(
        json!({"phase":"source-released","historyRetained":true,"serviceStarted":false,"retry":resumed}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(pid: u32) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let export = root.join("export");
        let source = root.join("source");
        fs::create_dir(&export).unwrap();
        fs::create_dir(&source).unwrap();
        let hold = source.join("native-upgrade-source-hold.json");
        let raw =
            serde_json::to_vec(&json!({"version":1,"environment":"test","export":export})).unwrap();
        fs::write(&hold, &raw).unwrap();
        fs::write(source.join("original-history"), b"KEEP").unwrap();
        fs::write(export.join("partial-history"), b"KEEP").unwrap();
        let proof = export.join("stopped-owner.json");
        fs::write(&proof, serde_json::to_vec(&json!({"version":1,"phase":"workers-stopped","pid":pid,"environment":"test","export":export,"source_hold":{"path":hold,"sha256":format!("{:x}",Sha256::digest(raw))}})).unwrap()).unwrap();
        (temp, proof, hold)
    }
    fn dead_pid() -> u32 {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        pid
    }
    #[test]
    fn an_exited_exporter_can_release_only_its_unpublished_stopped_generation() {
        let (_temp, proof, hold) = fixture(dead_pid());
        assert_eq!(run(&proof).unwrap()["retry"], false);
        assert!(!hold.exists());
        assert_eq!(run(&proof).unwrap()["retry"], true);
        assert_eq!(
            fs::read(hold.parent().unwrap().join("original-history")).unwrap(),
            b"KEEP"
        );
        assert_eq!(
            fs::read(proof.parent().unwrap().join("partial-history")).unwrap(),
            b"KEEP"
        );
        fs::write(&hold, b"NEWER HOLD").unwrap();
        assert!(run(&proof).is_err());
        assert_eq!(fs::read(&hold).unwrap(), b"NEWER HOLD");
    }
    #[test]
    fn live_owners_missing_proof_partial_receipts_and_selected_services_remain_blocked() {
        let (_temp, proof, hold) = fixture(std::process::id());
        assert!(run(&proof).is_err());
        assert!(hold.exists());
        for mode in [
            "missing",
            "partial",
            "selected",
            "different-hold",
            "bad-phase",
            "partial-journal",
        ] {
            let (_temp, proof, hold) = fixture(dead_pid());
            match mode {
                "missing" => fs::remove_file(&proof).unwrap(),
                "partial" => {
                    fs::write(proof.parent().unwrap().join("closed-owner.json"), b"{").unwrap()
                }
                "selected" => fs::write(
                    hold.parent()
                        .unwrap()
                        .join("native-upgrade-selected-service.json"),
                    b"selected",
                )
                .unwrap(),
                "different-hold" => fs::write(&hold, b"NEWER HOLD").unwrap(),
                "partial-journal" => {
                    fs::write(proof.parent().unwrap().join("source-recovery.json"), b"{").unwrap()
                }
                _ => {
                    let mut v: Value = serde_json::from_slice(&fs::read(&proof).unwrap()).unwrap();
                    v["phase"] = json!("stopping");
                    fs::write(&proof, serde_json::to_vec(&v).unwrap()).unwrap();
                }
            }
            assert!(run(&proof).is_err(), "{mode}");
            assert!(hold.exists(), "{mode}");
        }
    }
    #[test]
    fn held_source_lease_and_tampered_proof_refuse_recovery() {
        let (_temp, proof, hold) = fixture(dead_pid());
        let value: Value = serde_json::from_slice(&fs::read(&proof).unwrap()).unwrap();
        let lease = source_hold::recovery_lease(&value["source_hold"]).unwrap();
        assert!(run(&proof).is_err());
        assert!(hold.exists());
        drop(lease);
        run(&proof).unwrap();
        let mut value = value;
        value["pid"] = json!(dead_pid());
        fs::write(&proof, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(run(&proof).is_err());
    }
}
