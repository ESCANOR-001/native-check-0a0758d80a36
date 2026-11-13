use std::{fs, io, path::Path};

/// Create one private storage component. Call separately for the root and its
/// bucket so an existing linked component is rejected before file creation.
pub(crate) fn prepare(path: &Path) -> io::Result<()> {
    if !path
        .parent()
        .is_some_and(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(unix))]
    builder.recursive(false);
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || !crate::profile_snapshot::regular(&meta) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn existing_storage_is_private_but_symlink_targets_are_untouched() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("bucket");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        prepare(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("linked");
        symlink(&dir, &link).unwrap();
        assert!(prepare(&link).is_err());
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
