//! Read-only binding of a native transcript to its Remote Control identity.
//! Never infer a link from a title or accept a caller-supplied shared session.
use crate::claude_host::Identity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

const FILE_LIMIT: u64 = 512 * 1024 * 1024;
const LINE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub identity: Identity,
    pub cwd: PathBuf,
    pub title: String,
}

/// Scan fixed buffers, retaining only metadata. An unfinished trailing record
/// is ignored while the owning CLI appends it; an invalid complete record fails.
pub fn read(profile: &Path, workspace: &Path, native: &str) -> Result<Link, String> {
    let path = crate::branches::transcript(profile, native)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(&path)
        .map_err(|_| "Native Claude history is unavailable")?;
    let meta = file
        .metadata()
        .map_err(|_| "Native Claude history is unavailable")?;
    if !meta.is_file() || meta.len() > FILE_LIMIT {
        return Err("Native Claude history exceeds the scan budget".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("Native Claude history cannot be a reparse point".into());
        }
    }
    let root = workspace
        .canonicalize()
        .map_err(|_| "Workspace unavailable")?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(meta.len()));
    let mut line = Vec::new();
    let mut bridge: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|_| "Cannot read native Claude history")?;
        if chunk.is_empty() {
            break;
        }
        let end = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(chunk.len());
        if line.len() + end > LINE_LIMIT {
            return Err("Native Claude history entry exceeds the scan budget".into());
        }
        line.extend_from_slice(&chunk[..end]);
        reader.consume(end);
        if line.last() != Some(&b'\n') {
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            line.clear();
            continue;
        }
        let row: Value = serde_json::from_slice(&line)
            .map_err(|_| "Native Claude history contains an incomplete record")?;
        line.clear();
        if row["sessionId"] != native || row["isSidechain"] == true {
            continue;
        }
        if row["type"] == "bridge-session" {
            bridge = row["bridgeSessionId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
        }
        if let Some(path) = row["cwd"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 16384)
        {
            cwd = Some(path.into());
        }
        if row["type"] == "custom-title" {
            title = row["customTitle"]
                .as_str()
                .filter(|s| {
                    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                })
                .map(str::to_owned);
        }
    }
    let identity = Identity {
        native_session: native.into(),
        bridge_session: bridge
            .ok_or("This chat has no active Remote Control link; its history is retained")?,
    };
    identity.validate()?;
    let cwd = PathBuf::from(cwd.ok_or("The native chat has no verified project folder")?);
    if !cwd.is_absolute() {
        return Err("The native chat folder must be absolute".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|_| "The native chat folder is unavailable")?;
    if !cwd.is_dir() || !cwd.starts_with(&root) {
        return Err("The native chat is outside this isolated workspace".into());
    }
    Ok(Link {
        identity,
        cwd,
        title: title.unwrap_or_else(|| "Claude Code chat".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn binds_only_matching_main_session_and_last_complete_link() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("profile/projects/p");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = d.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let path = folder.join(format!("{id}.jsonl"));
        let rows = [
            json!({"type":"user","sessionId":id,"cwd":workspace}),
            json!({"type":"bridge-session","sessionId":id,"bridgeSessionId":"cse_previous"}),
            json!({"type":"bridge-session","sessionId":id,"bridgeSessionId":"cse_latest"}),
            json!({"type":"bridge-session","sessionId":other,"bridgeSessionId":"cse_unrelated"}),
            json!({"type":"user","sessionId":id,"isSidechain":true,"cwd":"/different"}),
            json!({"type":"custom-title","sessionId":id,"customTitle":"A test chat"}),
        ];
        let text = rows.iter().map(|v| format!("{v}\n")).collect::<String>();
        std::fs::write(&path, format!("{text}{{\"unfinished\":")).unwrap();
        let link = read(&d.path().join("profile"), &workspace, &id).unwrap();
        assert_eq!(link.identity.bridge_session, "cse_latest");
        assert_eq!(link.title, "A test chat");
        std::fs::write(
            &path,
            format!(
                "{text}{}\n",
                json!({"type":"bridge-session","sessionId":id,"bridgeSessionId":null})
            ),
        )
        .unwrap();
        assert!(read(&d.path().join("profile"), &workspace, &id).is_err());
        std::fs::write(
            &path,
            format!(
                "{text}{}\n",
                json!({"type":"user","sessionId":id,"cwd":d.path()})
            ),
        )
        .unwrap();
        assert!(read(&d.path().join("profile"), &workspace, &id).is_err());
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(d.path().join("other"), &path).unwrap();
            assert!(read(&d.path().join("profile"), &workspace, &id).is_err());
        }
    }
}
