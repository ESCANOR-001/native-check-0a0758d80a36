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

pub(crate) async fn is_proven_internal_input(
    profile: PathBuf,
    workspace: PathBuf,
    native: String,
    frame: &Value,
) -> bool {
    let Some(id) = crate::claude_turns::unproven_task_input(frame) else {
        return false;
    };
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        read_for_inputs(&profile, &workspace, &native, std::slice::from_ref(&id))
            .is_ok_and(|link| link.internal_inputs.contains(&id))
    })
    .await
    .unwrap_or(false)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub identity: Identity,
    pub cwd: PathBuf,
    pub title: String,
    /// Local proof for repairing old cursor metadata; never an API field.
    #[serde(skip)]
    pub(crate) internal_inputs: Vec<String>,
}

/// Scan fixed buffers, retaining only metadata. An unfinished trailing record
/// is ignored while the owning CLI appends it; an invalid complete record fails.
pub fn read(profile: &Path, workspace: &Path, native: &str) -> Result<Link, String> {
    read_for_inputs(profile, workspace, native, &[])
}
pub(crate) fn read_for_inputs(
    profile: &Path,
    workspace: &Path,
    native: &str,
    pending: &[String],
) -> Result<Link, String> {
    if pending.len() > 64
        || pending
            .iter()
            .any(|id| id.len() != 36 || uuid::Uuid::parse_str(id).is_err())
    {
        return Err("Invalid Claude pending input metadata".into());
    }
    let mut internal_inputs = Vec::new();
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
        let attachment = &row["attachment"];
        let queued_notice = row["type"] == "attachment"
            && row["renderedRole"] == "system"
            && attachment["type"] == "queued_command"
            && attachment["commandMode"] == "task-notification"
            && attachment["runId"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 256)
            && crate::claude_turns::task_notice_text(&attachment["prompt"]).is_some();
        let input_id = if queued_notice {
            &attachment["source_uuid"]
        } else {
            &row["uuid"]
        };
        if let Some(id) = input_id
            .as_str()
            .filter(|id| pending.iter().any(|p| p == id))
        {
            if (queued_notice || crate::claude_turns::internal_task_input(&row))
                && !internal_inputs.iter().any(|p| p == id)
            {
                internal_inputs.push(id.to_owned());
            }
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
        internal_inputs,
        title: title.unwrap_or_else(|| "Claude Code chat".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn unmarked_sdk_notice_requires_original_native_attachment_proof() {
        let d = tempfile::tempdir().unwrap();
        let profile = d.path().join("profile");
        let folder = profile.join("projects/p");
        std::fs::create_dir_all(&folder).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let text =
            "<task-notification><task-id>t</task-id><status>killed</status></task-notification>";
        let frame = json!({"type":"user","uuid":id,"message":{"role":"user","content":[{"type":"text","text":text}]}});
        let base = json!({"type":"bridge-session","sessionId":native,"cwd":d.path(),"bridgeSessionId":"cse_fixture"});
        let proof = json!({"type":"attachment","sessionId":native,"renderedRole":"system","attachment":{"type":"queued_command","source_uuid":id,"commandMode":"task-notification","runId":"run","prompt":text}});
        for variant in [
            "matching",
            "different-session",
            "different-input",
            "sidechain",
            "human-role",
            "human-origin",
            "no-proof",
        ] {
            let mut row = proof.clone();
            let mut input = frame.clone();
            match variant {
                "different-session" => row["sessionId"] = json!("other"),
                "different-input" => {
                    row["attachment"]["source_uuid"] = json!(uuid::Uuid::new_v4().to_string())
                }
                "sidechain" => row["isSidechain"] = json!(true),
                "human-role" => row["renderedRole"] = json!("user"),
                "human-origin" => input["origin"] = json!({"kind":"human"}),
                "no-proof" => row = Value::Null,
                _ => (),
            }
            std::fs::write(
                folder.join(format!("{native}.jsonl")),
                format!("{base}\n{row}\n"),
            )
            .unwrap();
            let proven =
                is_proven_internal_input(profile.clone(), d.path().into(), native.clone(), &input)
                    .await;
            assert_eq!(proven, variant == "matching", "{variant}");
            if proven {
                input["shouldQuery"] = json!(false);
            }
            assert_eq!(
                crate::claude_turns::input_identity(&input)
                    .unwrap()
                    .is_none(),
                proven
            );
            assert_eq!(
                crate::claude_turns::user_input(&input).unwrap().is_none(),
                proven
            );
        }
    }
    #[test]
    fn internal_input_proof_is_bounded_to_matching_pending_main_session_rows() {
        let d = tempfile::tempdir().unwrap();
        let profile = d.path().join("profile");
        let folder = profile.join("projects/p");
        std::fs::create_dir_all(&folder).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let ids = (0..4)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<Vec<_>>();
        let mut rows = vec![
            serde_json::json!({"type":"bridge-session","sessionId":native,"cwd":d.path(),"bridgeSessionId":"cse_test"}),
        ];
        for (i, id) in ids.iter().enumerate() {
            rows.push(serde_json::json!({"type":"user","uuid":id,"sessionId":if i==1 {"wrong"}else{&native},"isSidechain":i==2,"origin":{"kind":if i==3 {"human"}else{"task-notification"},"producer":"session-task","runId":"run"},"message":{"role":"user","content":"<task-notification><task-id>t</task-id><status>completed</status></task-notification>"}}));
        }
        std::fs::write(
            folder.join(format!("{native}.jsonl")),
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let link = read_for_inputs(&profile, d.path(), &native, &ids).unwrap();
        assert_eq!(link.internal_inputs, vec![ids[0].clone()]);
        assert!(read(&profile, d.path(), &native)
            .unwrap()
            .internal_inputs
            .is_empty());
        assert!(serde_json::to_value(link)
            .unwrap()
            .get("internal_inputs")
            .is_none());
        assert!(read_for_inputs(&profile, d.path(), &native, &vec![ids[0].clone(); 65]).is_err());
    }
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
