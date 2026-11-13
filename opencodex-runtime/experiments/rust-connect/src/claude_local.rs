//! Claude's local session registry and authenticated inbox (peer protocol 1).
//! This never enables Remote Control, edits a transcript, or launches a second
//! writer for a live session. A successful socket write is only an offer;
//! transcript/receipt confirmation is required before displaying delivery.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::time::Duration;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;
#[cfg(windows)]
#[path = "claude_local_windows.rs"]
mod windows;

pub(crate) enum Delivery {
    Offered,
    Held,
    Rejected(String),
}

/// Durable callback address and original kernel identity. Contains no prompt or token.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Receipt {
    owner: Owner,
    path: PathBuf,
    profile: PathBuf,
}
impl Receipt {
    pub(crate) fn prepare(profile: &Path, owner: &Owner) -> Option<Self> {
        #[cfg(unix)]
        {
            let parent = owner.messaging_socket_path.as_ref()?.parent()?;
            Some(Self {
                owner: owner.clone(),
                profile: profile.into(),
                path: parent.join(format!(
                    "{}-{}.sock",
                    std::process::id(),
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                )),
            })
        }
        #[cfg(windows)]
        {
            Some(Self {
                owner: owner.clone(),
                profile: profile.into(),
                path: PathBuf::from(format!(
                    r"\\.\pipe\cc-msg-{}",
                    uuid::Uuid::new_v4().simple()
                )),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (profile, owner);
            None
        }
    }
}

#[cfg(unix)]
struct InboxPath(PathBuf);
#[cfg(unix)]
impl Drop for InboxPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Owner {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub messaging_socket_path: Option<PathBuf>,
    #[serde(default)]
    pub peer_protocol: Option<u32>,
    #[serde(default)]
    pub proc_start: Option<String>,
    #[serde(default)]
    pub proc_start_ft: Option<String>,
    #[serde(default)]
    pub pid_domain: Option<String>,
}
impl Owner {
    pub fn busy(&self) -> bool {
        self.status != "idle"
    }
    pub fn identity_matches(&self) -> bool {
        process_start(self.pid).is_some_and(|start| {
            self.proc_start_ft.as_ref().or(self.proc_start.as_ref()) == Some(&start)
        })
    }
}

// Same process-start token as the installed CLI; PID existence alone is unsafe
// after a reboot or PID reuse. Inaccessible identity is never treated as idle.
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = text[text.rfind(')')? + 2..].split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    fields.get(19).map(|s| (*s).to_owned())
}
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}
#[cfg(windows)]
fn process_start(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut times: [FILETIME; 4] = std::mem::zeroed();
        let ok = GetProcessTimes(
            handle,
            &mut times[0],
            &mut times[1],
            &mut times[2],
            &mut times[3],
        );
        CloseHandle(handle);
        (ok != 0).then(|| {
            (((times[0].dwHighDateTime as u64) << 32) | times[0].dwLowDateTime as u64).to_string()
        })
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_start(_: u32) -> Option<String> {
    None
}

// Old registries can contain interrupted/empty writes and recycled PIDs. Only
// ignore an unreadable record when the live executable proves it is unrelated.
#[cfg(target_os = "linux")]
fn unrelated_process(pid: u32) -> bool {
    unrelated_linux_process(&PathBuf::from(format!("/proc/{pid}")), unsafe {
        libc::geteuid()
    })
}

#[cfg(target_os = "linux")]
fn unrelated_linux_process(process: &Path, user: u32) -> bool {
    // A stale registry PID may now belong to a different OS user. Its exe
    // symlink can be unreadable even though status exposes all four UIDs.
    // Require a complete, valid UID set: missing/partial metadata must not
    // turn an unverified same-user Claude owner into permission to resume.
    if fs::read_to_string(process.join("status")).is_ok_and(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .is_some_and(|line| {
                let ids: Option<Vec<u32>> =
                    line.split_whitespace().map(|id| id.parse().ok()).collect();
                ids.is_some_and(|ids| ids.len() == 4 && ids.iter().all(|id| *id != user))
            })
    }) {
        return true;
    }
    let Ok(exe) = fs::read_link(process.join("exe")) else {
        return false;
    };
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(name, "claude" | "claude.exe") {
        return false;
    }
    if matches!(name, "node" | "node.exe" | "bun" | "bun.exe") {
        return fs::read(process.join("cmdline")).is_ok_and(|cmd| {
            !cmd.split(|b| *b == 0)
                .any(|arg| String::from_utf8_lossy(arg).contains("/@anthropic-ai/claude-code/"))
        });
    }
    true
}
#[cfg(not(target_os = "linux"))]
fn unrelated_process(_: u32) -> bool {
    false
}

fn private_file(path: &Path, limit: u64, secret: bool) -> Result<fs::File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|_| "Local Claude session metadata is unavailable")?;
    let m = file
        .metadata()
        .map_err(|_| "Local Claude session metadata is unavailable")?;
    if !m.is_file() || m.len() > limit {
        return Err("Invalid local Claude session metadata".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o002 != 0
            || (secret && m.mode() & 0o077 != 0)
        {
            return Err(
                "Local Claude metadata must belong to this user with private permissions".into(),
            );
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err("Local Claude metadata cannot be a reparse point".into());
        }
        if secret {
            windows::private_key(&file)
                .map_err(|_| "Claude's local inbox key must be private to this user")?;
        }
    }
    Ok(file)
}

pub(crate) fn owners(profile: &Path) -> Result<Vec<Owner>, String> {
    let entries = match fs::read_dir(profile.join("sessions")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => {
            return Err(
                "Claude's local session registry cannot be read; no session was resumed".into(),
            )
        }
    };
    let mut found = vec![];
    for entry in entries {
        let entry = entry.map_err(|_| "Claude's session registry changed while reading")?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|v| v.to_str())
            .and_then(|v| v.parse::<u32>().ok())
        else {
            continue;
        };
        let parsed = private_file(&path, 32 * 1024, false).and_then(|f| {
            serde_json::from_reader::<_, Owner>(f)
                .map_err(|_| "Claude's live session record is incomplete".into())
        });
        let owner = match parsed {
            Ok(v) => v,
            Err(e) if process_start(pid).is_some() && !unrelated_process(pid) => return Err(e),
            Err(_) => continue,
        };
        if owner.pid != pid
            || uuid::Uuid::parse_str(&owner.session_id).is_err()
            || !owner.cwd.is_absolute()
        {
            continue;
        }
        if owner.proc_start.is_none()
            && owner.proc_start_ft.is_none()
            && process_start(pid).is_some()
        {
            return Err("An older Claude process has no verifiable start identity. Close that session before resuming it from Remodex.".into());
        }
        if !owner.identity_matches() {
            continue;
        }
        #[cfg(target_os = "linux")]
        if let Some(domain) = &owner.pid_domain {
            let machine = fs::read_to_string("/etc/machine-id")
                .map_err(|_| "Cannot verify Claude process domain")?;
            let ns = fs::read_link("/proc/self/ns/pid")
                .map_err(|_| "Cannot verify Claude process namespace")?;
            if domain != &format!("linux:{}:{}", machine.trim(), ns.display()) {
                return Err("A Claude session belongs to another process namespace; cannot safely resume it".into());
            }
        }
        if owner.identity_matches() {
            found.push(owner);
        }
    }
    Ok(found)
}

pub(crate) fn owner(profile: &Path, session: &str) -> Result<Option<Owner>, String> {
    let mut matches = owners(profile)?
        .into_iter()
        .filter(|o| o.session_id == session);
    let first = matches.next();
    if matches.next().is_some() {
        return Err("More than one Claude process owns this session; no prompt was sent".into());
    }
    Ok(first)
}

fn auth_frame(profile: &Path, owner: &Owner, socket: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    #[cfg(windows)]
    let canonical = windows::canonical_pipe(socket).ok_or("Invalid Claude inbox path")?;
    #[cfg(windows)]
    let path = canonical.as_str();
    #[cfg(not(windows))]
    let path = socket.to_str().ok_or("Invalid Claude inbox path")?;
    let key = profile.join("sessions").join(format!(
        "{}.{:x}.key",
        owner.pid,
        Sha256::digest(path.as_bytes())
    ));
    let file = private_file(&key, 4096, true)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "Claude inbox credential could not be read")?;
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid Claude inbox credential")?;
    let token = Zeroizing::new(value["peerToken"].as_str().unwrap_or("").to_owned());
    value["peerToken"] = Value::Null;
    // Windows Claude 2.1.302 publishes procStart in the registry and
    // procStartFt in its key. Both represent the same kernel FILETIME.
    #[cfg(windows)]
    let start_matches = value["procStartFt"].as_str().is_some_and(|start| {
        Some(start)
            == owner
                .proc_start_ft
                .as_deref()
                .or(owner.proc_start.as_deref())
            && owner
                .proc_start
                .as_deref()
                .is_none_or(|other| other == start)
            && value["procStart"]
                .as_str()
                .is_none_or(|other| other == start)
    });
    #[cfg(not(windows))]
    let start_matches = value["procStart"].as_str() == owner.proc_start.as_deref()
        && value["procStartFt"].as_str() == owner.proc_start_ft.as_deref();
    if token.len() != 32
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
        || !start_matches
        || value["pidDomain"].as_str() != owner.pid_domain.as_deref()
    {
        return Err("Claude inbox identity changed; no prompt was sent".into());
    }
    let mut out =
        Zeroizing::new(serde_json::to_vec(&json!({"type":"auth","token":token.as_str()})).unwrap());
    out.push(b'\n');
    Ok(out)
}

/// Offers one explicitly authorized input. No automatic retry after writing.
#[cfg(all(test, any(target_os = "linux", windows)))]
pub(crate) async fn offer(
    profile: PathBuf,
    expected: Owner,
    uuid: &str,
    text: &str,
    steer: bool,
) -> Result<Delivery, String> {
    offer_watched(profile, expected, uuid, text, steer, None, None).await
}

pub(crate) async fn offer_watched(
    profile: PathBuf,
    expected: Owner,
    uuid: &str,
    text: &str,
    steer: bool,
    updates: Option<tokio::sync::mpsc::Sender<Delivery>>,
    receipt: Option<Receipt>,
) -> Result<Delivery, String> {
    if uuid::Uuid::parse_str(uuid).is_err() || text.trim().is_empty() || text.len() > 32000 {
        return Err("Invalid local Claude prompt".into());
    }
    if expected.peer_protocol != Some(1) {
        return Err(
            "This Claude session has no compatible local inbox; update Claude Code and reopen it"
                .into(),
        );
    }
    let socket = expected
        .messaging_socket_path
        .clone()
        .ok_or("This Claude session did not publish a local inbox")?;
    if !socket.is_absolute() {
        return Err("Claude did not publish an absolute local inbox path".into());
    }
    let profile_read = profile.clone();
    let identity = expected.clone();
    let sock_read = socket.clone();
    let auth = tokio::task::spawn_blocking(move || {
        let current = owner(&profile_read, &identity.session_id)?
            .ok_or("Claude's original process has exited; no prompt was sent")?;
        if current.pid != identity.pid
            || current.proc_start != identity.proc_start
            || current.proc_start_ft != identity.proc_start_ft
            || current.messaging_socket_path != identity.messaging_socket_path
        {
            return Err("Claude's original process changed; no prompt was sent".into());
        }
        auth_frame(&profile_read, &identity, &sock_read)
    })
    .await
    .map_err(|_| "Claude inbox identity check stopped")??;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let m = fs::symlink_metadata(&socket).map_err(|_| "Claude's local inbox is unavailable")?;
        if !m.file_type().is_socket() || m.uid() != unsafe { libc::geteuid() } {
            return Err("Claude's local inbox is not owned by this user".into());
        }
        let parent = socket
            .parent()
            .ok_or("Claude inbox directory is unavailable")?;
        let parent_meta =
            fs::symlink_metadata(parent).map_err(|_| "Cannot verify Claude inbox directory")?;
        if !parent_meta.is_dir()
            || parent_meta.uid() != unsafe { libc::geteuid() }
            || parent_meta.mode() & 0o022 != 0
        {
            return Err("Claude inbox directory must be private to this user".into());
        }
        let receipt = receipt
            .or_else(|| Receipt::prepare(&profile, &expected))
            .ok_or("Cannot prepare delivery receipt")?;
        if receipt.owner.pid != expected.pid
            || receipt.owner.proc_start != expected.proc_start
            || receipt.path.parent() != Some(parent)
        {
            return Err("Delivery receipt identity changed; no prompt was sent".into());
        }
        let inbox_path = InboxPath(receipt.path);
        let inbox = tokio::net::UnixListener::bind(&inbox_path.0)
            .map_err(|_| "Could not prepare a local delivery receipt inbox")?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&inbox_path.0, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Cannot protect local receipt inbox")?;
        let frame = serde_json::to_vec(&json!({"type":"user","session_id":expected.session_id,"uuid":uuid,"msg_id":uuid,
            "from":format!("uds:{}",inbox_path.0.display()),"priority":if steer {"now"} else {"next"},"message":{"role":"user","content":text}})).unwrap();
        let mut stream = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::UnixStream::connect(&socket),
        )
        .await
        .map_err(|_| "Claude's local inbox did not connect")?
        .map_err(|_| "Claude's local inbox did not connect")?;
        let cred = stream
            .peer_cred()
            .map_err(|_| "Cannot verify Claude's local inbox owner")?;
        if cred.uid() != unsafe { libc::geteuid() }
            || cred.pid().map(|p| p as u32) != Some(expected.pid)
            || !expected.identity_matches()
        {
            return Err(
                "Claude's connected inbox belongs to a different process; no prompt was sent"
                    .into(),
            );
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            stream.write_all(&auth).await?;
            stream.write_all(&frame).await?;
            stream.write_all(b"\n").await?;
            stream.shutdown().await
        })
        .await
        .map_err(|_| "Claude delivery is uncertain; it was not retried")?
        .map_err(|_| "Claude delivery is uncertain; it was not retried")?;
        wait_receipts(inbox, &expected, uuid, updates).await
    }
    #[cfg(windows)]
    {
        let receipt = receipt
            .or_else(|| Receipt::prepare(&profile, &expected))
            .ok_or("Cannot prepare delivery receipt")?;
        windows::offer(
            &profile, &expected, uuid, text, steer, &auth, updates, receipt,
        )
        .await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (auth, updates, receipt);
        Err("Local Claude inbox transport is not available on this platform yet; no prompt was sent".into())
    }
}

/// Reattach only the receipt listener. Never writes a prompt to the native inbox.
pub(crate) async fn recover_receipt(
    receipt: Receipt,
    uuid: &str,
    updates: tokio::sync::mpsc::Sender<Delivery>,
) -> Result<Delivery, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let parent = receipt
            .owner
            .messaging_socket_path
            .as_ref()
            .and_then(|s| s.parent())
            .ok_or("Saved Claude receipt has no inbox directory")?;
        if uuid::Uuid::parse_str(uuid).is_err()
            || receipt.path.parent() != Some(parent)
            || !receipt.owner.identity_matches()
        {
            return Ok(Delivery::Offered);
        }
        let meta =
            fs::symlink_metadata(parent).map_err(|_| "Claude receipt directory unavailable")?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
            return Err("Cannot verify saved Claude receipt directory".into());
        }
        match fs::symlink_metadata(&receipt.path) {
            Ok(meta) => {
                if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                    return Err("Saved Claude receipt address is not a private socket".into());
                }
                match tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::UnixStream::connect(&receipt.path),
                )
                .await
                {
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                        let current = fs::symlink_metadata(&receipt.path)
                            .map_err(|_| "Receipt address changed")?;
                        if current.ino() != meta.ino() || current.dev() != meta.dev() {
                            return Err("Receipt address changed".into());
                        }
                        fs::remove_file(&receipt.path)
                            .map_err(|_| "Cannot recover receipt address")?;
                    }
                    _ => return Err("Saved Claude receipt address is already in use".into()),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Cannot inspect saved Claude receipt address".into()),
        }
        let inbox = tokio::net::UnixListener::bind(&receipt.path)
            .map_err(|_| "Cannot recover Claude receipt listener")?;
        let _path = InboxPath(receipt.path);
        fs::set_permissions(&_path.0, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Cannot protect recovered receipt inbox")?;
        wait_receipts(inbox, &receipt.owner, uuid, Some(updates)).await
    }
    #[cfg(windows)]
    {
        windows::recover_receipt(receipt, uuid, updates).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (receipt, uuid, updates);
        Ok(Delivery::Offered)
    }
}

#[cfg(unix)]
async fn wait_receipts(
    inbox: tokio::net::UnixListener,
    expected: &Owner,
    uuid: &str,
    updates: Option<tokio::sync::mpsc::Sender<Delivery>>,
) -> Result<Delivery, String> {
    let receive = async {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let mut check_owner = tokio::time::interval(Duration::from_secs(1));
        let mut held_reported = false;
        loop {
            let (peer, _) = tokio::select! {
                connection = inbox.accept() => connection.map_err(|_| "Claude receipt inbox closed")?,
                _ = check_owner.tick() => {
                    if !expected.identity_matches() { return Ok(Delivery::Offered); }
                    continue;
                }
            };
            let Ok(cred) = peer.peer_cred() else {
                continue;
            };
            if cred.uid() != unsafe { libc::geteuid() }
                || cred.pid().map(|p| p as u32) != Some(expected.pid)
                || !expected.identity_matches()
            {
                continue;
            }
            // One malformed/stalled callback cannot consume the listener.
            let receipt = tokio::time::timeout(Duration::from_secs(1), async {
                let mut reader = BufReader::new(peer.take(8192));
                let mut line = String::new();
                while reader.read_line(&mut line).await.ok()? > 0 {
                    let value = serde_json::from_str::<Value>(line.trim()).ok();
                    line.clear();
                    if let Some(status) = value.and_then(|v| receipt_status(&v, uuid)) {
                        return Some(status);
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            match receipt {
                Some(Delivery::Held) => {
                    let Some(updates) = &updates else {
                        return Ok(Delivery::Held);
                    };
                    if !held_reported {
                        if updates.send(Delivery::Held).await.is_err() {
                            return Ok(Delivery::Offered);
                        }
                        held_reported = true;
                    }
                }
                Some(delivery @ Delivery::Rejected(_)) => return Ok(delivery),
                _ => {}
            }
        }
    };
    if updates.is_some() {
        receive.await
    } else {
        tokio::time::timeout(Duration::from_millis(750), receive)
            .await
            .unwrap_or(Ok(Delivery::Offered))
    }
}

fn receipt_status(value: &Value, uuid: &str) -> Option<Delivery> {
    if value["type"] != "control"
        || value["action"] != "peer_message_status"
        || value["orig_msg_id"] != uuid
    {
        return None;
    }
    match value["status"].as_str()? {
        "held" => Some(Delivery::Held),
        "denied" | "refused" | "expired" | "dropped" => Some(Delivery::Rejected(
            value["reason"]
                .as_str()
                .unwrap_or("Claude did not accept this local message")
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .take(1024)
                .collect(),
        )),
        _ => None,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn unreadable_executable_only_ignores_proven_foreign_user() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("status");
        // Reproduces a recycled PID owned by root with an inaccessible exe.
        fs::write(&status, "Name:\tcuda00004800013\nUid:\t0\t0\t0\t0\n").unwrap();
        assert!(unrelated_linux_process(dir.path(), 1000));
        for value in [
            "Uid:\t1000\t1000\t1000\t1000\n",
            "Uid:\t1000\t0\t0\t0\n",
            "Uid:\t0\t0\t1000\t0\n",
            "Uid:\t0\t0\n",
            "Uid:\t0\t0\tbad\t0\n",
            "Name:\tclaude\n",
        ] {
            fs::write(&status, value).unwrap();
            assert!(!unrelated_linux_process(dir.path(), 1000), "{value}");
        }
        fs::remove_file(status).unwrap();
        assert!(!unrelated_linux_process(dir.path(), 1000));
    }

    #[test]
    fn registry_ignores_recycled_pids_and_rejects_unsafe_records() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sessions")).unwrap();
        let pid = std::process::id();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.path().join(format!("sessions/{pid}.json"));
        let mut row =
            json!({"pid":pid,"sessionId":id,"cwd":"/tmp","status":"busy","procStart":"old"});
        fs::write(&file, row.to_string()).unwrap();
        assert!(owners(dir.path()).unwrap().is_empty());
        row["procStart"] = json!(process_start(pid).unwrap());
        fs::write(&file, row.to_string()).unwrap();
        assert!(owner(dir.path(), &id).unwrap().unwrap().busy());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(owners(dir.path()).unwrap().is_empty());
    }
    #[tokio::test]
    async fn delayed_receipts_keep_native_priority_and_report_held_then_refused() {
        exercise_delayed_receipts(false).await;
    }
    #[tokio::test]
    async fn restart_recovers_callback_without_replaying_original_prompt() {
        exercise_delayed_receipts(true).await;
    }
    async fn exercise_delayed_receipts(restart: bool) {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sessions")).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("inbox.sock");
        let server = tokio::net::UnixListener::bind(&socket).unwrap();
        let pid = std::process::id();
        let session = uuid::Uuid::new_v4().to_string();
        let o = Owner {
            pid,
            session_id: session,
            cwd: dir.path().into(),
            status: "busy".into(),
            waiting_for: None,
            messaging_socket_path: Some(socket.clone()),
            peer_protocol: Some(1),
            proc_start: process_start(pid),
            proc_start_ft: None,
            pid_domain: None,
        };
        fs::write(
            dir.path().join(format!("sessions/{pid}.json")),
            serde_json::to_vec(&o).unwrap(),
        )
        .unwrap();
        let key = dir.path().join(format!(
            "sessions/{pid}.{:x}.key",
            Sha256::digest(socket.to_str().unwrap().as_bytes())
        ));
        fs::write(
            &key,
            json!({"peerToken":"a".repeat(32),"procStart":o.proc_start}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let receive = tokio::spawn(async move {
            let (mut s, _) = server.accept().await.unwrap();
            let mut data = String::new();
            s.read_to_string(&mut data).await.unwrap();
            let frame: Value = serde_json::from_str(data.lines().nth(1).unwrap()).unwrap();
            let address = frame["from"]
                .as_str()
                .unwrap()
                .strip_prefix("uds:")
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1100)).await;
            let mut callback = tokio::net::UnixStream::connect(address).await.unwrap();
            let status = json!({"type":"control","action":"peer_message_status","status":"held","orig_msg_id":frame["uuid"]});
            callback
                .write_all(format!("{status}\n").as_bytes())
                .await
                .unwrap();
            drop(callback);
            tokio::time::sleep(Duration::from_millis(1100)).await;
            let mut callback = tokio::net::UnixStream::connect(address).await.unwrap();
            let status = json!({"type":"control","action":"peer_message_status","status":"expired","status_detail":"refused","reason":"Owner refused","orig_msg_id":frame["uuid"]});
            callback
                .write_all(format!("{status}\n").as_bytes())
                .await
                .unwrap();
            data
        });
        let (updates, mut statuses) = tokio::sync::mpsc::channel(8);
        let receipt = Receipt::prepare(dir.path(), &o).unwrap();
        let saved = serde_json::to_vec(&receipt).unwrap();
        let uuid = uuid::Uuid::new_v4().to_string();
        let (profile, expected, uid) = (dir.path().to_owned(), o.clone(), uuid.clone());
        let next_updates = updates.clone();
        let delivery = tokio::spawn(async move {
            offer_watched(
                profile,
                expected,
                &uid,
                "continue",
                true,
                Some(next_updates),
                Some(receipt),
            )
            .await
        });
        let result = if restart {
            tokio::time::sleep(Duration::from_millis(250)).await;
            delivery.abort();
            let _ = delivery.await;
            let restored: Receipt = serde_json::from_slice(&saved).unwrap();
            // Simulate an unclean exit leaving the inode behind as well.
            drop(tokio::net::UnixListener::bind(&restored.path).unwrap());
            recover_receipt(restored, &uuid, updates).await
        } else {
            delivery.await.unwrap()
        };
        assert!(
            matches!(result.unwrap(), Delivery::Rejected(ref reason) if reason == "Owner refused")
        );
        assert!(matches!(statuses.try_recv().unwrap(), Delivery::Held));
        let rows: Vec<Value> = receive
            .await
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["type"], "auth");
        assert_eq!(rows[1]["priority"], "now");
        assert_eq!(rows[1]["session_id"], o.session_id);
        assert_eq!(rows[1]["message"]["content"], "continue");
        assert!(rows
            .iter()
            .all(|r| r["request"]["subtype"] != "enable_remote_control"));
        let receipt = Receipt::prepare(dir.path(), &o).unwrap();
        let (updates, _) = tokio::sync::mpsc::channel(8);
        let listening = tokio::net::UnixListener::bind(&receipt.path).unwrap();
        assert!(recover_receipt(receipt.clone(), &uuid, updates.clone())
            .await
            .is_err());
        drop(listening);
        fs::remove_file(&receipt.path).unwrap();
        let target = dir.path().join("keep.txt");
        fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &receipt.path).unwrap();
        assert!(recover_receipt(receipt, &uuid, updates).await.is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
        let mut stale = o;
        stale.proc_start = Some("recycled".into());
        assert!(offer(
            dir.path().into(),
            stale,
            &uuid::Uuid::new_v4().to_string(),
            "no",
            true
        )
        .await
        .is_err());
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod live_check {
    use super::*;
    #[test]
    #[ignore = "reads an explicitly selected live profile; never sends a prompt"]
    fn existing_session_authentication() {
        let profile = PathBuf::from(
            std::env::var("REMODEX_LOCAL_QA_PROFILE").expect("explicit profile required"),
        );
        let session =
            std::env::var("REMODEX_LOCAL_QA_SESSION").expect("explicit existing session required");
        let original = owner(&profile, &session)
            .unwrap()
            .expect("original process must be open");
        auth_frame(
            &profile,
            &original,
            original.messaging_socket_path.as_deref().unwrap(),
        )
        .unwrap();
    }
    // Explicit opt-in only: an existing QA session, no new conversation, and
    // no changes to Claude's inbound settings or application files.
    #[tokio::test]
    #[ignore]
    async fn existing_session_delivery() {
        let profile = PathBuf::from(
            std::env::var("REMODEX_LOCAL_QA_PROFILE").expect("explicit profile required"),
        );
        let session =
            std::env::var("REMODEX_LOCAL_QA_SESSION").expect("explicit existing session required");
        let text = std::env::var("REMODEX_LOCAL_QA_PROMPT").expect("explicit prompt required");
        let original = owner(&profile, &session)
            .unwrap()
            .expect("original process must be open");
        let uuid = uuid::Uuid::new_v4().to_string();
        let result = offer(profile.clone(), original.clone(), &uuid, &text, true)
            .await
            .unwrap();
        let status = match result {
            Delivery::Offered => "offered",
            Delivery::Held => "held",
            Delivery::Rejected(_) => "rejected",
        };
        let evidence = std::env::var("REMODEX_LOCAL_QA_RESULT").expect("evidence path required");
        fs::write(evidence,serde_json::to_vec_pretty(&json!({"uuid":uuid,"session":session,"ownerPid":original.pid,"status":status,"sameOwner":owner(&profile,&session).unwrap().is_some_and(|o|o.pid==original.pid)})).unwrap()).unwrap();
    }
}
