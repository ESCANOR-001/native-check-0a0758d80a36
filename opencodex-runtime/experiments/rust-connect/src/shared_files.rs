//! Exact files explicitly linked by the assistant belong to that chat even
//! when the agent worked outside its initial cwd. Never expose the directory,
//! arbitrary tool text, user prompts, or a link from a different chat.
use crate::store::Store;
use percent_encoding::percent_decode_str;
use pulldown_cmark::{Event, Parser, Tag};
use rusqlite::params;
use std::path::{Component, Path, PathBuf};

fn local_spelling(value: &str) -> Option<&str> {
    #[cfg(windows)]
    if let Some(drive) = value
        .strip_prefix(r"\\?\")
        .or_else(|| value.strip_prefix("//?/"))
    {
        // A canonical disk path is still the same local file. Its prefix's
        // question mark is not a URL query; other device namespaces stay out.
        let bytes = drive.as_bytes();
        return (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
        .then_some(drive);
    }
    Some(value)
}

fn local_path(workspace: &Path, target: &str) -> Option<PathBuf> {
    if target.is_empty() || target.len() > 4096 || target.contains(['\0', '\n', '\r']) {
        return None;
    }
    let decoded = if target.starts_with("file://") {
        let url = reqwest::Url::parse(target).ok()?;
        if url.host_str().is_some_and(|host| host != "localhost") {
            return None;
        }
        std::borrow::Cow::Owned(url.to_file_path().ok()?.to_str()?.to_owned())
    } else {
        // Decode after separating URL suffixes so an encoded # in a filename
        // does not turn into a fragment and silently truncate the exact link.
        let value = local_spelling(target)?.split(['?', '#']).next()?;
        percent_decode_str(value).decode_utf8().ok()?
    };
    let value = local_spelling(&decoded)?;
    // Match the Android link parser's optional :line[:column] suffix.
    let mut value = value;
    for _ in 0..2 {
        if let Some((head, tail)) = value.rsplit_once(':') {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                value = head;
            }
        }
    }
    // A URI is not a local file. Retain Windows drive paths on Windows.
    if value.starts_with("//")
        || value.contains("://")
        || value.split_once(':').is_some_and(|(scheme, _)| {
            !(cfg!(windows) && scheme.len() == 1 && scheme.as_bytes()[0].is_ascii_alphabetic())
                && !scheme.contains(['/', '\\'])
        })
    {
        return None;
    }
    let value = if cfg!(windows) {
        value.replace('/', "\\")
    } else {
        value.to_owned()
    };
    let mut normalized = PathBuf::new();
    let workspace = Path::new(local_spelling(workspace.to_str()?)?);
    for component in workspace.join(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.is_absolute().then_some(normalized)
}

fn links_file(text: &str, workspace: &Path, requested: &Path) -> bool {
    Parser::new(text).any(|event| match event {
        Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
            local_path(workspace, &dest_url).is_some_and(|linked| {
                if linked == requested {
                    return true;
                }
                #[cfg(windows)]
                {
                    // Windows can report one local directory as both RUNNER~1
                    // and runneradmin. Resolve both spellings to the same exact
                    // file; never grant another file merely for sharing a name.
                    // Only disk paths enter this fallback, avoiding network or
                    // device namespace lookups while inspecting message links.
                    let disk = |path: &Path| {
                        matches!(path.components().next(),
                        Some(Component::Prefix(prefix)) if matches!(prefix.kind(),
                            std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
                    };
                    if disk(&linked) && disk(requested) {
                        return linked
                            .canonicalize()
                            .ok()
                            .zip(requested.canonicalize().ok())
                            .is_some_and(|(linked, requested)| linked == requested);
                    }
                }
                false
            })
        }
        _ => false,
    })
}

impl Store {
    pub(crate) fn resolve_shared_file(
        &self,
        chat: &str,
        workspace: &Path,
        requested: &str,
    ) -> Result<Option<PathBuf>, String> {
        let Some(path) = local_path(workspace, requested) else {
            return Ok(None);
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(None);
        };
        // Use the durable rows, not the small loaded transcript window. Filter
        // candidate messages in SQLite and parse one at a time: no whole-chat
        // hydration, asset bytes, extra cache, or negative cache on a miss.
        let mut statement = self
            .db
            .prepare_cached(
                "SELECT json_extract(data,'$.text') FROM chat_rows
             WHERE chat=?1 AND kind='messages' AND json_extract(data,'$.role')='assistant'
             AND (instr(json_extract(data,'$.text'),?2)>0
                  OR instr(json_extract(data,'$.text'),'%')>0
                  OR instr(json_extract(data,'$.text'),'&')>0
                  OR instr(json_extract(data,'$.text'),char(92))>0)
             ORDER BY seq DESC",
            )
            .map_err(|_| "Shared file history unavailable")?;
        let rows = statement
            .query_map(params![chat, name], |row| row.get::<_, String>(0))
            .map_err(|_| "Shared file history unavailable")?;
        for row in rows {
            let text = row.map_err(|_| "Shared file history unavailable")?;
            if links_file(&text, workspace, &path) {
                let file = path.canonicalize().map_err(|_| {
                    "Shared file is unavailable on the PC. Check that its drive is connected, then retry."
                })?;
                if !file.metadata().is_ok_and(|m| m.is_file()) {
                    return Err("The shared link is not a regular file".into());
                }
                return Ok(Some(file));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(store: &Store, chat: &str, role: &str, text: &str) {
        store
            .db
            .execute(
                "INSERT INTO chat_rows(chat,kind,id,data) VALUES(?1,'messages',?2,?3)",
                params![
                    chat,
                    uuid::Uuid::new_v4().to_string(),
                    json!({"role":role,"text":text}).to_string()
                ],
            )
            .unwrap();
    }

    #[test]
    fn only_explicit_assistant_links_grant_exact_chat_scoped_access() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let workspace = dir.path().join("missing-original-workspace");
        let file = dir.path().join("comparison (final).png");
        std::fs::write(&file, b"test").unwrap();
        // Markdown treats a backslash before punctuation as an escape.
        // Use a valid link spelling on Windows, just as a file URL would.
        let link = file.to_str().unwrap().replace('\\', "/");
        let target = link.as_str();
        message(&store, "chat", "user", &format!("![user]({target})"));
        message(
            &store,
            "chat",
            "assistant",
            &format!("`![code](<{target}>)`\n\n```md\n![code](<{target}>)\n```\n\nPlain {target}"),
        );
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, target)
                .unwrap(),
            None
        );
        message(
            &store,
            "other",
            "assistant",
            &format!("![other](<{target}>)"),
        );
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, target)
                .unwrap(),
            None
        );
        message(
            &store,
            "chat",
            "assistant",
            &format!("![verification][image]\n\n[image]: <{target}> \"Reference\""),
        );
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, target)
                .unwrap(),
            Some(file.canonicalize().unwrap())
        );
        assert_eq!(
            store
                .resolve_shared_file(
                    "chat",
                    &workspace,
                    file.canonicalize().unwrap().to_str().unwrap()
                )
                .unwrap(),
            Some(file.canonicalize().unwrap()),
            "link={target:?}; parsed link={:?}; parsed canonical request={:?}",
            local_path(&workspace, target),
            local_path(&workspace, file.canonicalize().unwrap().to_str().unwrap())
        );
        let sibling = dir.path().join("private.txt");
        std::fs::write(&sibling, b"private").unwrap();
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, sibling.to_str().unwrap())
                .unwrap(),
            None
        );
        let other_directory = dir.path().join("not-shared");
        std::fs::create_dir(&other_directory).unwrap();
        let same_name = other_directory.join(file.file_name().unwrap());
        std::fs::write(&same_name, b"private").unwrap();
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, same_name.to_str().unwrap())
                .unwrap(),
            None,
            "matching filenames must not authorize a different file"
        );
    }

    #[test]
    fn encoded_paths_text_links_and_late_files_work_without_negative_caching() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let workspace = dir.path().join("workspace");
        let file = dir.path().join("report + notes.txt");
        let mut url = reqwest::Url::from_file_path(&file).unwrap();
        url.set_fragment(Some("L12"));
        let url = url.to_string();
        message(&store, "chat", "assistant", &format!("[Report]({url})"));
        assert!(store
            .resolve_shared_file("chat", &workspace, file.to_str().unwrap())
            .is_err());
        std::fs::write(&file, b"ready").unwrap();
        assert_eq!(
            store
                .resolve_shared_file("chat", &workspace, file.to_str().unwrap())
                .unwrap(),
            Some(file.canonicalize().unwrap())
        );
        assert_eq!(
            store.resolve_shared_file("chat", &workspace, &url).unwrap(),
            Some(file.canonicalize().unwrap())
        );
        assert!(!links_file(
            "[remote](https://host/report.txt)",
            &workspace,
            &workspace.join("report.txt")
        ));
        assert!(!links_file("[folder](../)", &workspace, &file));
        assert!(local_path(&workspace, "file://other-computer/share/report.txt").is_none());
    }
}
