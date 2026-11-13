//! Native background startup and verified historical Windows service transfer.
//! No foreign service replacement, shell-expanded paths or data removal.
use crate::{installation, profile_lock, profile_snapshot, storage_stage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
const ERROR: &str = "Native service registration changed or is unavailable; no foreign service or chat data was modified";
#[cfg(windows)]
#[path = "native_service_scm.rs"]
mod windows_scm;
#[cfg(windows)]
#[path = "native_service_windows.rs"]
mod windows_task;
#[cfg(windows)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyStartup {
    version: u32,
    installation: PathBuf,
    task: String,
    original_xml: String,
    disabled_xml: String,
    service: PathBuf,
    enabled: bool,
}

/// Inspect and stage the exact existing Windows registration before the source
/// is asked to stop. No background startup is enabled for a manual-only user.
pub fn stage_legacy_startup(installation: &Path, gui: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        if windows_scm::stage(installation)? {
            return Ok(());
        }
        windows_task::stage_legacy(installation, gui)
    }
    #[cfg(not(windows))]
    {
        let _ = (installation, gui);
        Ok(())
    }
}
pub(crate) fn legacy_codex_profile(installation: &Path) -> Result<Option<PathBuf>, String> {
    #[cfg(windows)]
    {
        windows_scm::codex_profile(installation)
    }
    #[cfg(not(windows))]
    {
        let _ = installation;
        Ok(None)
    }
}

pub async fn finish_legacy_startup(installation: &Path, gui: &Path) -> Result<bool, String> {
    #[cfg(windows)]
    {
        if let Some(running) = windows_scm::finish(installation, gui).await? {
            return Ok(running);
        }
        if let Some(saved) = windows_task::legacy_receipt(installation)? {
            let lock = saved
                .installation
                .parent()
                .ok_or(ERROR)?
                .join("native-startup-lock");
            crate::private_directory::prepare(&lock).map_err(|_| ERROR)?;
            let _lease = profile_lock::acquire(&lock)?;
            let next = windows_task::candidate_service(&saved.installation, gui)?;
            let previous = windows_task::active_service(&saved)?;
            prepare(
                &next,
                installation,
                gui,
                &std::env::current_exe().map_err(|_| ERROR)?,
            )?;
            windows_task::retire_with_approval(&saved, &next).await?;
            if !saved.enabled {
                return Ok(false);
            }
            if previous != next && previous.try_exists().map_err(|_| ERROR)? {
                replace(&next, &previous).await?;
            }
            let mut state = run("status", &next).await?;
            if state["registered"] != true {
                run("install", &next).await?;
                state = run("status", &next).await?;
            }
            windows_task::select_active_service(&saved, &next)?;
            if state["state"] != "Running" && state["state"] != "Queued" {
                run("start", &next).await?;
            }
            let until = tokio::time::Instant::now() + Duration::from_secs(45);
            while tokio::time::Instant::now() < until {
                if crate::npm_upgrade::command(installation, "status")
                    .await
                    .is_ok_and(|s| s["running"] == true)
                {
                    println!("{{\"runtime\":\"rust\",\"running\":true,\"backgroundStartup\":true,\"historyRetained\":true,\"pairingRetained\":true}}");
                    return Ok(true);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            return Err("Rust background startup was installed, but readiness could not be confirmed. Use rmx status; saved chats and pairing were retained".into());
        }
    }
    let _ = (installation, gui);
    Ok(false)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    installation: PathBuf,
    executable: PathBuf,
    binary_sha256: String,
    gui: PathBuf,
    gui_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_sha256: Option<String>,
}
fn canonical(p: &Path, dir: bool) -> Result<PathBuf, String> {
    if !p.is_absolute()
        || p.components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(ERROR.into());
    }
    storage_stage::open(p, dir).map_err(|_| ERROR)?;
    p.canonicalize().map_err(|_| ERROR.into())
}
fn private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok() {
        let f = storage_stage::open(path, false).map_err(|_| ERROR)?;
        if storage_stage::links(&f).map_err(|_| ERROR)? != 1
            || profile_snapshot::bytes(path, 1024 * 1024).map_err(|_| ERROR)? != bytes
        {
            return Err(ERROR.into());
        }
        return Ok(());
    }
    let mut f = tempfile::NamedTempFile::new_in(path.parent().ok_or(ERROR)?).map_err(|_| ERROR)?;
    f.write_all(bytes).map_err(|_| ERROR)?;
    f.as_file().sync_all().map_err(|_| ERROR)?;
    f.persist_noclobber(path).map_err(|_| ERROR)?;
    #[cfg(unix)]
    storage_stage::open(path.parent().ok_or(ERROR)?, true)
        .and_then(|f| f.sync_all())
        .map_err(|_| ERROR)?;
    Ok(())
}
fn gui_tree(source: &Path, destination: Option<&Path>) -> Result<String, String> {
    fn walk(
        root: &Path,
        rel: &Path,
        dest: Option<&Path>,
        hash: &mut Sha256,
        count: &mut usize,
        total: &mut usize,
    ) -> Result<(), String> {
        let mut entries = fs::read_dir(root.join(rel))
            .map_err(|_| ERROR)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ERROR)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            *count += 1;
            if *count > 8192 {
                return Err(ERROR.into());
            }
            let r = rel.join(e.file_name());
            let m = e.path().symlink_metadata().map_err(|_| ERROR)?;
            if !profile_snapshot::regular(&m) {
                return Err(ERROR.into());
            }
            hash.update(r.to_str().ok_or(ERROR)?.as_bytes());
            hash.update([0]);
            if m.is_dir() {
                hash.update(b"d");
                if let Some(d) = dest {
                    crate::private_directory::prepare(&d.join(&r)).map_err(|_| ERROR)?;
                }
                walk(root, &r, dest, hash, count, total)?;
            } else if m.is_file() {
                hash.update(b"f");
                let bytes =
                    profile_snapshot::bytes(&e.path(), 32 * 1024 * 1024).map_err(|_| ERROR)?;
                *total += bytes.len();
                if *total > 128 * 1024 * 1024 {
                    return Err(ERROR.into());
                }
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(&bytes);
                if let Some(d) = dest {
                    private_file(&d.join(&r), &bytes)?;
                }
            } else {
                return Err(ERROR.into());
            }
        }
        Ok(())
    }
    canonical(source, true)?;
    let mut hash = Sha256::new();
    walk(
        source,
        Path::new(""),
        destination,
        &mut hash,
        &mut 0,
        &mut 0,
    )?;
    Ok(format!("{:x}", hash.finalize()))
}
fn name(plan: &Plan) -> String {
    format!(
        "remodex-native-{:x}",
        Sha256::digest(plan.installation.to_string_lossy().as_bytes())
    )
}
fn literal(s: &str) -> Result<(), String> {
    if s.chars().any(char::is_control) {
        Err(ERROR.into())
    } else {
        Ok(())
    }
}
fn systemd(s: &str) -> Result<String, String> {
    literal(s)?;
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}
fn xml(s: &str) -> Result<String, String> {
    literal(s)?;
    Ok(s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
fn ps(s: &str) -> Result<String, String> {
    literal(s)?;
    Ok(format!("'{}'", s.replace('\'', "''")))
}
fn text(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or(ERROR.into())
}
fn args(plan: &Plan) -> Result<Vec<String>, String> {
    Ok(vec![
        "--native-service".into(),
        "launch".into(),
        text(plan.executable.parent().ok_or(ERROR)?)?.into(),
    ])
}
pub async fn launch(
    root: &Path,
    stop: impl std::future::Future<Output = ()>,
) -> Result<i32, String> {
    let plan = read(root)?;
    verify(&plan)?;
    #[cfg(windows)]
    windows_scm::environment(root)?;
    if installation::exists(&plan.installation)? {
        installation::start(&plan.installation, &plan.gui, None, stop).await
    } else {
        crate::npm_upgrade::resume_with_gui(&plan.installation, &plan.gui, stop).await
    }
}
/// Graceful stop used only by the adjacent windowless service host.
pub async fn halt(root: &Path) -> Result<Value, String> {
    let plan = read(root)?;
    verify(&plan)?;
    crate::npm_upgrade::command(&plan.installation, "stop").await
}
pub fn activate_windows_service(root: &Path) -> Result<Value, String> {
    #[cfg(windows)]
    {
        windows_scm::activate(root)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Err("Windows service approval is available only on Windows".into())
    }
}
pub fn retire_windows_task(root: &Path) -> Result<Value, String> {
    #[cfg(windows)]
    {
        let plan = read(root)?;
        verify(&plan)?;
        let saved = windows_task::legacy_receipt(&plan.installation)?.ok_or(ERROR)?;
        windows_task::retire_legacy(&saved, root)?;
        Ok(json!({"legacyStartupRetired":true,"historyRetained":true}))
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        Err("Windows startup approval is available only on Windows".into())
    }
}
fn descriptor(plan: &Plan, platform: &str) -> Result<Vec<u8>, String> {
    let exe = text(&plan.executable)?;
    let arguments = args(plan)?;
    let result=match platform {
        "linux"=>format!("[Unit]\nDescription=Remodex native bridge\nStartLimitIntervalSec=120\nStartLimitBurst=3\n[Service]\nType=exec\nExecStart=:{} {}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=45\nSendSIGKILL=no\nUMask=0077\n[Install]\nWantedBy=default.target\n",systemd(exe)?,arguments.iter().map(|a|systemd(a)).collect::<Result<Vec<_>,_>>()?.join(" ")),
        "macos"=>format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{}</array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>5</integer><key>ExitTimeOut</key><integer>45</integer><key>Umask</key><integer>63</integer></dict></plist>\n",name(plan),std::iter::once(exe.to_owned()).chain(arguments).map(|a|xml(&a).map(|a|format!("<string>{a}</string>"))).collect::<Result<String,_>>()?),
        // The scheduled action runs this script hidden and waits on the child;
        // no detached launcher falsely reports completion to Task Scheduler.
        "windows"=>format!("\u{feff}$ErrorActionPreference = 'Stop'\n$arguments = @({})\n$p = Start-Process -FilePath {} -ArgumentList $arguments -WindowStyle Hidden -PassThru -Wait\nexit $p.ExitCode\n",arguments.iter().map(|a|windows_argument(a).and_then(|a|ps(&a))).collect::<Result<Vec<_>,_>>()?.join(","),ps(exe)?),
        _=>return Err("Unsupported background-service platform".into()),
    };
    Ok(result.into_bytes())
}
pub(crate) fn windows_argument(s: &str) -> Result<String, String> {
    literal(s)?;
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    Ok(out)
}
fn artifact(platform: &str) -> &'static str {
    match platform {
        "linux" => "startup.service",
        "macos" => "startup.plist",
        _ => "startup.ps1",
    }
}
fn read(root: &Path) -> Result<Plan, String> {
    let root = canonical(root, true)?;
    let plan: Plan = serde_json::from_slice(
        &profile_snapshot::bytes(&root.join("service.json"), 16384).map_err(|_| ERROR)?,
    )
    .map_err(|_| ERROR)?;
    if !matches!(plan.version, 1 | 2)
        || (plan.version == 2 && cfg!(windows) && plan.startup_sha256.is_none())
        || (plan.version == 1 && plan.startup_sha256.is_some())
        || canonical(&plan.installation, true)? != plan.installation
        || plan.gui != root.join("gui")
        || plan.executable
            != root.join(if cfg!(windows) {
                "runtime.exe"
            } else {
                "runtime"
            })
    {
        return Err(ERROR.into());
    }
    Ok(plan)
}
fn verify(plan: &Plan) -> Result<(), String> {
    if crate::service_registration::fingerprint(&plan.executable, None)? != plan.binary_sha256
        || gui_tree(&plan.gui, None)? != plan.gui_sha256
    {
        return Err(ERROR.into());
    }
    if let Some(hash) = &plan.startup_sha256 {
        let startup = plan
            .executable
            .parent()
            .ok_or(ERROR)?
            .join("remodex-startup.exe");
        if crate::service_registration::fingerprint(&startup, None)? != *hash {
            return Err("Remodex startup helper changed; prepare a new service directory".into());
        }
    }
    installation::service_configuration(&plan.installation)
}
/// prepare copies the runtime and dashboard. Repeating it verifies the exact
/// existing generation; replacement uses a new directory, never overwrites it.
pub fn prepare(
    root: &Path,
    installation: &Path,
    gui: &Path,
    executable: &Path,
) -> Result<Value, String> {
    installation::service_configuration(installation)?;
    let installation = canonical(installation, true)?;
    let gui = canonical(gui, true)?;
    let executable = canonical(executable, false)?;
    if fs::symlink_metadata(root).is_ok() {
        let p = read(root)?;
        verify(&p)?;
        if p.installation != installation
            || p.binary_sha256 != crate::service_registration::fingerprint(&executable, None)?
            || p.gui_sha256 != gui_tree(&gui, None)?
            || (cfg!(windows)
                && p.startup_sha256.as_ref()
                    != Some(&crate::service_registration::fingerprint(
                        &executable
                            .parent()
                            .ok_or(ERROR)?
                            .join("remodex-startup.exe"),
                        None,
                    )?))
        {
            return Err(ERROR.into());
        }
        return Ok(json!({"phase":"prepared","name":name(&p)}));
    }
    if !root.is_absolute()
        || root
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(ERROR.into());
    }
    canonical(root.parent().ok_or(ERROR)?, true)?;
    if root.starts_with(&gui) || gui.starts_with(root) || installation.starts_with(root) {
        return Err(ERROR.into());
    }
    // Fail before creating a partial service directory when only the server
    // was built/copied without its windowless Windows startup companion.
    if cfg!(windows) {
        crate::service_registration::fingerprint(
            &executable
                .parent()
                .ok_or(ERROR)?
                .join("remodex-startup.exe"),
            None,
        )
        .map_err(|_| {
            "Windows startup helper is missing or unreadable; build/install both Rust binaries"
        })?;
    }
    crate::private_directory::prepare(root).map_err(|_| ERROR)?;
    let root = canonical(root, true)?;
    let _lock = profile_lock::acquire(&root)?;
    let pinned = root.join(if cfg!(windows) {
        "runtime.exe"
    } else {
        "runtime"
    });
    let binary_sha256 = crate::service_registration::fingerprint(&executable, Some(&pinned))?;
    let startup_sha256 = if cfg!(windows) {
        Some(crate::service_registration::fingerprint(
            &executable
                .parent()
                .ok_or(ERROR)?
                .join("remodex-startup.exe"),
            Some(&root.join("remodex-startup.exe")),
        )?)
    } else {
        None
    };
    let pinned_gui = root.join("gui");
    crate::private_directory::prepare(&pinned_gui).map_err(|_| ERROR)?;
    let gui_sha256 = gui_tree(&gui, Some(&pinned_gui))?;
    let plan = Plan {
        version: 2,
        installation,
        executable: pinned,
        binary_sha256,
        gui: pinned_gui,
        gui_sha256,
        startup_sha256,
    };
    verify(&plan)?;
    for platform in ["linux", "macos"] {
        private_file(
            &root.join(artifact(platform)),
            &descriptor(&plan, platform)?,
        )?;
    }
    private_file(
        &root.join("service.json"),
        &serde_json::to_vec(&plan).map_err(|_| ERROR)?,
    )?;
    Ok(json!({"phase":"prepared","name":name(&plan),"serviceStarted":false,"runtimePinned":true}))
}
async fn helper(program: &str, args: &[String]) -> Result<crate::subprocess::Output, String> {
    let mut c = tokio::process::Command::new(program);
    c.args(args);
    crate::subprocess::run(c, Duration::from_secs(25), 1024 * 1024, 16384).await
}
async fn ctl(args: &[&str]) -> Result<crate::subprocess::Output, String> {
    let mut a = vec![
        "--user".into(),
        "--no-pager".into(),
        "--no-ask-password".into(),
    ];
    a.extend(args.iter().map(|a| a.to_string()));
    helper("systemctl", &a).await
}
fn ensure_dir(p: &Path) -> Result<(), String> {
    if fs::symlink_metadata(p).is_err() {
        fs::create_dir(p).map_err(|_| ERROR)?;
    }
    canonical(p, true)?;
    Ok(())
}
fn exact(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let f = storage_stage::open(path, false).map_err(|_| ERROR)?;
            if storage_stage::links(&f).map_err(|_| ERROR)? != 1
                || profile_snapshot::bytes(path, 1024 * 1024).map_err(|_| ERROR)? != bytes
            {
                return Err(ERROR.into());
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        _ => Err(ERROR.into()),
    }
}
async fn linux(plan: &Plan, operation: &str) -> Result<Value, String> {
    let unit = format!("{}.service", name(plan));
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(ERROR)?);
    canonical(&home, true)?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    ensure_dir(&config)?;
    let systemd = config.join("systemd");
    ensure_dir(&systemd)?;
    let units = systemd.join("user");
    ensure_dir(&units)?;
    let path = units.join(&unit);
    let bytes = descriptor(plan, "linux")?;
    let installed = exact(&path, &bytes)?;
    let query=ctl(&["show",&unit,"--property=LoadState,ActiveState,MainPID,ControlPID,FragmentPath,DropInPaths,ControlGroup,TasksCurrent"]).await?;
    let s = String::from_utf8(query.stdout).map_err(|_| ERROR)?;
    let fields: std::collections::BTreeMap<_, _> =
        s.lines().filter_map(|s| s.split_once('=')).collect();
    if fields.get("DropInPaths") != Some(&"")
        || fields
            .get("FragmentPath")
            .is_none_or(|p| !p.is_empty() && *p != path.to_string_lossy())
    {
        return Err(ERROR.into());
    }
    if operation == "status" {
        return Ok(
            json!({"registered":installed,"state":fields.get("ActiveState"),"runtimeVerified":verify(plan).is_ok()}),
        );
    }
    if !matches!(fields.get("ActiveState"), Some(&"inactive" | &"failed"))
        || fields.get("MainPID") != Some(&"0")
        || fields.get("ControlPID") != Some(&"0")
        || (fields.get("ControlGroup") != Some(&"") && fields.get("TasksCurrent") != Some(&"0"))
    {
        return Err("Stop the bridge with rmx stop before changing its registration".into());
    }
    let wants = units.join("default.target.wants");
    let link = wants.join(&unit);
    if fs::symlink_metadata(&wants).is_ok() {
        canonical(&wants, true)?;
    }
    if fs::symlink_metadata(&link).is_ok() && fs::read_link(&link).map_err(|_| ERROR)? != path {
        return Err(ERROR.into());
    }
    if operation == "uninstall" {
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(link).map_err(|_| ERROR)?;
        }
        if installed {
            fs::remove_file(&path).map_err(|_| ERROR)?;
        }
    } else {
        verify(plan)?;
        if operation == "install" {
            private_file(&path, &bytes)?;
            ensure_dir(&wants)?;
            if fs::symlink_metadata(&link).is_err() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&path, &link).map_err(|_| ERROR)?;
            }
        } else if !installed {
            return Err("Install this registration before starting it".into());
        }
    }
    if ctl(&["daemon-reload"]).await?.code != 0 {
        return Err(ERROR.into());
    }
    if operation == "start" && ctl(&["start", &unit]).await?.code != 0 {
        return Err(ERROR.into());
    }
    Ok(json!({"phase":operation,"historyRetained":true}))
}
async fn macos(plan: &Plan, operation: &str) -> Result<Value, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(ERROR)?);
    canonical(&home, true)?;
    let library = home.join("Library");
    ensure_dir(&library)?;
    let agents = library.join("LaunchAgents");
    ensure_dir(&agents)?;
    let path = agents.join(format!("{}.plist", name(plan)));
    let bytes = descriptor(plan, "macos")?;
    let installed = exact(&path, &bytes)?;
    #[cfg(unix)]
    let uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let uid = 0;
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{}", name(plan));
    let status = helper("/bin/launchctl", &["print".into(), target.clone()]).await?;
    let output = String::from_utf8(status.stdout).map_err(|_| ERROR)?;
    if status.code == 0
        && (!installed
            || !output
                .lines()
                .any(|s| s.trim() == format!("path = {}", path.display())))
    {
        return Err(ERROR.into());
    }
    let running = output.lines().any(|s| s.trim().starts_with("pid = "));
    if operation == "status" {
        return Ok(
            json!({"registered":installed,"running":running,"runtimeVerified":verify(plan).is_ok()}),
        );
    }
    if running {
        return Err("Stop the bridge with rmx stop before changing its registration".into());
    }
    if operation == "uninstall" {
        if status.code == 0
            && helper("/bin/launchctl", &["bootout".into(), target])
                .await?
                .code
                != 0
        {
            return Err(ERROR.into());
        }
        if installed {
            fs::remove_file(path).map_err(|_| ERROR)?;
        }
    } else {
        verify(plan)?;
        if operation == "install" {
            private_file(&path, &bytes)?;
        } else {
            if !installed {
                return Err("Install this registration before starting it".into());
            }
            let args = if status.code == 0 {
                vec!["kickstart".into(), target]
            } else {
                vec!["bootstrap".into(), domain, text(&path)?.into()]
            };
            if helper("/bin/launchctl", &args).await?.code != 0 {
                return Err(ERROR.into());
            }
        }
    }
    Ok(json!({"phase":operation,"historyRetained":true}))
}
#[cfg(windows)]
async fn windows(root: &Path, plan: &Plan, operation: &str) -> Result<Value, String> {
    // Only the service CLI calls this path. Do not yield or detach a mutation:
    // cancellation must not release the registration locks while COM continues.
    // All COM objects are initialized, used, and released on this same thread.
    if !matches!(operation, "status" | "uninstall") {
        verify(plan)?;
    }
    if let Some(result) = windows_scm::run(root, operation)? {
        return Ok(result);
    }
    windows_task::run(root, operation)
}
#[cfg(not(windows))]
async fn windows(_: &Path, _: &Plan, _: &str) -> Result<Value, String> {
    Err(ERROR.into())
}
pub async fn run(operation: &str, root: &Path) -> Result<Value, String> {
    if !matches!(operation, "install" | "start" | "status" | "uninstall") {
        return Err("Unknown service operation".into());
    }
    let root = canonical(root, true)?;
    let plan = read(&root)?;
    let _lock = profile_lock::acquire(&root)?;
    // Service managers may start at login concurrently: lifetime leases in the
    // installation supervisor still prevent a second writer in that race.
    let _owner = if matches!(operation, "install" | "uninstall") {
        Some(profile_lock::acquire(&plan.installation)?)
    } else {
        None
    };
    let _state = if matches!(operation, "install" | "uninstall") {
        Some(profile_lock::acquire(
            &if installation::exists(&plan.installation)? {
                plan.installation.join("state")
            } else {
                crate::npm_upgrade::managed_generation(&plan.installation)?.join("bridge/state")
            },
        )?)
    } else {
        None
    };
    perform(operation, &root, &plan).await
}
async fn perform(operation: &str, root: &Path, plan: &Plan) -> Result<Value, String> {
    match std::env::consts::OS {
        "linux" => linux(plan, operation).await,
        "macos" => macos(plan, operation).await,
        "windows" => windows(root, plan, operation).await,
        _ => Err(ERROR.into()),
    }
}

/// Explicit stopped-service upgrade. Repeating after a crash recognizes the
/// selected new descriptor. The old copied runtime is retained for rollback.
pub async fn replace(next: &Path, previous: &Path) -> Result<Value, String> {
    let next = canonical(next, true)?;
    let previous = canonical(previous, true)?;
    if next == previous || next.starts_with(&previous) || previous.starts_with(&next) {
        return Err(ERROR.into());
    }
    let n = read(&next)?;
    let p = read(&previous)?;
    #[cfg(windows)]
    if n.version != 2 {
        return Err("The replacement must use a newly prepared Rust startup directory; the current registration was retained".into());
    }
    if n.installation != p.installation {
        return Err(ERROR.into());
    }
    let mut registrations = [next.clone(), previous.clone()];
    registrations.sort();
    let _first = profile_lock::acquire(&registrations[0])?;
    let _second = profile_lock::acquire(&registrations[1])?;
    let _owner = profile_lock::acquire(&n.installation)?;
    let state = if installation::exists(&n.installation)? {
        n.installation.join("state")
    } else {
        crate::npm_upgrade::managed_generation(&n.installation)?.join("bridge/state")
    };
    let _state = profile_lock::acquire(&state)?;
    verify(&n)?;
    let journal = next.join(format!(
        "replace-{:x}.json",
        Sha256::digest(previous.to_string_lossy().as_bytes())
    ));
    let evidence = serde_json::to_vec(
        &json!({"version":1,"previous":previous,"next":next,"installation":n.installation}),
    )
    .map_err(|_| ERROR)?;
    private_file(&journal, &evidence)?;
    if perform("status", &next, &n)
        .await
        .is_ok_and(|s| s["registered"] == true)
    {
        perform("install", &next, &n).await?;
        return Ok(json!({"phase":"replaced","serviceStarted":false,"historyRetained":true}));
    }
    perform("uninstall", &previous, &p).await?;
    if perform("install", &next, &n).await.is_err() {
        let restored = perform("install", &previous, &p).await.is_ok();
        return Err(if restored{"The new registration failed; the previous registration was restored without starting it"}else{"Registration replacement is incomplete. Retry the same replace command; runtime copies and chat data were retained"}.into());
    }
    Ok(json!({"phase":"replaced","serviceStarted":false,"historyRetained":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptors_quote_literal_paths_and_windows_stays_hidden() {
        let p = Plan {
            version: 1,
            installation: "/data/a $USER %h ' &".into(),
            executable: "/bin %h ' 测试/a \" &".into(),
            gui: "/gui path".into(),
            binary_sha256: "x".into(),
            gui_sha256: "x".into(),
            startup_sha256: None,
        };
        let linux = String::from_utf8(descriptor(&p, "linux").unwrap()).unwrap();
        assert!(linux.contains("ExecStart=:"));
        assert!(linux.contains("%%h"));
        assert!(!linux.contains("/bin/sh"));
        let mac = String::from_utf8(descriptor(&p, "macos").unwrap()).unwrap();
        assert!(mac.contains("&amp;"));
        assert!(mac.contains("<key>ProgramArguments</key>"));
        let win = String::from_utf8(descriptor(&p, "windows").unwrap()).unwrap();
        assert!(win.contains("-WindowStyle Hidden -PassThru -Wait"));
        assert!(win.contains("''"));
        assert!(win.starts_with('\u{feff}'));
        assert!(win.contains("测试"));
        assert!(systemd("x\ny").is_err());
        assert_eq!(
            windows_argument("C:\\folder\\").unwrap(),
            "\"C:\\folder\\\\\""
        );
    }
    #[test]
    fn copied_dashboard_is_bounded_and_detects_changes() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        let b = t.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        fs::write(a.join("index.html"), b"dashboard").unwrap();
        let h = gui_tree(&a, Some(&b)).unwrap();
        assert_eq!(h, gui_tree(&b, None).unwrap());
        fs::write(b.join("index.html"), b"changed").unwrap();
        assert_ne!(h, gui_tree(&b, None).unwrap());
    }
}
