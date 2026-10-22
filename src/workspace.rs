//! Bounded filesystem APIs within the explicitly configured native workspace.
//! No shell, persistent search index, symlink traversal or Desktop registry writes.
use crate::chat_projection::s;
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
const LIMIT: usize = 512 * 1024;
fn clean(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::ParentDir))
}
pub fn resolve(base: &Path, path: &str) -> Result<PathBuf, String> {
    if path.len() > 4096 || path.contains('\0') {
        return Err("Invalid workspace path".into());
    }
    let base = base.canonicalize().map_err(|_| "Workspace unavailable")?;
    let p = if path == "~" || path.is_empty() {
        base.clone()
    } else if let Some(rest) = path.strip_prefix("~/") {
        base.join(rest)
    } else {
        base.join(path)
    };
    if !clean(&p) {
        return Err("Path is outside the allowed workspace".into());
    }
    let p = p.canonicalize().map_err(|_| "Path unavailable")?;
    if !p.starts_with(base) {
        return Err("Path is outside the allowed workspace".into());
    }
    Ok(p)
}
pub fn directory(base: &Path, path: &str, create: bool) -> Result<PathBuf, String> {
    if let Ok(p) = resolve(base, path) {
        return if p.is_dir() {
            Ok(p)
        } else {
            Err("Select a folder".into())
        };
    }
    if !create || path.is_empty() || path.len() > 4096 || path.contains('\0') {
        return Err("Folder unavailable".into());
    }
    let p = base.join(path);
    if !clean(&p) {
        return Err("Invalid folder path".into());
    }
    // Each missing level is created only after its parent is canonicalized.
    let parent = p.parent().ok_or("Invalid folder path")?;
    let parent = directory(base, parent.to_str().ok_or("Invalid folder path")?, true)?;
    let target = parent.join(p.file_name().ok_or("Invalid folder name")?);
    match fs::create_dir(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("Cannot create folder".into()),
    }
    resolve(base, target.to_str().ok_or("Invalid folder path")?)
}
fn skipped(name: &str) -> bool {
    [
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        ".gradle",
        ".cache",
        ".npm",
        ".bun",
        "__pycache__",
        "target",
    ]
    .contains(&name)
}
fn walk(
    root: &Path,
    query: Option<&str>,
    recursive: bool,
    limit: usize,
) -> Result<(Vec<(PathBuf, bool)>, bool), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pending = VecDeque::from([root.to_path_buf()]);
    let mut rows = vec![];
    let mut visited = 0;
    let mut truncated = false;
    let query = query.map(str::to_lowercase);
    while let Some(dir) = pending.pop_front() {
        let children = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) if dir != root => continue,
            Err(_) => return Err("Cannot read folder".into()),
        };
        for child in children.flatten() {
            visited += 1;
            if visited > 20_000 || Instant::now() > deadline || rows.len() >= limit {
                truncated = true;
                break;
            }
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
                continue;
            }
            let path = child.path();
            let name = child.file_name().to_string_lossy().into_owned();
            if skipped(&name) {
                continue;
            }
            if query.as_ref().is_none_or(|q| {
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(q)
            }) {
                rows.push((path.clone(), kind.is_dir()));
            }
            if recursive && kind.is_dir() {
                pending.push_back(path);
            }
        }
        if truncated {
            break;
        }
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok((rows, truncated))
}
pub fn browse(base: &Path, p: &Value) -> Result<Value, String> {
    let raw = s(&p["partialPath"]);
    let query = p["query"].as_str();
    if query.is_some_and(|q| q.trim().is_empty() || q.len() > 256) {
        return Err("Enter a search of 1 to 256 characters".into());
    }
    let path = resolve(base, raw)?;
    let path = if path.is_file() {
        path.parent().ok_or("No parent folder")?.to_path_buf()
    } else {
        path
    };
    let (rows, truncated) = walk(&path, query, query.is_some(), 200)?;
    let entries:Vec<_>=rows.into_iter().map(|(path,dir)|json!({"name":path.file_name().unwrap_or_default().to_string_lossy(),"fullPath":path,"kind":if dir{"directory"}else{"file"}})).collect();
    let mut result =
        json!({"parentPath":path,"entries":entries,"truncated":truncated,"resolvedPath":true});
    if let Some(q) = query {
        result["searchQuery"] = json!(q);
    }
    Ok(result)
}
pub fn entries(base: &Path, p: &Value, search: bool) -> Result<Value, String> {
    let root = resolve(base, s(&p["cwd"]))?;
    let query = if search {
        let q = s(&p["query"]);
        if q.trim().is_empty() || q.len() > 256 {
            return Err("Invalid search".into());
        }
        Some(q)
    } else {
        None
    };
    let limit = p["limit"].as_u64().unwrap_or(200).clamp(1, 200) as usize;
    let (rows, mut truncated) = walk(&root, query, true, limit)?;
    let mut entries = vec![];
    let mut seen = HashSet::new();
    for (path, dir) in rows {
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| "Path outside workspace")?;
        let parts: Vec<_> = relative.components().collect();
        for n in 1..=parts.len() {
            let path: PathBuf = parts[..n].iter().collect();
            let path = path.to_string_lossy().replace('\\', "/");
            if seen.contains(&path) {
                continue;
            }
            if entries.len() >= limit {
                truncated = true;
                break;
            }
            seen.insert(path.clone());
            entries
                .push(json!({"path":path,"kind":if n<parts.len()||dir{"directory"}else{"file"}}));
        }
    }
    Ok(json!({"entries":entries,"truncated":truncated}))
}
pub fn read(base: &Path, p: &Value) -> Result<Value, String> {
    read_limited(base, p, LIMIT)
}
pub fn read_chat(workspace: &Path, path: &str) -> Result<Value, String> {
    let mut result = read_limited(workspace, &json!({"cwd":workspace,"path":path}), 256 * 1024)?;
    // Chat previews have their own compact wire shape. Do not send the same
    // bounded file body twice as both `content` and `contents`.
    result.as_object_mut().unwrap().remove("contents");
    Ok(result)
}
fn read_limited(base: &Path, p: &Value, limit: usize) -> Result<Value, String> {
    let root = resolve(base, s(&p["cwd"]))?;
    let requested = p["relativePath"]
        .as_str()
        .or_else(|| p["path"].as_str())
        .ok_or("Select a text file")?;
    let path = resolve(&root, requested)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "Cannot open file")?;
    let meta = file.metadata().map_err(|_| "Cannot inspect file")?;
    if !meta.is_file() {
        return Err("Select a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read file")?;
    if bytes.contains(&0) {
        return Err("Binary file cannot be shown as text".into());
    }
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let content = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(e) if truncated && e.error_len().is_none() => {
            std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap()
        }
        Err(_) => return Err("File is not UTF-8 text".into()),
    };
    Ok(
        json!({"relativePath":requested,"path":requested,"contents":content,"content":content,"byteLength":meta.len(),"sizeBytes":meta.len(),"truncated":truncated}),
    )
}
pub fn write(base: &Path, p: &Value) -> Result<Value, String> {
    let root = resolve(base, s(&p["cwd"]))?;
    let name = s(&p["relativePath"]);
    let text = p["contents"].as_str().ok_or("Text required")?;
    if name.is_empty()
        || name.len() > 512
        || text.len() > LIMIT
        || Path::new(name).is_absolute()
        || !clean(Path::new(name))
    {
        return Err("Invalid file path or content exceeds 512 KiB".into());
    }
    let destination = root.join(name);
    let parent = resolve(
        &root,
        destination
            .parent()
            .and_then(Path::to_str)
            .ok_or("Invalid path")?,
    )?;
    if fs::symlink_metadata(&destination).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
    {
        return Err("Cannot replace a symlink or special file".into());
    }
    let tmp = parent.join(format!(".remodex-write-{}", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut f = options.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        if let Ok(metadata) = fs::metadata(&destination) {
            f.set_permissions(metadata.permissions())?;
        }
        f.sync_all()?;
        fs::rename(&tmp, &destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
        return Err("Cannot save file".into());
    }
    Ok(json!({"relativePath":name}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_and_read_are_bounded_and_confined() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/note.txt"), "hello").unwrap();
        let p = json!({"partialPath":d.path(),"query":"note"});
        let b = browse(d.path(), &p).unwrap();
        assert_eq!(b["entries"][0]["kind"], "file");
        let e = entries(d.path(), &json!({"cwd":d.path(),"query":"note"}), true).unwrap();
        assert_eq!(e["entries"].as_array().unwrap().len(), 2);
        assert_eq!(
            read(
                d.path(),
                &json!({"cwd":d.path(),"relativePath":"sub/note.txt"})
            )
            .unwrap()["contents"],
            "hello"
        );
        assert!(resolve(d.path(), "../secret").is_err());
        assert!(directory(d.path(), "../escape", true).is_err());
    }
    #[test]
    fn create_write_and_large_text_do_not_hold_entire_file() {
        let d = tempfile::tempdir().unwrap();
        let sub = directory(d.path(), "a/b", true).unwrap();
        write(
            d.path(),
            &json!({"cwd":sub,"relativePath":"hello.txt","contents":"hello"}),
        )
        .unwrap();
        fs::write(sub.join("big.txt"), vec![b'x'; LIMIT * 3]).unwrap();
        let r = read(d.path(), &json!({"cwd":sub,"relativePath":"big.txt"})).unwrap();
        assert_eq!(s(&r["contents"]).len(), LIMIT);
        assert_eq!(r["truncated"], true);
    }
    #[cfg(unix)]
    #[test]
    fn pipe_preview_fails_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_chat(d.path(), "pipe")
            .unwrap_err()
            .contains("regular file"));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(out.path(), d.path().join("outside")).unwrap();
        assert!(directory(d.path(), "outside/new", true).is_err());
        assert!(read(
            d.path(),
            &json!({"cwd":d.path(),"relativePath":"outside/secret"})
        )
        .is_err());
        assert!(!out.path().join("new").exists());
    }
}
