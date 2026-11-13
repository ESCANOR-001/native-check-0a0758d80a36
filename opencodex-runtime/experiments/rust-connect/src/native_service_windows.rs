//! Per-user Task Scheduler operations through Microsoft's native interface.
//! No shell, encoded commands or script-policy changes. Protected legacy tasks
//! use the narrow, user-approved startup retirement command.
use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::{
    core::{Interface, BSTR},
    Win32::System::{Com::*, TaskScheduler::*, Variant::VARIANT},
};

fn api<T>(result: windows::core::Result<T>) -> Result<T, String> {
    result.map_err(|e| {
        format!(
            "Windows startup operation failed (0x{:08X}); no security settings were changed",
            e.code().0 as u32
        )
    })
}
fn bstr(get: impl FnOnce(*mut BSTR) -> windows::core::Result<()>) -> Result<String, String> {
    let mut value = BSTR::new();
    api(get(&mut value))?;
    String::from_utf16(&value).map_err(|_| ERROR.into())
}
fn sid_text(sid: *mut std::ffi::c_void) -> Result<String, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree, Security::Authorization::ConvertSidToStringSidW,
    };
    let mut raw = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 {
        return Err(ERROR.into());
    }
    let mut length = 0;
    while length < 256 && unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let result = if length == 256 {
        Err(ERROR.into())
    } else {
        String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) })
            .map_err(|_| ERROR.into())
    };
    unsafe {
        LocalFree(raw.cast());
    }
    result
}
pub(super) fn current_sid() -> Result<String, String> {
    use windows_sys::Win32::{Security::*, System::Threading::*};
    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(ERROR.into());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut required = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if !(std::mem::size_of::<TOKEN_USER>() as u32..=65536).contains(&required) {
        return Err(ERROR.into());
    }
    let mut buffer = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(ERROR.into());
    }
    sid_text(unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}
pub(super) fn account_sid(account: &str) -> Result<String, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{Authorization::ConvertStringSidToSidW, LookupAccountNameW},
    };
    literal(account)?;
    let value: Vec<u16> = account.encode_utf16().chain([0]).collect();
    if account.starts_with("S-") {
        let mut raw = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(value.as_ptr(), &mut raw) } == 0 {
            return Err(ERROR.into());
        }
        let result = sid_text(raw);
        unsafe {
            LocalFree(raw);
        }
        return result;
    }
    let (mut size, mut domain_size, mut kind) = (0, 0, 0);
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            value.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            &mut domain_size,
            &mut kind,
        );
    }
    if !(1..=65536).contains(&size) || domain_size > 65536 {
        return Err(ERROR.into());
    }
    let mut sid = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut domain = vec![0u16; domain_size as usize];
    if unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            value.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut size,
            domain.as_mut_ptr(),
            &mut domain_size,
            &mut kind,
        )
    } == 0
    {
        return Err(ERROR.into());
    }
    sid_text(sid.as_mut_ptr().cast())
}
fn legacy_powershell() -> Result<PathBuf, String> {
    let mut path = vec![0u16; 32768];
    let count = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
            path.as_mut_ptr(),
            path.len() as u32,
        )
    } as usize;
    if count == 0 || count >= path.len() {
        return Err(ERROR.into());
    }
    Ok(
        PathBuf::from(String::from_utf16(&path[..count]).map_err(|_| ERROR)?)
            .join("WindowsPowerShell/v1.0/powershell.exe"),
    )
}
fn action(plan: &Plan) -> Result<(PathBuf, String), String> {
    let root = plan.executable.parent().ok_or(ERROR)?;
    if plan.version == 2 {
        // Task Scheduler stores a Windows application path, not Rust's
        // canonical verbatim-path spelling. Keep the same absolute target.
        let path = root.join("remodex-startup.exe");
        let value = text(&path)?;
        let path = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(value))
        };
        return Ok((path, String::new()));
    }
    // Read-only compatibility with the old receipt. Never execute or create
    // this action: it is accepted only to inspect/remove an owned old task.
    if !exact(&root.join("startup.ps1"), &descriptor(plan, "windows")?)? {
        return Err(ERROR.into());
    }
    Ok((
        legacy_powershell()?,
        format!(
            "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File {}",
            windows_argument(text(&root.join("startup.ps1"))?)?
        ),
    ))
}
fn task_xml(plan: &Plan, sid: &str) -> Result<String, String> {
    if plan.version != 2 {
        return Err("Prepare a new Rust service directory before installing startup".into());
    }
    let (program, _) = action(plan)?;
    let sid = xml(sid)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
 <RegistrationInfo><Description>Remodex per-user Rust server startup</Description></RegistrationInfo>
 <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
 <Principals><Principal id="RemodexUser"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
 <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
 <Actions Context="RemodexUser"><Exec><Command>{}</Command></Exec></Actions>
</Task>"#,
        xml(text(&program)?)?
    ))
}
fn xml_text(value: &str) -> String {
    // Export-ScheduledTask (legacy) and COM may use different line endings.
    // Preserve every XML field; only normalize transport BOM/line endings.
    value
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .trim_end_matches('\n')
        .to_owned()
}
unsafe fn owned(
    task: &IRegisteredTask,
    plan: &Plan,
    receipt: &Path,
    sid: &str,
) -> Result<(), String> {
    let file = storage_stage::open(receipt, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    let saved =
        String::from_utf8(profile_snapshot::bytes(receipt, 1024 * 1024).map_err(|_| ERROR)?)
            .map_err(|_| ERROR)?;
    if xml_text(&api(task.Xml())?.to_string()) != xml_text(&saved) {
        return Err(ERROR.into());
    }
    let definition = api(task.Definition())?;
    let principal = api(definition.Principal())?;
    if account_sid(&bstr(|p| principal.UserId(p))?)? != sid {
        return Err(ERROR.into());
    }
    let mut logon = TASK_LOGON_NONE;
    let mut level = TASK_RUNLEVEL_HIGHEST;
    api(principal.LogonType(&mut logon))?;
    api(principal.RunLevel(&mut level))?;
    if logon != TASK_LOGON_INTERACTIVE_TOKEN || level != TASK_RUNLEVEL_LUA {
        return Err(ERROR.into());
    }
    let actions = api(definition.Actions())?;
    let mut count = 0;
    api(actions.Count(&mut count))?;
    if count != 1 {
        return Err(ERROR.into());
    }
    let exec: IExecAction = api(api(actions.get_Item(1))?.cast())?;
    let (program, arguments) = action(plan)?;
    if !bstr(|p| exec.Path(p))?.eq_ignore_ascii_case(text(&program)?)
        || bstr(|p| exec.Arguments(p))? != arguments
        || !bstr(|p| exec.WorkingDirectory(p))?.is_empty()
    {
        return Err(ERROR.into());
    }
    Ok(())
}
struct Com;
impl Com {
    fn new() -> Result<Self, String> {
        api(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok())?;
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn legacy_path(installation: &Path) -> Result<PathBuf, String> {
    // Published npm 1.2.27 has no native-installation directory yet. Resolve
    // its existing parent without requiring a handoff to already exist.
    if !installation.is_absolute()
        || installation.file_name().is_none()
        || installation.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(ERROR.into());
    }
    Ok(canonical(installation.parent().ok_or(ERROR)?, true)?.join("native-startup-handoff.json"))
}
pub(super) fn legacy_receipt(installation: &Path) -> Result<Option<LegacyStartup>, String> {
    let path = legacy_path(installation)?;
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    let saved: LegacyStartup = serde_json::from_slice(
        &profile_snapshot::bytes(&path, 2 * 1024 * 1024).map_err(|_| ERROR)?,
    )
    .map_err(|_| ERROR)?;
    let installation = canonical(installation, true)?;
    if saved.version != 1
        || saved.installation != installation
        || !matches!(saved.task.as_str(), "remodex-connect" | "opencodex-proxy")
        || saved.service.parent() != installation.parent()
        || saved
            .service
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| {
                n.strip_prefix("native-startup-")
                    .is_none_or(|s| s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()))
            })
    {
        return Err(ERROR.into());
    }
    Ok(Some(saved))
}
unsafe fn scheduler() -> Result<ITaskService, String> {
    let service: ITaskService = api(CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER))?;
    let empty = VARIANT::default();
    api(service.Connect(&empty, &empty, &empty, &empty))?;
    Ok(service)
}
pub(super) fn dos_path(path: &Path) -> Result<String, String> {
    let value = text(path)?;
    Ok(if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(value).to_owned()
    })
}
unsafe fn legacy_owned(task: &IRegisteredTask, source: &Path, sid: &str) -> Result<(), String> {
    let definition = api(task.Definition())?;
    let principal = api(definition.Principal())?;
    if account_sid(&bstr(|p| principal.UserId(p))?)? != sid {
        return Err(ERROR.into());
    }
    let mut logon = TASK_LOGON_NONE;
    api(principal.LogonType(&mut logon))?;
    if logon != TASK_LOGON_INTERACTIVE_TOKEN {
        return Err(ERROR.into());
    }
    let actions = api(definition.Actions())?;
    let mut count = 0;
    api(actions.Count(&mut count))?;
    if count != 1 {
        return Err(ERROR.into());
    }
    let exec: IExecAction = api(api(actions.get_Item(1))?.cast())?;
    let system = legacy_powershell()?
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or(ERROR)?
        .join("wscript.exe");
    if !bstr(|p| exec.Path(p))?.eq_ignore_ascii_case(&dos_path(&system)?)
        || bstr(|p| exec.Arguments(p))?
            != format!(
                "/b /nologo \"{}\"",
                dos_path(&source.join("opencodex-service-launcher.vbs"))?
            )
        || !bstr(|p| exec.WorkingDirectory(p))?.is_empty()
    {
        return Err(
            "The existing startup task belongs to another installation; it was not changed".into(),
        );
    }
    Ok(())
}

pub(super) fn stage_legacy(installation: &Path, gui: &Path) -> Result<(), String> {
    if legacy_receipt(installation)?.is_some() {
        return Ok(());
    }
    crate::private_directory::prepare(installation).map_err(|_| ERROR)?;
    let installation = canonical(installation, true)?;
    let source = installation.parent().ok_or(ERROR)?;
    let claims_startup = source
        .join("service-state.json")
        .try_exists()
        .map_err(|_| ERROR)?;
    let _com = Com::new()?;
    unsafe {
        let service = scheduler()?;
        let folder = api(service.GetFolder(&BSTR::from("\\")))?;
        let sid = current_sid()?;
        let mut selected = None;
        let mut conflict = None;
        for name in ["remodex-connect", "opencodex-proxy"] {
            let task = match folder.GetTask(&BSTR::from(name)) {
                Ok(task) => task,
                Err(e) if e.code().0 as u32 == 0x80070002 => continue,
                Err(e) => return api(Err(e)),
            };
            // A known name alone is not ownership. Reject conflicts before the
            // old bridge is stopped, rather than replacing another user's task.
            if let Err(error) = legacy_owned(&task, source, &sid) {
                conflict = Some(error);
                // Another profile may own the global legacy task name. A
                // manual-only installation must neither adopt nor disable it.
                continue;
            }
            if selected.is_some() {
                return Err("Two legacy startup registrations exist; resolve the duplicate before upgrading".into());
            }
            selected = Some((name, task));
        }
        let Some((name, task)) = selected else {
            if claims_startup {
                if let Some(error) = conflict {
                    return Err(error);
                }
            }
            let state = source.join("service-state.json");
            if state.try_exists().map_err(|_| ERROR)? {
                let value: Value = serde_json::from_slice(
                    &profile_snapshot::bytes(&state, 32768).map_err(|_| ERROR)?,
                )
                .map_err(|_| ERROR)?;
                if value["backend"] == "native" {
                    return Err("This installation uses a Windows system service. Its startup must be transferred explicitly before upgrading; the running server was retained".into());
                }
            }
            return Ok(());
        };
        // Stage without changing Windows. A protected old registration is
        // retired through the narrow UAC command only after Rust is prepared.
        let enabled = api(task.Enabled())?;
        // Disabled startup is an intentional preference, not permission to
        // enable a replacement background server.
        if !enabled.as_bool() {
            return Ok(());
        }
        let original_xml = api(task.Xml())?.to_string();
        let pinned = candidate_service(&installation, gui)?;
        let definition = api(task.Definition())?;
        api(api(definition.Settings())?.SetEnabled(false.into()))?;
        let disabled_xml = bstr(|p| definition.XmlText(p))?;
        let saved = LegacyStartup {
            version: 1,
            installation: installation.clone(),
            task: name.into(),
            original_xml,
            disabled_xml,
            service: pinned,
            enabled: enabled.as_bool(),
        };
        private_file(
            &legacy_path(&installation)?,
            &serde_json::to_vec(&saved).map_err(|_| ERROR)?,
        )?;
        Ok(())
    }
}

pub(super) fn has_enabled_legacy_task(source: &Path) -> Result<bool, String> {
    let _com = Com::new()?;
    unsafe {
        let folder = api(scheduler()?.GetFolder(&BSTR::from("\\")))?;
        let sid = current_sid()?;
        for name in ["remodex-connect", "opencodex-proxy"] {
            let task = match folder.GetTask(&BSTR::from(name)) {
                Ok(task) => task,
                Err(e) if e.code().0 as u32 == 0x80070002 => continue,
                Err(e) => return api(Err(e)),
            };
            if legacy_owned(&task, source, &sid).is_ok() && api(task.Enabled())?.as_bool() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Disarm future legacy launches without ending its currently retained tunnel.
/// Task Scheduler can keep that old instance alive until cloudflared exits;
/// ending it or reusing its task name would break remote-only phones.
pub(super) fn retire_legacy(saved: &LegacyStartup, next: &Path) -> Result<(), String> {
    let plan = read(next)?;
    verify(&plan)?;
    if plan.installation != saved.installation {
        return Err(ERROR.into());
    }
    let _com = Com::new()?;
    unsafe {
        let service = scheduler()?;
        let folder = api(service.GetFolder(&BSTR::from("\\")))?;
        let task = match folder.GetTask(&BSTR::from(&saved.task)) {
            Ok(task) => task,
            // An uninstalled legacy entry is already retired. Native updates
            // must not require keeping an obsolete disabled task forever.
            Err(e) if e.code().0 as u32 == 0x80070002 => return Ok(()),
            Err(e) => return api(Err(e)),
        };
        let current = api(task.Xml())?.to_string();
        // RegisteredTask.Xml is the compact persisted spelling. Definition's
        // XmlText expands default settings, so compare like with like after
        // changing Enabled; otherwise a successful disable looks like damage.
        let definition = api(task.Definition())?;
        let current_definition = bstr(|p| definition.XmlText(p))?;
        if xml_text(&current_definition) == xml_text(&saved.disabled_xml)
            && !api(task.Enabled())?.as_bool()
        {
            return Ok(());
        }
        if xml_text(&current) != xml_text(&saved.original_xml) {
            return Err(ERROR.into());
        }
        legacy_owned(
            &task,
            saved.installation.parent().ok_or(ERROR)?,
            &current_sid()?,
        )?;
        api(task.SetEnabled(false.into()))?;
        // A previously materialized Definition can remain cached in this COM
        // object. Reopen the registration to verify the persisted change.
        let updated = api(folder.GetTask(&BSTR::from(&saved.task)))?;
        let disabled = api(updated.Definition())?;
        if api(updated.Enabled())?.as_bool()
            || xml_text(&bstr(|p| disabled.XmlText(p))?) != xml_text(&saved.disabled_xml)
        {
            return Err(ERROR.into());
        }
        Ok(())
    }
}

pub(super) async fn retire_with_approval(saved: &LegacyStartup, next: &Path) -> Result<(), String> {
    match retire_legacy(saved, next) {
        Ok(()) => Ok(()),
        Err(error) if error.contains("0x80070005") => {
            super::windows_scm::elevate(next, "task-retire").await?;
            retire_legacy(saved, next)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn candidate_service(installation: &Path, gui: &Path) -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|_| ERROR)?;
    let image = crate::service_registration::fingerprint(&executable, None)?;
    let startup = crate::service_registration::fingerprint(
        &executable
            .parent()
            .ok_or(ERROR)?
            .join("remodex-startup.exe"),
        None,
    )?;
    let generation = format!(
        "{:x}",
        Sha256::digest(format!("{image}\n{startup}\n{}", gui_tree(gui, None)?))
    );
    Ok(installation
        .parent()
        .ok_or(ERROR)?
        .join(format!("native-startup-{generation}")))
}
fn active_path(saved: &LegacyStartup) -> Result<PathBuf, String> {
    Ok(saved
        .installation
        .parent()
        .ok_or(ERROR)?
        .join("native-startup-active.json"))
}
pub(super) fn active_service(saved: &LegacyStartup) -> Result<PathBuf, String> {
    let path = active_path(saved)?;
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(saved.service.clone());
    }
    let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    let selected: PathBuf =
        serde_json::from_slice(&profile_snapshot::bytes(&path, 32768).map_err(|_| ERROR)?)
            .map_err(|_| ERROR)?;
    if selected.parent() != saved.installation.parent()
        || selected
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| {
                n.strip_prefix("native-startup-")
                    .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
            })
        || read(&selected)?.installation != saved.installation
    {
        return Err(ERROR.into());
    }
    Ok(selected)
}
pub(super) fn select_active_service(saved: &LegacyStartup, service: &Path) -> Result<(), String> {
    let path = active_path(saved)?;
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or(ERROR)?).map_err(|_| ERROR)?;
    file.write_all(&serde_json::to_vec(service).map_err(|_| ERROR)?)
        .map_err(|_| ERROR)?;
    file.as_file().sync_all().map_err(|_| ERROR)?;
    file.persist(path).map_err(|_| ERROR)?;
    Ok(())
}

pub(super) fn run(root: &Path, operation: &str) -> Result<Value, String> {
    let plan = read(root)?;
    let _com = Com::new()?;
    unsafe {
        let service: ITaskService =
            api(CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER))?;
        let empty = VARIANT::default();
        api(service.Connect(&empty, &empty, &empty, &empty))?;
        let folder = api(service.GetFolder(&BSTR::from("\\")))?;
        let name = BSTR::from(name(&plan));
        let receipt = root.join("task-export.xml");
        let sid = current_sid()?;
        let existing = match folder.GetTask(&name) {
            Ok(task) => Some(task),
            // Only a genuinely missing task permits creation. Access errors
            // must not be mistaken for absence (the old silent query did so).
            Err(e) if e.code().0 as u32 == 0x80070002 => None,
            Err(e) => return api(Err(e)),
        };
        if let Some(task) = &existing {
            owned(task, &plan, &receipt, &sid)?;
            let state = api(task.State())?;
            if operation == "status" {
                let last_result = api(task.LastTaskResult())?;
                let startup = if plan.version == 1 {
                    "legacy-powershell"
                } else {
                    "rust"
                };
                let state = match state {
                    TASK_STATE_RUNNING => "Running",
                    TASK_STATE_QUEUED => "Queued",
                    TASK_STATE_READY => "Ready",
                    TASK_STATE_DISABLED => "Disabled",
                    _ => "Unknown",
                };
                return Ok(
                    json!({"registered":true,"state":state,"lastTaskResult":last_result,"runtimeVerified":verify(&plan).is_ok(),"startup":startup,"legacyRegistration":plan.version==1}),
                );
            }
            if !matches!(state, TASK_STATE_READY | TASK_STATE_DISABLED) {
                return Err("Stop the bridge and wait for startup to finish before changing its registration".into());
            }
        } else if operation == "status" {
            return Ok(
                json!({"registered":false,"state":"Missing","runtimeVerified":verify(&plan).is_ok()}),
            );
        }
        match operation {
            "install" => {
                verify(&plan)?;
                let xml = task_xml(&plan, &sid)?;
                if existing.is_none() {
                    // Do not overwrite an unexplained stale receipt or any
                    // existing task, including one created between our calls.
                    if fs::symlink_metadata(&receipt).is_ok() {
                        return Err(ERROR.into());
                    }
                    let task = api(folder.RegisterTask(
                        &name,
                        &BSTR::from(xml),
                        TASK_CREATE.0,
                        &VARIANT::from(sid.as_str()),
                        &empty,
                        TASK_LOGON_INTERACTIVE_TOKEN,
                        &empty,
                    ))?;
                    private_file(&receipt, api(task.Xml())?.to_string().as_bytes())?;
                    owned(&task, &plan, &receipt, &current_sid()?)?;
                }
            }
            "start" => {
                if plan.version != 2 {
                    return Err("Replace the legacy startup registration before starting it".into());
                }
                verify(&plan)?;
                let task = existing.ok_or("Install this registration before starting it")?;
                api(task.Run(&empty))?;
            }
            "uninstall" => {
                if existing.is_some() {
                    api(folder.DeleteTask(&name, 0))?;
                }
                if fs::symlink_metadata(&receipt).is_ok() {
                    let file = storage_stage::open(&receipt, false).map_err(|_| ERROR)?;
                    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
                        return Err(ERROR.into());
                    }
                    drop(file);
                    fs::remove_file(&receipt).map_err(|_| ERROR)?;
                }
            }
            _ => return Err(ERROR.into()),
        }
        Ok(json!({"phase":operation,"historyRetained":true,"startup":"rust"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_account_roundtrips_and_rejects_other_accounts() {
        let current = current_sid().unwrap();
        assert_eq!(account_sid(&current).unwrap(), current);
        assert_ne!(account_sid("S-1-5-18").unwrap(), current);
        assert!(account_sid("invalid\nidentity").is_err());
    }
}
