//! Windows SCM entry point. Keep the existing service's account and startup
//! policy; only the program it launches changes during migration.
use std::{io, ptr, sync::Mutex};
use windows_sys::Win32::{
    Foundation::*,
    System::{Services::*, Threading::*},
};

struct Control {
    status: usize,
    stop: usize,
    stopping: bool,
}
static CONTROL: Mutex<Option<Control>> = Mutex::new(None);

fn report(handle: usize, state: u32, code: u32) {
    let pending = matches!(state, SERVICE_START_PENDING | SERVICE_STOP_PENDING);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: u32::from(pending),
        dwWaitHint: if pending { 45000 } else { 0 },
    };
    unsafe {
        SetServiceStatus(handle as SERVICE_STATUS_HANDLE, &status);
    }
}

unsafe extern "system" fn control(
    code: u32,
    _: u32,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> u32 {
    let Ok(mut state) = CONTROL.lock() else {
        return ERROR_INVALID_HANDLE;
    };
    let Some(state) = state.as_mut() else {
        return ERROR_INVALID_HANDLE;
    };
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            if !state.stopping {
                state.stopping = true;
                report(state.status, SERVICE_STOP_PENDING, 0);
                unsafe {
                    SetEvent(state.stop as HANDLE);
                }
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(argc: u32, argv: *mut *mut u16) {
    if argc == 0 || argv.is_null() {
        return;
    }
    let handle = unsafe { RegisterServiceCtrlHandlerExW(*argv, Some(control), ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    report(handle as usize, SERVICE_START_PENDING, 0);
    let stop = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if stop.is_null() {
        report(handle as usize, SERVICE_STOPPED, unsafe { GetLastError() });
        return;
    }
    if let Ok(mut state) = CONTROL.lock() {
        *state = Some(Control {
            status: handle as usize,
            stop: stop as usize,
            stopping: false,
        });
        report(handle as usize, SERVICE_RUNNING, 0);
    } else {
        unsafe {
            CloseHandle(stop);
        }
        report(handle as usize, SERVICE_STOPPED, ERROR_INVALID_HANDLE);
        return;
    }
    let result = super::run(Some(stop));
    // Serialize final status with the handler. No callback can use the event
    // after it is closed or report STOP_PENDING after STOPPED.
    if let Ok(mut state) = CONTROL.lock() {
        *state = None;
        unsafe {
            CloseHandle(stop);
        }
        let code = result.unwrap_or_else(|e| e.raw_os_error().unwrap_or(1) as u32);
        report(handle as usize, SERVICE_STOPPED, code);
    }
}

pub(super) fn dispatch(name: &str) -> io::Result<u32> {
    if !matches!(name, "remodex-connect-native" | "opencodex-proxy-native") {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let entries = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(entries.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(0)
}
