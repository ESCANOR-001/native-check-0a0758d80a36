//! Keep an already connected Windows quick tunnel across a verified owner handoff.
//! A PID alone is never authority: the private receipt binds its creation time,
//! image fingerprint and complete source configuration before shutdown is offered.
use crate::{profile_snapshot, storage_stage};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

const ERROR: &str = "The existing temporary internet connection could not be retained; its saved pairing was not changed";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub pid: u32,
    pub executable: PathBuf,
    pub origin: SocketAddr,
    pub public_url: String,
    pub environment_id: String,
    pub receipt: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    configuration: String,
    created: u64,
    image: String,
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let url = reqwest::Url::parse(&self.public_url).map_err(|_| ERROR)?;
        let host = url.host_str().ok_or(ERROR)?;
        if self.pid == 0
            || !self.executable.is_absolute()
            || !self.receipt.is_absolute()
            || !self.origin.ip().is_loopback()
            || self.origin.port() == 0
            || url.scheme() != "https"
            || !host.ends_with(".trycloudflare.com")
            || host.trim_end_matches(".trycloudflare.com").is_empty()
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || self
                .environment_id
                .strip_prefix("opencodex-")
                .is_none_or(|s| s.len() != 24 || !s.bytes().all(|c| c.is_ascii_hexdigit()))
            || self
                .receipt
                .file_name()
                .is_none_or(|p| p != "quick-tunnel-process.json")
        {
            return Err(ERROR.into());
        }
        Ok(())
    }
    fn digest(&self) -> Result<String, String> {
        Ok(crate::auth::digest(
            &serde_json::to_string(self).map_err(|_| ERROR)?,
        ))
    }

    /// A deliberate "internet access off" choice ends the old temporary
    /// address. Record it so enabling access later can create a new connection,
    /// rather than repeatedly trying to adopt the intentionally stopped PID.
    pub fn retired(&self) -> Result<bool, String> {
        let path = self.receipt.with_file_name("quick-tunnel-retired.json");
        if !path.try_exists().map_err(|_| ERROR)? {
            return Ok(false);
        }
        let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
        if storage_stage::links(&file).map_err(|_| ERROR)? != 1
            || profile_snapshot::bytes(&path, 128).map_err(|_| ERROR)? != self.digest()?.as_bytes()
        {
            return Err(ERROR.into());
        }
        Ok(true)
    }
    pub fn retire(&self) -> Result<(), String> {
        if self.retired()? {
            return Ok(());
        }
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(self.receipt.parent().ok_or(ERROR)?)
            .map_err(|_| ERROR)?;
        file.write_all(self.digest()?.as_bytes())
            .map_err(|_| ERROR)?;
        file.as_file().sync_all().map_err(|_| ERROR)?;
        file.persist_noclobber(self.receipt.with_file_name("quick-tunnel-retired.json"))
            .map_err(|_| ERROR)?;
        Ok(())
    }
}

/// Only called before the authenticated source shutdown request. The path is
/// fixed inside that source instance, never selected by a remote phone.
pub fn capture(config: &Config, source_directory: &Path) -> Result<(), String> {
    config.validate()?;
    if config.receipt != source_directory.join("quick-tunnel-process.json") {
        return Err(ERROR.into());
    }
    if config.receipt.try_exists().map_err(|_| ERROR)? {
        Process::open(config)?;
        return Ok(());
    }
    let process = Process::inspect(config)?;
    let receipt = Receipt {
        version: 1,
        configuration: config.digest()?,
        created: process.created,
        image: crate::service_registration::fingerprint(&config.executable, None)?,
    };
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(source_directory).map_err(|_| ERROR)?;
    file.write_all(&serde_json::to_vec(&receipt).map_err(|_| ERROR)?)
        .map_err(|_| ERROR)?;
    file.as_file().sync_all().map_err(|_| ERROR)?;
    file.persist_noclobber(&config.receipt).map_err(|_| ERROR)?;
    Ok(())
}

pub struct Process {
    created: u64,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}
impl Process {
    pub fn open(config: &Config) -> Result<Self, String> {
        config.validate()?;
        let file = storage_stage::open(&config.receipt, false).map_err(|_| ERROR)?;
        if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
            return Err(ERROR.into());
        }
        let receipt: Receipt = serde_json::from_slice(
            &profile_snapshot::bytes(&config.receipt, 8192).map_err(|_| ERROR)?,
        )
        .map_err(|_| ERROR)?;
        if receipt.version != 1
            || receipt.configuration != config.digest()?
            || receipt.image != crate::service_registration::fingerprint(&config.executable, None)?
        {
            return Err(ERROR.into());
        }
        let process = Self::inspect(config)?;
        if process.created != receipt.created {
            return Err(ERROR.into());
        }
        Ok(process)
    }
    #[cfg(windows)]
    fn inspect(config: &Config) -> Result<Self, String> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::*};
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | 0x00100000,
                0,
                config.pid,
            )
        };
        if raw.is_null() {
            return Err(ERROR.into());
        }
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        if unsafe {
            QueryFullProcessImageNameW(handle.as_raw_handle(), 0, path.as_mut_ptr(), &mut length)
        } == 0
        {
            return Err(ERROR.into());
        }
        let actual =
            PathBuf::from(String::from_utf16(&path[..length as usize]).map_err(|_| ERROR)?);
        if actual.canonicalize().map_err(|_| ERROR)?
            != config.executable.canonicalize().map_err(|_| ERROR)?
        {
            return Err(ERROR.into());
        }
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        if unsafe {
            GetProcessTimes(
                handle.as_raw_handle(),
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3],
            )
        } == 0
        {
            return Err(ERROR.into());
        }
        let process = Self {
            created: ((times[0].dwHighDateTime as u64) << 32) | times[0].dwLowDateTime as u64,
            handle,
        };
        if !process.alive() {
            return Err(ERROR.into());
        }
        Ok(process)
    }
    #[cfg(not(windows))]
    fn inspect(_: &Config) -> Result<Self, String> {
        Err("Retaining a legacy temporary tunnel is currently supported on Windows only".into())
    }
    pub fn alive(&self) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    self.handle.as_raw_handle(),
                    0,
                ) == 258
            }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    pub fn wait(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    self.handle.as_raw_handle(),
                    u32::MAX,
                );
            }
        }
    }
    /// Explicitly disabling internet access terminates only this verified handle.
    /// Ordinary server exit drops the handle without killing the connector.
    pub fn disable(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            if !self.alive()
                || unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        self.handle.as_raw_handle(),
                        0,
                    )
                } != 0
            {
                return Ok(());
            }
        }
        Err(ERROR.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_foreign_urls_and_nonlocal_origins() {
        let root = tempfile::tempdir().unwrap();
        let mut c = Config {
            pid: 1,
            executable: std::env::current_exe().unwrap(),
            origin: "127.0.0.1:10105".parse().unwrap(),
            public_url: "https://retained.trycloudflare.com".into(),
            environment_id: format!("opencodex-{}", "a".repeat(24)),
            receipt: root.path().join("quick-tunnel-process.json"),
        };
        assert!(c.validate().is_ok());
        for url in [
            "https://evil.example",
            "http://retained.trycloudflare.com",
            "https://retained.trycloudflare.com@evil.example",
            "https://retained.trycloudflare.com/?token=x",
        ] {
            c.public_url = url.into();
            assert!(c.validate().is_err());
        }
        c.public_url = "https://retained.trycloudflare.com".into();
        c.origin = "0.0.0.0:10105".parse().unwrap();
        assert!(c.validate().is_err());
    }
    #[cfg(windows)]
    #[test]
    fn receipt_binds_creation_time_image_and_config_without_signalling_process() {
        let root = tempfile::tempdir().unwrap();
        let c = Config {
            pid: std::process::id(),
            executable: std::env::current_exe().unwrap(),
            origin: "127.0.0.1:10105".parse().unwrap(),
            public_url: "https://retained.trycloudflare.com".into(),
            environment_id: format!("opencodex-{}", "a".repeat(24)),
            receipt: root.path().join("quick-tunnel-process.json"),
        };
        capture(&c, root.path()).unwrap();
        assert!(Process::open(&c).unwrap().alive());
        capture(&c, root.path()).unwrap();
        assert!(!c.retired().unwrap());
        c.retire().unwrap();
        c.retire().unwrap();
        assert!(c.retired().unwrap());
        assert!(
            Process::open(&c).unwrap().alive(),
            "retiring a receipt alone must not signal a process"
        );
        let mut foreign = c.clone();
        foreign.pid += 1;
        assert!(foreign.retired().is_err());
        assert!(Process::open(&foreign).is_err());
        let mut receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&c.receipt).unwrap()).unwrap();
        receipt.created += 1;
        std::fs::write(&c.receipt, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(Process::open(&c).is_err());
        // PID reuse and altered receipts do not authorize any process signal.
    }
}
