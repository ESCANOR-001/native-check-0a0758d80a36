//! One server owns an isolated profile. OS locks release on crash and do not
//! depend on deleting a PID file or guessing whether another process is alive.
use std::{fs, path::Path};

pub fn acquire(directory: &Path) -> Result<fs::File, String> {
    if !directory.is_absolute() {
        return Err("An absolute isolated state directory is required".into());
    }
    fs::create_dir_all(directory).map_err(|_| "Cannot open the private profile")?;
    if !fs::symlink_metadata(directory).is_ok_and(|m| m.is_dir()) {
        return Err("The private profile directory must not be a link".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect the private profile")?;
    }
    let path = directory.join("server.lock");
    if path.symlink_metadata().is_ok_and(|m| !m.is_file()) {
        return Err("The private profile lock must be a regular file".into());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot open the private profile lock")?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("The private profile lock must be a regular file".into());
    }
    match file.try_lock() {
        Ok(())=>Ok(file),
        Err(fs::TryLockError::WouldBlock)=>Err("This private profile already has a running bridge. Use a different test profile or stop its owner first.".into()),
        Err(fs::TryLockError::Error(_))=>Err("Cannot lock this private profile; refusing to run two writers".into()),
    }
}
