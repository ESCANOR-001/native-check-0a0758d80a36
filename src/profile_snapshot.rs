//! Bounded copies of a native SQLite database and its WAL for read-only reports.
//! SQLite may recover/create sidecars in our disposable copy, never in the profile.
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(crate) fn regular(m: &Metadata) -> bool {
    if m.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    if !regular(b) || a.len() != b.len() || a.modified().ok() != b.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return false;
        }
    }
    true
}
pub(crate) fn open(path: &Path, limit: u64) -> io::Result<(File, Metadata)> {
    let before = fs::symlink_metadata(path)?;
    if !regular(&before) || !before.is_file() || before.len() > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    if !same(&before, &file.metadata()?) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok((file, before))
}
pub(crate) fn bytes(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let (mut file, before) = open(path, limit)?;
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != before.len() || !same(&before, &fs::symlink_metadata(path)?) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}
pub(crate) struct Snapshot {
    dir: PathBuf,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Snapshot {
    pub fn new(path: &Path) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("remodex-report-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        let result = Self { dir };
        let wal = path.with_file_name(format!(
            "{}-wal",
            path.file_name().unwrap().to_string_lossy()
        ));
        let mut observed = Vec::new();
        for (source, name, limit, required) in [
            (path, "state.sqlite", 64 * 1024 * 1024, true),
            (wal.as_path(), "state.sqlite-wal", 16 * 1024 * 1024, false),
        ] {
            let (mut file, meta) = match open(source, limit) {
                Ok(value) => value,
                Err(e) if !required && e.kind() == io::ErrorKind::NotFound => {
                    observed.push((source, None));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut destination = options.open(result.dir.join(name))?;
            if io::copy(&mut (&mut file).take(limit + 1), &mut destination)? != meta.len() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            observed.push((source, Some(meta)));
        }
        for (source, before) in observed {
            match (before, fs::symlink_metadata(source)) {
                (Some(before), Ok(after)) if same(&before, &after) => {}
                (None, Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(io::ErrorKind::WouldBlock.into()),
            }
        }
        Ok(result)
    }
    pub fn path(&self) -> PathBuf {
        self.dir.join("state.sqlite")
    }
}
