//! Prepare a new native history before replacing the visible history. The source
//! native session is never rolled back in place; SQLite retains the prior revision.
use crate::{
    agent::{Agent, Connection, Event, Launch},
    chat::{AgentConfig, AgentKind, Chat},
    chat_projection::s,
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read},
    time::Duration,
};

pub(crate) struct Target {
    pub turn: String,
    pub native: String,
    pub cut: i64,
    pub previous: Option<String>,
}
pub(crate) struct Prepared {
    pub target: Target,
    pub native: String,
    pub fork: Option<crate::branches::Fork>,
}
impl Store {
    pub(crate) fn native_turn(&self, chat: &str, turn: &str, native: &str) -> rusqlite::Result<()> {
        if turn.is_empty() || native.is_empty() || native.len() > 128 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        self.db.execute("INSERT INTO native_turns(chat,turn,native) VALUES(?1,?2,?3) ON CONFLICT(chat,turn) DO NOTHING",params![chat,turn,native])?;
        let saved: String = self.db.query_row(
            "SELECT native FROM native_turns WHERE chat=?1 AND turn=?2",
            params![chat, turn],
            |r| r.get(0),
        )?;
        if saved != native {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(())
    }
    pub(crate) fn edit_target(&self, chat: &str, target: &str) -> Result<Target, String> {
        let native: Option<String> = self
            .db
            .query_row(
                "SELECT native FROM native_turns WHERE chat=?1 AND turn=?2",
                params![chat, target],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "Cannot read the native turn")?;
        let native = native.ok_or(
            "This older turn has no verified native edit point. Its history was not changed.",
        )?;
        let cut:Option<i64>=self.db.query_row("SELECT MIN(seq) FROM chat_rows WHERE chat=?1 AND kind='messages' AND turn=?2 AND json_extract(data,'$.role')='user'",params![chat,target],|r|r.get(0)).map_err(|_|"Cannot find the prompt")?;
        let cut = cut.ok_or("The prompt to edit is no longer in this chat")?;
        let previous:Option<String>=self.db.query_row("SELECT json_extract(data,'$.createdAt') FROM chat_rows WHERE chat=?1 AND kind='messages' AND seq<?2 AND json_extract(data,'$.role')='user' ORDER BY seq DESC LIMIT 1",params![chat,cut],|r|r.get(0)).optional().map_err(|_|"Cannot read the preceding prompt")?;
        let bytes: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM chat_rows WHERE chat=?1",
                [chat],
                |r| r.get(0),
            )
            .map_err(|_| "Cannot measure the edit revision")?;
        self.check_history(
            (bytes.max(0) as u64)
                .saturating_mul(2)
                .saturating_add(1024 * 1024),
        )?;
        Ok(Target {
            turn: target.into(),
            native,
            cut,
            previous,
        })
    }
}
impl Prepared {
    pub(crate) fn apply(&self, chat: &mut Chat) {
        chat.native_id = self.native.clone();
        chat.native_control = None;
        chat.remote_identity = None;
        chat.fork_from = self.fork.clone();
        chat.resumed = chat.agent == AgentKind::Codex;
        chat.stream_id.clear();
        chat.stream_model.clear();
        chat.pending_message = Value::Null;
        for key in ["messages", "activities", "proposedPlans", "checkpoints"] {
            chat.thread[key] = json!([]);
        }
        chat.thread["latestTurn"] = Value::Null;
        chat.thread["latestUserMessageAt"] = json!(self.target.previous);
        chat.thread["hasPendingApprovals"] = json!(false);
        chat.thread["hasPendingUserInput"] = json!(false);
        chat.thread["hasActionableProposedPlan"] = json!(false);
    }
}
fn claude_parent(
    profile: &std::path::Path,
    session: &str,
    prompt: &str,
) -> Result<Option<String>, String> {
    let path = crate::branches::transcript(profile, session)?;
    let file = std::fs::File::open(path).map_err(|_| "Cannot open the native history")?;
    let len = file
        .metadata()
        .map_err(|_| "Cannot read native history size")?
        .len();
    if len > 512 * 1024 * 1024 {
        return Err("This native history exceeds the bounded edit reader limit".into());
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut row = Vec::new();
    let mut found = None;
    let mut total = 0u64;
    let mut previous: Option<String> = None;
    let mut resume_at: Option<String> = None;
    loop {
        row.clear();
        let n = reader
            .by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut row)
            .map_err(|_| "Cannot read native history")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > len {
            return Err("Native history changed during edit preparation; retry".into());
        }
        if n > 8 * 1024 * 1024 || row.last() != Some(&b'\n') {
            return Err("Native history is incomplete or has an oversized entry".into());
        }
        let value: Value =
            serde_json::from_slice(&row).map_err(|_| "Native history is incomplete")?;
        if value["uuid"] == prompt && value["type"] == "user" && value["isSidechain"] != true {
            if found.is_some() {
                return Err("The native prompt has ambiguous history entries".into());
            }
            if value.get("parentUuid").is_none() {
                return Err("The native prompt is missing its parent link".into());
            }
            let parent = if value["parentUuid"].is_null() {
                None
            } else {
                let p = s(&value["parentUuid"]);
                uuid::Uuid::parse_str(p).map_err(|_| "Invalid native edit point")?;
                if previous.as_deref() != Some(p) || resume_at.is_none() {
                    return Err("The native prompt has no verified preceding assistant message; history was not changed".into());
                }
                resume_at.clone()
            };
            found = Some(parent);
        }
        if value["isSidechain"] != true
            && value["type"] != "progress"
            && value.get("parentUuid").is_some()
            && value["uuid"].is_string()
        {
            resume_at = if value["type"] == "assistant" {
                Some(s(&value["uuid"]).into())
            } else if value["type"] == "attachment"
                && value["attachment"]["type"] == "prompt_snapshot"
                && previous.as_deref() == value["parentUuid"].as_str()
            {
                // CLI bookkeeping may follow the final assistant response. Pin
                // the fork to that response, not the prompt snapshot payload.
                resume_at
            } else {
                None
            };
            previous = Some(s(&value["uuid"]).into());
        }
    }
    if reader
        .get_ref()
        .metadata()
        .map_err(|_| "Native history unavailable")?
        .len()
        != len
    {
        return Err("Native history changed during edit preparation; retry".into());
    }
    found.ok_or_else(|| "The native prompt could not be verified; history was not changed".into())
}
async fn rpc(c: &mut Connection, method: &str, params: Value) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .map_err(|_| "Edit preparation disconnected")?;
    while let Some(Event::Message(v)) = c.events.recv().await {
        if v["id"] == id {
            return v.get("result").cloned().ok_or_else(|| {
                "The CLI rejected edit preparation; the original history is intact".into()
            });
        }
        if let Some((reply, _)) = crate::controls::automatic(&v, true) {
            c.send(&reply)
                .await
                .map_err(|_| "Edit preparation disconnected")?;
        } else if v.get("id").is_some() && v["method"].is_string() {
            c.send(&json!({"id":v["id"],"error":{"code":-32601,"message":"Interactive requests are unavailable during edit preparation"}})).await.map_err(|_|"Edit preparation disconnected")?;
        }
    }
    Err("Edit preparation disconnected; the original history is intact".into())
}
pub(crate) async fn prepare(
    config: AgentConfig,
    chat: Chat,
    target: Target,
) -> Result<Prepared, String> {
    if chat.agent == AgentKind::Claude {
        let source = chat.native_id.clone();
        let prompt = target.native.clone();
        let profile = config.profile;
        let parent = tokio::task::spawn_blocking(move || claude_parent(&profile, &source, &prompt))
            .await
            .map_err(|_| "Native history reader stopped")??;
        if parent.is_none() && target.previous.is_some() {
            return Err("The CLI no longer has the earlier context needed for this edit; history was not changed".into());
        }
        return Ok(Prepared {
            target,
            native: uuid::Uuid::new_v4().to_string(),
            fork: parent.map(|point| crate::branches::Fork {
                session: chat.native_id,
                point,
            }),
        });
    }
    let mut c = config
        .connect(Launch {
            agent: Agent::Codex,
            executable: config.executable.clone(),
            cwd: s(&chat.thread["worktreePath"]).into(),
            env: crate::discovery::environment(AgentKind::Codex, &config.profile),
            extra_args: crate::discovery::isolated_args(AgentKind::Codex),
        })
        .await
        .map_err(|_| "Could not start isolated edit preparation")?;
    let result=tokio::time::timeout(Duration::from_secs(25),async{
        rpc(&mut c,"initialize",json!({"clientInfo":{"name":"remodex_native_edit","version":"0.0.0"},"capabilities":{"experimentalApi":true}})).await?;
        c.send(&json!({"method":"initialized","params":{}})).await.map_err(|_|"Edit preparation disconnected")?;
        let value=rpc(&mut c,"thread/fork",json!({"threadId":chat.native_id,"beforeTurnId":target.native,"excludeTurns":true,"deferGoalContinuation":true,"cwd":chat.thread["worktreePath"],"model":chat.thread["modelSelection"]["model"],"approvalPolicy":"on-request","sandbox":"workspace-write"})).await?;
        let native=s(&value["thread"]["id"]);if native.is_empty()||native.len()>128||native==chat.native_id {return Err("CLI did not create a separate editable history".into());}
        Ok(Prepared{target,native:native.into(),fork:None})
    }).await.unwrap_or_else(|_|Err("Edit preparation timed out; the original history is intact".into()));
    c.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_parent_requires_a_complete_verified_user_entry() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/test");
        std::fs::create_dir_all(&folder).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let prompt = uuid::Uuid::new_v4().to_string();
        let path = folder.join(format!("{session}.jsonl"));
        let mut row = json!({"type":"user","uuid":prompt,"parentUuid":null});
        std::fs::write(&path, format!("{row}\n")).unwrap();
        assert_eq!(claude_parent(dir.path(), &session, &prompt).unwrap(), None);
        row.as_object_mut().unwrap().remove("parentUuid");
        std::fs::write(&path, format!("{row}\n")).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
        row["parentUuid"] = json!(uuid::Uuid::new_v4().to_string());
        std::fs::write(&path, format!("{row}\n{row}\n")).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
        std::fs::write(&path, row.to_string()).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
        row["isSidechain"] = json!(true);
        std::fs::write(&path, format!("{row}\n")).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
    }
    #[test]
    fn edit_accepts_linked_prompt_snapshots_but_not_unrelated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/fixture");
        std::fs::create_dir_all(&folder).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let path = folder.join(format!("{session}.jsonl"));
        let assistant = uuid::Uuid::new_v4().to_string();
        let snapshot = uuid::Uuid::new_v4().to_string();
        let prompt = uuid::Uuid::new_v4().to_string();
        let first = json!({"type":"assistant","uuid":assistant,"parentUuid":null});
        let mut metadata = json!({"type":"attachment","uuid":snapshot,"parentUuid":assistant,"attachment":{"type":"prompt_snapshot"}});
        let user = json!({"type":"user","uuid":prompt,"parentUuid":snapshot});
        std::fs::write(&path, format!("{first}\n{metadata}\n{user}\n")).unwrap();
        assert_eq!(
            claude_parent(dir.path(), &session, &prompt).unwrap(),
            Some(assistant)
        );
        metadata["parentUuid"] = json!(uuid::Uuid::new_v4().to_string());
        std::fs::write(&path, format!("{first}\n{metadata}\n{user}\n")).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
        metadata["parentUuid"] = first["uuid"].clone();
        metadata["attachment"]["type"] = json!("unknown-new-context");
        std::fs::write(&path, format!("{first}\n{metadata}\n{user}\n")).unwrap();
        assert!(claude_parent(dir.path(), &session, &prompt).is_err());
    }
    #[test]
    fn edit_revision_and_pruning_roll_back_when_receipt_commit_fails() {
        let d = tempfile::tempdir().unwrap();
        let mut db = Store::open(d.path()).unwrap();
        db.init_chats().unwrap();
        let state = json!({"agent":"codex","native_id":"original","thread":{"id":"test","messages":[{"id":"first","turnId":"one","role":"user","text":"keep","createdAt":"2026-01-01T00:00:00Z"},{"id":"second","turnId":"two","role":"user","text":"original","createdAt":"2026-01-01T00:01:00Z"}],"activities":[],"proposedPlans":[]}});
        db.save_chat(&state, Some(("used-command", "digest")))
            .unwrap();
        db.native_turn("test", "two", "native-two").unwrap();
        let target = db.edit_target("test", "two").unwrap();
        let mut changed = state.clone();
        changed["native_id"] = json!("replacement");
        changed["thread"]["messages"] = json!([]);
        assert!(db
            .save_edit(&changed, Some(("used-command", "digest")), &target)
            .is_err());
        assert_eq!(db.load_chat("test").unwrap()["native_id"], "original");
        assert_eq!(
            db.load_chat("test").unwrap()["thread"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            db.db
                .query_row("SELECT COUNT(*) FROM chat_revisions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(db.edit_target("test", "two").is_ok());
    }
}
