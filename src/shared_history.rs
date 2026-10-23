//! Read-only catch-up for shared Codex sessions. A bounded page is committed
//! with its cursor; reconnect never resends a prompt or changes CLI settings.
use crate::{
    agent::{Connection, Event},
    auth_service::AuthService,
    chat::AgentConfig,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
const PROJECTION_VERSION: u8 = 6;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    #[serde(default)]
    generation: u64,
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
            generation: 0,
            projection_version: PROJECTION_VERSION,
            turns: None,
            turn: String::new(),
            items: None,
            complete: false,
            final_pass: false,
        }
    }
}
impl Cursor {
    pub(crate) fn invalidated(previous: Option<&str>) -> Result<String, String> {
        let previous: Self = previous
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| "Saved history cursor is invalid")?
            .unwrap_or_default();
        let generation = previous
            .generation
            .checked_add(1)
            .ok_or("History generation exhausted")?;
        serde_json::to_string(&Self {
            generation,
            ..Self::default()
        })
        .map_err(|_| "Cannot reset history cursor".into())
    }
}
pub(crate) struct Batch {
    pub native: String,
    pub before: Cursor,
    pub after: Cursor,
    pub turn: Option<Value>,
    pub public_turn: Option<String>,
    pub items: Vec<Value>,
    pub recovered_images: std::collections::BTreeMap<String, Vec<String>>,
    pub final_page: bool,
    pub caught_up: bool,
    pub busy: bool,
    pub missing_empty_session: bool,
}

async fn rpc(c: &mut Connection, method: &str, params: Value) -> Result<Value, String> {
    rpc_response(c, method, params)
        .await?
        .map_err(|_| format!("Shared history read failed ({method}); existing history is intact"))
}
async fn rpc_response(
    c: &mut Connection,
    method: &str,
    params: Value,
) -> Result<Result<Value, Value>, String> {
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
) -> Result<Batch, String> {
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
    let mut connection = server.connect(&config.profile).await?;
    let result=tokio::time::timeout(Duration::from_secs(12), async {
        rpc(&mut connection,"initialize",json!({"clientInfo":{"name":"remodex_history_reader","version":"0.0.0"},"capabilities":{"experimentalApi":true}})).await?;
        connection.send(&json!({"method":"initialized","params":{}})).await.map_err(|_|"Shared history disconnected")?;
        let metadata=match rpc_response(&mut connection,"thread/read",json!({"threadId":native,"includeTurns":false})).await? {
            Ok(v)=>v,
            Err(error)=>{
                let cid=chat.clone();let nid=native.clone();
                if !explicit_send || error["message"]!=format!("thread not loaded: {native}")
                    || !db.call(move|a|a.store.empty_shared_session(&cid,&nid)).await?.map_err(|_|"Cannot verify the empty session")? {
                    return Err("Shared history read failed (thread/read); existing history is intact".into());
                }
                // Only an explicit retry of a first prompt that never started
                // may check for an expired, unpersisted native session. No model
                // settings, history override or fallback CLI are supplied.
                match rpc_response(&mut connection,"thread/resume",json!({"threadId":native,"excludeTurns":true})).await? {
                    Ok(v)=>v,
                    Err(error) if error["message"]==format!("no rollout found for thread id {native}")=>{
                        return Ok(Batch{native:native.clone(),before:before.clone(),after:before,turn:None,public_turn:None,items:vec![],recovered_images:Default::default(),final_page:false,caught_up:true,busy:false,missing_empty_session:true});
                    }
                    Err(_)=>return Err("The original session could not be verified; history is intact".into()),
                }
            }
        };
        if metadata["thread"]["id"]!=native {return Err("Shared history identity changed".into());}
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
        let after=if before.projection_version<PROJECTION_VERSION {Cursor{generation:before.generation,..Default::default()}}else{before.clone()};
        let mut batch=Batch{native:native.clone(),before,after,turn:None,public_turn:None,items:vec![],recovered_images:Default::default(),final_page:false,caught_up:false,busy,missing_empty_session:false};
        // Skip a bounded number of turns already reconciled by this projection.
        for _ in 0..8 {
            let page=rpc(&mut connection,"thread/turns/list",json!({"threadId":native,"cursor":batch.after.turns,"limit":1,"sortDirection":"asc","itemsView":"notLoaded"})).await?;
            let turns=page["data"].as_array().filter(|a|a.len()<=1).ok_or("Invalid shared turn page")?;
            let next=cursor(&page["nextCursor"])?;
            let Some(turn)=turns.first() else {
                if next.is_some() {return Err("Invalid empty shared turn page".into());}
                batch.caught_up=true; return Ok(batch);
            };
            let tid=turn["id"].as_str().filter(|s|!s.is_empty() && s.len()<=128).ok_or("Invalid shared turn identity")?.to_owned();
            let completed=match turn["status"].as_str() {
                Some("completed"|"interrupted"|"failed")=>true,
                Some("inProgress")=>false,
                _=>return Err("Unknown shared turn status".into()),
            };
            if batch.after.turn==tid && batch.after.complete && completed {
                if !completed || next.is_none() {batch.caught_up=true;return Ok(batch);}
                if next==batch.after.turns {return Err("Shared history cursor did not advance".into());}
                batch.after=Cursor{generation:batch.after.generation,turns:next,..Default::default()};
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
            } else if completed && !batch.after.final_pass {
                // Re-read a turn once on completion: earlier running tools and
                // message chunks may have changed after their first page.
                batch.after.items=None;batch.after.final_pass=true;
            }
            let page=rpc(&mut connection,"thread/items/list",json!({"threadId":native,"turnId":tid,"cursor":batch.after.items,"limit":8,"sortDirection":"asc"})).await?;
            let rows=page["data"].as_array().filter(|a|a.len()<=8).ok_or("Invalid shared item page")?;
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
                    if !images.complete {
                        batch.items.clear();
                        return Ok(batch);
                    }
                }
            }
            batch.final_page=completed && item_next.is_none();
            batch.turn=Some(turn.clone());
            if let Some(item_next)=item_next {
                if batch.after.items.as_ref()==Some(&item_next) {return Err("Shared item cursor did not advance".into());}
                batch.after.items=Some(item_next);
            } else if completed {
                batch.after.complete=true;
                if next.is_some() {
                    if next==batch.after.turns {return Err("Shared history cursor did not advance".into());}
                    batch.after=Cursor{generation:batch.after.generation,turns:next,..Default::default()};
                } else {batch.caught_up=true;}
            } else {
                // Keep the input cursor of the last page so streaming updates
                // and newly appended items are both visible on the next poll.
                batch.caught_up=true;
            }
            return Ok(batch);
        }
        Ok(batch)
    }).await.unwrap_or_else(|_|Err("Shared history check timed out; existing history is intact".into()));
    connection.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalidated_cursor_rejects_an_old_page_without_restoring_removed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let before = Cursor::default();
        let next = Cursor::invalidated(None).unwrap();
        let latest = Cursor::invalidated(Some(&next)).unwrap();
        assert_ne!(before, serde_json::from_str::<Cursor>(&next).unwrap());
        assert_ne!(next, latest);
        store
            .db
            .execute(
                "INSERT INTO native_sync VALUES('chat','native',?1)",
                [&next],
            )
            .unwrap();
        let batch = Batch {
            native: "native".into(),
            before: before.clone(),
            after: before,
            turn: None,
            public_turn: None,
            items: vec![],
            recovered_images: Default::default(),
            final_page: true,
            caught_up: true,
            busy: false,
            missing_empty_session: false,
        };
        let state = json!({"agent":"codex","native_id":"native","thread":{"id":"chat","messages":[{"id":"removed","turnId":"removed-turn","text":"must not return"}],"activities":[],"proposedPlans":[]}});
        assert!(store.save_shared_history(&state, &batch).is_err());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chat_rows", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT cursor FROM native_sync", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            next
        );
    }
}
