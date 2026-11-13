//! Desktop updates remove the old, content-addressed CLI directory. Resolve
//! that one known managed layout again at validation and at each launch. Never
//! fall back to PATH, another vendor, or a different user-selected executable.
use std::path::{Path, PathBuf};
#[cfg(windows)]
#[path = "codex_packaged.rs"]
mod packaged;
const MISSING: &str = "The configured Codex program is unavailable. Finish updating or reopen Codex Desktop, then retry";

#[cfg(windows)]
pub(crate) fn retained_store_path(selected: &Path) -> Option<PathBuf> {
    packaged::retained_path(selected)
}

pub(crate) fn resolve(selected: &Path) -> Result<PathBuf, String> {
    if !selected.is_absolute() {
        return Err(MISSING.into());
    }
    if selected.is_file() {
        return Ok(selected.to_owned());
    }
    #[cfg(windows)]
    {
        if let Some(path) = packaged::replacement(selected)? {
            return Ok(path);
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return managed_replacement(selected, &PathBuf::from(local));
        }
    }
    Err(MISSING.into())
}

#[cfg(windows)]
fn managed_replacement(selected: &Path, local: &Path) -> Result<PathBuf, String> {
    fn key(path: &Path) -> String {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    fn generation(name: &std::ffi::OsStr) -> bool {
        name.to_str()
            .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    }
    fn regular(path: &Path, directory: bool) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|m| {
            crate::profile_snapshot::regular(&m) && if directory { m.is_dir() } else { m.is_file() }
        })
    }
    let root = local.join("OpenAI/Codex/bin");
    let parent = selected.parent().ok_or(MISSING)?;
    if !local.is_absolute()
        || selected
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
        || !selected
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("codex.exe"))
        || !parent.file_name().is_some_and(generation)
        || parent.parent().is_none_or(|p| key(p) != key(&root))
        || !regular(&root, true)
    {
        return Err(MISSING.into());
    }
    let mut found = None;
    let entries = std::fs::read_dir(&root).map_err(|_| MISSING)?;
    for (index, entry) in entries.enumerate() {
        if index >= 64 {
            return Err(MISSING.into());
        }
        let entry = entry.map_err(|_| MISSING)?;
        if !generation(&entry.file_name()) || !regular(&entry.path(), true) {
            continue;
        }
        let binary = entry.path().join("codex.exe");
        if !regular(&binary, false) {
            continue;
        }
        // Do not guess between two installed channels/builds. Keep the saved
        // selection until the updater completes or the user selects one.
        if found.replace(binary).is_some() {
            return Err("Codex has multiple replacement programs. Finish its update or select the intended executable in Remodex settings".into());
        }
    }
    found.ok_or_else(|| MISSING.into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn desktop_update_recovers_only_the_same_managed_installation() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path();
        let root = local.join("OpenAI/Codex/bin");
        let old = root.join("1111111111111111/codex.exe");
        let replacement = root.join("2222222222222222/codex.exe");
        std::fs::create_dir_all(replacement.parent().unwrap()).unwrap();
        std::fs::write(&replacement, b"replacement fixture").unwrap();
        assert_eq!(managed_replacement(&old, local).unwrap(), replacement);
        assert!(managed_replacement(&local.join("custom/codex.exe"), local).is_err());
        assert!(
            managed_replacement(&local.join("OpenAI/Codex/bin/custom/codex.exe"), local).is_err()
        );
        assert!(managed_replacement(&old, &local.join("another-user")).is_err());
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"existing selection").unwrap();
        assert_eq!(resolve(&old).unwrap(), old);
        assert!(managed_replacement(&root.join("3333333333333333/codex.exe"), local).is_err());
    }
    #[test]
    fn missing_custom_program_does_not_launch_an_unrelated_cli() {
        let temp = tempfile::tempdir().unwrap();
        assert!(resolve(&temp.path().join("missing-custom.exe")).is_err());
        assert!(resolve(Path::new("codex.exe")).is_err());
    }
}
