//! Authenticate the connected Windows pipe before exposing Desktop IPC data.
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER, HANDLE},
    Security::{EqualSid, GetTokenInformation, TokenSessionId, TokenUser, TOKEN_QUERY, TOKEN_USER},
    System::{
        Pipes::GetNamedPipeServerProcessId,
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

fn normal_profile(profile: &Path, home: &Path) -> bool {
    home.join(".codex")
        .canonicalize()
        .is_ok_and(|normal| profile == normal)
}
pub(super) fn standard_endpoint(path: &Path, profile: &Path) -> bool {
    path == Path::new(r"\\.\pipe\codex-ipc")
        && std::env::var_os("USERPROFILE")
            .is_some_and(|home| normal_profile(profile, &PathBuf::from(home)))
}
fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
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
pub(super) fn verify_peer(pipe: RawHandle) -> io::Result<()> {
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 || pid == 0 {
        return Err(denied());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(denied());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let peer = token(process.as_raw_handle())?;
    let own = token(unsafe { GetCurrentProcess() })?;
    let peer_user = user(&peer)?;
    let own_user = user(&own)?;
    let peer_sid = unsafe { &*peer_user.as_ptr().cast::<TOKEN_USER>() }
        .User
        .Sid;
    let own_sid = unsafe { &*own_user.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    if unsafe { EqualSid(peer_sid, own_sid) } == 0 || session(&peer)? != session(&own)? {
        return Err(denied());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[tokio::test]
    async fn connected_pipe_checks_server_identity_before_any_protocol_frame() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().canonicalize().unwrap();
        let pipe = format!(
            r"\\.\pipe\remodex-desktop-test-{:x}",
            Sha256::digest(profile.as_os_str().as_encoded_bytes())
        );
        let server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .unwrap();
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&pipe)
            .unwrap();
        server.connect().await.unwrap();
        verify_peer(client.as_raw_handle()).unwrap();
        assert!(verify_peer(std::ptr::null_mut()).is_err());
        assert!(verify_peer(unsafe { GetCurrentProcess() }).is_err());
        assert!(super::super::Endpoint { path: pipe.into() }
            .validate(&profile)
            .is_ok());
        assert!(super::super::Endpoint {
            path: r"\\.\pipe\codex-ipc".into()
        }
        .validate(&profile)
        .is_err());
    }
    #[test]
    fn normal_pipe_cannot_be_bound_to_an_unrelated_or_claude_profile() {
        let root = tempfile::tempdir().unwrap();
        for name in [".codex", ".claude", "other"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
            let profile = root.path().join(name).canonicalize().unwrap();
            assert_eq!(normal_profile(&profile, root.path()), name == ".codex");
        }
    }
}
