//! Same-session conversation revert. A durable revision precedes the native
//! mutation. An uncertain delivery is inspected, never automatically repeated.
use crate::{
    agent::{Agent, Connection, Event, Launch},
    auth_service::AuthService,
    chat::{AgentConfig, Chat},
    chat_projection::{now, s},
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) const PENDING: &str = "Conversation revert is not confirmed. Retry Revert to check its result; it will not be sent twice. Saved history is retained.";
const STORE_ERROR: &str = "Cannot save the conversation revert; saved history is retained";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub key: String,
    pub digest: String,
    pub chat: String,
    pub native: String,
    pub target: String,
    pub public: String,
    pub cut: i64,
    pub previous: Option<String>,
    pub revision: String,
}
pub(crate) enum Preparation {
    Complete,
    Run(Box<Intent>, bool),
}
pub(crate) enum Outcome {
    Applied,
    Rejected(String),
    Uncertain,
}
impl Store {
    pub(crate) fn init_reverts(&self) -> rusqlite::Result<()> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS native_reverts(id TEXT PRIMARY KEY,chat TEXT NOT NULL,digest TEXT NOT NULL,phase TEXT NOT NULL,data TEXT NOT NULL,error TEXT,removed TEXT); CREATE UNIQUE INDEX IF NOT EXISTS native_reverts_pending ON native_reverts(chat) WHERE phase='pending';")
    }
    pub(crate) fn pending_reverts(&self) -> rusqlite::Result<Vec<String>> {
        self.db
            .prepare("SELECT chat FROM native_reverts WHERE phase='pending'")?
            .query_map([], |r| r.get(0))?
            .collect()
    }
    pub(crate) fn prepare_revert(
        &mut self,
        chat: &Chat,
        command: &Value,
        key: &str,
        digest: &str,
    ) -> Result<Preparation, String> {
        let id = s(&chat.thread["id"]);
        if let Some(old) = self.command_digest(key).map_err(|_| STORE_ERROR)? {
            return if old == digest {
                Ok(Preparation::Complete)
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        let prior: Option<(String, String, String, Option<String>)> = self
            .db
            .query_row(
                "SELECT digest,phase,data,error FROM native_reverts WHERE id=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| STORE_ERROR)?;
        if let Some((old, phase, raw, error)) = prior {
            if old != digest {
                return Err("Command ID was already used for different content".into());
            }
            return match phase.as_str() {
                "applied" => Ok(Preparation::Complete),
                "rejected" => {
                    Err(error.unwrap_or_else(|| "The native CLI refused this revert".into()))
                }
                "pending" => Ok(Preparation::Run(
                    Box::new(serde_json::from_str(&raw).map_err(|_| STORE_ERROR)?),
                    false,
                )),
                _ => Err(STORE_ERROR.into()),
            };
        }
        let pending: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM native_reverts WHERE chat=?1 AND phase='pending'",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| STORE_ERROR)?;
        if let Some(raw) = pending {
            let intent: Intent = serde_json::from_str(&raw).map_err(|_| STORE_ERROR)?;
            if command["targetTurnId"] != intent.public {
                return Err(PENDING.into());
            }
            return Ok(Preparation::Run(Box::new(intent), false));
        }
        let count = command["turnCount"]
            .as_u64()
            .ok_or("Select a non-negative conversation position")?;
        let target = if let Some(target) = command.get("targetTurnId") {
            target
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 128)
                .ok_or("Select a valid prompt to revert")?
                .to_owned()
        } else {
            // Older clients only send a count. Resolve it against stable local
            // user turns once, then send a native turn ID, never a turn count.
            let total:i64=self.db.query_row("SELECT count(DISTINCT turn) FROM chat_rows WHERE chat=?1 AND kind='messages' AND json_extract(data,'$.role')='user'",[id],|r|r.get(0)).map_err(|_|STORE_ERROR)?;
            if count > total as u64 {
                return Err("Turn count exceeds this chat's history".into());
            }
            if count == total as u64 {
                self.db
                    .execute(
                        "INSERT INTO chat_commands(id,digest) VALUES(?1,?2)",
                        params![key, digest],
                    )
                    .map_err(|_| STORE_ERROR)?;
                return Ok(Preparation::Complete);
            }
            self.db.query_row("SELECT turn FROM chat_rows WHERE chat=?1 AND kind='messages' AND json_extract(data,'$.role')='user' GROUP BY turn ORDER BY MIN(seq) LIMIT 1 OFFSET ?2",params![id,count as i64],|r|r.get::<_,String>(0)).map_err(|_|STORE_ERROR)?
        };
        let point = self.edit_target(id, &target)?;
        let intent = Intent {
            key: key.into(),
            digest: digest.into(),
            chat: id.into(),
            native: chat.native_id.clone(),
            target: point.native,
            public: target,
            cut: point.cut,
            previous: point.previous,
            revision: format!("revert-revision-{}", uuid::Uuid::new_v4()),
        };
        let raw = serde_json::to_string(&intent).map_err(|_| STORE_ERROR)?;
        let tx = self.db.transaction().map_err(|_| STORE_ERROR)?;
        let old:String=tx.query_row("SELECT data FROM chats WHERE id=?1 AND agent='codex' AND json_extract(data,'$.native_id')=?2",params![id,intent.native],|r|r.get(0)).map_err(|_|STORE_ERROR)?;
        tx.execute(
            "INSERT INTO chat_revisions VALUES(?1,?2,?3)",
            params![intent.revision, id, old],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.execute("INSERT INTO chat_rows(chat,kind,id,turn,data) SELECT ?1,kind,id,turn,data FROM chat_rows WHERE chat=?2 ORDER BY seq",params![intent.revision,id]).map_err(|_|STORE_ERROR)?;
        tx.execute(
            "INSERT INTO tool_images(chat,path) SELECT ?1,path FROM tool_images WHERE chat=?2",
            params![intent.revision, id],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.execute("INSERT INTO native_turns(chat,turn,native) SELECT ?1,turn,native FROM native_turns WHERE chat=?2",params![intent.revision,id]).map_err(|_|STORE_ERROR)?;
        tx.execute(
            "INSERT INTO native_reverts VALUES(?1,?2,?3,'pending',?4,NULL,NULL)",
            params![key, id, digest, raw],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.commit().map_err(|_| STORE_ERROR)?;
        Ok(Preparation::Run(Box::new(intent), true))
    }
    pub(crate) fn finish_revert(
        &mut self,
        intent: &Intent,
        outcome: Outcome,
    ) -> Result<Value, String> {
        if matches!(outcome, Outcome::Uncertain) {
            return Err(PENDING.into());
        }
        let tx = self.db.transaction().map_err(|_| STORE_ERROR)?;
        let raw: String = tx
            .query_row(
                "SELECT data FROM native_reverts WHERE id=?1 AND digest=?2 AND phase='pending'",
                params![intent.key, intent.digest],
                |r| r.get(0),
            )
            .map_err(|_| STORE_ERROR)?;
        if raw != serde_json::to_string(intent).map_err(|_| STORE_ERROR)? {
            return Err(STORE_ERROR.into());
        }
        if let Outcome::Rejected(error) = outcome {
            tx.execute(
                "UPDATE native_reverts SET phase='rejected',error=?2 WHERE id=?1",
                params![intent.key, error],
            )
            .map_err(|_| STORE_ERROR)?;
            tx.commit().map_err(|_| STORE_ERROR)?;
            return Err(error);
        }
        let removed: String = tx
            .query_row(
                "SELECT removed FROM native_reverts WHERE id=?1",
                [&intent.key],
                |r| r.get(0),
            )
            .map_err(|_| STORE_ERROR)?;
        let removed_ids: Vec<String> = serde_json::from_str(&removed).map_err(|_| STORE_ERROR)?;
        if removed_ids.is_empty()
            || removed_ids.len() > 16384
            || removed_ids.last() != Some(&intent.target)
        {
            return Err(STORE_ERROR.into());
        }
        let old:String=tx.query_row("SELECT data FROM chats WHERE id=?1 AND agent='codex' AND json_extract(data,'$.native_id')=?2",params![intent.chat,intent.native],|r|r.get(0)).map_err(|_|STORE_ERROR)?;
        let mut state: Value = serde_json::from_str(&old).map_err(|_| STORE_ERROR)?;
        // Retain the exact native session, preferences and all earlier rows.
        // The prior revision and its image references remain recoverable.
        for k in ["messages", "activities", "proposedPlans", "checkpoints"] {
            state["thread"][k] = json!([]);
        }
        state["thread"]["latestTurn"] = Value::Null;
        state["thread"]["latestUserMessageAt"] = json!(intent.previous);
        state["thread"]["hasPendingApprovals"] = json!(false);
        state["thread"]["hasPendingUserInput"] = json!(false);
        state["thread"]["hasActionableProposedPlan"] = json!(false);
        state["thread"]["updatedAt"] = json!(now());
        state["thread"]["session"]["status"] = json!("ready");
        state["thread"]["session"]["activeTurnId"] = Value::Null;
        state["thread"]["session"]["lastError"] = Value::Null;
        state["pending_message"] = Value::Null;
        // A retained Desktop turn can arrive after a later local prompt.
        // Preserve it by native identity, regardless of projection row order.
        tx.execute("DELETE FROM chat_rows WHERE chat=?1 AND (turn IN (SELECT turn FROM native_turns WHERE chat=?1 AND native IN (SELECT value FROM json_each(?3))) OR (seq>=?2 AND (turn IS NULL OR turn NOT IN (SELECT turn FROM native_turns WHERE chat=?1))))",params![intent.chat,intent.cut,removed]).map_err(|_|STORE_ERROR)?;
        tx.execute("DELETE FROM native_turns WHERE chat=?1 AND native IN (SELECT value FROM json_each(?2))",params![intent.chat,removed]).map_err(|_|STORE_ERROR)?;
        tx.execute(
            "DELETE FROM native_checkpoints WHERE chat=?1",
            [&intent.chat],
        )
        .map_err(|_| STORE_ERROR)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                params![intent.chat, intent.native],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| STORE_ERROR)?;
        let cursor = crate::shared_history::Cursor::invalidated(previous.as_deref())?;
        tx.execute("INSERT INTO native_sync VALUES(?1,?2,?3) ON CONFLICT(chat,native) DO UPDATE SET cursor=excluded.cursor",params![intent.chat,intent.native,cursor]).map_err(|_|STORE_ERROR)?;
        tx.execute(
            "UPDATE chats SET data=?2 WHERE id=?1",
            params![intent.chat, state.to_string()],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.execute(
            "INSERT INTO chat_commands(id,digest) VALUES(?1,?2)",
            params![intent.key, intent.digest],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.execute(
            "UPDATE native_reverts SET phase='applied' WHERE id=?1",
            [&intent.key],
        )
        .map_err(|_| STORE_ERROR)?;
        tx.commit().map_err(|_| STORE_ERROR)?;
        self.load_chat(&intent.chat).map_err(|_| STORE_ERROR.into())
    }
}

async fn rpc(
    c: &mut Connection,
    method: &str,
    params: Value,
) -> Result<Result<Value, Value>, String> {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .map_err(|_| PENDING)?;
    loop {
        match c.events.recv().await {
            Some(Event::Message(v)) if v["id"] == id && v.get("method").is_none() => {
                return if let Some(e) = v.get("error") {
                    Ok(Err(e.clone()))
                } else {
                    v.get("result")
                        .cloned()
                        .map(Ok)
                        .ok_or_else(|| PENDING.into())
                }
            }
            Some(Event::Message(_)) => {} // Never answer another client's approvals.
            _ => return Err(PENDING.into()),
        }
    }
}
async fn removed_turns(
    c: &mut Connection,
    intent: &Intent,
    collect: bool,
) -> Result<Option<Vec<String>>, String> {
    let mut removed = Vec::new();
    let mut collected_ids = std::collections::HashSet::new();
    let mut cursor = Value::Null;
    for _ in 0..4096 {
        let page=rpc(c,"thread/turns/list",json!({"threadId":intent.native,"cursor":cursor,"limit":50,"sortDirection":"desc","itemsView":"notLoaded"})).await?.map_err(|_|PENDING)?;
        let rows = page["data"]
            .as_array()
            .filter(|r| r.len() <= 50)
            .ok_or(PENDING)?;
        let mut page_ids = std::collections::HashSet::new();
        for row in rows {
            let id = row["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or(PENDING)?;
            if !page_ids.insert(id) {
                return Err(PENDING.into());
            }
            if collect {
                if removed.len() >= 16384 || !collected_ids.insert(id.to_owned()) {
                    return Err(PENDING.into());
                }
                removed.push(id.to_owned());
            }
            if id == intent.target {
                return Ok(Some(removed));
            }
        }
        let next = &page["nextCursor"];
        if next.is_null() {
            return Ok(None);
        }
        if rows.is_empty()
            || next == &cursor
            || !next
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 8192)
        {
            return Err(PENDING.into());
        }
        cursor = next.clone();
    }
    Err(PENDING.into())
}
pub(crate) async fn run(
    config: AgentConfig,
    chat: Chat,
    intent: Intent,
    deliver: bool,
    db: AuthService,
) -> Outcome {
    if !deliver {
        let key = intent.key.clone();
        if matches!(
            db.call(move |a| a.store.db.query_row(
                "SELECT removed IS NOT NULL FROM native_reverts WHERE id=?1",
                [key],
                |r| r.get::<_, bool>(0)
            ))
            .await,
            Ok(Ok(false))
        ) {
            return Outcome::Rejected("The interrupted revert was never sent; saved history is intact. Retry to start a new revert.".into());
        }
    }
    let mut sent = false;
    let mut connection = match config
        .connect(Launch {
            agent: Agent::Codex,
            executable: config.executable.clone(),
            cwd: s(&chat.thread["worktreePath"]).into(),
            env: crate::discovery::environment(crate::chat::AgentKind::Codex, &config.profile),
            extra_args: crate::discovery::isolated_args(crate::chat::AgentKind::Codex),
        })
        .await
    {
        Ok(c) => c,
        Err(_) => {
            return if deliver {
                Outcome::Rejected("Could not connect to Codex; no revert was sent".into())
            } else {
                Outcome::Uncertain
            }
        }
    };
    let result=tokio::time::timeout(Duration::from_secs(25),async {
        rpc(&mut connection,"initialize",json!({"clientInfo":{"name":"remodex_native_revert","version":"0.0.0"},"capabilities":{"experimentalApi":true}})).await?.map_err(|_|PENDING)?;
        connection.send(&json!({"method":"initialized","params":{}})).await.map_err(|_|PENDING)?;
        // Inspect without resuming or changing the owner's settings. Native
        // revert enforces the final idle-state check in its own session owner.
        let meta=rpc(&mut connection,"thread/read",json!({"threadId":intent.native,"includeTurns":false})).await?.map_err(|_|PENDING)?;
        if meta["thread"]["id"]!=intent.native {return Err(PENDING.to_owned());}
        if deliver && meta["thread"]["historyMode"]!="paginated" {return Ok(Outcome::Rejected("Safe conversation Revert requires a paginated Codex session; saved history was not changed.".into()));}
        if !matches!(meta["thread"]["status"]["type"].as_str(),Some("idle"|"notLoaded")) {
            return if deliver {Ok(Outcome::Rejected("Stop the native task before reverting its conversation".into()))}else{Ok(Outcome::Uncertain)};
        }
        if deliver && meta["thread"]["status"]["type"]=="notLoaded" {
            // Revert requires a loaded session. Do not resume a goal-bearing
            // chat as resume may automatically start more work. Supply no model,
            // access, workspace or other overrides to the existing session.
            let goal=rpc(&mut connection,"thread/goal/get",json!({"threadId":intent.native})).await?;
            if !goal.is_ok_and(|v|v.get("goal")==Some(&Value::Null)) {
                return Ok(Outcome::Rejected("This unloaded chat has a goal or its goal state could not be verified. Open it in its owner and finish or pause its work before reverting; no conversation change was sent.".into()));
            }
            let loaded=rpc(&mut connection,"thread/resume",json!({"threadId":intent.native,"excludeTurns":true})).await?.map_err(|_|PENDING)?;
            if loaded["thread"]["id"]!=intent.native || loaded["thread"]["status"]["type"]!="idle" {
                return Ok(Outcome::Rejected("Codex did not confirm an idle original session; no conversation change was sent.".into()));
            }
        }
        let removed=removed_turns(&mut connection,&intent,deliver).await?;
        let present=removed.is_some();
        if !deliver {return Ok(if present {Outcome::Uncertain}else{Outcome::Applied});}
        if !present {return Ok(Outcome::Rejected("The selected native turn is no longer present; saved history was not changed".into()));}
        let key=intent.key.clone();let plan=serde_json::to_string(&removed.unwrap()).map_err(|_|PENDING)?;
        let recorded=db.call(move|a|{
            a.store.check_history((plan.len() as u64).saturating_mul(2).saturating_add(65536))?;
            a.store.db.execute("UPDATE native_reverts SET removed=?2 WHERE id=?1 AND phase='pending' AND removed IS NULL",params![key,plan]).map_err(|_|PENDING.to_owned())
        }).await.map_err(|_|PENDING)??;
        if recorded!=1 {return Err(PENDING.to_owned());}
        sent=true;
        let value=rpc(&mut connection,"thread/revert",json!({"threadId":intent.native,"beforeTurnId":intent.target})).await?;
        match value {
            Err(error)=>Ok(Outcome::Rejected(if error["code"]==-32601 {"This Codex CLI does not support safe turn-based Revert. Update the CLI; saved history was not changed.".into()}else{"Codex refused the conversation revert; saved history was not changed.".into()})),
            Ok(value) if value["thread"]["id"]==intent.native && removed_turns(&mut connection,&intent,false).await?.is_none()=>Ok(Outcome::Applied),
            _=>Ok(Outcome::Uncertain),
        }
    }).await;
    connection.close().await;
    match result {
        Ok(Ok(value)) => value,
        _ if sent || !deliver => Outcome::Uncertain,
        _ => {
            Outcome::Rejected("Could not verify the native conversation; no revert was sent".into())
        }
    }
}

pub(crate) async fn finish(
    db: AuthService,
    intent: Intent,
    outcome: Outcome,
) -> Result<Chat, String> {
    let value = db
        .call(move |a| a.store.finish_revert(&intent, outcome))
        .await??;
    serde_json::from_value(value).map_err(|_| STORE_ERROR.into())
}
