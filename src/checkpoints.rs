//! CLI-owned checkpoints. A preview owns its worker for at most sixty seconds;
//! restoration consumes it once and checks the exact files shown to the user.
use crate::{
    agent::{Agent, Connection, Event, Launch},
    chat::{AgentConfig, AgentKind, Chat},
    chat_projection::s,
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

impl Store {
    pub(crate) fn checkpoint(&self, chat: &str, id: &str, label: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO native_checkpoints(chat,id,label) VALUES(?1,?2,?3)",
            params![chat, id, label],
        )?;
        self.db.execute("DELETE FROM native_checkpoints WHERE chat=?1 AND seq NOT IN (SELECT seq FROM native_checkpoints WHERE chat=?1 ORDER BY seq DESC LIMIT 256)",[chat])?;
        Ok(())
    }
    pub(crate) fn checkpoints(&self, chat: &str) -> rusqlite::Result<Value> {
        let rows = self
            .db
            .prepare(
                "SELECT id,label FROM native_checkpoints WHERE chat=?1 ORDER BY seq DESC LIMIT 256",
            )?
            .query_map([chat], |r| {
                Ok(json!({"id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"points":rows,"external":false}))
    }
    pub(crate) fn has_checkpoint(&self, chat: &str, id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT 1 FROM native_checkpoints WHERE chat=?1 AND id=?2",
                params![chat, id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
pub(crate) struct Preview {
    pub chat: String,
    pub client: String,
    pub version: String,
    pub created: Instant,
    point: String,
    cwd: PathBuf,
    files: Vec<String>,
    fingerprint: String,
    connection: Connection,
    events: mpsc::Receiver<Event>,
}
pub(crate) fn version(chat: &Chat) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            json!([
                chat.native_id,
                chat.thread["worktreePath"],
                chat.thread["updatedAt"],
                chat.thread["latestTurn"]
            ])
            .to_string()
            .as_bytes()
        )
    )
}
async fn rpc(
    c: &Connection,
    events: &mut mpsc::Receiver<Event>,
    request: Value,
) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"type":"control_request","request_id":id,"request":request}))
        .await
        .map_err(|_| "Claude checkpoint connection closed")?;
    while let Some(Event::Message(v)) = events.recv().await {
        if v["response"]["request_id"] == id {
            return if v["response"]["subtype"] == "success" {
                Ok(v["response"]["response"].clone())
            } else {
                Err("Claude could not read or restore this checkpoint".into())
            };
        }
    }
    Err("Claude checkpoint connection closed".into())
}
fn files(response: &Value) -> Result<Vec<String>, String> {
    if response["canRewind"] != true {
        return Err("This file checkpoint is no longer available".into());
    }
    let Some(rows) = response["filesChanged"].as_array() else {
        return Ok(vec![]);
    };
    if rows.len() > 100 {
        return Err("Review restores affecting more than 100 files on the PC".into());
    }
    let mut files = rows
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| "Invalid restore path".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    files.dedup();
    Ok(files)
}
fn fingerprint(cwd: &Path, files: &[String]) -> Result<String, String> {
    let root = cwd
        .canonicalize()
        .map_err(|_| "Project folder unavailable")?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    for name in files {
        let path = root.join(name);
        if path.components().any(|c| matches!(c, Component::ParentDir))
            || path == root
            || !path.starts_with(&root)
        {
            return Err("Restore path is outside this project".into());
        }
        let parent = path.parent().ok_or("Invalid restore path")?;
        if parent
            .canonicalize()
            .map_err(|_| "Restore parent folder unavailable")?
            != parent
        {
            return Err("A restore parent is linked or changed; review it on the PC".into());
        }
        hash.update(name.as_bytes());
        hash.update([0]);
        let meta = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                hash.update(b"missing\0");
                continue;
            }
            Err(_) => return Err("Cannot preview a restore file".into()),
        };
        if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
            return Err("A restore file is linked, special or too large to preview".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Err("A restore file has hard links; review it on the PC".into());
            }
        }
        hash.update(b"file\0");
        hash.update(meta.len().to_be_bytes());
        let mut file = fs::File::open(&path).map_err(|_| "Cannot preview a restore file")?;
        let mut buffer = [0; 64 * 1024];
        let mut read = 0u64;
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|_| "Cannot preview a restore file")?;
            if n == 0 {
                break;
            }
            read += n as u64;
            total += n as u64;
            if read > 16 * 1024 * 1024 || total > 128 * 1024 * 1024 {
                return Err("Restore preview exceeds the file-size limit".into());
            }
            hash.update(&buffer[..n]);
        }
        if read != meta.len() {
            return Err("A file changed during the preview; preview again".into());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
async fn hash_files(cwd: PathBuf, files: Vec<String>) -> Result<String, String> {
    tokio::task::spawn_blocking(move || fingerprint(&cwd, &files))
        .await
        .map_err(|_| "File preview stopped")?
}
pub(crate) async fn preview(
    config: AgentConfig,
    chat: Chat,
    client: String,
    point: String,
) -> Result<(Option<Preview>, Value), String> {
    let cwd = PathBuf::from(s(&chat.thread["worktreePath"]));
    let mut args = crate::discovery::isolated_args(AgentKind::Claude);
    // No MCP processes or model prompts are needed for a file checkpoint.
    args.extend([
        "--resume".into(),
        chat.native_id.clone(),
        "--tools".into(),
        "".into(),
    ]);
    let mut env = crate::discovery::environment(AgentKind::Claude, &config.profile);
    env.insert(
        "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".into(),
        "true".into(),
    );
    let mut connection = Connection::start(Launch {
        agent: Agent::Claude,
        executable: config.executable,
        cwd: cwd.clone(),
        env,
        extra_args: args,
    })
    .map_err(|_| "Could not open the checkpoint reader")?;
    let (_, empty) = mpsc::channel(1);
    let mut events = std::mem::replace(&mut connection.events, empty);
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        rpc(&connection, &mut events, json!({"subtype":"initialize"})).await?;
        let response = rpc(
            &connection,
            &mut events,
            json!({"subtype":"rewind_files","user_message_id":point,"dry_run":true}),
        )
        .await?;
        let files = files(&response)?;
        let fingerprint = hash_files(cwd.clone(), files.clone()).await?;
        Ok::<_, String>((response, files, fingerprint))
    })
    .await
    .unwrap_or_else(|_| Err("Checkpoint preview timed out; no files were restored".into()));
    match result {
        Ok((response, files, fingerprint)) if !files.is_empty() => {
            let value = json!({"files":files,"insertions":response["insertions"].as_u64().unwrap_or(0),"deletions":response["deletions"].as_u64().unwrap_or(0)});
            Ok((
                Some(Preview {
                    chat: s(&chat.thread["id"]).into(),
                    client,
                    version: version(&chat),
                    created: Instant::now(),
                    point,
                    cwd,
                    files,
                    fingerprint,
                    connection,
                    events,
                }),
                value,
            ))
        }
        result => {
            drop(events);
            connection.close().await;
            result.map(|_| (None, json!({"files":[],"token":null})))
        }
    }
}
pub(crate) async fn restore(mut preview: Preview) -> Result<Value, String> {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        // Recheck the CLI's affected-file set before touching the filesystem.
        let response = rpc(
            &preview.connection,
            &mut preview.events,
            json!({"subtype":"rewind_files","user_message_id":preview.point,"dry_run":true}),
        )
        .await?;
        if files(&response)? != preview.files
            || hash_files(preview.cwd.clone(), preview.files.clone()).await? != preview.fingerprint
        {
            return Err(
                "Project files changed after the preview. Review a new preview before restoring"
                    .into(),
            );
        }
        let response = rpc(
            &preview.connection,
            &mut preview.events,
            json!({"subtype":"rewind_files","user_message_id":preview.point,"dry_run":false}),
        )
        .await?;
        if response["canRewind"] != true {
            return Err(
                "Claude could not complete the restore; inspect files before retrying".into(),
            );
        }
        Ok(json!({"restored":true,"skippedLinks":response["skippedLinks"].as_u64().unwrap_or(0)}))
    })
    .await
    .unwrap_or_else(|_| {
        Err(
            "Restore confirmation timed out. Inspect the files before requesting another preview"
                .into(),
        )
    });
    drop(preview.events);
    preview.connection.close().await;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_detects_changes_missing_files_and_rejects_escape_and_links() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let files = vec!["a.txt".into()];
        let absent = fingerprint(&root, &files).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        let first = fingerprint(&root, &files).unwrap();
        assert_ne!(absent, first);
        fs::write(root.join("a.txt"), "two").unwrap();
        assert_ne!(first, fingerprint(&root, &files).unwrap());
        assert!(fingerprint(&root, &["../outside".into()]).is_err());
        assert!(fingerprint(&root, &[".".into()]).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("a.txt"), root.join("link")).unwrap();
            assert!(fingerprint(&root, &["link".into()]).is_err());
            fs::hard_link(root.join("a.txt"), root.join("hard")).unwrap();
            assert!(fingerprint(&root, &files).is_err());
        }
    }
}
