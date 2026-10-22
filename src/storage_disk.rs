use std::path::Path;
fn failure() -> String {
    "Cannot read available disk space".into()
}
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // statvfs fields differ across Unix targets.
pub(crate) fn available(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| failure())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // Both pointers remain valid for this synchronous call. Read only on success.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(failure());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}
#[cfg(windows)]
pub(crate) fn available(path: &Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0;
    // NUL-terminated path and the output pointer live through the call.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(failure());
    }
    Ok(free)
}
