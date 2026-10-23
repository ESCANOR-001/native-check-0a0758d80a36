//! One server owns an isolated profile. OS locks release on crash and do not
//! depend on deleting a PID file or guessing whether another process is alive.
use std::{fs, path::Path};

pub struct Lease(fs::File);
impl Drop for Lease {
    fn drop(&mut self) {
        // A forked helper may briefly retain this open file description. The
        // bridge's orderly release must not wait for that helper to exec.
        let _ = self.0.unlock();
    }
}

pub fn acquire(directory: &Path) -> Result<Lease, String> {
    if !directory.is_absolute() {
        return Err("An absolute isolated state directory is required".into());
    }
    fs::create_dir_all(directory).map_err(|_| "Cannot open the private profile")?;
    if !fs::symlink_metadata(directory)
        .is_ok_and(|m| m.is_dir() && crate::profile_snapshot::regular(&m))
    {
        return Err("The private profile directory must not be a link".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect the private profile")?;
    }
    let path = directory.join("server.lock");
    if path
        .symlink_metadata()
        .is_ok_and(|m| !m.is_file() || !crate::profile_snapshot::regular(&m))
    {
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
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot open the private profile lock")?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && crate::profile_snapshot::regular(&m))
    {
        return Err("The private profile lock must be a regular file".into());
    }
    match file.try_lock() {
        Ok(())=>Ok(Lease(file)),
        Err(fs::TryLockError::WouldBlock)=>Err("This private profile already has a running bridge. Use a different test profile or stop its owner first.".into()),
        Err(fs::TryLockError::Error(_))=>Err("Cannot lock this private profile; refusing to run two writers".into()),
    }
}

pub(crate) fn standard_agent_profile(profile: &Path, home: &Path) -> bool {
    if !home.is_absolute() {
        return false;
    }
    let profile = profile
        .canonicalize()
        .unwrap_or_else(|_| profile.to_owned());
    [".codex", ".claude"].iter().any(|name| {
        let normal = home.join(name);
        profile == normal.canonicalize().unwrap_or(normal)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn orderly_release_does_not_wait_for_an_inherited_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let lease = acquire(root.path()).unwrap();
        let inherited = lease.0.try_clone().unwrap();
        assert!(acquire(root.path()).is_err());
        drop(lease);
        let next = acquire(root.path()).unwrap();
        drop(inherited);
        assert!(acquire(root.path()).is_err());
        drop(next);
        assert!(acquire(root.path()).is_ok());
    }
    #[test]
    fn default_profile_check_resolves_native_absolute_paths() {
        let root = tempfile::tempdir().unwrap();
        for name in [".codex", ".claude", "isolated"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        for name in [".codex", ".claude"] {
            let path = root.path().join(name).canonicalize().unwrap();
            assert!(standard_agent_profile(&path, root.path()));
        }
        assert!(!standard_agent_profile(
            &root.path().join("isolated"),
            root.path()
        ));
        assert!(!standard_agent_profile(
            &root.path().join(".codex"),
            Path::new("relative")
        ));
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn linked_state_and_lock_cannot_claim_a_different_profile() {
        let root = tempfile::tempdir().unwrap();
        let actual = root.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let alias = root.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&actual, &alias).unwrap();
        assert!(acquire(&alias).is_err());
        assert!(!actual.join("server.lock").exists());
        let outside = root.path().join("outside-lock");
        fs::write(&outside, "UNCHANGED").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, actual.join("server.lock")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, actual.join("server.lock")).unwrap();
        assert!(acquire(&actual).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "UNCHANGED");
    }
}
