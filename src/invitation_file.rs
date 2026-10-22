//! Pairing invitations belong to one server lifetime. A crash cannot leave an
//! old invitation advertised as usable on the next launch.
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub struct InvitationFile(PathBuf);
impl InvitationFile {
    /// The caller must own the profile lock before removing prior invitations.
    pub fn clear_previous(directory: &Path) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name
                .strip_prefix("pairing-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_file() || kind.is_symlink() {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    pub fn create(directory: &Path, value: &serde_json::Value) -> io::Result<Self> {
        let path = directory.join(format!("pairing-{}.json", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let guard = Self(path);
        let result = (|| {
            serde_json::to_writer(&mut file, value)?;
            file.flush()?;
            file.sync_all()
        })();
        // Windows cannot remove an open invitation after a write failure.
        drop(file);
        result?;
        Ok(guard)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for InvitationFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifetime_cleanup_only_removes_generated_invitations() {
        let d = tempfile::tempdir().unwrap();
        let kept = d.path().join("pairing-not-a-generated-id.json");
        fs::write(&kept, "keep").unwrap();
        let old = d
            .path()
            .join(format!("pairing-{}.json", uuid::Uuid::new_v4()));
        fs::write(&old, "expired").unwrap();
        InvitationFile::clear_previous(d.path()).unwrap();
        assert!(!old.exists());
        assert!(kept.exists());
        let path;
        {
            let file = InvitationFile::create(d.path(), &serde_json::json!({"test":true})).unwrap();
            path = file.path().to_owned();
            assert!(path.is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        assert!(!path.exists());
        assert!(kept.exists());
    }
}
