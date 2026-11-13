//! Keep the listening executable at one path per saved installation. Windows
//! application firewall rules match paths, not npm versions. This does not
//! create firewall rules or change Defender settings. The first launch still
//! needs the user's ordinary firewall permission.
use crate::{
    private_directory, profile_lock, profile_snapshot, service_registration, storage_stage,
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub(super) struct Runtime {
    pub executable: PathBuf,
    // Held through all child restarts: a second updater cannot replace this
    // executable, rename its directory, or race another supervisor.
    _image: File,
    _lease: profile_lock::Lease,
    _directory: File,
}

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn plain_file(path: &Path) -> io::Result<()> {
    let file = storage_stage::open(path, false)?;
    if storage_stage::links(&file)? != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

pub(super) fn prepare(state: &Path, source: &Path) -> io::Result<Runtime> {
    if !state.is_absolute()
        || state
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // State is already selected/owned by the installation command. Low-level
    // supervisor callers may supply a new directory, as before.
    if !state.try_exists()? {
        private_directory::prepare(state)?;
    }
    storage_stage::open(state, true)?;
    let root = state.canonicalize()?.join("windows-runtime");
    private_directory::prepare(&root)?;
    let directory = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&root)?;
    if !profile_snapshot::regular(&directory.metadata()?) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let lease = profile_lock::acquire(&root).map_err(failure)?;
    let executable = root.join("remodex-server.exe");
    let expected = service_registration::fingerprint(source, None).map_err(failure)?;
    let current = match fs::symlink_metadata(&executable) {
        Ok(_) => {
            plain_file(&executable)?;
            Some(service_registration::fingerprint(&executable, None).map_err(failure)?)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if current.as_ref() != Some(&expected) {
        // Copy and verify completely before committing. A failed copy leaves
        // the installed file intact. A crash after rename leaves a complete
        // executable that the next start re-verifies; there is no partial file.
        let staging = tempfile::tempdir_in(&root)?;
        let staged = staging.path().join("remodex-server.exe");
        if service_registration::fingerprint(source, Some(&staged)).map_err(failure)? != expected {
            return Err(failure(
                "The selected Remodex build changed while preparing startup",
            ));
        }
        if current.is_some() {
            plain_file(&executable)?;
        }
        fs::rename(&staged, &executable).map_err(|_| failure(
            "The installed Remodex server could not be updated. Stop its previous instance first; keep Windows protection enabled. Saved chats and pairing were retained"
        ))?;
    }
    let image = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&executable)?;
    if !profile_snapshot::regular(&image.metadata()?)
        || storage_stage::links(&image)? != 1
        || service_registration::fingerprint(&executable, None).map_err(failure)? != expected
    {
        return Err(failure(
            "The installed Remodex server could not be verified",
        ));
    }
    Ok(Runtime {
        executable,
        _image: image,
        _lease: lease,
        _directory: directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_and_rollbacks_keep_one_path_and_preserve_saved_data() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("saved state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("chats.fixture"), b"unchanged history").unwrap();
        let one = root.path().join("old-package.exe");
        let two = root.path().join("new-package.exe");
        fs::write(&one, b"first complete build").unwrap();
        fs::write(&two, b"second complete build").unwrap();
        let first = prepare(&state, &one).unwrap();
        let path = first.executable.clone();
        assert!(prepare(&state, &two).is_err());
        assert!(fs::write(&path, b"cannot replace a running image").is_err());
        drop(first);
        for source in [&two, &one] {
            let runtime = prepare(&state, source).unwrap();
            assert_eq!(runtime.executable, path);
            assert_eq!(fs::read(&path).unwrap(), fs::read(source).unwrap());
        }
        assert_eq!(
            fs::read(state.join("chats.fixture")).unwrap(),
            b"unchanged history"
        );
        let before = storage_stage::identity(&File::open(&path).unwrap()).unwrap();
        let _same = prepare(&state, &one).unwrap();
        assert_eq!(
            storage_stage::identity(&File::open(&path).unwrap()).unwrap(),
            before
        );
    }
    #[test]
    fn rejects_linked_destination_and_failed_copy_preserves_previous_image() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        let source = root.path().join("source.exe");
        fs::write(&source, b"complete build").unwrap();
        let runtime = prepare(&state, &source).unwrap();
        let path = runtime.executable.clone();
        drop(runtime);
        assert!(prepare(&state, &root.path().join("missing.exe")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"complete build");
        let outside = root.path().join("outside.exe");
        fs::hard_link(&path, &outside).unwrap();
        assert!(prepare(&state, &source).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"complete build");
    }
}
