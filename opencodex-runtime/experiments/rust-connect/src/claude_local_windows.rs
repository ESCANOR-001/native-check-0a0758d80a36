//! Native Windows peer transport. Authenticate kernel process identity before
//! writing credentials; receipts use a local, current-user-only named pipe.
use super::{receipt_status, Delivery, Owner, Receipt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
    sync::mpsc,
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        EqualSid, GetAce, GetTokenInformation, IsWellKnownSid, TokenSessionId, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW},
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};
fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}
fn pid_domain() -> io::Result<String> {
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut length) } == 0
        || length == 0
        || length as usize >= buffer.len()
    {
        return Err(denied());
    }
    let hostname = String::from_utf16(&buffer[..length as usize]).map_err(|_| denied())?;
    Ok(format!("win32:{}", hostname.to_lowercase()))
}
fn token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 || token.is_null() {
        return Err(denied());
    }
    // OpenProcessToken returns a newly owned handle. Never own the pseudo-handle
    // returned by GetCurrentProcess.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}
fn user(token: &OwnedHandle) -> io::Result<Vec<usize>> {
    let mut length = 0;
    let result = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut length,
        )
    };
    if result != 0
        || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        || !(std::mem::size_of::<TOKEN_USER>() as u32..=65536).contains(&length)
    {
        return Err(denied());
    }
    // TOKEN_USER contains a pointer, so byte-vector alignment is insufficient.
    let mut data = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut used = length;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            length,
            &mut used,
        )
    } == 0
        || used > length
        || used < std::mem::size_of::<TOKEN_USER>() as u32
    {
        return Err(denied());
    }
    let sid = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    let start = data.as_ptr() as usize;
    let end = start.checked_add(used as usize).ok_or_else(denied)?;
    let position = sid as usize;
    // A SID begins with revision/count and six authority bytes, followed by
    // count 32-bit subauthorities. Bound the whole SID before EqualSid reads it.
    if position < start + std::mem::size_of::<TOKEN_USER>()
        || position.checked_add(8).is_none_or(|limit| limit > end)
    {
        return Err(denied());
    }
    let count = unsafe { *sid.cast::<u8>().add(1) } as usize;
    if count > 15 || position + 8 + count * 4 > end {
        return Err(denied());
    }
    Ok(data)
}
fn session(token: &OwnedHandle) -> io::Result<u32> {
    let mut value = 0u32;
    let mut length = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenSessionId,
            (&mut value as *mut u32).cast(),
            4,
            &mut length,
        )
    } == 0
        || length != 4
    {
        return Err(denied());
    }
    Ok(value)
}

fn own_user() -> io::Result<Vec<usize>> {
    user(&token(unsafe { GetCurrentProcess() })?)
}
fn sid(data: &[usize]) -> *mut std::ffi::c_void {
    unsafe { (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}
fn verify(pipe: RawHandle, expected: &Owner, client: bool) -> io::Result<()> {
    let mut pid = 0;
    let ok = unsafe {
        if client {
            GetNamedPipeClientProcessId(pipe, &mut pid)
        } else {
            GetNamedPipeServerProcessId(pipe, &mut pid)
        }
    };
    if ok == 0 || pid != expected.pid || !expected.identity_matches() {
        return Err(denied());
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(denied());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    let peer = token(process.as_raw_handle())?;
    let own = token(unsafe { GetCurrentProcess() })?;
    if unsafe { EqualSid(sid(&user(&peer)?), sid(&user(&own)?)) } == 0
        || session(&peer)? != session(&own)?
    {
        return Err(denied());
    }
    Ok(())
}
struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

// The key file is opened without following reparse points before this check.
// Permit only this user, SYSTEM and administrators; reject broad inherited ACLs.
pub(super) fn private_key(file: &std::fs::File) -> io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut acl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(denied());
    }
    let _guard = LocalAllocation(descriptor);
    let own = own_user()?;
    if owner.is_null() || acl.is_null() || unsafe { EqualSid(owner, sid(&own)) } == 0 {
        return Err(denied());
    }
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(acl, index as u32, &mut ace) } == 0 {
            return Err(denied());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Ignore inherited-only entries and denied ACEs, reject unfamiliar grants.
        if header.AceFlags & 0x08 != 0 || header.AceType == 1 {
            continue;
        }
        if header.AceType != 0
            || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(denied());
        }
        let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let trustee = std::ptr::addr_of!(allow.SidStart).cast::<u8>();
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        if (header.AceSize as usize) < sid_offset + 8 {
            return Err(denied());
        }
        let count = unsafe { *trustee.add(1) } as usize;
        if count > 15 || (header.AceSize as usize) < sid_offset + 8 + count * 4 {
            return Err(denied());
        }
        let trustee = trustee.cast_mut().cast();
        if unsafe { EqualSid(trustee, sid(&own)) } == 0
            && unsafe { IsWellKnownSid(trustee, WinLocalSystemSid) } == 0
            && unsafe { IsWellKnownSid(trustee, WinBuiltinAdministratorsSid) } == 0
        {
            return Err(denied());
        }
    }
    Ok(())
}
fn private_descriptor() -> io::Result<LocalAllocation> {
    let own = own_user()?;
    let mut sid_string = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid(&own), &mut sid_string) } == 0 {
        return Err(denied());
    }
    let sid_guard = LocalAllocation(sid_string.cast());
    let mut len = 0;
    while len < 256 && unsafe { *sid_string.add(len) } != 0 {
        len += 1;
    }
    if len == 256 {
        return Err(denied());
    }
    let sid_text = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_string, len) })
        .map_err(|_| denied())?;
    drop(sid_guard);
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid_text})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(denied());
    }
    Ok(LocalAllocation(descriptor))
}
fn inbox(address: &str) -> io::Result<NamedPipeServer> {
    let descriptor = private_descriptor()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                address,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}
struct ReceiptKey(PathBuf);
impl Drop for ReceiptKey {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
// Windows requires a key belonging to a live process even for status callbacks.
// Publish our own ephemeral endpoint key, never a fake Claude session record.
fn receipt_key(profile: &Path, address: &str) -> io::Result<ReceiptKey> {
    use std::{io::Write, os::windows::ffi::OsStrExt};
    let canonical = canonical_pipe(Path::new(address)).ok_or_else(denied)?;
    let path = profile.join("sessions").join(format!(
        "{}.{:x}.key",
        std::process::id(),
        Sha256::digest(canonical.as_bytes())
    ));
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let guard = ReceiptKey(path);
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    private_key(&file)?;
    let start = super::process_start(std::process::id()).ok_or_else(denied)?;
    let peer = zeroize::Zeroizing::new(uuid::Uuid::new_v4().simple().to_string());
    let child = zeroize::Zeroizing::new(uuid::Uuid::new_v4().simple().to_string());
    let domain = serde_json::to_string(&pid_domain()?).map_err(|_| denied())?;
    let bytes = zeroize::Zeroizing::new(format!(
        "{{\"peerToken\":\"{}\",\"childToken\":\"{}\",\"procStartFt\":\"{}\",\"pidDomain\":{domain}}}",
        peer.as_str(),
        child.as_str(),
        start
    ));
    file.write_all(bytes.as_bytes())?;
    file.flush()?;
    Ok(guard)
}

pub(super) fn canonical_pipe(path: &Path) -> Option<String> {
    let raw = path.to_str()?;
    if raw.starts_with(r"\\?\") && raw.contains('/') {
        return None;
    }
    let normalized = raw.replace('/', "\\");
    let name = normalized.get(9..)?;
    if !normalized[..9].eq_ignore_ascii_case(r"\\.\pipe\")
        && !normalized[..9].eq_ignore_ascii_case(r"\\?\pipe\")
    {
        return None;
    }
    let base = if name
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("LOCAL\\"))
    {
        &name[6..]
    } else {
        name
    };
    if base.is_empty()
        || base.len() > 240
        || base.contains(['\\', '/', '\0'])
        || base.ends_with(['.', ' '])
    {
        return None;
    }
    Some(format!(r"\\.\pipe\{}", name.to_ascii_lowercase()))
}
fn local_pipe(path: &Path) -> bool {
    canonical_pipe(path).is_some()
}

// Keep the authenticated wire envelope separate from the durable receipt.
#[allow(clippy::too_many_arguments)]
pub(super) async fn offer(
    profile: &Path,
    expected: &Owner,
    uuid: &str,
    text: &str,
    steer: bool,
    auth: &[u8],
    updates: Option<mpsc::Sender<Delivery>>,
    saved: Receipt,
) -> Result<Delivery, String> {
    let socket = expected
        .messaging_socket_path
        .as_deref()
        .ok_or("Claude did not publish a local Windows inbox")?;
    if !local_pipe(socket) {
        return Err("Claude did not publish a local Windows inbox".into());
    }
    if saved.owner.pid != expected.pid
        || saved.owner.proc_start != expected.proc_start
        || saved.owner.proc_start_ft != expected.proc_start_ft
        || saved.profile != profile
    {
        return Err("Saved Claude receipt identity changed; no prompt was sent".into());
    }
    let address = receipt_address(&saved)?;
    let receipt = inbox(&address).map_err(|_| "Cannot protect the local Claude receipt pipe")?;
    let _key = receipt_key(profile, &address)
        .map_err(|_| "Cannot protect the local Claude receipt key")?;
    let mut stream = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ClientOptions::new().open(socket) {
                Ok(stream) => return Ok(stream),
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| "Claude's local inbox did not connect")?
    .map_err(|_| "Claude's local inbox did not connect")?;
    verify(stream.as_raw_handle(), expected, false)
        .map_err(|_| "Claude's inbox belongs to a different process; no prompt was sent")?;
    let frame=serde_json::to_vec(&json!({"type":"user","session_id":expected.session_id,"uuid":uuid,"msg_id":uuid,
        "from":format!("uds:{address}"),"priority":if steer {"now"} else {"next"},"message":{"role":"user","content":text}})).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        stream.write_all(auth).await?;
        stream.write_all(&frame).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| "Claude delivery is uncertain; it was not retried")?
    .map_err(|_| "Claude delivery is uncertain; it was not retried")?;
    wait_receipts(receipt, expected, uuid, updates).await
}
fn receipt_address(receipt: &Receipt) -> Result<String, String> {
    let address = canonical_pipe(&receipt.path).ok_or("Invalid saved receipt pipe")?;
    let suffix = address
        .strip_prefix(r"\\.\pipe\cc-msg-")
        .ok_or("Invalid saved receipt pipe")?;
    if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid saved receipt pipe".into());
    }
    Ok(address)
}
pub(super) async fn recover_receipt(
    saved: Receipt,
    uuid: &str,
    updates: mpsc::Sender<Delivery>,
) -> Result<Delivery, String> {
    if uuid::Uuid::parse_str(uuid).is_err() || !saved.owner.identity_matches() {
        return Ok(Delivery::Offered);
    }
    let address = receipt_address(&saved)?;
    // first_pipe_instance prevents taking over an endpoint that is still owned.
    let receipt = inbox(&address).map_err(|_| "Cannot recover local Claude receipt pipe")?;
    let _key = receipt_key(&saved.profile, &address)
        .map_err(|_| "Cannot protect recovered Claude receipt key")?;
    wait_receipts(receipt, &saved.owner, uuid, Some(updates)).await
}
async fn wait_receipts(
    mut receipt: NamedPipeServer,
    expected: &Owner,
    uuid: &str,
    updates: Option<mpsc::Sender<Delivery>>,
) -> Result<Delivery, String> {
    let receive = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut held = false;
        loop {
            tokio::select! {
                result=receipt.connect()=>{result.map_err(|_|"Claude receipt pipe closed")?;},
                _=tick.tick()=>{if !expected.identity_matches() {return Ok(Delivery::Offered);} continue;}
            }
            let result = if verify(receipt.as_raw_handle(), expected, true).is_ok() {
                tokio::time::timeout(Duration::from_secs(1), async {
                    let mut reader = BufReader::new((&mut receipt).take(8192));
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
                .flatten()
            } else {
                None
            };
            receipt
                .disconnect()
                .map_err(|_| "Claude receipt pipe closed")?;
            match result {
                Some(Delivery::Held) => {
                    let Some(updates) = &updates else {
                        return Ok(Delivery::Held);
                    };
                    if !held {
                        if updates.send(Delivery::Held).await.is_err() {
                            return Ok(Delivery::Offered);
                        }
                        held = true;
                    }
                }
                Some(rejected @ Delivery::Rejected(_)) => return Ok(rejected),
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
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn queue_and_steer_authenticate_the_owner_and_receive_refusals() {
        for steer in [false, true] {
            let dir = tempfile::Builder::new()
                .prefix("Claude pipe 测试 ")
                .tempdir()
                .unwrap();
            let profile = dir.path().to_path_buf();
            std::fs::create_dir(profile.join("sessions")).unwrap();
            let address = format!(r"\\.\pipe\remodex-claude-test-{}", uuid::Uuid::new_v4());
            let server = inbox(&address).unwrap();
            let _key = receipt_key(&profile, &address).unwrap();
            let owner = Owner {
                pid: std::process::id(),
                session_id: uuid::Uuid::new_v4().to_string(),
                cwd: profile.clone(),
                status: "idle".into(),
                waiting_for: None,
                messaging_socket_path: Some(address.into()),
                peer_protocol: Some(1),
                proc_start: if steer {
                    None
                } else {
                    super::super::process_start(std::process::id())
                },
                proc_start_ft: if steer {
                    super::super::process_start(std::process::id())
                } else {
                    None
                },
                pid_domain: Some(pid_domain().unwrap()),
            };
            std::fs::write(
                profile.join("sessions").join(format!("{}.json", owner.pid)),
                serde_json::to_vec(&owner).unwrap(),
            )
            .unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let mut stale = owner.clone();
            stale.proc_start_ft = Some("0".into());
            assert!(
                super::super::offer(profile.clone(), stale, &id, "test", steer)
                    .await
                    .is_err()
            );
            let receive = async {
                server.connect().await.unwrap();
                let mut lines = BufReader::new(server).lines();
                let auth: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(auth["type"], "auth");
                assert_eq!(auth["token"].as_str().unwrap().len(), 32);
                let frame: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(frame["uuid"], id);
                assert_eq!(frame["msg_id"], id);
                assert_eq!(frame["session_id"], owner.session_id);
                assert_eq!(frame["priority"], if steer { "now" } else { "next" });
                assert_eq!(frame["message"]["content"], "Windows delivery 测试");
                let callback = frame["from"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("uds:")
                    .unwrap();
                let mut pipe = ClientOptions::new().open(callback).unwrap();
                let status = json!({"type":"control","action":"peer_message_status",
                    "orig_msg_id":id,"status":"refused","reason":"test refusal"});
                pipe.write_all(format!("{status}\n").as_bytes())
                    .await
                    .unwrap();
                pipe.shutdown().await.unwrap();
                // Windows pipes do not half-close. The sender drops its inbox
                // connection after reading the callback; then require EOF,
                // proving that no second prompt was written on this connection.
                assert!(lines.next_line().await.unwrap().is_none());
            };
            let send = super::super::offer(
                profile.clone(),
                owner.clone(),
                &id,
                "Windows delivery 测试",
                steer,
            );
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(send, receive)
            })
            .await
            .unwrap();
            assert!(matches!(result, Ok(Delivery::Rejected(reason)) if reason == "test refusal"));
            // Only the simulated owner's key remains; callback credentials are removed.
            assert_eq!(
                std::fs::read_dir(profile.join("sessions")).unwrap().count(),
                2
            );
        }
    }

    #[test]
    fn pipe_names_are_local_only() {
        assert!(local_pipe(Path::new(r"\\.\pipe\claude-123")));
        for path in [
            r"\\server\pipe\claude",
            r"C:\tmp\socket",
            r"\\.\pipe\",
            r"\\.\pipe\x\y",
        ] {
            assert!(!local_pipe(Path::new(path)));
        }
    }
    #[test]
    fn callback_key_is_private_live_and_removed_after_delivery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sessions")).unwrap();
        let address = format!(r"\\.\pipe\cc-msg-{}", uuid::Uuid::new_v4().simple());
        let key = receipt_key(dir.path(), &address).unwrap();
        let file = std::fs::File::open(&key.0).unwrap();
        private_key(&file).unwrap();
        let data: Value = serde_json::from_reader(file).unwrap();
        assert_eq!(data["peerToken"].as_str().unwrap().len(), 32);
        assert_eq!(data["pidDomain"], pid_domain().unwrap());
        assert_eq!(
            data["procStartFt"],
            super::super::process_start(std::process::id()).unwrap()
        );
        assert!(receipt_key(dir.path(), &address).is_err());
        let path = key.0.clone();
        drop(key);
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn private_receipt_pipe_rejects_remote_and_duplicate_binding() {
        let name = format!(r"\\.\pipe\remodex-claude-test-{}", uuid::Uuid::new_v4());
        let server = inbox(&name).unwrap();
        assert!(inbox(&name).is_err());
        let _client = ClientOptions::new().open(&name).unwrap();
        server.connect().await.unwrap();
        let mut pid = 0;
        assert_ne!(
            unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid) },
            0
        );
        assert_eq!(pid, std::process::id());
    }
}
