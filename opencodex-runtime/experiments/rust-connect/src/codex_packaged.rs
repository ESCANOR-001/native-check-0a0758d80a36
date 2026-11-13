//! Resolve a removed Store-app version through Windows' current-user package
//! registration. Never search PATH or substitute a differently signed package.
use super::MISSING;
use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
    Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily},
};
const FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";

fn package_name(name: &str) -> bool {
    let fields: Vec<_> = name.split('_').collect();
    fields.len() == 5
        && fields[0].eq_ignore_ascii_case("OpenAI.Codex")
        && fields[1].split('.').count() == 4
        && fields[1]
            .split('.')
            .all(|s| !s.is_empty() && s.parse::<u16>().is_ok())
        && matches!(fields[2], "x64" | "arm64" | "x86")
        && fields[3].is_empty()
        && fields[4] == "2p2nqsd0c76g0"
}
fn selected_package(selected: &Path, program_files: &Path) -> bool {
    if selected
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let Some(root) = selected.ancestors().nth(3) else {
        return false;
    };
    let key = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    root.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(package_name)
        && root
            .parent()
            .is_some_and(|p| key(p) == key(&program_files.join("WindowsApps")))
        && key(selected) == key(&root.join("app/resources/codex.exe"))
}
fn choose(roots: impl IntoIterator<Item = PathBuf>) -> Result<PathBuf, String> {
    let mut found = None;
    for root in roots {
        let candidate = root.join("app/resources/codex.exe");
        if !candidate.is_file() {
            continue;
        }
        if found.replace(candidate).is_some() {
            return Err("Codex has multiple registered replacement programs. Finish its Windows Store update, then retry".into());
        }
    }
    found.ok_or_else(|| MISSING.into())
}
pub(super) fn replacement(selected: &Path) -> Result<Option<PathBuf>, String> {
    let Some(program_files) =
        std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles"))
    else {
        return Ok(None);
    };
    if !selected_package(selected, &PathBuf::from(program_files)) {
        return Ok(None);
    }
    choose(registered_roots()?).map(Some)
}
// Windows permits access to this user's registered package but deliberately
// denies opening the WindowsApps container itself. Once an old package has
// been removed, preserve its prior identity using the canonical Program Files
// parent. This is only an ownership digest, never a program to execute.
pub(super) fn retained_path(selected: &Path) -> Option<PathBuf> {
    let base = PathBuf::from(
        std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles"))?,
    );
    if !selected_package(selected, &base) {
        return None;
    }
    let name = selected.ancestors().nth(3)?.file_name()?;
    Some(
        base.canonicalize()
            .ok()?
            .join("WindowsApps")
            .join(name)
            .join("app/resources/codex.exe"),
    )
}
fn registered_roots() -> Result<Vec<PathBuf>, String> {
    let family: Vec<u16> = FAMILY.encode_utf16().chain(Some(0)).collect();
    let (mut count, mut size) = (0, 0);
    // Windows fills pointers into our caller-owned UTF-16 buffer. Bound every
    // allocation and pointer before reading, and fail if an update races us.
    let status = unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            ptr::null_mut(),
            &mut size,
            ptr::null_mut(),
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER
        || !(1..=32).contains(&count)
        || !(1..=32768).contains(&size)
    {
        return Err(MISSING.into());
    }
    let mut names = vec![ptr::null_mut(); count as usize];
    let mut buffer = vec![0u16; size as usize];
    let status = unsafe {
        GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            names.as_mut_ptr(),
            &mut size,
            buffer.as_mut_ptr(),
        )
    };
    if status != ERROR_SUCCESS || count as usize > names.len() {
        return Err(MISSING.into());
    }
    let mut roots = Vec::new();
    for name in names.into_iter().take(count as usize) {
        let start = (name as usize)
            .checked_sub(buffer.as_ptr() as usize)
            .ok_or(MISSING)?;
        if !start.is_multiple_of(2) || start / 2 >= buffer.len() {
            return Err(MISSING.into());
        }
        let chars = &buffer[start / 2..];
        let end = chars.iter().position(|c| *c == 0).ok_or(MISSING)?;
        let full_name = String::from_utf16(&chars[..end]).map_err(|_| MISSING)?;
        if !package_name(&full_name) {
            continue;
        }
        let mut length = 0;
        if unsafe { GetPackagePathByFullName(name, &mut length, ptr::null_mut()) }
            != ERROR_INSUFFICIENT_BUFFER
            || !(2..=32768).contains(&length)
        {
            return Err(MISSING.into());
        }
        let mut path = vec![0u16; length as usize];
        if unsafe { GetPackagePathByFullName(name, &mut length, path.as_mut_ptr()) }
            != ERROR_SUCCESS
            || length as usize > path.len()
        {
            return Err(MISSING.into());
        }
        let end = path.iter().position(|c| *c == 0).ok_or(MISSING)?;
        let root = PathBuf::from(OsString::from_wide(&path[..end]));
        if !root.is_absolute() {
            return Err(MISSING.into());
        }
        roots.push(root);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_official_store_layout_can_follow_updates() {
        let base = Path::new(r"C:\Program Files");
        let old = base.join(
            "WindowsApps/OpenAI.Codex_26.1103.7773.0_x64__2p2nqsd0c76g0/app/resources/codex.exe",
        );
        assert!(selected_package(&old, base));
        for text in [
            "OpenAI.Codex_26.1103.7773.0_x64__different",
            "Other_26.1103.7773.0_x64__2p2nqsd0c76g0",
            "OpenAI.Codex_invalid_x64__2p2nqsd0c76g0",
        ] {
            assert!(!selected_package(
                &base
                    .join("WindowsApps")
                    .join(text)
                    .join("app/resources/codex.exe"),
                base
            ));
        }
        assert!(!selected_package(&old, Path::new(r"D:\Another")));
        assert!(!selected_package(
            &old.parent().unwrap().join("other.exe"),
            base
        ));
    }
    #[test]
    fn updated_store_installation_requires_one_available_program() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("new");
        let two = temp.path().join("another");
        for root in [&one, &two] {
            std::fs::create_dir_all(root.join("app/resources")).unwrap();
            std::fs::write(root.join("app/resources/codex.exe"), b"fixture").unwrap();
        }
        assert_eq!(
            choose([one.clone()]).unwrap(),
            one.join("app/resources/codex.exe")
        );
        assert!(choose([one, two]).is_err());
        assert!(choose([temp.path().join("removed")]).is_err());
    }
}
