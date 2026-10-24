//! A handoff keeps the old gateway's durable startup inhibition in place.
//! Only cancellation before native startup may release that exact marker.
use crate::{profile_snapshot, storage_stage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
const ERROR: &str = "The old service is not safely held for this export; no service was started";
fn read(path: &Path) -> Result<Vec<u8>, String> {
    if !path.is_absolute()
        || path.file_name().and_then(|s| s.to_str()) != Some("native-upgrade-source-hold.json")
    {
        return Err(ERROR.into());
    }
    if path.components().any(|c| {
        matches!(
            c,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(ERROR.into());
    }
    let parent = path
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    for p in parent.ancestors() {
        storage_stage::open(p, true).map_err(|_| ERROR)?;
    }
    profile_snapshot::bytes(&parent.join("native-upgrade-source-hold.json"), 4096)
        .map_err(|_| ERROR.into())
}
pub(crate) fn bind(path: &Path, export: &Path, environment: &str) -> Result<Value, String> {
    let raw = read(path)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    let saved = value["export"].as_str().ok_or(ERROR)?;
    if value["version"] != 1
        || value["environment"] != environment
        || Path::new(saved).canonicalize().map_err(|_| ERROR)? != export
    {
        return Err(ERROR.into());
    }
    Ok(json!({"path":path,"sha256":format!("{:x}",Sha256::digest(raw))}))
}
pub(crate) fn check(binding: &Value) -> Result<(), String> {
    let path = Path::new(binding["path"].as_str().ok_or(ERROR)?);
    let raw = read(path)?;
    if binding["sha256"] != format!("{:x}", Sha256::digest(raw)) {
        return Err(ERROR.into());
    }
    Ok(())
}
fn selection(binding: &Value, service: &Path) -> Result<(PathBuf, Vec<u8>), String> {
    check(binding)?;
    let root = Path::new(binding["path"].as_str().ok_or(ERROR)?)
        .parent()
        .ok_or(ERROR)?;
    let service = service.canonicalize().map_err(|_| ERROR)?;
    let value = json!({"version":1,"hold":binding,"service":service});
    Ok((
        root.join("native-upgrade-selected-service.json"),
        serde_json::to_vec(&value).map_err(|_| ERROR)?,
    ))
}
pub(crate) fn lease(binding: &Value) -> Result<crate::profile_lock::Lease, String> {
    check(binding)?;
    let root = Path::new(binding["path"].as_str().ok_or(ERROR)?)
        .parent()
        .ok_or(ERROR)?;
    crate::profile_lock::acquire(&root.join("native-upgrade-owner"))
}
pub(crate) fn claim(binding: &Value, service: &Path) -> Result<(), String> {
    let (path, bytes) = selection(binding, service)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes).map_err(|_| ERROR)?;
            file.sync_all().map_err(|_| ERROR)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(ERROR.into()),
    }
    if profile_snapshot::bytes(&path, 16384).map_err(|_| ERROR)? != bytes {
        return Err(ERROR.into());
    }
    #[cfg(unix)]
    {
        storage_stage::open(path.parent().ok_or(ERROR)?, true)
            .map_err(|_| ERROR)?
            .sync_all()
            .map_err(|_| ERROR)?;
    }
    Ok(())
}
pub(crate) fn release(binding: &Value, service: &Path) -> Result<(), String> {
    check(binding)?;
    let (selected, expected) = selection(binding, service)?;
    if profile_snapshot::bytes(&selected, 16384).map_err(|_| ERROR)? != expected {
        return Err(ERROR.into());
    }
    fs::remove_file(selected).map_err(|_| ERROR)?;
    let path = Path::new(binding["path"].as_str().ok_or(ERROR)?);
    fs::remove_file(path).map_err(|_| ERROR)?;
    #[cfg(unix)]
    {
        storage_stage::open(path.parent().ok_or(ERROR)?, true)
            .map_err(|_| ERROR)?
            .sync_all()
            .map_err(|_| ERROR)?;
    }
    Ok(())
}
