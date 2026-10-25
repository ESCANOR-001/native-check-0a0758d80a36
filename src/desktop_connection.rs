//! Translate one shared control connection into original Desktop-owner IPC.
//! A separate saved-history reader never resumes or mutates the native chat.
use super::*;
use crate::{
    codex_history_reader::Reader,
    desktop_ipc::{Client, Endpoint, Owner},
};
use serde_json::json;
use std::collections::BTreeMap;

const ERROR:&str="The original Codex Desktop connection is unavailable. History is retained and no replacement session was started";
struct Desktop {
    client: Client,
    profile: PathBuf,
    reader: Reader,
    native: String,
    owner: Option<Owner>,
    turn: String,
    items: BTreeMap<String, String>,
    requests: BTreeMap<String, Value>,
    lifecycle: std::sync::Arc<std::sync::Mutex<crate::codex_lifecycle::Tracker>>,
}
impl Desktop {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if method == "initialize" {
            return Ok(json!({"userAgent":"remodex-desktop-follower"}));
        }
        if method == "initialized" {
            return Ok(Value::Null);
        }
        if matches!(method, "model/list" | "account/rateLimits/read") {
            return self.reader.rpc(method, params).await;
        }
        let native = params["threadId"].as_str().ok_or(ERROR)?;
        if uuid::Uuid::parse_str(native).is_err()
            || !self.native.is_empty() && self.native != native
        {
            return Err(ERROR.into());
        }
        match method {
            "thread/read" => {
                if params["includeTurns"] != false {
                    return Err("Read Desktop history using bounded turn and item pages".into());
                }
                // Discovery proves the original owner is connected; the bounded
                // native turn page provides execution state. Never resume it.
                self.client.owner(native).await?;
                let (thread, _) = self.metadata(native).await?;
                Ok(json!({"thread":thread}))
            }
            "thread/turns/list" => {
                let (_, latest) = self.metadata(native).await?;
                let mut page = self.reader.rpc(method, params).await?;
                if let Some(rows) = page["data"].as_array_mut() {
                    for row in rows {
                        if row["id"] == latest["data"][0]["id"] {
                            row["status"] = latest["data"][0]["status"].clone();
                        }
                    }
                }
                Ok(page)
            }
            "thread/items/list" => self.reader.rpc(method, params).await,
            // The private transport never sends this mutation to its CLI.
            // It only binds this follower to the discovered Desktop owner.
            "thread/resume" => {
                if self.owner.is_some() {
                    return Err(ERROR.into());
                }
                let owner = self.client.owner(native).await?;
                let (thread, page) = self.metadata(native).await?;
                self.native = native.into();
                self.owner = Some(owner);
                if page["data"][0]["status"] == "inProgress" {
                    self.turn = page["data"][0]["id"].as_str().ok_or(ERROR)?.into();
                }
                Ok(json!({"thread":thread,"initialTurnsPage":page}))
            }
            "turn/start" => {
                let owner = self.owner.as_ref().ok_or(ERROR)?;
                let result = self
                    .client
                    .start(owner, params, &uuid::Uuid::new_v4().to_string())
                    .await?;
                let result = result.get("result").unwrap_or(&result).clone();
                self.turn = result["turn"]["id"].as_str().ok_or(ERROR)?.into();
                self.items.clear();
                Ok(result)
            }
            "turn/steer" => {
                self.client
                    .steer(
                        self.owner.as_ref().ok_or(ERROR)?,
                        params,
                        &uuid::Uuid::new_v4().to_string(),
                    )
                    .await
            }
            "turn/interrupt" => {
                let turn = params["turnId"].as_str().ok_or(ERROR)?;
                self.client
                    .stop(self.owner.as_ref().ok_or(ERROR)?, turn)
                    .await
            }
            _ => Err("This operation is not available through the Desktop-owner connection".into()),
        }
    }
    async fn metadata(&mut self, native: &str) -> Result<(Value, Value), String> {
        let mut thread = self.reader.metadata(native).await?;
        let mut page = self
            .reader
            .rpc(
                "thread/turns/list",
                json!({"threadId":native,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            )
            .await?;
        if page["data"].as_array().is_some_and(|a| !a.is_empty()) {
            let path = PathBuf::from(thread["path"].as_str().ok_or(ERROR)?);
            let profile = self.profile.clone();
            let id = native.to_owned();
            let tracker = self.lifecycle.clone();
            let (turn, status) = tokio::task::spawn_blocking(move || {
                tracker
                    .lock()
                    .map_err(|_| ERROR)?
                    .read(&profile, &path, &id)
            })
            .await
            .map_err(|_| ERROR)??;
            if page["data"][0]["id"] != turn {
                return Err(ERROR.into());
            }
            page["data"][0]["status"] = json!(status);
        }
        let turns = page["data"]
            .as_array()
            .filter(|a| a.len() <= 1)
            .ok_or(ERROR)?;
        let state = match turns.first().and_then(|t| t["status"].as_str()) {
            None if turns.is_empty() => "idle",
            Some("completed" | "interrupted" | "failed") => "idle",
            Some("inProgress") => "active",
            _ => return Err(ERROR.into()),
        };
        thread["status"] = json!({"type":state});
        if state == "active" {
            let owner = self.client.owner(native).await?;
            if let Some((turn, item)) = self
                .client
                .compacting(&owner, page["data"][0]["id"].as_str().ok_or(ERROR)?)
                .await?
            {
                if page["data"][0]["id"] == turn {
                    thread["remodexLiveCompaction"] = json!({"turnId":turn,"itemId":item});
                }
            }
        }
        Ok((thread, page))
    }
    async fn poll(&mut self, output: &mpsc::Sender<Event>) -> Result<(), String> {
        if self.turn.is_empty() {
            return Ok(());
        }
        let (metadata, page) = self.metadata(&self.native.clone()).await?;
        let requests = self
            .client
            .pending_requests(self.owner.as_ref().ok_or(ERROR)?)
            .await?;
        let mut next = BTreeMap::new();
        for request in requests {
            if request["params"]
                .get("turnId")
                .is_some_and(|t| t != &self.turn)
            {
                return Err("Desktop control does not belong to the current task".into());
            }
            let key = request["id"].to_string();
            if let Some(prior) = self.requests.get(&key) {
                if prior != &request {
                    return Err("Desktop reused a pending request identity".into());
                }
            } else {
                send(output, request.clone()).await?;
            }
            next.insert(key, request);
        }
        for (key, prior) in &self.requests {
            if !next.contains_key(key) {
                send(output, json!({"method":"serverRequest/resolved","params":{"threadId":self.native,"requestId":prior["id"]}})).await?;
            }
        }
        self.requests = next;
        if metadata["remodexLiveCompaction"]["turnId"] == self.turn {
            send(output, json!({"method":"item/started","params":{"threadId":self.native,"turnId":self.turn,
                "item":{"type":"contextCompaction","id":metadata["remodexLiveCompaction"]["itemId"]}}})).await?;
        }
        let turns = page["data"]
            .as_array()
            .filter(|v| v.len() <= 1)
            .ok_or(ERROR)?;
        // A Desktop prompt can begin before we observe the preceding terminal.
        // Complete the expected turn from its native event, never from whichever
        // turn happened to be latest when this poll ran.
        let old_turn = self
            .lifecycle
            .lock()
            .map_err(|_| ERROR)?
            .state(&self.turn)
            .map(|status| json!({"id":self.turn,"status":status}));
        let turn = turns
            .first()
            .filter(|t| t["id"] == self.turn)
            .or(old_turn.as_ref())
            .ok_or(ERROR)?;
        let mut cursor = Value::Null;
        let mut cursors = std::collections::BTreeSet::new();
        for _ in 0..256 {
            let page=self.reader.rpc("thread/items/list",json!({"threadId":self.native,"turnId":self.turn,"cursor":cursor,"limit":8,"sortDirection":"asc"})).await?;
            let rows = page["data"]
                .as_array()
                .filter(|v| v.len() <= 8)
                .ok_or(ERROR)?;
            for row in rows {
                let item = row.get("item").unwrap_or(row);
                let id = item["id"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .ok_or(ERROR)?;
                let complete = !row["completedAtMs"].is_null() || turn["status"] != "inProgress";
                let hash = crate::auth::digest(&json!([item, complete]).to_string());
                if self.items.get(id) == Some(&hash) {
                    continue;
                }
                if self.items.len() >= 2048 {
                    return Err("Desktop turn exceeds its bounded live item inventory; saved history remains available".into());
                }
                let method = if complete {
                    "item/completed"
                } else {
                    "item/started"
                };
                send(output,json!({"method":method,"params":{"threadId":self.native,"turnId":self.turn,"item":item}})).await?;
                self.items.insert(id.into(), hash);
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                if matches!(
                    turn["status"].as_str(),
                    Some("completed" | "interrupted" | "failed")
                ) {
                    send(output,json!({"method":"turn/completed","params":{"threadId":self.native,"turn":turn}})).await?;
                    self.turn.clear();
                    self.items.clear();
                }
                return Ok(());
            }
            let token = cursor
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 8192)
                .ok_or(ERROR)?;
            if rows.is_empty() || !cursors.insert(token.to_owned()) {
                return Err(ERROR.into());
            }
        }
        Err(ERROR.into())
    }
}
async fn send(output: &mpsc::Sender<Event>, value: Value) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(5), output.send(Event::Message(value)))
        .await
        .map_err(|_| ERROR)?
        .map_err(|_| ERROR.into())
}
impl Connection {
    pub(crate) async fn connect_desktop(
        endpoint: Endpoint,
        profile: PathBuf,
        executable: PathBuf,
        cwd: PathBuf,
    ) -> Result<Self, String> {
        let client = endpoint.connect(&profile).await?;
        let config = crate::chat::AgentConfig {
            executable,
            profile: profile.clone(),
            model: String::new(),
            tools: Default::default(),
            shared_server: None,
            remote_control: false,
        };
        let reader = Reader::open(&config, &cwd).await?;
        let mut desktop = Desktop {
            client,
            profile,
            reader,
            native: String::new(),
            owner: None,
            turn: String::new(),
            items: BTreeMap::new(),
            requests: BTreeMap::new(),
            lifecycle: Default::default(),
        };
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(750));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _=stopping.changed()=>break,
                    packet=input.recv()=>{
                        let Some((bytes,reply))=packet else{break;};
                        let request=serde_json::from_slice::<Value>(&bytes);
                        let Ok(request)=request else{let _=reply.send(Err(io::ErrorKind::InvalidData.into()));break;};
                        if request.get("method").is_none() {
                            let result=match desktop.owner.as_ref() {
                                Some(owner)=>desktop.client.respond(owner,&request).await,
                                None=>Err(ERROR.into()),
                            };
                            let failed=result.is_err();
                            let _=reply.send(result.map_err(io::Error::other));
                            if failed {break;}
                            continue;
                        }
                        let _=reply.send(Ok(()));
                        let result=desktop.request(request["method"].as_str().unwrap_or(""),request["params"].clone()).await;
                        if request.get("id").is_none() {if result.is_err(){break;}continue;}
                        let response=match result {Ok(value)=>json!({"id":request["id"],"result":value}),Err(error)=>json!({"id":request["id"],"error":{"code":-32000,"message":error}})};
                        if send(&output,response).await.is_err(){break;}
                    }
                    _=interval.tick(),if !desktop.turn.is_empty()=>{if desktop.poll(&output).await.is_err(){break;}},
                }
            }
            desktop.reader.close().await;
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                output.send(Event::Closed {
                    code: None,
                    forced: false,
                }),
            )
            .await;
        });
        Ok(Self {
            writes,
            events,
            stop,
            task: Some(task),
        })
    }
}
