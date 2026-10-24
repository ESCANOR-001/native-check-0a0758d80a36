//! Explicit, per-user registration of an already selected native generation.
//! Existing foreign units and active owners are never replaced or stopped.
use crate::{profile_lock, profile_snapshot, storage_stage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const ERROR: &str = "Background registration is unavailable or changed; existing services and chat data were retained";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    service: PathBuf,
    executable: PathBuf,
}

fn canonical(path: &Path, directory: bool) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.components().any(|p| {
            matches!(
                p,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(ERROR.into());
    }
    storage_stage::open(path, directory).map_err(|_| ERROR)?;
    let result = path.canonicalize().map_err(|_| ERROR)?;
    for p in result.parent().ok_or(ERROR)?.ancestors() {
        storage_stage::open(p, true).map_err(|_| ERROR)?;
    }
    Ok(result)
}
fn name(service: &Path) -> Result<String, String> {
    let path = service.to_str().ok_or(ERROR)?;
    Ok(format!(
        "remodex-native-{:x}.service",
        Sha256::digest(path.as_bytes())
    ))
}
fn quoted(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err(ERROR.into());
    }
    // systemd specifiers and environment expansion are separate from shell
    // quoting. ':' disables environment expansion; '%%' quotes specifiers.
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}
fn unit(plan: &Plan) -> Result<Vec<u8>, String> {
    let exe = quoted(plan.executable.to_str().ok_or(ERROR)?)?;
    let service = quoted(plan.service.to_str().ok_or(ERROR)?)?;
    Ok(format!("[Unit]\nDescription=Remodex native bridge\nStartLimitIntervalSec=120\nStartLimitBurst=3\n\n[Service]\nType=exec\nExecStart=:{exe} --service-cutover run {service}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=45\nSendSIGKILL=no\nUMask=0077\n\n[Install]\nWantedBy=default.target\n").into_bytes())
}
fn private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| ERROR)?;
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    #[cfg(unix)]
    {
        storage_stage::open(path.parent().ok_or(ERROR)?, true)
            .map_err(|_| ERROR)?
            .sync_all()
            .map_err(|_| ERROR)?;
    }
    Ok(())
}
fn read_plan(root: &Path) -> Result<Plan, String> {
    canonical(root, true)?;
    let raw = profile_snapshot::bytes(&root.join("registration.json"), 16384).map_err(|_| ERROR)?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    if plan.version != 1
        || canonical(&plan.service, true)? != plan.service
        || canonical(&plan.executable, false)? != plan.executable
    {
        return Err(ERROR.into());
    }
    unit(&plan)?;
    Ok(plan)
}
pub fn prepare(root: &Path, service: &Path, executable: &Path) -> Result<Value, String> {
    let service = canonical(service, true)?;
    let executable = canonical(executable, false)?;
    crate::service_cutover::registration(&service)?;
    if !root.is_absolute()
        || root.components().any(|p| {
            matches!(
                p,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || fs::symlink_metadata(root).is_ok()
    {
        return Err(ERROR.into());
    }
    let parent = canonical(root.parent().ok_or(ERROR)?, true)?;
    let root = parent.join(root.file_name().ok_or(ERROR)?);
    if root.starts_with(&service) || service.starts_with(&root) {
        return Err(ERROR.into());
    }
    let plan = Plan {
        version: 1,
        service,
        executable,
    };
    let bytes = unit(&plan)?;
    crate::private_directory::prepare(&root).map_err(|_| ERROR)?;
    private_new(
        &root.join("registration.json"),
        &serde_json::to_vec(&plan).map_err(|_| ERROR)?,
    )?;
    private_new(&root.join(name(&plan.service)?), &bytes)?;
    Ok(
        json!({"phase":"prepared","unit":name(&plan.service)?,"serviceStarted":false,"migrationComplete":false}),
    )
}

async fn systemctl(args: &[&str]) -> Result<crate::subprocess::Output, String> {
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args(["--user", "--no-pager", "--no-ask-password"])
        .args(args);
    crate::subprocess::run(command, Duration::from_secs(20), 65536, 16384).await
}
async fn status(unit: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    let output = systemctl(&[
        "show",
        unit,
        "--property=LoadState,ActiveState,MainPID,ControlPID,ControlGroup,TasksCurrent,FragmentPath,DropInPaths",
    ])
    .await?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| ERROR)?;
    let result: std::collections::BTreeMap<_, _> = text
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    if !result.contains_key("LoadState") || !result.contains_key("ActiveState") {
        return Err(ERROR.into());
    }
    Ok(result)
}
fn own_status(
    state: &std::collections::BTreeMap<String, String>,
    path: &Path,
) -> Result<(), String> {
    if !state.get("DropInPaths").is_some_and(String::is_empty) {
        return Err(ERROR.into());
    }
    let fragment = state.get("FragmentPath").ok_or(ERROR)?;
    if !fragment.is_empty() && Path::new(fragment) != path {
        return Err(ERROR.into());
    }
    Ok(())
}
fn stopped(state: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    if !state
        .get("ActiveState")
        .is_some_and(|s| matches!(s.as_str(), "inactive" | "failed"))
        || state.get("MainPID").map(String::as_str) != Some("0")
        || state.get("ControlPID").map(String::as_str) != Some("0")
        || (state.get("ControlGroup").map(String::as_str) != Some("")
            && state.get("TasksCurrent").map(String::as_str) != Some("0"))
    {
        return Err("Stop the selected bridge safely before changing its background registration; no running service was stopped".into());
    }
    Ok(())
}
fn verify_file(path: &Path, expected: &[u8]) -> Result<(), String> {
    let file = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    if profile_snapshot::bytes(path, 16384).map_err(|_| ERROR)? != expected {
        return Err(ERROR.into());
    }
    Ok(())
}
/// The systemd user directory is resolved from the user's ordinary configuration
/// root; no system-wide service, sudo, or shell command is used.
pub async fn run(operation: &str, root: &Path) -> Result<Value, String> {
    if !cfg!(target_os = "linux") {
        return Err("Native background registration for this platform has not been qualified yet; no service was changed".into());
    }
    if !matches!(operation, "install" | "start" | "status" | "uninstall") {
        return Err(ERROR.into());
    }
    let plan = read_plan(root)?;
    let _lease = profile_lock::acquire(root)?;
    let unit_name = name(&plan.service)?;
    let bytes = unit(&plan)?;
    verify_file(&root.join(&unit_name), &bytes)?;
    let config = if let Some(p) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(p)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or(ERROR)?).join(".config")
    };
    let config = canonical(&config, true)?;
    let systemd = config.join("systemd");
    let units = systemd.join("user");
    // Do not chmod shared user configuration directories or follow linked units.
    for path in [&systemd, &units] {
        if !path.try_exists().map_err(|_| ERROR)? {
            fs::create_dir(path).map_err(|_| ERROR)?;
        }
        canonical(path, true)?;
    }
    let path = units.join(&unit_name);
    let state = status(&unit_name).await?;
    own_status(&state, &path)?;
    let exists = fs::symlink_metadata(&path).is_ok();
    if exists {
        verify_file(&path, &bytes)?;
    }
    if operation == "status" {
        return Ok(
            json!({"registered":exists,"unit":unit_name,"state":state.get("ActiveState"),"pid":state.get("MainPID")}),
        );
    }
    stopped(&state)?;
    let wants = units.join("default.target.wants");
    if fs::symlink_metadata(&wants).is_ok() {
        canonical(&wants, true)?;
    }
    let enabled = wants.join(&unit_name);
    if fs::symlink_metadata(&enabled).is_ok() && fs::read_link(&enabled).map_err(|_| ERROR)? != path
    {
        return Err(ERROR.into());
    }
    if operation == "uninstall" {
        // No wildcard disable: it can erase foreign aliases/drop-ins. Remove
        // only the exact, verified unit and enablement link that we own.
        if fs::symlink_metadata(&enabled).is_ok() {
            if fs::read_link(&enabled).map_err(|_| ERROR)? != path {
                return Err(ERROR.into());
            }
            fs::remove_file(&enabled).map_err(|_| ERROR)?;
        }
        if exists {
            verify_file(&path, &bytes)?;
            fs::remove_file(&path).map_err(|_| ERROR)?;
        }
        let output = systemctl(&["daemon-reload"]).await?;
        if output.code != 0 {
            return Err(ERROR.into());
        }
        return Ok(json!({"phase":"uninstalled","historyRetained":true,"serviceStopped":false}));
    }
    crate::service_cutover::registration(&plan.service)?;
    if operation == "install" {
        if !exists {
            private_new(&path, &bytes)?;
        }
        if !wants.try_exists().map_err(|_| ERROR)? {
            fs::create_dir(&wants).map_err(|_| ERROR)?;
        }
        canonical(&wants, true)?;
        if fs::symlink_metadata(&enabled).is_ok() {
            if fs::read_link(&enabled).map_err(|_| ERROR)? != path {
                return Err(ERROR.into());
            }
        } else {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&path, &enabled).map_err(|_| ERROR)?;
        }
        let output = systemctl(&["daemon-reload"]).await?;
        if output.code != 0 {
            return Err(ERROR.into());
        }
        own_status(&status(&unit_name).await?, &path)?;
        return Ok(json!({"phase":"installed","unit":unit_name,"serviceStarted":false}));
    }
    if !exists || state.get("FragmentPath").map(String::as_str) != path.to_str() {
        return Err(ERROR.into());
    }
    let output = systemctl(&["start", &unit_name]).await?;
    if output.code != 0 {
        return Err(ERROR.into());
    }
    let state = status(&unit_name).await?;
    own_status(&state, &path)?;
    if state.get("ActiveState").map(String::as_str) != Some("active")
        || state.get("MainPID").map(String::as_str) == Some("0")
    {
        return Err(ERROR.into());
    }
    Ok(json!({"phase":"started","unit":unit_name,"pid":state.get("MainPID")}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_parser_accepts_literal_paths_with_spaces_and_specifiers() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("binary $USER %h with spaces");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let plan = Plan {
            version: 1,
            executable,
            service: root.path().join("service $USER %h with spaces"),
        };
        let path = root.path().join(name(&plan.service).unwrap());
        private_new(&path, &unit(&plan).unwrap()).unwrap();
        let mut command = tokio::process::Command::new("systemd-analyze");
        command
            .args(["--user", "--generators=no", "--man=no", "verify"])
            .arg(path)
            .env("XDG_RUNTIME_DIR", root.path());
        let result = crate::subprocess::run(command, Duration::from_secs(10), 16384, 16384)
            .await
            .unwrap();
        assert_eq!(result.code, 0);
    }
    #[test]
    fn systemd_arguments_are_literal_and_have_no_implicit_shell() {
        let plan = Plan {
            version: 1,
            service: PathBuf::from("/tmp/a $x %h \\\" service"),
            executable: PathBuf::from("/tmp/a $x %h \\\" binary"),
        };
        let text = String::from_utf8(unit(&plan).unwrap()).unwrap();
        assert!(text.contains("ExecStart=:\"/tmp/a $x %%h \\\\\\\" binary\" --service-cutover run"));
        assert!(!text.contains("/bin/sh"));
        assert!(text.contains("SendSIGKILL=no"));
        assert!(quoted("bad\nExecStart=evil").is_err());
        assert_eq!(name(&plan.service).unwrap(), name(&plan.service).unwrap());
        assert_ne!(
            name(&plan.service).unwrap(),
            name(Path::new("/other")).unwrap()
        );
    }
    #[test]
    fn active_foreign_and_modified_service_material_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("own.service");
        private_new(&file, b"exact").unwrap();
        assert!(private_new(&file, b"replacement").is_err());
        verify_file(&file, b"exact").unwrap();
        assert!(verify_file(&file, b"different").is_err());
        let mut state = std::collections::BTreeMap::from([
            ("FragmentPath".into(), file.to_str().unwrap().into()),
            ("DropInPaths".into(), "".into()),
            ("MainPID".into(), "1".into()),
            ("ControlPID".into(), "0".into()),
            ("ControlGroup".into(), "".into()),
            ("ActiveState".into(), "active".into()),
        ]);
        assert!(stopped(&state).is_err());
        own_status(&state, &file).unwrap();
        state.insert("FragmentPath".into(), "/foreign".into());
        assert!(own_status(&state, &file).is_err());
        state.insert("FragmentPath".into(), file.to_str().unwrap().into());
        state.insert("DropInPaths".into(), "override".into());
        assert!(own_status(&state, &file).is_err());
        state.insert("ActiveState".into(), "inactive".into());
        assert!(stopped(&state).is_err());
        state.insert("MainPID".into(), "0".into());
        stopped(&state).unwrap();
        state.insert("ControlGroup".into(), "/remaining-workers".into());
        assert!(stopped(&state).is_err());
        state.insert("TasksCurrent".into(), "1".into());
        assert!(stopped(&state).is_err());
        state.insert("TasksCurrent".into(), "0".into());
        stopped(&state).unwrap();
        state.insert("ControlPID".into(), "99".into());
        assert!(stopped(&state).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"exact");
    }
}
