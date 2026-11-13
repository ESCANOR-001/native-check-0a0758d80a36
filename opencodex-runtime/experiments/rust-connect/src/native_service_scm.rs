//! Upgrade the two historical WinSW registrations in place. Never creates a
//! SYSTEM service, changes an account/password, or accepts ownership by name.
use super::*;
use std::{collections::BTreeMap, ptr};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::DELETE, System::Services::*};

const NAMES: [&str; 2] = ["remodex-connect-native", "opencodex-proxy-native"];
const WINSW_HASH: &str = "b5066b7bbdfba1293e5d15cda3caaea88fbeab35bd5b38c41c913d492aadfc4f";
const ADMIN: &str = "Windows administrator approval is required to update this older Remodex service. Its account, registration and saved chats were retained";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Configuration {
    kind: u32,
    start: u32,
    error: u32,
    binary: String,
    account: String,
    group: String,
    dependencies: Vec<String>,
    display: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    installation: PathBuf,
    name: String,
    sid: String,
    original: Configuration,
    xml_sha256: String,
    environment: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    version: u32,
    name: String,
    installation: PathBuf,
    receipt_sha256: String,
}
struct Handle(SC_HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
fn failure() -> String {
    let code = unsafe { GetLastError() };
    if code == ERROR_ACCESS_DENIED {
        ADMIN.into()
    } else {
        format!("Windows service operation failed ({code}); saved chats and service recovery files were retained")
    }
}
fn wide(value: &str) -> Result<Vec<u16>, String> {
    if value.contains('\0') || value.len() > 32760 {
        return Err(ERROR.into());
    }
    Ok(value.encode_utf16().chain([0]).collect())
}
fn open(name: &str, access: u32) -> Result<Option<Handle>, String> {
    if !NAMES.contains(&name) {
        return Err(ERROR.into());
    }
    let raw = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if raw.is_null() {
        return Err(failure());
    }
    let manager = Handle(raw);
    let raw = unsafe { OpenServiceW(manager.0, wide(name)?.as_ptr(), access) };
    if raw.is_null() {
        if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(None);
        }
        return Err(failure());
    }
    Ok(Some(Handle(raw)))
}
// All pointers returned by QueryServiceConfig must remain inside its own buffer.
fn field(buffer: &[usize], raw: *const u16) -> Result<(String, usize), String> {
    let first = buffer.as_ptr() as usize;
    let end = first + std::mem::size_of_val(buffer);
    let address = raw as usize;
    if address < first || address >= end || !address.is_multiple_of(2) {
        return Err(ERROR.into());
    }
    let slice = unsafe { std::slice::from_raw_parts(raw, (end - address) / 2) };
    let length = slice.iter().position(|c| *c == 0).ok_or(ERROR)?;
    Ok((
        String::from_utf16(&slice[..length]).map_err(|_| ERROR)?,
        length + 1,
    ))
}
fn configuration(service: &Handle) -> Result<Configuration, String> {
    let mut needed = 0;
    unsafe {
        QueryServiceConfigW(service.0, ptr::null_mut(), 0, &mut needed);
    }
    if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        || !(32..=1024 * 1024).contains(&needed)
    {
        return Err(failure());
    }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    let query = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    if unsafe { QueryServiceConfigW(service.0, query, needed, &mut needed) } == 0 {
        return Err(failure());
    }
    let query = unsafe { &*query };
    let mut dependencies = Vec::new();
    if !query.lpDependencies.is_null() {
        let mut raw = query.lpDependencies as *const u16;
        loop {
            let (value, consumed) = field(&buffer, raw)?;
            if value.is_empty() {
                break;
            }
            dependencies.push(value);
            raw = unsafe { raw.add(consumed) };
        }
    }
    Ok(Configuration {
        kind: query.dwServiceType,
        start: query.dwStartType,
        error: query.dwErrorControl,
        binary: field(&buffer, query.lpBinaryPathName)?.0,
        account: field(&buffer, query.lpServiceStartName)?.0,
        group: field(&buffer, query.lpLoadOrderGroup)?.0,
        dependencies,
        display: field(&buffer, query.lpDisplayName)?.0,
    })
}
fn state(service: &Handle) -> Result<u32, String> {
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of_val(&status) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(failure());
    }
    Ok(status.dwCurrentState)
}
fn receipt_path(installation: &Path) -> Result<PathBuf, String> {
    Ok(canonical(installation.parent().ok_or(ERROR)?, true)?.join("native-scm-handoff.json"))
}
fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let f = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&f).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn receipt(installation: &Path) -> Result<Option<Receipt>, String> {
    let path = receipt_path(installation)?;
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    let r: Receipt = serde_json::from_slice(&bytes(&path, 1024 * 1024)?).map_err(|_| ERROR)?;
    if r.version != 1
        || r.installation != canonical(installation, true)?
        || !NAMES.contains(&r.name.as_str())
        || r.sid != windows_task::current_sid()?
        || windows_task::account_sid(&r.original.account)? != r.sid
        || r.original.kind != SERVICE_WIN32_OWN_PROCESS
        || !matches!(
            r.original.start,
            SERVICE_AUTO_START | SERVICE_DEMAND_START | SERVICE_DISABLED
        )
    {
        return Err(ERROR.into());
    }
    for (key, value) in &r.environment {
        if !environment_key(key) || value.contains('\0') {
            return Err(ERROR.into());
        }
    }
    Ok(Some(r))
}
fn environment_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "USERPROFILE"
            | "HOME"
            | "APPDATA"
            | "LOCALAPPDATA"
            | "CODEX_HOME"
            | "OPENCODEX_HOME"
    )
}
type XmlFields = (BTreeMap<String, String>, BTreeMap<String, String>);
fn xml_fields(xml: &str) -> Result<XmlFields, String> {
    // roxmltree rejects DTDs by default. Do not execute or expand any XML value.
    let doc = roxmltree::Document::parse(xml).map_err(|_| ERROR)?;
    let root = doc.root_element();
    if root.tag_name().name() != "service" || root.tag_name().namespace().is_some() {
        return Err(ERROR.into());
    }
    let mut fields = BTreeMap::new();
    let mut environment = BTreeMap::new();
    for node in root.children().filter(|n| n.is_element()) {
        if node.tag_name().name() == "env" {
            let key = node.attribute("name").ok_or(ERROR)?;
            let value = node.attribute("value").ok_or(ERROR)?;
            if environment.insert(key.into(), value.into()).is_some() {
                return Err(ERROR.into());
            }
        } else if matches!(node.tag_name().name(), "id" | "executable" | "arguments")
            && (node.children().any(|n| n.is_element())
                || fields
                    .insert(
                        node.tag_name().name().into(),
                        node.text().ok_or(ERROR)?.into(),
                    )
                    .is_some())
        {
            return Err(ERROR.into());
        }
    }
    Ok((fields, environment))
}
fn validate_legacy(
    source: &Path,
    name: &str,
    config: &Configuration,
    saved: &Value,
) -> Result<(String, BTreeMap<String, String>), String> {
    let sid = windows_task::current_sid()?;
    if config.kind != SERVICE_WIN32_OWN_PROCESS
        || windows_task::account_sid(&config.account)? != sid
        || !matches!(
            config.start,
            SERVICE_AUTO_START | SERVICE_DEMAND_START | SERVICE_DISABLED
        )
    {
        return Err("The old service uses another Windows account or unsupported settings; it was not changed".into());
    }
    let wrapper = source.join("winsw").join(format!("{name}.exe"));
    let plain = windows_task::dos_path(&wrapper)?;
    if config.binary != plain && config.binary != windows_argument(&plain)? {
        return Err(ERROR.into());
    }
    if crate::service_registration::fingerprint(&wrapper, None)? != WINSW_HASH {
        return Err(
            "The historical Windows service program could not be verified; it was not changed"
                .into(),
        );
    }
    let raw = bytes(
        &source.join("winsw").join(format!("{name}.xml")),
        1024 * 1024,
    )?;
    let (fields, mut env) = xml_fields(std::str::from_utf8(&raw).map_err(|_| ERROR)?)?;
    if fields.get("id").map(String::as_str) != Some(name)
        || fields.get("executable").map(String::as_str) != saved["bunPath"].as_str()
        || canonical(Path::new(env.get("OPENCODEX_HOME").ok_or(ERROR)?), true)? != source
    {
        return Err(ERROR.into());
    }
    let cli = saved["cliPath"].as_str().ok_or(ERROR)?;
    let prefix = format!("\"{cli}\" start --port ");
    if fields
        .get("arguments")
        .and_then(|v| v.strip_prefix(&prefix))
        .is_none_or(|p| p.parse::<u16>().is_err() || p == "0")
    {
        return Err(ERROR.into());
    }
    env.retain(|key, _| environment_key(key));
    let codex_home = canonical(Path::new(saved["codexHome"].as_str().ok_or(ERROR)?), true)?;
    if let Some(baked) = env.get("CODEX_HOME") {
        if canonical(Path::new(baked), true)? != codex_home {
            return Err(
                "The old service and its saved Codex profile disagree; neither was changed".into(),
            );
        }
    }
    env.insert("CODEX_HOME".into(), windows_task::dos_path(&codex_home)?);
    for key in ["USERPROFILE", "HOME", "APPDATA", "LOCALAPPDATA"] {
        if let Ok(value) = std::env::var(key) {
            env.entry(key.into()).or_insert(value);
        }
    }
    Ok((digest(&raw), env))
}

pub(super) fn stage(installation: &Path) -> Result<bool, String> {
    if receipt(installation)?.is_some() {
        return Ok(true);
    }
    let source = canonical(installation.parent().ok_or(ERROR)?, true)?;
    let state_file = source.join("service-state.json");
    if !state_file.try_exists().map_err(|_| ERROR)? {
        return Ok(false);
    }
    let saved: Value = serde_json::from_slice(&bytes(&state_file, 32768)?).map_err(|_| ERROR)?;
    if saved["backend"] != "native" {
        return Ok(false);
    }
    if windows_task::has_enabled_legacy_task(&source)? {
        return Err("This profile has both a Windows service and an enabled old startup task. Resolve the duplicate before upgrading; neither registration was changed".into());
    }
    if saved["version"] != 2
        || saved["winswVersion"] != "2.12.0"
        || saved["winswSha256"] != WINSW_HASH
        || canonical(
            Path::new(saved["opencodexHome"].as_str().ok_or(ERROR)?),
            true,
        )? != source
    {
        return Err(ERROR.into());
    }
    let owner: Value = serde_json::from_slice(&bytes(&source.join(".opencodex-owner.json"), 8192)?)
        .map_err(|_| ERROR)?;
    if owner["version"] != 1
        || uuid::Uuid::parse_str(owner["ownerId"].as_str().ok_or(ERROR)?).is_err()
        || canonical(Path::new(owner["root"].as_str().ok_or(ERROR)?), true)? != source
    {
        return Err(ERROR.into());
    }
    let mut selected = None;
    for name in NAMES {
        let Some(service) = open(name, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)? else {
            continue;
        };
        let config = configuration(&service)?;
        // A similarly named service belonging to another profile is not ours.
        let expected = windows_task::dos_path(&source.join("winsw").join(format!("{name}.exe")))?;
        if config.binary != expected && config.binary != windows_argument(&expected)? {
            continue;
        }
        if selected.is_some() {
            return Err("Two old Remodex services own this profile; neither was changed".into());
        }
        let (xml_sha256, environment) = validate_legacy(&source, name, &config, &saved)?;
        // Published npm 1.2.27 must already have completed a clean stop. A
        // surviving WinSW process may restart its child, so require SCM stopped.
        if state(&service)? != SERVICE_STOPPED {
            return Err("Stop the old Remodex Windows service before upgrading; its saved chats and pairing were retained".into());
        }
        crate::private_directory::prepare(installation).map_err(|_| ERROR)?;
        selected = Some(Receipt {
            version: 1,
            installation: canonical(installation, true)?,
            name: name.into(),
            sid: windows_task::current_sid()?,
            original: config,
            xml_sha256,
            environment,
        });
    }
    let selected = selected.ok_or("The saved old Windows service registration is missing or belongs to another profile; no service was changed")?;
    private_file(
        &receipt_path(installation)?,
        &serde_json::to_vec(&selected).map_err(|_| ERROR)?,
    )?;
    Ok(true)
}
fn command(root: &Path, name: &str) -> Result<String, String> {
    Ok(format!(
        "{} --service {}",
        windows_argument(&windows_task::dos_path(&root.join("remodex-startup.exe"))?)?,
        name
    ))
}
fn selected(root: &Path) -> Result<Option<Receipt>, String> {
    let path = root.join("windows-service.json");
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    let s: Selection = serde_json::from_slice(&bytes(&path, 16384)?).map_err(|_| ERROR)?;
    let plan = read(root)?;
    if s.version != 1
        || s.installation != plan.installation
        || digest(&bytes(&receipt_path(&plan.installation)?, 1024 * 1024)?) != s.receipt_sha256
    {
        return Err(ERROR.into());
    }
    let r = receipt(&plan.installation)?.ok_or(ERROR)?;
    if s.name != r.name {
        return Err(ERROR.into());
    }
    Ok(Some(r))
}
fn expected(r: &Receipt, root: &Path) -> Result<Configuration, String> {
    let mut c = r.original.clone();
    c.binary = command(root, &r.name)?;
    Ok(c)
}
fn change(service: &Handle, binary: &str) -> Result<(), String> {
    // NULL account/password and SERVICE_NO_CHANGE preserve the old credentials,
    // boot policy, dependencies, display name, and recovery configuration.
    if unsafe {
        ChangeServiceConfigW(
            service.0,
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            wide(binary)?.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        )
    } == 0
    {
        return Err(failure());
    }
    Ok(())
}
fn active_path(r: &Receipt) -> Result<PathBuf, String> {
    Ok(r.installation
        .parent()
        .ok_or(ERROR)?
        .join("native-scm-active.json"))
}
fn previous(r: &Receipt) -> Result<Option<PathBuf>, String> {
    let path = active_path(r)?;
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    let root: PathBuf = serde_json::from_slice(&bytes(&path, 32768)?).map_err(|_| ERROR)?;
    if root.parent() != r.installation.parent()
        || root.file_name().and_then(|n| n.to_str()).is_none_or(|n| {
            n.strip_prefix("native-startup-")
                .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        || selected(&root)?.is_none_or(|s| s.name != r.name || s.installation != r.installation)
    {
        return Err(ERROR.into());
    }
    verify(&read(&root)?)?;
    Ok(Some(root))
}
fn select(r: &Receipt, root: &Path) -> Result<(), String> {
    let path = active_path(r)?;
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or(ERROR)?).map_err(|_| ERROR)?;
    file.write_all(&serde_json::to_vec(root).map_err(|_| ERROR)?)
        .map_err(|_| ERROR)?;
    file.as_file().sync_all().map_err(|_| ERROR)?;
    file.persist(path).map_err(|_| ERROR)?;
    Ok(())
}
pub(super) fn run(root: &Path, operation: &str) -> Result<Option<Value>, String> {
    let Some(r) = selected(root)? else {
        return Ok(None);
    };
    // Inspect ownership with read-only access first. A healthy repeated start
    // must not request administrator rights for a change it does not need.
    let service = open(&r.name, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)?
        .ok_or("The migrated Windows service was removed; saved data was retained")?;
    let current = configuration(&service)?;
    let target = expected(&r, root)?;
    let status = state(&service)?;
    if operation == "install" {
        if current != target {
            let previous = previous(&r)?.map(|p| expected(&r, &p)).transpose()?;
            if current != r.original && previous.as_ref() != Some(&current) {
                return Err(ERROR.into());
            }
            if status != SERVICE_STOPPED {
                return Err(
                    "Stop the old Windows service before changing its startup program".into(),
                );
            }
            if current == r.original {
                let xml = r
                    .installation
                    .parent()
                    .ok_or(ERROR)?
                    .join("winsw")
                    .join(format!("{}.xml", r.name));
                if digest(&bytes(&xml, 1024 * 1024)?) != r.xml_sha256 {
                    return Err(ERROR.into());
                }
            }
            let writable = mutation_handle(&r.name, SERVICE_CHANGE_CONFIG, &current)?;
            change(&writable, &target.binary)?;
            if configuration(&writable)? != target {
                return Err(ERROR.into());
            }
        }
        select(&r, root)?;
    } else if current != target {
        return Err(ERROR.into());
    }
    if operation == "status" {
        return Ok(Some(
            json!({"registered":true,"state": match status { SERVICE_RUNNING => "Running", SERVICE_START_PENDING => "Queued", SERVICE_STOPPED => "Ready", _ => "Stopping" },"startup":"rust-windows-service","runtimeVerified":verify(&read(root)?).is_ok(),"startsBeforeSignIn":r.original.start == SERVICE_AUTO_START,"enabled":r.original.start != SERVICE_DISABLED}),
        ));
    }
    if operation == "start" && status != SERVICE_RUNNING && status != SERVICE_START_PENDING {
        if status != SERVICE_STOPPED {
            return Err(ERROR.into());
        }
        let writable = mutation_handle(&r.name, SERVICE_START, &target)?;
        if unsafe { StartServiceW(writable.0, 0, ptr::null()) } == 0 {
            return Err(failure());
        }
    }
    if operation == "uninstall" {
        if status != SERVICE_STOPPED {
            return Err("Stop Remodex before removing its background service".into());
        }
        let writable = mutation_handle(&r.name, DELETE, &target)?;
        if unsafe { DeleteService(writable.0) } == 0 {
            return Err(failure());
        }
    }
    Ok(Some(
        json!({"phase":operation,"historyRetained":true,"serviceAccountRetained":true}),
    ))
}
fn mutation_handle(name: &str, access: u32, expected: &Configuration) -> Result<Handle, String> {
    let service = open(name, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | access)?.ok_or(ERROR)?;
    if configuration(&service)? != *expected || state(&service)? != SERVICE_STOPPED {
        return Err(
            "The Windows service changed during the upgrade; no further change was made".into(),
        );
    }
    Ok(service)
}
pub(super) fn environment(root: &Path) -> Result<(), String> {
    if let Some(r) = selected(root)? {
        for (key, value) in r.environment {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}
pub(super) fn codex_profile(installation: &Path) -> Result<Option<PathBuf>, String> {
    receipt(installation)?
        .map(|r| {
            canonical(
                Path::new(r.environment.get("CODEX_HOME").ok_or(ERROR)?),
                true,
            )
        })
        .transpose()
}
/// The elevated command has one narrow purpose and rechecks ownership, the
/// pinned program, and the exact existing configuration before changing it.
pub(super) fn activate(root: &Path) -> Result<Value, String> {
    verify(&read(root)?)?;
    let r = selected(root)?.ok_or(ERROR)?;
    let _lease = profile_lock::acquire(root)?;
    run(root, "install")?;
    if r.original.start != SERVICE_DISABLED {
        run(root, "start")?;
    }
    Ok(json!({"startupTransferred":true,"serviceAccountRetained":true}))
}
async fn activate_with_approval(root: &Path) -> Result<(), String> {
    match activate(root) {
        Ok(_) => return Ok(()),
        Err(e) if e == ADMIN => {}
        Err(e) => return Err(e),
    }
    elevate(root, "scm-activate").await
}
pub(super) async fn elevate(root: &Path, action: &str) -> Result<(), String> {
    if !matches!(action, "scm-activate" | "task-retire") {
        return Err(ERROR.into());
    }
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        System::Threading::*,
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    };
    let program = wide(&windows_task::dos_path(
        &std::env::current_exe().map_err(|_| ERROR)?,
    )?)?;
    let arguments = wide(&format!(
        "--native-service {action} {}",
        windows_argument(&windows_task::dos_path(root)?)?
    ))?;
    let verb = wide("runas")?;
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of_val(&info) as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = program.as_ptr();
    info.lpParameters = arguments.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err("Windows did not approve the service upgrade. The prepared Rust files and original service settings were retained; retry rmx start when ready".into());
    }
    if info.hProcess.is_null() {
        return Err(ERROR.into());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    let until = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == 0 {
            let mut code = 1;
            if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 || code != 0 {
                return Err("Windows service approval did not complete the upgrade. No account or password was changed; retry rmx start to resume".into());
            }
            return Ok(());
        }
        if tokio::time::Instant::now() >= until {
            return Err("Windows service approval is still pending. The original service settings and prepared upgrade were retained".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
pub(super) async fn finish(installation: &Path, gui: &Path) -> Result<Option<bool>, String> {
    let Some(r) = receipt(installation)? else {
        return Ok(None);
    };
    let lock = installation.parent().ok_or(ERROR)?.join("native-scm-lock");
    crate::private_directory::prepare(&lock).map_err(|_| ERROR)?;
    let _lease = profile_lock::acquire(&lock)?;
    let next = windows_task::candidate_service(installation, gui)?;
    prepare(
        &next,
        installation,
        gui,
        &std::env::current_exe().map_err(|_| ERROR)?,
    )?;
    let s = Selection {
        version: 1,
        name: r.name.clone(),
        installation: r.installation.clone(),
        receipt_sha256: digest(&bytes(&receipt_path(installation)?, 1024 * 1024)?),
    };
    private_file(
        &next.join("windows-service.json"),
        &serde_json::to_vec(&s).map_err(|_| ERROR)?,
    )?;
    activate_with_approval(&next).await?;
    if r.original.start == SERVICE_DISABLED {
        return Ok(Some(false));
    }
    let until = tokio::time::Instant::now() + Duration::from_secs(45);
    while tokio::time::Instant::now() < until {
        if crate::npm_upgrade::command(installation, "status")
            .await
            .is_ok_and(|v| v["running"] == true)
        {
            println!("{{\"runtime\":\"rust\",\"running\":true,\"backgroundStartup\":true,\"serviceAccountRetained\":true,\"historyRetained\":true,\"pairingRetained\":true}}");
            return Ok(Some(true));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Err("The Rust Windows service was selected, but readiness could not be confirmed. Saved chats, pairing and the old service configuration were retained".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_xml_keeps_literal_paths_and_rejects_duplicate_ownership() {
        let (f,e) = xml_fields(r#"<service><id>remodex-connect-native</id><executable>C:\A &amp; B\bun.exe</executable><arguments>&quot;C:\app.mjs&quot; start --port 10100</arguments><env name="OPENCODEX_HOME" value="C:\a &amp; b"/></service>"#).unwrap();
        assert_eq!(f["executable"], r"C:\A & B\bun.exe");
        assert_eq!(e["OPENCODEX_HOME"], r"C:\a & b");
        assert!(xml_fields(r#"<service><env name="OPENCODEX_HOME" value="a"/><env name="OPENCODEX_HOME" value="b"/></service>"#).is_err());
        assert!(xml_fields("<service><id>a</id><id>b</id></service>").is_err());
        assert!(xml_fields(
            "<!DOCTYPE service [<!ENTITY root 'c:'>]><service><id>&root;</id></service>"
        )
        .is_err());
    }
    #[test]
    fn never_restores_old_secret_or_bun_environment() {
        for key in [
            "OPENCODEX_ADMIN_AUTH_TOKEN",
            "OCX_API_AUTH_TOKEN",
            "OPENCODEX_BUN_PATH",
            "OCX_SERVICE",
            "COMSPEC",
        ] {
            assert!(!environment_key(key));
        }
        assert!(environment_key("PATH"));
        assert!(environment_key("CODEX_HOME"));
    }
    #[test]
    fn service_change_preserves_every_setting_except_program() {
        let original = Configuration {
            kind: SERVICE_WIN32_OWN_PROCESS,
            start: SERVICE_DISABLED,
            error: SERVICE_ERROR_NORMAL,
            binary: "old".into(),
            account: "some-user".into(),
            group: "custom-group".into(),
            dependencies: vec!["Tcpip".into()],
            display: "My Remodex".into(),
        };
        let r = Receipt {
            version: 1,
            installation: "C:\\fixture\\native-installation".into(),
            name: NAMES[0].into(),
            sid: String::new(),
            original: original.clone(),
            xml_sha256: String::new(),
            environment: BTreeMap::new(),
        };
        let changed = expected(&r, Path::new("C:\\fixture path\\new")).unwrap();
        assert_ne!(changed.binary, original.binary);
        let mut restored = changed;
        restored.binary = original.binary.clone();
        assert_eq!(restored, original);
    }
}

#[cfg(test)]
#[path = "native_service_scm_tests.rs"]
mod windows_tests;
