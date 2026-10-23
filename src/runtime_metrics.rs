//! Scalar-only process measurement. Does not inspect agents or Desktop processes.
use serde_json::{json, Value};
use std::time::Instant;

#[derive(Clone)]
pub(crate) struct Metrics {
    started: Instant,
}
impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}
impl Metrics {
    pub fn read(&self) -> Option<Value> {
        let (rss, private) = memory()?;
        let platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            s => s,
        };
        Some(json!({
            "pid":std::process::id(),"platform":platform,"runtime":"rust",
            "uptimeSeconds":self.started.elapsed().as_secs_f64(),
            "rss":rss,"nativePrivateBytes":private,"observedBytes":rss,"observedMetric":"rss",
            // There is no JavaScript heap in this process. Native allocator
            // totals are not inferred from RSS or mislabeled as a JS heap.
            "heapUsed":0,"heapTotal":0,"external":0,"arrayBuffers":0,"jscHeap":null,
            "metricAvailability":{"jsHeap":"not_applicable","nativeHeap":"not_reported"},
            "watchdog":null
        }))
        // Do not advertise activeTurnCount until drain/restart is implemented:
        // the existing UI uses that field as its restart capability flag.
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_rss(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
    let mut parts = line.split_whitespace();
    let value = parts.next()?.parse::<u64>().ok()?;
    if parts.next()? != "kB" || parts.next().is_some() {
        return None;
    }
    value.checked_mul(1024).filter(|n| *n > 0)
}
#[cfg(target_os = "linux")]
fn memory() -> Option<(u64, Option<u64>)> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open("/proc/self/status")
        .ok()?
        .take(16 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    Some((linux_rss(&text)?, None))
}
#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, Option<u64>)> {
    let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // Kernel writes exactly this ABI struct after checking the supplied count.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            (&mut info as *mut libc::mach_task_basic_info).cast(),
            &mut count,
        )
    };
    let rss = info.resident_size;
    (result == libc::KERN_SUCCESS && count == libc::MACH_TASK_BASIC_INFO_COUNT && rss > 0)
        .then_some((rss, None))
}
#[cfg(windows)]
fn memory() -> Option<(u64, Option<u64>)> {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::GetCurrentProcess,
    };
    let mut info = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let size = info.cb;
    // The current-process pseudo-handle is borrowed; it must never be closed.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut info as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size,
        )
    };
    (ok != 0 && info.WorkingSetSize > 0)
        .then_some((info.WorkingSetSize as u64, Some(info.PrivateUsage as u64)))
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn memory() -> Option<(u64, Option<u64>)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_report_is_real_scalar_only_and_does_not_advertise_unsupported_controls() {
        let m = Metrics::default();
        let v = m.read().expect("Supported OS must report process memory");
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["runtime"], "rust");
        assert!(v["rss"].as_u64().unwrap() > 0);
        assert_eq!(v["observedBytes"], v["rss"]);
        assert_eq!(v["heapUsed"], 0);
        assert!(v["jscHeap"].is_null());
        assert!(v["watchdog"].is_null());
        assert!(v.get("activeTurnCount").is_none());
        assert!(v.get("bunVersion").is_none());
        assert!(
            m.read().unwrap()["uptimeSeconds"].as_f64().unwrap()
                >= v["uptimeSeconds"].as_f64().unwrap()
        );
        #[cfg(windows)]
        assert!(v["nativePrivateBytes"].as_u64().unwrap() > 0);
        #[cfg(not(windows))]
        assert!(v["nativePrivateBytes"].is_null());
    }
    #[test]
    fn linux_counter_requires_correct_units_and_checked_arithmetic() {
        assert_eq!(linux_rss("Name: x\nVmRSS:\t1024 kB\n"), Some(1_048_576));
        for s in [
            "VmRSS: 4 MB",
            "VmRSS: -1 kB",
            "VmRSS: 18446744073709551615 kB",
            "VmRSS: 0 kB",
            "VmRSS: 5 kB extra",
            "Name: x",
        ] {
            assert_eq!(linux_rss(s), None);
        }
    }
}
