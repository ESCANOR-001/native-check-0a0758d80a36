//! Compatibility launch bridge for Windows Bun sources. Bun's detached children
//! inherit its listening sockets; disabling handle inheritance prevents
//! the retained connector from keeping the old server ports occupied.
#[cfg(windows)]
use crate::quick_handoff;
use crate::quick_handoff::{Config, Process};
use std::path::Path;

pub fn launch(
    executable: &Path,
    origin: &str,
    protocol: &str,
    directory: &Path,
) -> Result<Config, String> {
    #[cfg(not(windows))]
    {
        let _ = (executable, origin, protocol, directory);
        Err("This compatibility launcher is Windows-only".into())
    }
    #[cfg(windows)]
    {
        use std::os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        };
        use windows_sys::Win32::System::Threading::*;
        let origin: std::net::SocketAddr = origin
            .parse()
            .map_err(|_| "Invalid private tunnel origin")?;
        if !origin.ip().is_loopback()
            || origin.port() == 0
            || !matches!(protocol, "http2" | "quic")
            || !executable.is_absolute()
            || !directory.is_absolute()
            || directory.exists()
        {
            return Err("A new private tunnel directory and loopback origin are required".into());
        }
        crate::storage_stage::open(executable, false)
            .map_err(|_| "Tunnel executable unavailable")?;
        crate::private_directory::prepare(directory).map_err(|_| "Tunnel directory unavailable")?;
        let log = |name| {
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(directory.join(name))
        };
        drop(log("stdout.log").map_err(|_| "Tunnel log unavailable")?);
        drop(log("stderr.log").map_err(|_| "Tunnel log unavailable")?);
        let arguments = [
            executable.to_str().ok_or("Invalid tunnel path")?.to_owned(),
            "tunnel".into(),
            "--url".into(),
            format!("http://{origin}"),
            "--no-autoupdate".into(),
            "--protocol".into(),
            protocol.into(),
            "--logfile".into(),
            directory
                .join("stderr.log")
                .to_str()
                .ok_or("Invalid log path")?
                .to_owned(),
        ];
        let command = arguments
            .iter()
            .map(|v| crate::native_service::windows_argument(v))
            .collect::<Result<Vec<_>, _>>()?
            .join(" ");
        let mut command: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let executable_wide: Vec<u16> = executable.as_os_str().encode_wide().chain([0]).collect();
        let mut values: Vec<_> = std::env::vars_os()
            .filter(|(k, _)| {
                let k = k.to_string_lossy().to_uppercase();
                !k.starts_with("TUNNEL_")
                    && !k.starts_with("CLOUDFLARED_")
                    && !k.starts_with("CF_TUNNEL_")
            })
            .collect();
        values.push(("TUNNEL_NO_PRECHECKS".into(), "true".into()));
        values.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
        let mut environment: Vec<u16> = Vec::new();
        for (k, v) in values {
            environment.extend(k.encode_wide());
            environment.push(61);
            environment.extend(v.encode_wide());
            environment.push(0);
        }
        environment.push(0);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // No standard pipes, socket handles or file handles cross this boundary.
        // cloudflared opens its private log itself. This also lets Bun's output
        // pipes close as soon as this short broker exits.
        startup.dwFlags = STARTF_USESTDHANDLES;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup,
                &mut info,
            )
        } == 0
        {
            return Err("Tunnel could not be started".into());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        let _thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
        let config = Config {
            pid: info.dwProcessId,
            executable: executable.to_owned(),
            origin,
            public_url: "https://pending.trycloudflare.com".into(),
            environment_id: format!("opencodex-{}", "0".repeat(24)),
            receipt: directory.join("quick-tunnel-process.json"),
        };
        let save = || -> Result<(), String> {
            quick_handoff::capture(&config, directory)?;
            use std::io::Write;
            let mut record = tempfile::NamedTempFile::new_in(directory)
                .map_err(|_| "Tunnel record unavailable")?;
            record
                .write_all(&serde_json::to_vec(&config).map_err(|_| "Tunnel record invalid")?)
                .map_err(|_| "Tunnel record unavailable")?;
            record
                .as_file()
                .sync_all()
                .map_err(|_| "Tunnel record unavailable")?;
            record
                .persist_noclobber(directory.join("process.json"))
                .map_err(|_| "Tunnel record unavailable")?;
            Ok(())
        };
        if let Err(error) = save() {
            unsafe {
                TerminateProcess(process.as_raw_handle(), 1);
                WaitForSingleObject(process.as_raw_handle(), 5000);
            }
            return Err(error);
        }
        // Drop does not kill the independent connector. Only its verified
        // process handle, not a reused PID, is accepted by the stop operation.
        Ok(config)
    }
}
pub fn control(config: &Path, operation: &str) -> Result<bool, String> {
    if !matches!(operation, "status" | "stop" | "wait") {
        return Err("Invalid tunnel control operation".into());
    }
    let file =
        crate::storage_stage::open(config, false).map_err(|_| "Tunnel record unavailable")?;
    if crate::storage_stage::links(&file).map_err(|_| "Tunnel record unavailable")? != 1 {
        return Err("Tunnel record changed".into());
    }
    let config: Config = serde_json::from_slice(
        &crate::profile_snapshot::bytes(config, 8192).map_err(|_| "Tunnel record unavailable")?,
    )
    .map_err(|_| "Tunnel record invalid")?;
    match Process::open(&config) {
        Ok(process) => {
            if operation == "stop" {
                process.disable()?;
            } else if operation == "wait" {
                process.wait();
            }
            Ok(process.alive())
        }
        Err(_) if operation != "stop" => Ok(false),
        Err(error) => Err(error),
    }
}
