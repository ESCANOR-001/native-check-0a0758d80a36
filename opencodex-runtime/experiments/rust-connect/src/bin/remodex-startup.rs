//! Windowless, non-detached Task Scheduler entry point. It starts only the
//! adjacent pinned runtime, which verifies its service plan before opening data.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "startup/service_host.rs"]
mod service_host;

#[cfg(windows)]
fn run(stop: Option<windows_sys::Win32::Foundation::HANDLE>) -> std::io::Result<u32> {
    use std::{
        mem::{size_of, size_of_val},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr,
    };
    use windows_sys::Win32::System::{JobObjects::*, Threading::*};
    let exe = std::env::current_exe()?;
    let root = exe.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let runtime = root.join("runtime.exe");
    let mut app: Vec<u16> = runtime.as_os_str().encode_wide().collect();
    app.push(0);
    // Windows paths cannot contain a quote. Quote the full program and root;
    // double any final backslashes before closing the argument (CRT convention).
    fn quote(value: &std::ffi::OsStr) -> Vec<u16> {
        let mut result = vec![34];
        let mut slashes = 0;
        for c in value.encode_wide() {
            if c == 92 {
                slashes += 1;
                continue;
            }
            result.extend(std::iter::repeat_n(
                92,
                if c == 34 { 2 * slashes + 1 } else { slashes },
            ));
            result.push(c);
            slashes = 0;
        }
        result.extend(std::iter::repeat_n(92, slashes * 2));
        result.push(34);
        result
    }
    let mut command = quote(runtime.as_os_str());
    command.extend(" --native-service launch ".encode_utf16());
    command.extend(quote(root.as_os_str()));
    command.push(0);
    let mut cwd: Vec<u16> = root.as_os_str().encode_wide().collect();
    cwd.push(0);
    // Create the job before the child. Suspend the child until it belongs to
    // this job, so a failed/killed launcher cannot leave an unowned server.
    let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw_job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut info: STARTUPINFOW = unsafe { std::mem::zeroed() };
    info.cb = size_of::<STARTUPINFOW>() as u32;
    let mut child: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            app.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_SUSPENDED,
            ptr::null(),
            cwd.as_ptr(),
            &info,
            &mut child,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(child.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(child.hThread) };
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            TerminateProcess(process.as_raw_handle(), 1);
        }
        return Err(error);
    }
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(std::io::Error::last_os_error());
    }
    if let Some(stop) = stop {
        let handles = [process.as_raw_handle(), stop];
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
            0 => {}
            1 => {
                // Ask the verified runtime to close its writers before the job
                // is released. No shell and no inherited console window.
                use std::os::windows::process::CommandExt;
                let mut request = std::process::Command::new(&runtime)
                    .args(["--native-service", "halt"])
                    .arg(root)
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                let until = std::time::Instant::now() + std::time::Duration::from_secs(35);
                loop {
                    if unsafe { WaitForSingleObject(process.as_raw_handle(), 200) } == 0 {
                        break;
                    }
                    if std::time::Instant::now() >= until {
                        let _ = request.kill();
                        let _ = request.wait();
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                    let _ = request.try_wait()?;
                }
                let _ = request.kill();
                let _ = request.wait();
            }
            _ => return Err(std::io::Error::last_os_error()),
        }
    } else if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut code = 1;
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(code)
}

/// Optional desktop-shortcut entry point. It invokes the adjacent native
/// runtime without a console, then exits after the independent tray is ready.
#[cfg(windows)]
fn tray(root: &str, gui: &str) -> std::io::Result<u32> {
    use std::{
        os::windows::process::CommandExt,
        path::Path,
        process::{Command, Stdio},
    };
    if !Path::new(root).is_absolute() || !Path::new(gui).is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let exe = std::env::current_exe()?;
    let folder = exe.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let found: Vec<_> = ["runtime.exe", "rmx.exe", "remodex-connect-native.exe"]
        .into_iter()
        .map(|name| folder.join(name))
        .filter(|p| p.is_file())
        .collect();
    if found.len() != 1 {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let status = Command::new(&found[0])
        .args(["--native-tray", "start", root, gui])
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.code().unwrap_or(1) as u32)
}

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args().skip(1).collect();
        let result = match args.as_slice() {
            [] => run(None),
            [flag, name] if flag == "--service" => service_host::dispatch(name),
            [flag, root, gui] if flag == "--tray" => tray(root, gui),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        };
        std::process::exit(
            result.unwrap_or_else(|error| error.raw_os_error().unwrap_or(1) as u32) as i32,
        );
    }
    #[cfg(not(windows))]
    {
        eprintln!("This startup helper is for Windows only");
        std::process::exit(1);
    }
}
