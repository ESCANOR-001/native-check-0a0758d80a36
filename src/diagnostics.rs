//! Bounded native event logs. Only typed metadata is recorded or exported.
use crate::profile_snapshot::{open, regular};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
const MAX_LOG: u64 = 1024 * 1024;
const TAIL: u64 = 128 * 1024;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Lifecycle {
    #[serde(rename = "runtime.ready")]
    Ready,
    #[serde(rename = "runtime.stopping")]
    Stopping,
    #[serde(rename = "runtime.stopped")]
    Stopped,
    #[serde(rename = "log.rotated")]
    Rotated,
    #[serde(rename = "http.response")]
    Request,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum Surface {
    Dashboard,
    Phone,
    Health,
    Other,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    version: u8,
    at_ms: i64,
    event: Lifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    surface: Option<Surface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
}
impl Event {
    fn valid(&self) -> bool {
        self.version == 1
            && (0..=9_007_199_254_740_991).contains(&self.at_ms)
            && match self.event {
                Lifecycle::Request => {
                    self.surface.is_some() && self.status.is_some_and(|s| (100..=599).contains(&s))
                }
                _ => self.surface.is_none() && self.status.is_none(),
            }
    }
}
enum Entry {
    Event(Event),
    Flush(tokio::sync::oneshot::Sender<()>),
}
#[derive(Clone)]
pub struct Log {
    directory: PathBuf,
    tx: mpsc::SyncSender<Entry>,
    dropped: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    reads: Arc<tokio::sync::Semaphore>,
}
fn single_link(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() != 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.nNumberOfLinks != 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    Ok(())
}
fn append(path: &Path, event: &Event, limit: u64) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| !regular(&m) || !m.is_file()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options.open(path)?;
    if !file.metadata().is_ok_and(|m| m.is_file() && regular(&m)) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    single_link(&file)?;
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    if file.metadata()?.len().saturating_add(bytes.len() as u64) > limit {
        file.set_len(0)?;
        let mut rotation = serde_json::to_vec(&Event {
            version: 1,
            at_ms: event.at_ms,
            event: Lifecycle::Rotated,
            surface: None,
            status: None,
        })?;
        rotation.push(b'\n');
        file.write_all(&rotation)?;
    }
    file.write_all(&bytes)?;
    file.flush()
}
impl Log {
    pub fn new(directory: &Path) -> io::Result<Self> {
        if !fs::symlink_metadata(directory).is_ok_and(|m| m.is_dir() && regular(&m)) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let directory = directory.canonicalize()?;
        let (tx, rx) = mpsc::sync_channel(64);
        let dropped = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicU64::new(0));
        let errors = failures.clone();
        let path = directory.join("service.log");
        std::thread::Builder::new()
            .name("connect-log".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut retry = Instant::now();
                while let Ok(entry) = rx.recv() {
                    match entry {
                        Entry::Event(event) => {
                            if Instant::now() < retry {
                                errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            if append(&path, &event, MAX_LOG).is_err() {
                                errors.fetch_add(1, Ordering::Relaxed);
                                retry = Instant::now() + Duration::from_secs(5);
                            }
                        }
                        Entry::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self {
            directory,
            tx,
            dropped,
            failures,
            reads: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub fn lifecycle(&self, event: Lifecycle) {
        if matches!(event, Lifecycle::Request | Lifecycle::Rotated) {
            return;
        }
        self.send(Event {
            version: 1,
            at_ms: crate::auth::now(),
            event,
            surface: None,
            status: None,
        });
    }
    pub(crate) fn request(&self, path: &str, status: u16) {
        let surface = if path == "/healthz" {
            Surface::Health
        } else if path == "/ws"
            || path.starts_with("/oauth/")
            || path.starts_with("/api/auth/")
            || path.starts_with("/api/orchestration/")
            || path.starts_with("/api/android-remote/assets/")
        {
            Surface::Phone
        } else if path.starts_with("/api/") {
            Surface::Dashboard
        } else {
            Surface::Other
        };
        self.send(Event {
            version: 1,
            at_ms: crate::auth::now(),
            event: Lifecycle::Request,
            surface: Some(surface),
            status: Some(status),
        });
    }
    fn send(&self, event: Event) {
        if self.tx.try_send(Entry::Event(event)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.try_send(Entry::Flush(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
        }
    }
    pub async fn report(&self, stopping: bool) -> Result<Value, &'static str> {
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Diagnostics read is busy; retry shortly")?;
        let directory = self.directory.clone();
        let dropped = self.dropped.load(Ordering::Relaxed);
        let failures = self.failures.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            report(&directory, stopping, dropped, failures)
        })
        .await
        .map_err(|_| "Diagnostics reader stopped")
    }
}
fn tail(directory: &Path, name: &str) -> Value {
    let read = || -> io::Result<(u64, bool, String)> {
        let (mut file, metadata) = open(&directory.join(name), u64::MAX - 1)?;
        single_link(&file)?;
        let size = metadata.len();
        let start = size.saturating_sub(TAIL);
        file.seek(SeekFrom::Start(start))?;
        let mut raw = vec![0; (size - start) as usize];
        file.read_exact(&mut raw)?;
        let raw = if start > 0 {
            raw.iter()
                .position(|b| *b == b'\n')
                .map(|i| &raw[i + 1..])
                .unwrap_or(&[])
        } else {
            &raw[..]
        };
        let mut output = String::new();
        let mut truncated = start > 0;
        for line in raw.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let safe = match serde_json::from_slice::<Event>(line) {
                Ok(event) if event.valid() => {
                    if matches!(event.event, Lifecycle::Rotated) {
                        truncated = true;
                    }
                    serde_json::to_string(&event)?
                }
                _ => "[Unrecognized log entry redacted]".into(),
            };
            if output.len() + safe.len() + 1 > TAIL as usize {
                truncated = true;
                break;
            }
            output.push_str(&safe);
            output.push('\n');
        }
        if output.is_empty() {
            output = "[No readable log entries]".into();
        }
        Ok((size, truncated, output))
    };
    match read() {
        Ok((size, truncated, log)) => {
            json!({"name":name,"sourceBytes":size,"logExists":true,"truncated":truncated,"log":log})
        }
        Err(e) => {
            let missing = e.kind() == io::ErrorKind::NotFound;
            json!({"name":name,"sourceBytes":0,"logExists":!missing,"truncated":!missing,"log":if missing {"[Log has not been created yet]"} else {"[Log unavailable or refused; other logs are still included]"}})
        }
    }
}
fn report(directory: &Path, stopping: bool, dropped: u64, failures: u64) -> Value {
    let sources = [tail(directory, "service.log"), tail(directory, "crash.log")];
    let log = sources
        .iter()
        .map(|s| {
            format!(
                "----- {} -----\n{}",
                s["name"].as_str().unwrap(),
                s["log"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        s => s,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        s => s,
    };
    json!({"reportVersion":1,"generatedAtMs":crate::auth::now(),"appVersion":env!("CARGO_PKG_VERSION"),"platform":platform,"architecture":arch,"runtime":{"state":if stopping {"stopping"} else {"ready"},"endpoint":null},"logPath":"[Connect profile]/service.log, [Connect profile]/crash.log","logExists":sources.iter().any(|s|s["logExists"]==true),"sourceBytes":sources.iter().fold(0u64,|sum,s|sum.saturating_add(s["sourceBytes"].as_u64().unwrap())),"includedBytes":log.len(),"truncated":sources.iter().any(|s|s["truncated"]==true),"nativeLogDroppedEntries":dropped,"nativeLogWriteFailures":failures,"log":log})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_log_exports_typed_metadata_and_redacts_unrecognized_bodies() {
        let root = tempfile::tempdir().unwrap();
        let log = Log::new(root.path()).unwrap();
        log.lifecycle(Lifecycle::Ready);
        log.request("/api/android-remote/assets/SECRET/image?token=SECRET", 200);
        log.flush().await;
        fs::write(root.path().join("crash.log"),"authorization=SECRET\n{\"version\":1,\"at_ms\":1,\"event\":\"runtime.ready\",\"prompt\":\"PRIVATE_BODY\"}\n{\"version\":1,\"at_ms\":2,\"event\":\"runtime.stopped\"}\n").unwrap();
        let report = log.report(false).await.unwrap();
        let text = report.to_string();
        assert_eq!(report["reportVersion"], 1);
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("PRIVATE_BODY"));
        assert!(!text.contains(root.path().to_str().unwrap()));
        assert!(text.contains("runtime.ready"));
        assert!(text.contains("runtime.stopped"));
        assert!(text.contains("Unrecognized log entry redacted"));
        assert_eq!(
            report["includedBytes"].as_u64().unwrap() as usize,
            report["log"].as_str().unwrap().len()
        );
    }
    #[test]
    fn writer_and_export_remain_bounded_and_partial_first_line_is_discarded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.log");
        let event = Event {
            version: 1,
            at_ms: 1,
            event: Lifecycle::Ready,
            surface: None,
            status: None,
        };
        for _ in 0..200 {
            append(&path, &event, 1024).unwrap();
            assert!(path.metadata().unwrap().len() <= 1024);
        }
        assert_eq!(tail(root.path(), "service.log")["truncated"], true);
        let mut huge = vec![b'x'; TAIL as usize + 10];
        huge.extend_from_slice(
            b"SECRET\n{\"version\":1,\"at_ms\":2,\"event\":\"runtime.stopped\"}\n",
        );
        fs::write(&path, huge).unwrap();
        let result = tail(root.path(), "service.log");
        assert_eq!(result["truncated"], true);
        assert!(!result.to_string().contains("SECRET"));
        assert!(result.to_string().contains("runtime.stopped"));
        assert!(result["log"].as_str().unwrap().len() <= TAIL as usize);
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn redirected_logs_are_never_written_or_exported() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, "PRIVATE_UNCHANGED").unwrap();
        let linked = root.path().join("service.log");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, &linked).unwrap();
        let event = Event {
            version: 1,
            at_ms: 1,
            event: Lifecycle::Ready,
            surface: None,
            status: None,
        };
        assert!(append(&linked, &event, 1024).is_err());
        assert!(!tail(root.path(), "service.log")
            .to_string()
            .contains("PRIVATE_UNCHANGED"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "PRIVATE_UNCHANGED");
        fs::remove_file(&linked).unwrap();
        fs::hard_link(&outside, &linked).unwrap();
        assert!(append(&linked, &event, 1024).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "PRIVATE_UNCHANGED");
    }
}
