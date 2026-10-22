//! Own only the process tree we spawned, including cancellation of the runtime.
use std::io;
use tokio::process::Child;

pub struct ProcessTree {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}
impl ProcessTree {
    pub fn attach(child: &Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { pid: child.id() })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::JobObjects::*;
            // The CLI has not received any input yet. Assign it before allowing
            // initialization to start tools; nested jobs work on supported Windows.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("Missing owned child handle"))?;
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
                || unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { _job: job })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Err(io::Error::other("Unsupported process-tree platform"))
        }
    }
    pub fn reaped(&mut self) {
        #[cfg(unix)]
        {
            self.pid = None;
        }
    }
}
#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            // Used if an async task/runtime is dropped before graceful cleanup.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}
