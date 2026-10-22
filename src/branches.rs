//! A branch pins the source's last durable native chain entry. The first prompt
//! asks the CLI to fork at that point, so later source messages cannot leak in.
use crate::{
    auth_service::AuthService,
    chat::Chat,
    chat_projection::{now, s},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Fork {
    pub session: String,
    pub point: String,
}

pub(crate) fn transcript(profile: &Path, id: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(id).map_err(|_| "Invalid native session identity")?;
    let root = profile
        .join("projects")
        .canonicalize()
        .map_err(|_| "Native session history is unavailable")?;
    if !root.starts_with(
        profile
            .canonicalize()
            .map_err(|_| "Test profile unavailable")?,
    ) {
        return Err("The session directory is linked outside the test profile".into());
    }
    let mut found = None;
    let mut count = 0;
    for entry in fs::read_dir(&root).map_err(|_| "Native session history is unavailable")? {
        let entry = entry.map_err(|_| "Cannot inspect native session directory")?;
        count += 1;
        if count > 10000 {
            return Err("Native session directory limit exceeded".into());
        }
        if !entry
            .file_type()
            .map_err(|_| "Cannot inspect session folder")?
            .is_dir()
        {
            continue;
        }
        let path = entry.path().join(format!("{id}.jsonl"));
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.is_file() {
            return Err("The source transcript is not a regular file".into());
        }
        if found.replace(path).is_some() {
            return Err("The source session has ambiguous history locations".into());
        }
    }
    found.ok_or_else(|| {
        "The source native history is unavailable; no empty branch was created".into()
    })
}
fn point(profile: &Path, id: &str) -> Result<String, String> {
    let path = transcript(profile, id)?;
    let mut file = fs::File::open(path).map_err(|_| "Cannot read the native session")?;
    let end = file
        .metadata()
        .map_err(|_| "Cannot read session size")?
        .len();
    // Walk the tail in fixed chunks; large histories do not load into memory.
    let mut pos = end;
    let mut pending = Vec::new();
    let mut read_total = 0usize;
    while pos > 0 {
        let n = pos.min(64 * 1024) as usize;
        pos -= n as u64;
        read_total += n;
        if read_total > 32 * 1024 * 1024 {
            return Err("No branch point found within the bounded history tail".into());
        }
        file.seek(SeekFrom::Start(pos))
            .map_err(|_| "Cannot read native history")?;
        let mut block = vec![0; n];
        file.read_exact(&mut block)
            .map_err(|_| "Native history changed; retry branching")?;
        block.extend_from_slice(&pending);
        let first = if pos == 0 {
            0
        } else {
            block
                .iter()
                .position(|c| *c == b'\n')
                .map(|i| i + 1)
                .unwrap_or(block.len())
        };
        for line in block[first..]
            .split(|c| *c == b'\n')
            .rev()
            .filter(|l| !l.is_empty())
        {
            let row: Value = serde_json::from_slice(line)
                .map_err(|_| "The native history tail is incomplete; retry branching")?;
            if row["isSidechain"] != true
                && row["type"] != "progress"
                && row.get("parentUuid").is_some()
                && row["sessionId"] == id
            {
                let uuid = s(&row["uuid"]);
                uuid::Uuid::parse_str(uuid).map_err(|_| "Invalid native branch point")?;
                if file
                    .metadata()
                    .map_err(|_| "Native history unavailable")?
                    .len()
                    != end
                {
                    return Err("Native history changed; retry branching".into());
                }
                return Ok(uuid.into());
            }
        }
        pending = block[..first].to_vec();
        if pending.len() > 8 * 1024 * 1024 {
            return Err("A native transcript entry exceeds the branch reader limit".into());
        }
    }
    Err("Send a message in this chat before branching it".into())
}
pub(crate) async fn create(
    db: AuthService,
    profile: PathBuf,
    source: Chat,
    key: String,
    digest: String,
) -> Result<(Chat, Value), String> {
    let native = source.native_id.clone();
    let point = tokio::task::spawn_blocking(move || point(&profile, &native))
        .await
        .map_err(|_| "Branch reader stopped")??;
    let source_id = s(&source.thread["id"]).to_owned();
    let mut chat = source;
    let id = uuid::Uuid::new_v4().to_string();
    let time = now();
    chat.fork_from = Some(Fork {
        session: chat.native_id.clone(),
        point,
    });
    chat.native_id = uuid::Uuid::new_v4().to_string();
    chat.remote_identity = None;
    chat.resumed = false;
    chat.external_claude = false;
    chat.native_control = None;
    chat.queue.clear();
    chat.paused = false;
    chat.stream_id.clear();
    chat.stream_model.clear();
    chat.pending_message = Value::Null;
    let t = &mut chat.thread;
    t["id"] = json!(id);
    t["title"] = json!(format!(
        "{} (branch)",
        s(&t["title"]).chars().take(220).collect::<String>()
    ));
    for field in ["messages", "activities", "proposedPlans", "checkpoints"] {
        t[field] = json!([]);
    }
    for field in [
        "hasPendingApprovals",
        "hasPendingUserInput",
        "hasActionableProposedPlan",
    ] {
        t[field] = json!(false);
    }
    for field in ["archivedAt", "deletedAt"] {
        t[field] = Value::Null;
    }
    for field in ["historyPage", "nativeSessionId"] {
        t.as_object_mut().unwrap().remove(field);
    }
    t["createdAt"] = json!(time);
    t["updatedAt"] = json!(time);
    t["runtimeMode"] = json!("approval-required");
    t["interactionMode"] = json!("default");
    t["session"] = json!({"threadId":id,"providerName":"claude","providerInstanceId":"claude-code","status":"idle","statusConfidence":"confirmed","runtimeMode":"approval-required","activeTurnId":null,"lastError":null,"updatedAt":time});
    let result = json!({"threadId":id});
    let value = result.clone();
    let state = serde_json::to_value(&chat).map_err(|_| "Cannot save branch")?;
    db.call(move |a| {
        a.store
            .save_branch(&state, &key, &digest, &value, &source_id)
    })
    .await?
    .map_err(|_| "Could not save the branch and its history")?;
    Ok((chat, result))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tail_reader_handles_chunk_boundaries_and_ignores_metadata_and_sidechains() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("projects/p");
        fs::create_dir_all(&folder).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let kept = uuid::Uuid::new_v4().to_string();
        let rows = [
            json!({"type":"assistant","parentUuid":null,"uuid":kept,"sessionId":id,"message":{"content":"x".repeat(80*1024)}}),
            json!({"type":"assistant","parentUuid":null,"uuid":uuid::Uuid::new_v4().to_string(),"sessionId":id,"isSidechain":true}),
            json!({"type":"cost-state","data":"x".repeat(64*1024)}),
        ];
        fs::write(
            folder.join(format!("{id}.jsonl")),
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        assert_eq!(point(d.path(), &id).unwrap(), kept);
    }
}
