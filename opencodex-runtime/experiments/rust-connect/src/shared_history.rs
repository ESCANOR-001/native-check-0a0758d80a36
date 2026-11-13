//! Read-only catch-up for shared Codex sessions. A bounded page is committed
//! with its cursor; reconnect never resends a prompt or changes CLI settings.
use crate::{
    agent::{Connection, Event},
    auth_service::AuthService,
    chat::AgentConfig,
};
use base64::Engine;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
const PROJECTION_VERSION: u8 = 9;

pub(crate) fn work_revision(turn: &str, status: &str) -> String {
    format!("{turn}:{status}:v{PROJECTION_VERSION}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    #[serde(default)]
    projection_version: u8,
    turns: Option<String>,
    turn: String,
    items: Option<String>,
    complete: bool,
    final_pass: bool,
}
impl Default for Cursor {
    fn default() -> Self {
        Self {
            projection_version: PROJECTION_VERSION,
            turns: None,
            turn: String::new(),
            items: None,
            complete: false,
            final_pass: false,
        }
    }
}
#[derive(Clone)]
pub(crate) struct WindowPage {
    pub older_request: bool,
    pub next: Option<String>,
    pub conversation: bool,
    pub work_cursor: Option<String>,
    pub work_page: bool,
}
#[derive(Clone)]
pub(crate) struct Batch {
    /// At most two earlier summaries included with the newest conversation.
    pub previous: Vec<Batch>,
    pub window: Option<WindowPage>,
    pub native: String,
    pub before: Cursor,
    pub after: Cursor,
    pub turn: Option<Value>,
    pub public_turn: Option<String>,
    pub items: Vec<Value>,
    pub desktop_replies: Vec<Value>,
    pub recovered_images: std::collections::BTreeMap<String, Vec<String>>,
    pub final_page: bool,
    pub caught_up: bool,
    pub busy: bool,
    pub live_status: bool,
    pub task_status_known: bool,
    pub missing_empty_session: bool,
    pub live_compaction: Option<(String, String)>,
    pub usage: Value,
}

// Work cursors identify a turn, not its changing position in the turn list.
// Read timing from the already verified disclosure and fetch items by turn ID.
async fn work_turn(db: &AuthService, chat: &str, native_turn: &str) -> Result<Value, String> {
    let cid = chat.to_owned();
    let tid = native_turn.to_owned();
    let marker: Value = db.call(move |a| -> rusqlite::Result<Value> {
        let raw: String = a.store.db.query_row(
            "SELECT r.data FROM native_turns t JOIN chat_rows r ON r.chat=t.chat AND r.turn=t.turn AND r.kind='activities' AND r.id='desktop-work-'||t.turn WHERE t.chat=?1 AND t.native=?2 LIMIT 1",
            params![cid,tid], |r|r.get(0))?;
        serde_json::from_str(&raw).map_err(|_|rusqlite::Error::InvalidQuery)
    }).await?.map_err(|_|"Activity is not available yet; retry shortly")?;
    let timestamp = |v: &Value| {
        v.as_str()
            .and_then(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
            })
            .map(|t| t.unix_timestamp())
    };
    let start = timestamp(&marker["payload"]["turnStartedAt"])
        .or_else(|| timestamp(&marker["createdAt"]))
        .ok_or("Activity timing is unavailable")?;
    let end = timestamp(&marker["payload"]["turnCompletedAt"]);
    let revision = marker["payload"]["revision"].as_str().unwrap_or_default();
    let status = revision
        .strip_prefix(&format!("{native_turn}:"))
        .and_then(|s| s.split(':').next())
        .filter(|s| matches!(*s, "completed" | "interrupted" | "failed" | "inProgress"))
        .unwrap_or(if end.is_some() {
            "completed"
        } else {
            "inProgress"
        });
    Ok(
        json!({"id":native_turn,"status":status,"error":crate::codex_error::thin(&marker["payload"]["error"]),"startedAt":start,"completedAt":end,"items":[]}),
    )
}

fn with_previous(mut pages: Vec<Batch>, last: Batch) -> Batch {
    if pages.is_empty() {
        return last;
    }
    pages.push(last);
    let mut newest = pages.remove(0);
    let last = pages.last().unwrap();
    newest.after = last.after.clone();
    newest.window.as_mut().unwrap().next = last.window.as_ref().unwrap().next.clone();
    newest.previous = pages;
    newest
}

// Older conversation and Work pages use Codex's read-only APIs directly. Discovering the
// live Desktop owner (status, questions, compaction) belongs to the live stream,
// not each historical page. In particular an unresponsive owner must not delay
// viewing already saved work or change the current task's execution state.
enum HistoryConnection {
    Shared(Connection),
    Saved(crate::codex_history_reader::Reader),
}
impl HistoryConnection {
    async fn close(self) {
        match self {
            Self::Shared(connection) => connection.close().await,
            Self::Saved(reader) => reader.close().await,
        }
    }
}

async fn rpc(c: &mut HistoryConnection, method: &str, params: Value) -> Result<Value, String> {
    rpc_response(c, method, params).await?.map_err(|error| {
        format!(
            "Shared history read failed ({method}): {}",
            crate::codex_error::message(&error)
                .unwrap_or_else(|| "Codex did not provide error details".into())
        )
    })
}
async fn rpc_response(
    c: &mut HistoryConnection,
    method: &str,
    params: Value,
) -> Result<Result<Value, Value>, String> {
    let c = match c {
        HistoryConnection::Saved(reader) => {
            // Reader::open already initialized its private app-server. It has
            // an allowlist of read operations and cannot resume/control a chat.
            if method == "initialize" || method == "initialized" {
                return Ok(Ok(json!({})));
            }
            let mut response = reader.rpc(method, params).await?;
            if method == "thread/read" {
                response["thread"]["remodexLiveStatusKnown"] = json!(false);
                response["thread"]["remodexTaskStatusKnown"] = json!(false);
            }
            return Ok(Ok(response));
        }
        HistoryConnection::Shared(connection) => connection,
    };
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .map_err(|_| "Shared history disconnected")?;
    // Notifications are not approvals: this reader must never answer a tool
    // request belonging to Desktop or establish ownership of a running turn.
    loop {
        match c.events.recv().await {
            Some(Event::Message(v)) if v["id"] == id && v.get("method").is_none() => {
                if let Some(error) = v.get("error") {
                    return Ok(Err(error.clone()));
                }
                return v
                    .get("result")
                    .cloned()
                    .map(Ok)
                    .ok_or_else(|| "Shared history response is invalid".into());
            }
            Some(Event::Message(_)) => {}
            _ => return Err("Shared history disconnected".into()),
        }
    }
}
fn cursor(v: &Value) -> Result<Option<String>, String> {
    if v.is_null() {
        return Ok(None);
    }
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= 8192)
        .map(|s| Some(s.to_owned()))
        .ok_or_else(|| "Shared history cursor is invalid".into())
}

pub(crate) async fn read(
    config: AgentConfig,
    db: AuthService,
    chat: String,
    native: String,
    explicit_send: bool,
    require_live: bool,
) -> Result<Batch, String> {
    read_page(config, db, chat, native, explicit_send, require_live, None).await
}

pub(crate) async fn read_older(
    config: AgentConfig,
    db: AuthService,
    chat: String,
    native: String,
    token: String,
) -> Result<Batch, String> {
    // Include startup and teardown in the page deadline, not only the RPCs.
    // Retain time for committing the page within the phone's RPC deadline and
    // return a retryable error without advancing its cursor on cancellation.
    tokio::time::timeout(
        Duration::from_secs(13),
        read_page(config, db, chat, native, false, false, Some(token)),
    )
    .await
    .unwrap_or_else(|_| {
        Err("Activity history is taking longer than expected; retry shortly".into())
    })
}

fn encode_page(chat: &str, native: &str, cursor: &Cursor) -> String {
    format!(
        "d1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!([chat, native, cursor]).to_string())
    )
}
fn decode_page(token: &str, chat: &str, native: &str) -> Result<Cursor, String> {
    if token.len() > 32768 {
        return Err("Invalid history cursor".into());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(
            token
                .strip_prefix("d1:")
                .or_else(|| token.strip_prefix("i1:"))
                .ok_or("Invalid history cursor")?,
        )
        .map_err(|_| "Invalid history cursor")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid history cursor")?;
    if value[0] != chat || value[1] != native {
        return Err("History cursor belongs to another chat".into());
    }
    serde_json::from_value(value[2].clone()).map_err(|_| "Invalid history cursor".into())
}

async fn read_page(
    config: AgentConfig,
    db: AuthService,
    chat: String,
    native: String,
    explicit_send: bool,
    require_live: bool,
    older: Option<String>,
) -> Result<Batch, String> {
    let recent = matches!(
        config.shared_server,
        Some(crate::agent::SharedServer::Desktop { .. })
    );
    if older.is_some() && !recent {
        return Err("Invalid history cursor".into());
    }
    let work = older.as_ref().is_some_and(|s| s.starts_with("i1:"));
    let summary = recent && !work;
    let older_cursor = older
        .as_ref()
        .map(|v| decode_page(v, &chat, &native))
        .transpose()?;
    let cid = chat.clone();
    let nid = native.clone();
    let before: Cursor = db
        .call(move |a| -> Result<Cursor, String> {
            let raw: Option<String> = a
                .store
                .db
                .query_row(
                    "SELECT cursor FROM native_sync WHERE chat=?1 AND native=?2",
                    params![cid, nid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| "Cannot read shared history cursor")?;
            raw.map(|s| {
                serde_json::from_str(&s).map_err(|_| "Shared history cursor is invalid".into())
            })
            .unwrap_or(Ok(Cursor::default()))
        })
        .await??;
    let server = config
        .shared_server
        .as_ref()
        .ok_or("Shared Codex is not configured")?;
    let mut connection = if older.is_some() {
        let crate::agent::SharedServer::Desktop {
            reader_executable,
            reader_workspace,
            ..
        } = server
        else {
            return Err("Invalid history cursor".into());
        };
        let reader_config = AgentConfig {
            executable: reader_executable.clone(),
            ..config.clone()
        };
        HistoryConnection::Saved(
            crate::codex_history_reader::Reader::open(&reader_config, reader_workspace).await?,
        )
    } else if require_live {
        HistoryConnection::Shared(server.connect(&config.profile).await?)
    } else {
        HistoryConnection::Shared(server.connect_history(&config.profile).await?)
    };
    let result=tokio::time::timeout(Duration::from_secs(12), async {
        rpc(&mut connection,"initialize",json!({"clientInfo":{"name":"remodex_history_reader","version":"0.0.0"},"capabilities":{"experimentalApi":true}})).await?;
        if let HistoryConnection::Shared(connection) = &connection {
            connection.send(&json!({"method":"initialized","params":{}})).await.map_err(|_|"Shared history disconnected")?;
        }
        let mut read_params=json!({"threadId":native,"includeTurns":false});
        if explicit_send && matches!(config.shared_server, Some(crate::agent::SharedServer::Desktop { .. })) {
            read_params["remodexAllowActivation"]=json!(true);
        }
        let metadata=match rpc_response(&mut connection,"thread/read",read_params).await? {
            Ok(v)=>v,
            Err(error)=>{
                let cid=chat.clone();let nid=native.clone();
                if matches!(config.shared_server, Some(crate::agent::SharedServer::Desktop { .. })) || !explicit_send || error["message"]!=format!("thread not loaded: {native}")
                    || !db.call(move|a|a.store.empty_shared_session(&cid,&nid)).await?.map_err(|_|"Cannot verify the empty session")? {
                    return Err("Shared history read failed (thread/read); existing history is intact".into());
                }
                // Only an explicit retry of a first prompt that never started
                // may check for an expired, unpersisted native session. No model
                // settings, history override or fallback CLI are supplied.
                match rpc_response(&mut connection,"thread/resume",json!({"threadId":native,"excludeTurns":true})).await? {
                    Ok(v)=>v,
                    Err(error) if error["message"]==format!("no rollout found for thread id {native}")=>{
                        return Ok(Batch{previous:vec![],window:None,native:native.clone(),before:before.clone(),after:before,turn:None,public_turn:None,items:vec![],desktop_replies:vec![],recovered_images:Default::default(),final_page:false,caught_up:true,busy:false,missing_empty_session:true,live_status:true,task_status_known:true,live_compaction:None,usage:Value::Null});
                    }
                    Err(_)=>return Err("The original session could not be verified; history is intact".into()),
                }
            }
        };
        if metadata["thread"]["id"]!=native {return Err("Shared history identity changed".into());}
        let live_status=older.is_none() && metadata["thread"]["remodexLiveStatusKnown"] != false;
        let task_status_known=older.is_none() && metadata["thread"]["remodexTaskStatusKnown"] != false;
        if require_live && !live_status { return Err("The original Codex connection is unavailable; no prompt was sent".into()); }
        let busy=match metadata["thread"]["status"]["type"].as_str() {
            Some("idle"|"notLoaded")=>false,
            Some("active")=>true,
            _=>return Err("Shared chat state is unavailable; no prompt was sent".into()),
        };
        if before.projection_version>PROJECTION_VERSION {return Err("Shared history was written by a newer backend".into());}
        // v0 skipped owned turns. Re-read them without deleting rows or changing
        // public identities. The old cursor remains the transaction precondition.
        // v1 also missed local identities when the turn-start acknowledgement
        // was lost. Exact native client IDs can recover those receipts.
        // v2 did not capture image results during history catch-up.
        // v3 omitted MCP images carried inside native JSON text envelopes.
        let after=if recent {older_cursor.unwrap_or_default()} else if before.projection_version<PROJECTION_VERSION {Cursor::default()}else{before.clone()};
        let live_compaction = metadata["thread"]["remodexLiveCompaction"]["turnId"].as_str().zip(metadata["thread"]["remodexLiveCompaction"]["itemId"].as_str()).map(|(t,i)|(t.to_owned(),i.to_owned()));
        let desktop_replies=metadata["thread"]["remodexQuestionReplies"].as_array().cloned().unwrap_or_default();
        if desktop_replies.len()>512 || serde_json::to_string(&desktop_replies).map_err(|_|"Invalid Desktop question replies")?.len()>4*1024*1024 {return Err("Desktop question replies exceed the bounded read limit".into());}
        let mut batch=Batch{previous:vec![],window:recent.then_some(WindowPage{older_request:older.is_some(),next:None,conversation:summary,work_cursor:None,work_page:work}),native:native.clone(),before,after,turn:None,public_turn:None,items:vec![],desktop_replies,recovered_images:Default::default(),final_page:false,caught_up:false,busy,live_status,task_status_known,missing_empty_session:false,live_compaction,usage:json!({"context":metadata["thread"]["remodexTokenUsage"],"limits":metadata["thread"]["remodexRateLimits"]})};
        let template = batch.clone();
        let mut recent_pages = vec![];
        // Skip a bounded number of turns already reconciled by this projection.
        for _ in 0..8 {
            let page=if work { json!({"data":[work_turn(&db,&chat,&batch.after.turn).await?],"nextCursor":null}) } else {rpc(&mut connection,"thread/turns/list",json!({"threadId":native,"cursor":batch.after.turns,"limit":1,"sortDirection":if recent {"desc"}else{"asc"},"itemsView":if summary {"summary"} else {"notLoaded"}})).await?};
            let turns=page["data"].as_array().filter(|a|a.len()<=1).ok_or("Invalid shared turn page")?;
            let next=cursor(&page["nextCursor"])?;
            let Some(turn)=turns.first() else {
                if next.is_some() {return Err("Invalid empty shared turn page".into());}
                if older.is_none() && !work && batch.after.turns.is_none() && batch.after.turn.is_empty()
                    && matches!(config.shared_server, Some(crate::agent::SharedServer::Desktop { .. })) {
                    if let Some(saved) = metadata["thread"].get("remodexSavedTerminal").filter(|v| v.is_object()) {
                        let id = saved["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 256).ok_or("Invalid saved task identity")?;
                        batch.turn = Some(saved.clone());
                        batch.public_turn = Some(format!("desktop-{id}"));
                        batch.final_page = true;
                    }
                }
                batch.caught_up=true; return Ok(with_previous(recent_pages,batch));
            };
            let tid=turn["id"].as_str().filter(|s|!s.is_empty() && s.len()<=128).ok_or("Invalid shared turn identity")?.to_owned();
            if work && batch.after.turn != tid {return Err("Work history changed; reopen this response".into());}
            // A cursor retained by an older Android build may point inside the
            // current turn. Conversation paging now advances to the prior turn.
            if summary && older.is_some() && batch.after.items.is_some() {
                let Some(next)=next else {batch.caught_up=true;return Ok(batch);};
                batch.after=Cursor{turns:Some(next),..Default::default()};
                continue;
            }
            let completed=match turn["status"].as_str() {
                Some("completed"|"interrupted"|"failed")=>true,
                Some("inProgress")=>false,
                _=>return Err("Unknown shared turn status".into()),
            };
            if batch.after.turn==tid && batch.after.complete && completed {
                if !completed || next.is_none() {batch.caught_up=true;return Ok(batch);}
                if next==batch.after.turns {return Err("Shared history cursor did not advance".into());}
                batch.after=Cursor{turns:next.clone(),..Default::default()};
                continue;
            }
            let cid=chat.clone();let turn_id=tid.clone();
            let owned:Vec<String>=db.call(move |a| {
                let mut q=a.store.db.prepare("SELECT turn FROM native_turns WHERE chat=?1 AND native=?2 AND turn<>?3 LIMIT 2")?;
                let rows:rusqlite::Result<Vec<String>>=q.query_map(params![cid,turn_id,format!("desktop-{turn_id}")],|r|r.get(0))?.collect();rows
            }).await?.map_err(|_|"Cannot read shared turn receipt")?;
            if owned.len()>1 {return Err("Shared turn has conflicting local receipts; history is intact".into());}
            batch.public_turn=Some(owned.first().cloned().unwrap_or_else(||format!("desktop-{tid}")));
            if batch.after.turn!=tid {
                batch.after.turn=tid.clone(); batch.after.items=None; batch.after.complete=false; batch.after.final_pass=completed;
            } else if completed && !batch.after.final_pass && !recent {
                // Re-read a turn once on completion: earlier running tools and
                // message chunks may have changed after their first page.
                batch.after.items=None;batch.after.final_pass=true;
            }
            let page=if summary {
                let items=turn["items"].as_array().filter(|a|a.len()<=64).ok_or("Invalid turn summary")?;
                let start=turn["startedAt"].as_i64().unwrap_or_default().saturating_mul(1000);
                let end=turn["completedAt"].as_i64().map(|t|t.saturating_mul(1000));
                let mut rows:Vec<Value>=items.iter().filter(|i|matches!(i["type"].as_str(),Some("userMessage"|"agentMessage"))).map(|item|json!({"turnId":tid,"item":item,"summaryTime":true,"startedAtMs":if item["type"]=="userMessage"{start}else{end.unwrap_or(start)},"completedAtMs":if item["type"]=="userMessage"{Some(start)}else{end}})).collect();
                if !completed {
                    let tail=rpc(&mut connection,"thread/items/list",json!({"threadId":native,"turnId":tid,"limit":8,"sortDirection":"desc"})).await?;
                    for item in tail["data"].as_array().filter(|a|a.len()<=8).ok_or("Invalid live tail")?.iter().rev() {
                        if let Some(existing)=rows.iter_mut().find(|r|r["item"]["id"]==item["item"]["id"]) {*existing=item.clone();} else {rows.push(item.clone());}
                    }
                }
                let work_cursor=Cursor{turns:batch.after.turns.clone(),turn:tid.clone(),final_pass:completed,..Default::default()};
                batch.window.as_mut().unwrap().work_cursor=Some(encode_page(&chat,&native,&work_cursor).replacen("d1:","i1:",1));
                json!({"data":rows,"nextCursor":null})
            } else {rpc(&mut connection,"thread/items/list",json!({"threadId":native,"turnId":tid,"cursor":batch.after.items,"limit":if work {64}else{8},"sortDirection":if recent {"desc"}else{"asc"}})).await?};
            let rows=page["data"].as_array().filter(|a|a.len()<=if summary {72}else if work {64}else{8}).ok_or("Invalid shared item page")?;
            for row in rows {
                if row["turnId"]!=tid || !row["item"]["id"].as_str().is_some_and(|s|!s.is_empty() && s.len()<=256) {return Err("Shared history item belongs to an unexpected turn".into());}
                let item=&row["item"];
                if item["type"]=="userMessage" {
                    if let Some(client)=item["clientId"].as_str().filter(|s|!s.is_empty() && s.len()<=128) {
                        let cid=chat.clone();let client=client.to_owned();let nid=tid.clone();
                        let receipt=db.call(move |a|->rusqlite::Result<Option<String>> {
                            let public:Option<String>=a.store.db.query_row("SELECT turn FROM chat_rows WHERE chat=?1 AND kind='messages' AND id=?2 AND json_extract(data,'$.role')='user' AND turn IS NOT NULL",params![cid,client],|r|r.get(0)).optional()?;
                            if let Some(public)=&public {
                                let saved:Option<String>=a.store.db.query_row("SELECT native FROM native_turns WHERE chat=?1 AND turn=?2",params![cid,public],|r|r.get(0)).optional()?;
                                if saved.is_some_and(|saved|saved!=nid) {return Err(rusqlite::Error::InvalidQuery);}
                            }
                            Ok(public)
                        }).await?.map_err(|_|"Shared prompt has a conflicting delivery receipt; history is intact")?;
                        if let Some(public)=receipt {
                            if batch.public_turn.as_deref()!=Some(&format!("desktop-{tid}")) && batch.public_turn.as_deref()!=Some(&public) {
                                return Err("Shared prompt has a conflicting turn identity; history is intact".into());
                            }
                            batch.public_turn=Some(public);
                        }
                    }
                }
                batch.items.push(row.clone());
            }
            let item_next=cursor(&page["nextCursor"])?;
            // The native projection can truncate an MCP image inside a JSON
            // text envelope. Recover only a matching image from this verified
            // session/turn; raw rollout text is never projected. An incremental
            // scan retains this page until its bounded source read finishes.
            if completed {
                if let Some(path)=metadata["thread"]["path"].as_str() {
                    let images=crate::codex_assets::recover(db.clone(),config.profile.clone(),path.into(),chat.clone(),native.clone(),tid.clone(),rows).await?;
                    batch.recovered_images=images.paths;
                    if !images.complete && !recent {
                        batch.items.clear();
                        return Ok(batch);
                    }
                }
            }
            batch.final_page=completed && (recent || item_next.is_none());
            if recent && !summary {batch.items.reverse();}
            batch.turn=Some(turn.clone());
            if work {
                batch.after.items=item_next.clone();
                batch.window.as_mut().unwrap().work_cursor=item_next.map(|_|encode_page(&chat,&native,&batch.after).replacen("d1:","i1:",1));
                batch.caught_up=true;
                return Ok(batch);
            }
            if let Some(item_next)=item_next {
                if batch.after.items.as_ref()==Some(&item_next) {return Err("Shared item cursor did not advance".into());}
                batch.after.items=Some(item_next);
            } else if completed || recent {
                batch.after.complete=true;
                if next.is_some() {
                    if next==batch.after.turns {return Err("Shared history cursor did not advance".into());}
                    batch.after=Cursor{turns:next.clone(),..Default::default()};
                } else {batch.caught_up=true;}
            } else {
                // Keep the input cursor of the last page so streaming updates
                // and newly appended items are both visible on the next poll.
                batch.caught_up=true;
            }
            if recent {
                let has_older = item_next_present(&page) || next.is_some();
                batch.window.as_mut().unwrap().next=has_older.then(||encode_page(&chat,&native,&batch.after));
                // The recent window is complete. Older content is never a
                // prerequisite for live state or a reason to keep backfilling.
                batch.caught_up=true;
            }
            if summary && older.is_none() && batch.window.as_ref().unwrap().next.is_some() && recent_pages.len()<2 {
                let after=batch.after.clone();
                recent_pages.push(batch);
                batch=Batch{after,..template.clone()};
                continue;
            }
            return Ok(with_previous(recent_pages,batch));
        }
        Ok(with_previous(recent_pages,batch))
    }).await.unwrap_or_else(|_|Err("Shared history check timed out; existing history is intact".into()));
    connection.close().await;
    result
}

fn item_next_present(page: &Value) -> bool {
    !page["nextCursor"].is_null()
}
