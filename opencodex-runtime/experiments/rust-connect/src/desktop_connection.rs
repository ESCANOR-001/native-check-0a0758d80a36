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
// Shared history connections are short lived; a per-connection cache alone
// would poll the account API on every history refresh. Bound retained profiles
// and reserve refreshes before awaiting, so concurrent readers share the result.
#[derive(Default)]
struct QuotaCache(BTreeMap<PathBuf, (std::time::Instant, Option<Value>)>);
impl QuotaCache {
    fn claim(
        &mut self,
        profile: &std::path::Path,
        now: std::time::Instant,
    ) -> (bool, Option<Value>) {
        if let Some((at, value)) = self.0.get_mut(profile) {
            if now.duration_since(*at) < Duration::from_secs(60) {
                return (false, value.clone());
            }
            *at = now;
            return (true, value.clone());
        }
        if self.0.len() >= 8 {
            if let Some(old) = self
                .0
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                self.0.remove(&old);
            }
        }
        self.0.insert(profile.into(), (now, None));
        (true, None)
    }
}
static QUOTAS: std::sync::OnceLock<std::sync::Mutex<QuotaCache>> = std::sync::OnceLock::new();

struct Desktop {
    client: Option<Client>,
    history_only: bool,
    live_status: bool,
    profile: PathBuf,
    reader: Reader,
    activation: Option<crate::desktop_activation::Activation>,
    native: String,
    owner: Option<Owner>,
    turn: String,
    compaction: Option<String>,
    items: BTreeMap<String, String>,
    requests: BTreeMap<String, Value>,
    reported_usage: Option<Value>,
    reported_limits: Option<Value>,
    lifecycle: std::sync::Arc<std::sync::Mutex<crate::codex_lifecycle::Tracker>>,
}
impl Desktop {
    async fn account_limits(&mut self) -> Option<Value> {
        let cache = QUOTAS.get_or_init(Default::default);
        let (refresh, previous) = cache
            .lock()
            .ok()?
            .claim(&self.profile, std::time::Instant::now());
        if !refresh {
            return previous;
        }
        // One optional account read across all history readers for this profile.
        // An old chat's recorded allowance must never replace current quota.
        if let Ok(Ok(mut value)) = tokio::time::timeout(
            Duration::from_secs(2),
            self.reader.rpc("account/rateLimits/read", json!({})),
        )
        .await
        {
            if value.is_object() && value.to_string().len() <= 65536 {
                value["remodexCheckedAt"] = json!(crate::chat_projection::now());
                if let Ok(mut cache) = cache.lock() {
                    if let Some((_, saved)) = cache.0.get_mut(&self.profile) {
                        *saved = Some(value.clone());
                    }
                }
                return Some(value);
            }
        }
        previous
    }

    async fn load_owner(&mut self, native: &str, allow_activation: bool) -> Result<Owner, String> {
        if let Some(owner) = self
            .client
            .as_mut()
            .ok_or(ERROR)?
            .find_owner(native)
            .await?
        {
            return Ok(owner);
        }
        let activation = self
            .activation
            .as_ref()
            .filter(|_| allow_activation)
            .ok_or(ERROR)?;
        // Only a conclusive no-client-found may open this existing route. This
        // cannot replay a prompt or create a competing CLI owner.
        tokio::time::timeout(Duration::from_secs(9), async {
            activation.open(&self.profile, native).await?;
            for delay in [0, 100, 250, 500, 750, 1000, 1500, 2000] {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if let Some(owner) = self
                    .client
                    .as_mut()
                    .ok_or(ERROR)?
                    .find_owner(native)
                    .await?
                {
                    return Ok(owner);
                }
            }
            Err(ERROR.to_owned())
        })
        .await
        .map_err(|_| ERROR)?
    }

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
        if self.history_only
            && !matches!(
                method,
                "thread/read" | "thread/turns/list" | "thread/items/list"
            )
        {
            return Err("Saved-history connections cannot control a Desktop chat".into());
        }
        match method {
            "thread/read" => {
                if params["includeTurns"] != false {
                    return Err("Read Desktop history using bounded turn and item pages".into());
                }
                // Discovery proves the original owner is connected; the bounded
                // native turn page provides execution state. Never resume it.
                let activate = params["remodexAllowActivation"] == true;
                let owner = if self.history_only && !activate {
                    match tokio::time::timeout(
                        Duration::from_secs(2),
                        self.load_owner(native, false),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            // A dormant Desktop router must not delay saved
                            // content. Discard the partially read IPC stream;
                            // no uncertain discovery grants execution rights.
                            self.client = None;
                            Err(ERROR.into())
                        }
                    }
                } else {
                    self.load_owner(native, activate).await
                };
                self.live_status = owner.is_ok();
                if !self.history_only {
                    owner?;
                }
                let (mut thread, _) = self.metadata(native).await?;
                thread["remodexRateLimits"] = self.account_limits().await.unwrap_or(Value::Null);
                // Saved question reconciliation is optional history work. Never
                // subscribe to a potentially large snapshot on the send preflight.
                if self.history_only && self.live_status {
                    if let Some(client) = self.client.as_mut() {
                        if let Some(owner) = client.find_owner(native).await.ok().flatten() {
                            if let Ok(replies) = client.question_replies(&owner).await {
                                thread["remodexQuestionReplies"] = json!(replies);
                            }
                        }
                    }
                }
                Ok(json!({"thread":thread}))
            }
            "thread/turns/list" => {
                let (_, latest) = self.metadata(native).await?;
                let summary = params["itemsView"] == "summary";
                let mut page = self.reader.rpc(method, params.clone()).await?;
                let async_owner = if summary && (!self.history_only || self.live_status) {
                    if let Some(client) = self.client.as_mut() {
                        client.find_owner(native).await.ok().flatten()
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(rows) = page["data"].as_array_mut() {
                    for row in rows {
                        if summary && row["items"].is_array() {
                            if let (Some(client), Some(owner)) =
                                (self.client.as_mut(), async_owner.as_ref())
                            {
                                let turn = row["id"].as_str().ok_or(ERROR)?.to_owned();
                                for item in client.questions(owner, &turn).await? {
                                    let items = row["items"].as_array_mut().unwrap();
                                    if !items.iter().any(|i| i["id"] == item["id"])
                                        && items.len() < 64
                                    {
                                        items.push(item);
                                    }
                                }
                            }
                        }
                        if row["id"] == latest["data"][0]["id"] {
                            row["status"] = latest["data"][0]["status"].clone();
                            row["error"] = latest["data"][0]["error"].clone();
                        } else if let Some(turn) = row["id"].as_str() {
                            let tracker = self.lifecycle.lock().map_err(|_| ERROR)?;
                            if let Some(error) = tracker.error(turn) {
                                if row["status"] != "interrupted" {
                                    row["status"] = json!("failed");
                                }
                                row["error"] = error;
                            }
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
                let owner = self.client.as_mut().ok_or(ERROR)?.owner(native).await?;
                let (thread, page) = self.metadata(native).await?;
                self.native = native.into();
                self.owner = Some(owner);
                if page["data"][0]["status"] == "inProgress" {
                    self.turn = page["data"][0]["id"].as_str().ok_or(ERROR)?.into();
                    self.compaction = thread["remodexLiveCompaction"]["itemId"]
                        .as_str()
                        .map(str::to_owned);
                    // The shared-history window was committed before attachment.
                    // Seed its hashes so the follower does not re-emit saved
                    // rows with new arrival timestamps or replay old activity.
                    let tail=self.reader.rpc("thread/items/list",json!({"threadId":native,"turnId":self.turn,"limit":8,"sortDirection":"desc"})).await?;
                    for row in tail["data"]
                        .as_array()
                        .filter(|a| a.len() <= 8)
                        .ok_or(ERROR)?
                    {
                        if row["turnId"] != self.turn {
                            return Err(ERROR.into());
                        }
                        let item = &row["item"];
                        let id = item["id"].as_str().ok_or(ERROR)?;
                        self.items.insert(
                            id.into(),
                            crate::auth::digest(
                                &json!([item, !row["completedAtMs"].is_null()]).to_string(),
                            ),
                        );
                    }
                }
                Ok(json!({"thread":thread,"initialTurnsPage":page}))
            }
            "thread/compact/start" => {
                let owner = self.load_owner(native, true).await?;
                let (_, page) = self.metadata(native).await?;
                if page["data"][0]["status"] == "inProgress" {
                    return Err("Wait for the current task to finish before using /compact.".into());
                }
                self.client.as_mut().ok_or(ERROR)?.compact(&owner).await
            }
            "turn/start" => {
                let owner = self.owner.as_ref().ok_or(ERROR)?;
                let result = self
                    .client
                    .as_mut()
                    .ok_or(ERROR)?
                    .start(owner, params, &uuid::Uuid::new_v4().to_string())
                    .await?;
                let result = result.get("result").unwrap_or(&result).clone();
                self.turn = result["turn"]["id"].as_str().ok_or(ERROR)?.into();
                self.items.clear();
                Ok(result)
            }
            "turn/steer" => {
                self.client
                    .as_mut()
                    .ok_or(ERROR)?
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
                    .as_mut()
                    .ok_or(ERROR)?
                    .stop(self.owner.as_ref().ok_or(ERROR)?, turn)
                    .await
            }
            _ => Err("This operation is not available through the Desktop-owner connection".into()),
        }
    }
    async fn metadata(&mut self, native: &str) -> Result<(Value, Value), String> {
        let mut thread = self.reader.metadata(native).await?;
        let mut terminal_proven = false;
        let mut page = self
            .reader
            .native_rpc(
                "thread/turns/list",
                json!({"threadId":native,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            )
            .await?;
        let empty_page = page["data"].as_array().is_some_and(Vec::is_empty);
        if !empty_page || self.history_only {
            let path = PathBuf::from(thread["path"].as_str().ok_or(ERROR)?);
            let profile = self.profile.clone();
            let id = native.to_owned();
            let tracker = self.lifecycle.clone();
            let lifecycle = tokio::task::spawn_blocking(move || {
                let mut tracker = tracker.lock().map_err(|_| ERROR)?;
                let (turn, status) = tracker.read(&profile, &path, &id)?;
                let error = tracker.error(&turn);
                let saved = tracker.saved_terminal();
                Ok::<_, String>((turn, status, error, saved))
            })
            .await
            .map_err(|_| ERROR.to_owned())
            .and_then(|result| result);
            match lifecycle {
                Ok((_, _, _, Some(saved))) if empty_page => {
                    // Windows profiles migrated without their history index can
                    // return zero turns despite intact native completion events.
                    // Keep this status proof separate from the empty history page.
                    terminal_proven = true;
                    thread["remodexSavedTerminal"] = saved;
                }
                Ok((turn, status, error, _)) if page["data"][0]["id"] == turn => {
                    terminal_proven =
                        matches!(status.as_str(), "completed" | "interrupted" | "failed");
                    let row = &mut page["data"][0];
                    if status != "completed" || crate::codex_error::thin(&row["error"]).is_none() {
                        row["status"] = json!(status);
                    } else {
                        row["status"] = json!("failed");
                    }
                    if let Some(error) = error {
                        row["error"] = error;
                    }
                }
                _ if self.history_only => self.live_status = false,
                _ => return Err(ERROR.into()),
            }
        }
        if page["data"][0]["status"] == "inProgress" && self.live_status {
            let observed = async {
                let owner = self.client.as_mut().ok_or(ERROR)?.owner(native).await?;
                self.client
                    .as_mut()
                    .ok_or(ERROR)?
                    .task_status(&owner, page["data"][0]["id"].as_str().ok_or(ERROR)?)
                    .await
            }
            .await;
            match observed {
                Ok(Some((status, error))) => {
                    // A restarted owner may settle an old native task_started.
                    terminal_proven =
                        matches!(status.as_str(), "completed" | "interrupted" | "failed");
                    page["data"][0]["status"] = json!(status);
                    if let Some(error) = error {
                        page["data"][0]["error"] = error;
                    }
                }
                Ok(None) => {}
                // Live status failures must not hide independently readable
                // saved messages or grant the private reader control authority.
                Err(_) if self.history_only => self.live_status = false,
                Err(e) => return Err(e),
            }
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
        thread["remodexLiveStatusKnown"] = json!(!self.history_only || self.live_status);
        // A saved native terminal event proves the task ended even when its
        // Desktop controller is dormant. This grants no execution authority.
        thread["remodexTaskStatusKnown"] =
            json!(!self.history_only || self.live_status || terminal_proven);
        if state == "active" && (!self.history_only || self.live_status) {
            let phase = async {
                let owner = self.client.as_mut().ok_or(ERROR)?.owner(native).await?;
                self.client
                    .as_mut()
                    .ok_or(ERROR)?
                    .compacting(&owner, page["data"][0]["id"].as_str().ok_or(ERROR)?)
                    .await
            }
            .await;
            match phase {
                Ok(Some((turn, item))) if page["data"][0]["id"] == turn => {
                    thread["remodexLiveCompaction"] = json!({"turnId":turn,"itemId":item});
                }
                Ok(_) => {}
                Err(_) if self.history_only => {
                    self.live_status = false;
                    thread["remodexLiveStatusKnown"] = json!(false);
                    thread["remodexTaskStatusKnown"] = json!(terminal_proven);
                }
                Err(e) => return Err(e),
            }
        }
        thread["remodexTokenUsage"] = self
            .lifecycle
            .lock()
            .map_err(|_| ERROR)?
            .usage()
            .unwrap_or(Value::Null);
        Ok((thread, page))
    }
    async fn poll(&mut self, output: &mut Outbox) -> Result<(), String> {
        if self.turn.is_empty() {
            return Ok(());
        }
        let (metadata, page) = self.metadata(&self.native.clone()).await?;
        let usage = &metadata["remodexTokenUsage"];
        if !usage.is_null() && self.reported_usage.as_ref() != Some(usage) {
            send(output, json!({"method":"thread/tokenUsage/updated","params":{"threadId":self.native,"turnId":self.turn,"tokenUsage":usage}})).await?;
            self.reported_usage = Some(usage.clone());
        }
        let limits = self.account_limits().await.unwrap_or(Value::Null);
        if !limits.is_null() && self.reported_limits.as_ref() != Some(&limits) {
            send(
                output,
                json!({"method":"account/rateLimits/updated","params":limits}),
            )
            .await?;
            self.reported_limits = Some(limits.clone());
        }
        let requests = self
            .client
            .as_mut()
            .ok_or(ERROR)?
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
        let compact = metadata["remodexLiveCompaction"]["turnId"]
            .as_str()
            .filter(|id| *id == self.turn)
            .and_then(|_| metadata["remodexLiveCompaction"]["itemId"].as_str())
            .map(str::to_owned);
        if self.compaction != compact {
            if let Some(item) = self.compaction.take() {
                send(output,json!({"method":"item/completed","params":{"threadId":self.native,"turnId":self.turn,"item":{"type":"contextCompaction","id":item}}})).await?;
            }
        }
        if let Some(item) = &compact {
            send(output,json!({"method":"item/started","params":{"threadId":self.native,"turnId":self.turn,"item":{"type":"contextCompaction","id":item}}})).await?;
        }
        self.compaction = compact;
        let turns = page["data"]
            .as_array()
            .filter(|v| v.len() <= 1)
            .ok_or(ERROR)?;
        // A Desktop prompt can begin before we observe the preceding terminal.
        // Complete the expected turn from its native event, never from whichever
        // turn happened to be latest when this poll ran.
        let old_turn = {
            let tracker = self.lifecycle.lock().map_err(|_| ERROR)?;
            tracker.state(&self.turn).map(
                |status| json!({"id":self.turn,"status":status,"error":tracker.error(&self.turn)}),
            )
        };
        let turn = turns
            .first()
            .filter(|t| t["id"] == self.turn)
            .or(old_turn.as_ref())
            .ok_or(ERROR)?;
        // Saved content is paged separately on demand. A live follower only
        // needs the recent tail and pending controls, never a replay of every
        // command since the beginning of this task.
        let page = self
            .reader
            .rpc(
                "thread/items/list",
                json!({"threadId":self.native,"turnId":self.turn,"limit":8,"sortDirection":"desc"}),
            )
            .await?;
        let rows = page["data"]
            .as_array()
            .filter(|v| v.len() <= 8)
            .ok_or(ERROR)?;
        let mut present = std::collections::BTreeSet::new();
        for row in rows.iter().rev() {
            if row["turnId"] != self.turn {
                return Err(ERROR.into());
            }
            let item = &row["item"];
            let id = item["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or(ERROR)?;
            present.insert(id.to_owned());
            let complete = !row["completedAtMs"].is_null() || turn["status"] != "inProgress";
            let hash = crate::auth::digest(&json!([item, complete]).to_string());
            if self.items.get(id) == Some(&hash) {
                continue;
            }
            let method = if complete {
                "item/completed"
            } else {
                "item/started"
            };
            send(output,json!({"method":method,"params":{"threadId":self.native,"turnId":self.turn,"item":item,"startedAtMs":row["startedAtMs"],"completedAtMs":row["completedAtMs"]}})).await?;
            self.items.insert(id.into(), hash);
        }
        for item in self
            .client
            .as_mut()
            .ok_or(ERROR)?
            .questions(self.owner.as_ref().ok_or(ERROR)?, &self.turn)
            .await?
        {
            let id = item["id"].as_str().ok_or(ERROR)?;
            present.insert(id.to_owned());
            let hash = crate::auth::digest(&json!([item, true]).to_string());
            if self.items.get(id) != Some(&hash) {
                send(output,json!({"method":"item/completed","params":{"threadId":self.native,"turnId":self.turn,"item":item}})).await?;
                self.items.insert(id.into(), hash);
            }
        }
        self.items.retain(|id, _| present.contains(id));
        if matches!(
            turn["status"].as_str(),
            Some("completed" | "interrupted" | "failed")
        ) {
            send(
                output,
                json!({"method":"turn/completed","params":{"threadId":self.native,"turn":turn}}),
            )
            .await?;
            self.turn.clear();
            self.items.clear();
        }
        Ok(())
    }
}
// Drain notifications independently from input admission. Waiting on a full
// receive channel inside poll/request deadlocks a caller that is awaiting its
// next write acknowledgement before reading more events.
#[derive(Default)]
struct Outbox {
    rows: std::collections::VecDeque<(Value, usize)>,
    bytes: usize,
}
impl Outbox {
    fn push(&mut self, value: Value) -> Result<(), String> {
        let bytes = value.to_string().len();
        if self.rows.len() >= 1024 || self.bytes.saturating_add(bytes) > 16 * 1024 * 1024 {
            return Err("Desktop updates exceeded the bounded buffer; reconnecting".into());
        }
        self.bytes += bytes;
        self.rows.push_back((value, bytes));
        Ok(())
    }
    fn pop(&mut self) -> Event {
        let (value, bytes) = self.rows.pop_front().expect("nonempty outbox");
        self.bytes -= bytes;
        Event::Message(value)
    }
}
async fn send(output: &mut Outbox, value: Value) -> Result<(), String> {
    output.push(value)
}
impl Connection {
    pub(crate) async fn connect_desktop(
        endpoint: Endpoint,
        profile: PathBuf,
        executable: PathBuf,
        cwd: PathBuf,
        activation: Option<crate::desktop_activation::Activation>,
        history_only: bool,
    ) -> Result<Self, String> {
        let client = match endpoint.connect(&profile).await {
            Ok(client) => Some(client),
            Err(_) if history_only => None,
            Err(error) => return Err(error),
        };
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
            history_only,
            live_status: false,
            profile,
            reader,
            activation,
            native: String::new(),
            owner: None,
            turn: String::new(),
            compaction: None,
            reported_usage: None,
            reported_limits: None,
            items: BTreeMap::new(),
            requests: BTreeMap::new(),
            lifecycle: Default::default(),
        };
        let (writes, mut input) = mpsc::channel::<Write>(2);
        let (output, events) = mpsc::channel(2);
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut pending = Outbox::default();
            let mut interval = tokio::time::interval(Duration::from_millis(750));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _=stopping.changed()=>break,
                    permit=output.reserve(),if !pending.rows.is_empty()=>{
                        let Ok(permit)=permit else {break;};
                        permit.send(pending.pop());
                    }
                    packet=input.recv()=>{
                        let Some((bytes,reply))=packet else{break;};
                        let request=serde_json::from_slice::<Value>(&bytes);
                        let Ok(request)=request else{let _=reply.send(Err(io::ErrorKind::InvalidData.into()));break;};
                        if request.get("method").is_none() {
                            let result=match desktop.owner.as_ref() {
                                Some(owner) if !desktop.history_only => match desktop.client.as_mut() { Some(client) => client.respond(owner,&request).await, None => Err(ERROR.into()) },
                                _=>Err(ERROR.into()),
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
                        if pending.push(response).is_err(){break;}
                    }
                    _=interval.tick(),if !desktop.turn.is_empty() && pending.rows.is_empty()=>{if desktop.poll(&mut pending).await.is_err(){break;}},
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

#[cfg(test)]
mod usage_tests {
    use super::*;
    #[test]
    fn account_quota_is_shared_between_readers_bounded_and_profile_scoped() {
        let mut cache = QuotaCache::default();
        let now = std::time::Instant::now();
        let p = std::path::Path::new("profile-a");
        assert_eq!(cache.claim(p, now), (true, None));
        // Concurrent opens do not each start an API request.
        assert_eq!(cache.claim(p, now), (false, None));
        cache.0.get_mut(p).unwrap().1 = Some(json!({"rateLimits":{"primary":{"usedPercent":25}}}));
        assert_eq!(
            cache.claim(p, now).1.unwrap()["rateLimits"]["primary"]["usedPercent"],
            25
        );
        assert_eq!(
            cache.claim(std::path::Path::new("profile-b"), now),
            (true, None)
        );
        assert!(cache.claim(p, now + Duration::from_secs(60)).0);
        for n in 0..20 {
            cache.claim(
                &PathBuf::from(format!("profile-{n}")),
                now + Duration::from_secs(61 + n),
            );
        }
        assert_eq!(cache.0.len(), 8);
    }
}
