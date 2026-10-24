use futures_util::{SinkExt, StreamExt};
use remodex_connect_native::{
    agent::SharedServer,
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct HistoryGate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct Fixture {
    _dir: tempfile::TempDir,
    hub: Chats,
    config: Config,
    db: AuthService,
    turns: Arc<Mutex<Vec<Value>>>,
    log: Arc<Mutex<Vec<Value>>>,
    corrupt: Arc<Mutex<bool>>,
    offline: Arc<AtomicBool>,
    notices: Arc<Mutex<Vec<Value>>>,
    history_gate: Arc<HistoryGate>,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "fixture-private-shared-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let turns: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let log = Arc::new(Mutex::new(vec![]));
        let corrupt = Arc::new(Mutex::new(false));
        let offline = Arc::new(AtomicBool::new(false));
        let unavailable = offline.clone();
        let ts = turns.clone();
        let logs = log.clone();
        let bad = corrupt.clone();
        let notices: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let outbound = notices.clone();
        let missing = dir.path().join("missing-native");
        let history_gate = Arc::new(HistoryGate::default());
        let gate = history_gate.clone();
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                if unavailable.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                let offline = unavailable.clone();
                let turns = ts.clone();
                let log = logs.clone();
                let corrupt = bad.clone();
                let notices = outbound.clone();
                let missing = missing.clone();
                let gate = gate.clone();
                clients.spawn(async move {
   // A canceled history read may close before the handshake completes.
   let Ok(mut ws)=tokio_tungstenite::accept_async(stream).await else {return;};
   let mut subscribed:Option<String>=None;let mut sent=Value::Null;let mut notice_index=0;let mut pulse=tokio::time::interval(Duration::from_millis(30));
   loop {
    if offline.load(Ordering::SeqCst) {let _=ws.close(None).await;break;}
    let incoming=tokio::select! {
     message=ws.next()=>message,
     _=pulse.tick(),if subscribed.is_some()=>{
      let t=turns.lock().unwrap().iter().find(|t|t["id"]==subscribed.as_deref().unwrap()).cloned();
      if let Some(t)=t {if t!=sent {
       for row in t["items"].as_array().unwrap() {
        let event=json!({"method":if t["status"]=="inProgress"{"item/started"}else{"item/completed"},"params":{"threadId":"shared-native","turnId":t["id"],"item":row["item"]}});
        if ws.send(Message::Text(event.to_string().into())).await.is_err(){return;}
       }
       if t["status"]!="inProgress" {
        // Final text and turn completion are distinct native events. Exercise
        // the scheduling gap observed on Windows instead of relying on both
        // events being consumed before the next snapshot request.
        if t["id"]=="stream" {tokio::time::sleep(Duration::from_millis(35)).await;}
        let event=json!({"method":"turn/completed","params":{"threadId":"shared-native","turn":t}});if ws.send(Message::Text(event.to_string().into())).await.is_err(){return;}}
       sent=t;
      }}
      let pending:Vec<_>=notices.lock().unwrap().iter().skip(notice_index).cloned().collect();notice_index+=pending.len();
      for v in pending {if ws.send(Message::Text(v.to_string().into())).await.is_err(){return;}}
      continue;
     }
    };
    let Some(Ok(Message::Text(text)))=incoming else {break};
    let v:Value=serde_json::from_str(&text).unwrap();log.lock().unwrap().push(v.clone());let method=v["method"].as_str().unwrap_or("");
    if missing.exists() && ["thread/read","thread/resume"].contains(&method) {
        let reason=std::fs::read_to_string(&missing).unwrap();
        let message=if reason=="auth" {"authorization unavailable"}else if method=="thread/read" {"thread not loaded: shared-native"}else{"no rollout found for thread id shared-native"};
        ws.send(Message::Text(json!({"id":v["id"],"error":{"code":-32602,"message":message}}).to_string().into())).await.unwrap();continue;
    }
    if method=="thread/start" {let _=std::fs::remove_file(&missing);}
    let result=match method {
     "initialize"=>json!({}),"initialized"=>continue,
     "model/list"=>json!({"data":[{"model":"test","displayName":"Test","supportedReasoningEfforts":[]}]}),"account/rateLimits/read"=>json!({}),
     "thread/start"|"thread/resume"|"thread/read"=>{
      let ts=turns.lock().unwrap();let busy=ts.iter().any(|t|t["status"]=="inProgress");
      let mut result=json!({"thread":{"id":"shared-native","status":{"type":if busy{"active"}else{"idle"}}}});
      let rollout=missing.parent().unwrap().join("sessions/rollout.jsonl");
      if rollout.exists() {result["thread"]["path"]=json!(rollout);}
      if let Some(flags)=ts.last().and_then(|t|t.get("activeFlags")) {result["thread"]["status"]["activeFlags"]=flags.clone();}
      if v["id"]=="attach-resume" {
       assert_eq!(v["params"].as_object().unwrap().len(),3);assert_eq!(v["params"]["excludeTurns"],true);
       let mut t=ts.last().unwrap().clone();subscribed=Some(t["id"].as_str().unwrap().into());t["items"]=json!([]);
       result["initialTurnsPage"]=json!({"data":[t],"nextCursor":null});result["model"]=json!("test");result["reasoningEffort"]=Value::Null;result["approvalPolicy"]=json!("on-request");
      }
      result
     },
     "turn/steer"=>{let mut ts=turns.lock().unwrap();let t=ts.last_mut().unwrap();assert_eq!(v["params"]["expectedTurnId"],t["id"]);let n=t["items"].as_array().unwrap().len();let tid=t["id"].clone();t["items"].as_array_mut().unwrap().push(json!({"turnId":tid,"item":{"id":format!("steered-{n}"),"type":"userMessage","clientId":v["params"]["clientUserMessageId"],"content":[{"type":"text","text":v["params"]["input"][0]["text"]}]},"startedAtMs":remodex_connect_native::auth::now()}));json!({"turnId":tid})},
     "turn/interrupt"=>{let mut ts=turns.lock().unwrap();let t=ts.last_mut().unwrap();assert_eq!(v["params"]["turnId"],t["id"]);t["status"]=json!("interrupted");t["completedAt"]=json!(remodex_connect_native::auth::now()/1000);json!({})},
     ""=>continue,
     "turn/start"=>{let n=turns.lock().unwrap().len();let id=format!("owned-{n}");let held=v["params"]["input"][0]["text"]=="hold_owned";let at=remodex_connect_native::auth::now()/1000;
      turns.lock().unwrap().push(json!({"id":id,"status":if held{"inProgress"}else{"completed"},"startedAt":if held{at}else{1700000000},"completedAt":if held{Value::Null}else{json!(1700000001)},"items":if held{json!([{ "turnId":id,"item":{"id":"held-native-prompt","clientId":"hold_owned","type":"userMessage","content":[{"type":"text","text":"hold_owned"}]}}])}else{json!([])}}));if held{subscribed=Some(id.clone());}json!({"turn":{"id":id}})},
     "thread/turns/list"=>{
      assert_eq!(v["params"]["itemsView"],"notLoaded");assert_eq!(v["params"]["limit"],1);assert_eq!(v["params"]["sortDirection"],"asc");
      let index=v["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);let ts=turns.lock().unwrap();
      let data:Vec<_>=ts.get(index).map(|t|{let mut t=t.clone();t["items"]=json!([]);t}).into_iter().collect();
      json!({"data":data,"nextCursor":if index+1<ts.len(){json!((index+1).to_string())}else{Value::Null}})
     },
     "thread/items/list"=>{
      assert_eq!(v["params"]["limit"],8);assert_eq!(v["params"]["sortDirection"],"asc");
      let ts=turns.lock().unwrap();let t=ts.iter().find(|t|t["id"]==v["params"]["turnId"]).unwrap();let rows=t["items"].as_array().unwrap();
      let index=v["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
      let mut page:Vec<_>=rows.iter().skip(index).take(8).cloned().collect();if *corrupt.lock().unwrap() {if let Some(row)=page.first_mut(){row["turnId"]=json!("foreign-turn");}}
      json!({"data":page,"nextCursor":if index+8<rows.len(){json!((index+8).to_string())}else{Value::Null}})
     },
     _=>panic!("Unexpected shared RPC {method}"),
    };
    if method=="thread/turns/list" && gate.armed.swap(false, Ordering::SeqCst) {
     gate.entered.notify_one();gate.release.notified().await;
    }
    if ws.send(Message::Text(json!({"id":v["id"],"result":result}).to_string().into())).await.is_err(){break;}
    if method=="turn/start" && v["params"]["input"][0]["text"]!="hold_owned" {
     let id=result["turn"]["id"].clone();
     let event=json!({"method":"item/completed","params":{"threadId":"shared-native","turnId":id,"item":{"id":format!("reply-{id}"),"type":"agentMessage","text":"OWN_REPLY"}}});
     if ws.send(Message::Text(event.to_string().into())).await.is_err(){break;}
     let event=json!({"method":"turn/completed","params":{"threadId":"shared-native","turn":{"id":id,"status":"completed"}}});
     if ws.send(Message::Text(event.to_string().into())).await.is_err(){break;}
    }
   }
  });
            }
        });
        let agent = AgentConfig {
            executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
            profile: dir.path().into(),
            model: "test".into(),
            tools: Default::default(),
            shared_server: None,
            remote_control: false,
        };
        let config = Config {
            storage: Default::default(),
            native_profile_handoff: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            claude: agent.clone(),
            codex: AgentConfig {
                shared_server: Some(SharedServer::Capability {
                    address,
                    token_file: token,
                }),
                ..agent
            },
        };
        let db = AuthService::new(Auth::new(Store::open(dir.path()).unwrap())).unwrap();
        let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
        let f = Self {
            offline,
            _dir: dir,
            hub,
            db,
            config,
            turns,
            log,
            corrupt,
            notices,
            history_gate,
            server,
        };
        f.call("orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":"shared","projectId":"native-test-project"})).await.unwrap();
        f.send("initial").await.unwrap();
        f.wait_text("OWN_REPLY").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while f.detail().await["session"]["status"] != "ready" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        f
    }
    async fn call(&self, method: &str, p: Value) -> Result<Value, String> {
        self.hub.call(AgentKind::Codex, "phone", method, p).await
    }
    async fn send(&self, id: &str) -> Result<Value, String> {
        self.call("orchestration.dispatchCommand",json!({"type":"thread.turn.start","commandId":id,"threadId":"shared","message":{"messageId":id,"text":id,"attachments":[]}})).await
    }
    async fn watch(&self) {
        self.call("native.watchShared", json!({"threadId":"shared"}))
            .await
            .unwrap();
    }
    async fn detail(&self) -> Value {
        self.call(
            "orchestration.subscribeThread",
            json!({"threadId":"shared"}),
        )
        .await
        .unwrap()["snapshot"]["thread"]
            .clone()
    }
    async fn wait_text(&self, text: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                self.watch().await;
                let v = self.detail().await;
                if v.to_string().contains(text) {
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Missing {text}"))
    }
    async fn restart(&mut self) {
        self.call("native.shutdown", json!({})).await.unwrap();
        self.hub = Chats::start(self.db.clone(), self.config.clone())
            .await
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn external(id: &str, status: &str, count: usize) -> Value {
    let stamp = remodex_connect_native::auth::now() + 2000;
    let items:Vec<_>=(0..count).map(|i|json!({"turnId":id,"item":{"id":format!("{id}-{i}"),"type":if i==0{"userMessage"}else{"agentMessage"},"text":format!("{id}_ITEM_{i}"),"content":[{"type":"text","text":format!("{id}_PROMPT")} ]},"startedAtMs":stamp+i as i64,"completedAtMs":if status=="completed"{json!((stamp+1)+i as i64)}else{Value::Null}})).collect();
    json!({"id":id,"status":status,"startedAt":stamp/1000,"completedAt":if status=="completed"{json!(stamp/1000+1)}else{Value::Null},"items":items})
}
#[tokio::test]
async fn owned_turn_disconnect_recovers_without_phone_watch_cancellation_or_replay() {
    let f = Fixture::new().await;
    f.send("hold_owned").await.unwrap();
    let before = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f.detail().await;
            if v["latestTurn"]["startedAt"].is_string() && v["session"]["status"] == "running" {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    f.watch().await; // Three-second viewer lease must not own the running task.
    f.offline.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let disconnected = f.detail().await;
    assert_eq!(disconnected["session"]["statusConfidence"], "unknown");
    assert_eq!(disconnected["latestTurn"], before["latestTurn"]);
    assert_eq!(disconnected["session"]["status"], "running");
    f.call("orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"second-create","threadId":"second","projectId":"native-test-project"})).await.unwrap();
    let error=f.call("orchestration.dispatchCommand",json!({"type":"thread.turn.start","commandId":"second-send","threadId":"second","message":{"messageId":"second-prompt","text":"Must not start while the original CLI is disconnected","attachments":[]}})).await.unwrap_err();
    assert!(error.contains("Active chat limit"), "{error}");
    tokio::time::sleep(Duration::from_millis(3300)).await;
    {
        let mut turns = f.turns.lock().unwrap();
        let t = turns.last_mut().unwrap();
        t["status"] = json!("completed");
        t["completedAt"] = json!(remodex_connect_native::auth::now() / 1000);
        t["items"].as_array_mut().unwrap().push(json!({"turnId":"owned-1","item":{"id":"offline-finished","type":"agentMessage","text":"OFFLINE_FINISHED_ONCE"}}));
    }
    f.offline.store(false, Ordering::SeqCst);
    let recovered = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let v = f.detail().await;
            if v.to_string().contains("OFFLINE_FINISHED_ONCE") {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("Running task recovers after its phone lease expires");
    assert_eq!(
        recovered["latestTurn"]["turnId"],
        before["latestTurn"]["turnId"]
    );
    assert_eq!(recovered["latestTurn"]["state"], "completed");
    assert_eq!(recovered["session"]["statusConfidence"], "confirmed");
    assert_eq!(
        recovered["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["text"] == "hold_owned")
            .count(),
        1
    );
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|r| r["method"] == "turn/start").count(),
        2
    );
    assert!(!log.iter().any(|r| r["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn missed_tool_images_are_recovered_without_raw_output_and_survive_sparse_native_replay() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let mut f = Fixture::new().await;
    let bytes = b"\x89PNG\r\n\x1a\nshared-image-fixture";
    let mut turn = external("images", "completed", 2);
    turn["items"].as_array_mut().unwrap().push(json!({
        "turnId":"images","completedAtMs":1700000001000i64,
        "item":{"id":"missed-screenshot","type":"mcpToolCall","tool":"browser_screenshot",
            "arguments":{"name":"test"},"status":"completed",
            "result":{"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(bytes)},
                {"type":"text","text":"RAW_TOOL_OUTPUT_MUST_STAY_HIDDEN"}]}}
    }));
    f.turns.lock().unwrap().push(turn);
    let v = f.wait_text("images_ITEM_1").await;
    let row = v["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "missed-screenshot")
        .unwrap();
    let paths = row["payload"]["data"]["toolImages"]
        .as_array()
        .expect("Recovered tool image references");
    assert_eq!(paths.len(), 1);
    let path = paths[0].as_str().unwrap().to_owned();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!v.to_string().contains("RAW_TOOL_OUTPUT_MUST_STAY_HIDDEN"));
    assert!(!v.to_string().contains(&STANDARD.encode(bytes)));
    f.call(
        "assets.createUrl",
        json!({"resource":{"_tag":"workspace-file","threadId":"shared","path":path}}),
    )
    .await
    .unwrap();
    // Some native history responses omit already streamed result bytes. Re-reading
    // their metadata must not remove a previously verified preview reference.
    f.turns.lock().unwrap()[1]["items"][2]["item"]
        .as_object_mut()
        .unwrap()
        .remove("result");
    f.call("native.shutdown", json!({})).await.unwrap();
    rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite"))
        .unwrap()
        .execute(
            "UPDATE native_sync SET cursor=json_remove(cursor,'$.projection_version')",
            [],
        )
        .unwrap();
    f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
    f.watch().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let repeated = f.detail().await;
    let row = repeated["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "missed-screenshot")
        .unwrap();
    assert_eq!(row["payload"]["data"]["toolImages"], json!([path]));
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "turn/start")
            .count(),
        1
    );
}
#[tokio::test]
async fn recovers_missing_owned_output_after_legacy_cursor_without_duplicating_prompt_or_turn() {
    let mut f = Fixture::new().await;
    let initial = f.detail().await;
    let public = initial["latestTurn"]["turnId"].clone();
    f.turns
        .lock()
        .unwrap()
        .push(external("newer", "completed", 2));
    let latest = f.wait_text("newer_ITEM_1").await["latestTurn"].clone();
    {
        let mut turns = f.turns.lock().unwrap();
        turns[0]["items"] = json!([
            {"turnId":"owned-0","item":{"id":"native-original-prompt","clientId":"initial","type":"userMessage","content":[{"type":"text","text":"initial"}]}},
            {"turnId":"owned-0","item":{"id":"reply-owned-0","type":"agentMessage","text":"RECOVERED_OWN_REPLY"}},
            {"turnId":"owned-0","item":{"id":"missing-owned-reply","type":"agentMessage","text":"MISSED_WHILE_DISCONNECTED"}}
        ]);
    }
    // Upgrade a persisted cursor from the version that skipped owned turns.
    f.call("native.shutdown", json!({})).await.unwrap();
    rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite"))
        .unwrap()
        .execute(
            "UPDATE native_sync SET cursor=json_remove(cursor,'$.projection_version')",
            [],
        )
        .unwrap();
    f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
    let restored = f.wait_text("MISSED_WHILE_DISCONNECTED").await;
    assert_eq!(restored["latestTurn"], latest);
    let rows = restored["messages"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|m| m["role"] == "user" && m["text"] == "initial")
            .count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|m| m["id"] == "reply-owned-0").count(),
        1
    );
    for id in ["initial", "reply-owned-0", "missing-owned-reply"] {
        assert_eq!(
            rows.iter().find(|m| m["id"] == id).unwrap()["turnId"],
            public
        );
    }
    assert!(rows.iter().any(|m| m["text"] == "RECOVERED_OWN_REPLY"));
    f.restart().await;
    let repeated = f.wait_text("MISSED_WHILE_DISCONNECTED").await;
    assert_eq!(restored["messages"], repeated["messages"]);
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|r| r["method"] == "turn/start").count(),
        1
    );
    assert!(!log.iter().any(|r| r["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn imports_desktop_turns_with_paged_cursor_no_duplicates_and_restart() {
    let mut f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("desktop", "completed", 19));
    let v = f.wait_text("desktop_ITEM_18").await;
    assert_eq!(v["session"]["status"], "ready");
    assert!(v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["text"] == "desktop_PROMPT"));
    let read = || Store::open(f._dir.path()).unwrap();
    let before = read().history_page("shared", None, None).unwrap();
    f.restart().await;
    f.wait_text("desktop_ITEM_18").await;
    tokio::time::sleep(Duration::from_millis(950)).await;
    let after = Store::open(f._dir.path())
        .unwrap()
        .history_page("shared", None, None)
        .unwrap();
    assert_eq!(before, after);
    assert!(!f
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "thread/resume" || r["method"] == "turn/interrupt"));
    f.call("native.shutdown", json!({})).await.unwrap();
}
#[tokio::test]
async fn restart_recovers_running_owned_turn_without_phone_watch_or_prompt_replay() {
    let mut f = Fixture::new().await;
    let public = f.detail().await["latestTurn"]["turnId"].clone();
    f.call("native.shutdown", json!({})).await.unwrap();
    // A crash leaves the last saved state active while the shared server lives on.
    let db = rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite")).unwrap();
    db.execute("UPDATE chats SET data=json_set(data,'$.thread.session.status','running','$.thread.session.activeTurnId',?1,'$.thread.latestTurn.state','running','$.thread.latestTurn.completedAt',NULL) WHERE id='shared'", [public.as_str().unwrap()]).unwrap();
    {
        let mut turns = f.turns.lock().unwrap();
        turns[0]["status"] = json!("inProgress");
        turns[0]["completedAt"] = Value::Null;
        turns[0]["items"] = json!([{ "turnId":"owned-0", "item":{"id":"reply-owned-0","type":"agentMessage","text":"RUNNING_AFTER_RESTART"}}]);
    }
    f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let t = f.detail().await;
            if t.to_string().contains("RUNNING_AFTER_RESTART")
                && t["session"]["statusConfidence"] == "confirmed"
            {
                assert_eq!(t["latestTurn"]["turnId"], public);
                assert_eq!(t["session"]["status"], "running");
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    {
        let mut turns = f.turns.lock().unwrap();
        turns[0]["status"] = json!("completed");
        turns[0]["completedAt"] = json!(remodex_connect_native::auth::now() / 1000);
        turns[0]["items"][0]["item"]["text"] = json!("COMPLETED_AFTER_RESTART");
    }
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let t = f.detail().await;
            if t.to_string().contains("COMPLETED_AFTER_RESTART")
                && t["session"]["status"] == "ready"
            {
                assert_eq!(t["latestTurn"]["turnId"], public);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    f.call("native.shutdown", json!({})).await.unwrap();
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|r| r["method"] == "turn/start").count(),
        1
    );
    assert!(!log.iter().any(|r| r["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn native_client_id_recovers_a_turn_when_the_start_ack_was_not_saved() {
    let mut f = Fixture::new().await;
    let public = f.detail().await["latestTurn"]["turnId"].clone();
    f.call("native.shutdown", json!({})).await.unwrap();
    let db = rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite")).unwrap();
    db.execute_batch("DELETE FROM native_sync; DELETE FROM native_turns; DELETE FROM chat_rows WHERE kind='messages' AND id='reply-owned-0';").unwrap();
    db.execute("UPDATE chats SET data=json_set(data,'$.thread.session.status','starting','$.thread.session.activeTurnId',?1,'$.thread.latestTurn.state','running','$.thread.latestTurn.completedAt',NULL) WHERE id='shared'",[public.as_str().unwrap()]).unwrap();
    {
        let mut turns = f.turns.lock().unwrap();
        turns[0]["items"] = json!([
            {"turnId":"owned-0","item":{"id":"native-original-prompt","type":"userMessage","clientId":"initial","content":[{"type":"text","text":"initial"}]}},
            {"turnId":"owned-0","item":{"id":"reply-owned-0","type":"agentMessage","text":"RECOVERED_AFTER_LOST_ACK"}}
        ]);
    }
    f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
    let restored = f.wait_text("RECOVERED_AFTER_LOST_ACK").await;
    assert_eq!(restored["session"]["status"], "ready");
    assert_eq!(restored["latestTurn"]["turnId"], public);
    let rows = restored["messages"].as_array().unwrap();
    assert_eq!(rows.iter().filter(|m| m["role"] == "user").count(), 1);
    assert!(rows.iter().all(|m| m["turnId"] == public));
    let saved: String = db
        .query_row(
            "SELECT native FROM native_turns WHERE chat='shared' AND turn=?1",
            [public.as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, "owned-0");
    f.restart().await;
    let repeated = f.wait_text("RECOVERED_AFTER_LOST_ACK").await;
    assert_eq!(repeated["messages"], restored["messages"]);
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "turn/start")
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_retry_can_replace_only_a_confirmed_missing_empty_native_session() {
    let mut f = Fixture::new().await;
    f.call("native.shutdown", json!({})).await.unwrap();
    let db = rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite")).unwrap();
    db.execute_batch("DELETE FROM native_turns; DELETE FROM native_sync; DELETE FROM chat_rows WHERE kind<>'messages' OR json_extract(data,'$.role')<>'user'; UPDATE chats SET data=json_set(data,'$.thread.session.status','error','$.thread.latestTurn.startedAt',NULL);").unwrap();
    f.turns.lock().unwrap().clear();
    std::fs::write(f._dir.path().join("missing-native"), "missing").unwrap();
    f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
    f.watch().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!f
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "thread/resume"));
    // An auth failure is not proof of a missing session.
    std::fs::write(f._dir.path().join("missing-native"), "auth").unwrap();
    assert!(f.send("auth-retry-not-sent").await.is_err());
    assert!(!f.detail().await.to_string().contains("auth-retry-not-sent"));
    std::fs::write(f._dir.path().join("missing-native"), "missing").unwrap();
    f.send("explicit-empty-retry").await.unwrap();
    let result = f.wait_text("OWN_REPLY").await;
    for text in ["initial", "explicit-empty-retry"] {
        assert_eq!(
            result["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user" && m["text"] == text)
                .count(),
            1
        );
    }
    assert!(result.to_string().contains("empty-session-recovered"));
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|r| r["method"] == "thread/start").count(),
        2
    );
    assert_eq!(
        log.iter()
            .filter(|r| r["method"] == "thread/resume")
            .count(),
        1
    );
    assert_eq!(
        log.iter().filter(|r| r["method"] == "turn/start").count(),
        2
    );
}

#[tokio::test]
async fn missing_session_with_existing_output_is_never_replaced() {
    let f = Fixture::new().await;
    let before = f.detail().await;
    std::fs::write(f._dir.path().join("missing-native"), "missing").unwrap();
    assert!(f.send("must-not-replace").await.is_err());
    let after = f.detail().await;
    assert_eq!(before["messages"], after["messages"]);
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|r| r["method"] == "thread/start").count(),
        1
    );
    assert!(!log.iter().any(|r| r["method"] == "thread/resume"));
}
#[tokio::test]
async fn conflicting_native_client_receipt_cannot_move_an_existing_prompt_to_another_turn() {
    let f = Fixture::new().await;
    let before = f.detail().await;
    let mut turn = external("foreign-receipt", "completed", 2);
    turn["items"][0]["item"]["clientId"] = json!("initial");
    f.turns.lock().unwrap().push(turn);
    let error = f.send("not-sent-after-conflict").await.unwrap_err();
    assert!(error.contains("conflicting delivery receipt"));
    let after = f.detail().await;
    assert_eq!(before["messages"], after["messages"]);
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "turn/start")
            .count(),
        1
    );
}

#[tokio::test]
async fn background_history_read_started_before_send_cannot_authorize_that_send() {
    let f = Fixture::new().await;
    let before = f.detail().await;
    f.history_gate.armed.store(true, Ordering::SeqCst);
    f.watch().await;
    tokio::time::timeout(Duration::from_secs(5), f.history_gate.entered.notified())
        .await
        .unwrap();
    // The background read has already sampled the old native turns. Introduce
    // a conflicting receipt before the phone asks to send a new prompt.
    let mut turn = external("foreign-receipt", "completed", 2);
    turn["items"][0]["item"]["clientId"] = json!("initial");
    f.turns.lock().unwrap().push(turn);
    let send = f.send("not-sent-after-stale-check");
    tokio::pin!(send);
    tokio::select! {
        biased;
        result = &mut send => panic!("Send finished before history was released: {result:?}"),
        _ = f.detail() => {}, // FIFO request after the pending send was polled.
    }
    f.history_gate.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(8), send)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.contains("conflicting delivery receipt"), "{error}");
    assert_eq!(before["messages"], f.detail().await["messages"]);
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "turn/start")
            .count(),
        1
    );
}
#[tokio::test]
async fn updates_streaming_items_then_finalizes_without_interrupting_desktop() {
    let f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("stream", "inProgress", 2));
    let v = f.wait_text("stream_ITEM_1").await;
    assert_eq!(v["session"]["status"], "running");
    let err = f.send("must-wait").await.unwrap_err();
    assert!(err.contains("running in Desktop") || err.contains("Active chat limit"));
    assert!(!f.detail().await["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "must-wait"));
    {
        let mut turns = f.turns.lock().unwrap();
        let t = turns.last_mut().unwrap();
        *t = external("stream", "completed", 2);
        t["items"][1]["item"]["text"] = json!("FINISHED_DESKTOP");
    }
    let v = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            f.watch().await;
            let v = f.detail().await;
            if v.to_string().contains("FINISHED_DESKTOP") && v["session"]["status"] == "ready" {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("Final text and completion were not both observed");
    assert_eq!(v["session"]["status"], "ready");
    assert_eq!(
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["id"] == "stream-1")
            .count(),
        1
    );
    f.call("native.shutdown", json!({})).await.unwrap();
    assert!(!f
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn failed_page_preserves_cursor_and_blocks_sending_until_caught_up() {
    let f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("repair", "completed", 2));
    *f.corrupt.lock().unwrap() = true;
    let error = f.send("not-delivered").await.unwrap_err();
    assert!(error.contains("unexpected turn"));
    assert!(!f.detail().await.to_string().contains("repair_PROMPT"));
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "turn/start")
            .count(),
        1
    );
    *f.corrupt.lock().unwrap() = false;
    f.send("after-repair").await.unwrap();
    let v = f.wait_text("repair_ITEM_1").await;
    assert!(v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "after-repair"));
    assert!(!v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "not-delivered"));
    f.call("native.shutdown", json!({})).await.unwrap();
}

#[tokio::test]
async fn failed_history_transaction_does_not_advance_cursor_or_lose_previous_rows() {
    let f = Fixture::new().await;
    // Install the fault before publishing the new native turn. The viewer's
    // background reader may otherwise commit it before the fixture is armed.
    let db = rusqlite::Connection::open(f._dir.path().join("native-connect.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_sync BEFORE INSERT ON chat_rows WHEN NEW.id='transaction-1' BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END;").unwrap();
    f.turns
        .lock()
        .unwrap()
        .push(external("transaction", "completed", 2));
    let err = f.send("not-accepted").await.unwrap_err();
    assert!(err.contains("Cannot commit shared history"));
    let text = f.detail().await.to_string();
    assert!(text.contains("OWN_REPLY"));
    assert!(!text.contains("transaction_PROMPT"));
    let cursor: String = db
        .query_row("SELECT cursor FROM native_sync", [], |r| r.get(0))
        .unwrap();
    // The preceding owned turn can commit independently; the failed page cannot.
    assert!(!cursor.contains("transaction"));
    let again = f.send("still-not-accepted").await.unwrap_err();
    assert!(again.contains("Cannot commit shared history"));
    let unchanged: String = db
        .query_row("SELECT cursor FROM native_sync", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursor, unchanged);
    db.execute_batch("DROP TRIGGER fail_sync").unwrap();
    f.send("retry-after-disk").await.unwrap();
    let v = f.wait_text("transaction_ITEM_1").await;
    assert_eq!(
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["id"] == "transaction-1")
            .count(),
        1
    );
    f.call("native.shutdown", json!({})).await.unwrap();
}

#[tokio::test]
async fn catch_up_hides_raw_output_and_stops_polling_after_phone_leaves() {
    let f = Fixture::new().await;
    let mut t = external("tools", "completed", 3);
    t["items"][1]["item"] = json!({"type":"commandExecution","id":"tools-1","command":"printf marker","cwd":f._dir.path(),"status":"completed","aggregatedOutput":"PRIVATE_RAW_OUTPUT_MUST_STAY_HIDDEN","exitCode":0});
    f.turns.lock().unwrap().push(t);
    let v = f.wait_text("tools_ITEM_2").await;
    assert!(v.to_string().contains("printf marker"));
    assert!(!v
        .to_string()
        .contains("PRIVATE_RAW_OUTPUT_MUST_STAY_HIDDEN"));
    tokio::time::sleep(Duration::from_millis(3400)).await;
    let before = f
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["method"] == "thread/turns/list")
        .count();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let after = f
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["method"] == "thread/turns/list")
        .count();
    assert_eq!(before, after);
    f.turns
        .lock()
        .unwrap()
        .push(external("after-leaving", "completed", 2));
    f.wait_text("after-leaving_ITEM_1").await;
    f.call("native.shutdown", json!({})).await.unwrap();
}

impl Fixture {
    async fn wait_attached(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.watch().await;
                if self
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|v| v["id"] == "attach-resume")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}
#[tokio::test]
async fn shared_desktop_queue_and_same_turn_steer_keep_the_original_task() {
    let f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("controls", "inProgress", 2));
    f.wait_attached().await;
    f.call("orchestration.dispatchCommand",json!({"type":"thread.turn.start","commandId":"steer","threadId":"shared","deliveryMode":"queue","message":{"messageId":"steer-message","text":"Use the new direction","attachments":[]}})).await.unwrap();
    f.call("orchestration.dispatchCommand",json!({"type":"thread.turn.queue.steer","commandId":"convert-to-steer","threadId":"shared","messageId":"steer-message"})).await.unwrap();
    let v = f.wait_text("Steering message received").await;
    assert_eq!(
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["text"] == "Use the new direction")
            .count(),
        1
    );
    f.call("orchestration.dispatchCommand",json!({"type":"thread.turn.start","commandId":"queued","threadId":"shared","deliveryMode":"queue","message":{"messageId":"queued-message","text":"Run after Desktop finishes","attachments":[]}})).await.unwrap();
    assert_eq!(
        f.log
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "turn/start")
            .count(),
        1
    );
    {
        let mut ts = f.turns.lock().unwrap();
        let t = ts.last_mut().unwrap();
        t["status"] = json!("completed");
        t["completedAt"] = json!(remodex_connect_native::auth::now() / 1000);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            f.watch().await;
            let d = f.detail().await;
            if d["session"]["status"] == "ready"
                && d["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == "queued-message" && m["phase"] != "queued")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    f.call("native.shutdown", json!({})).await.unwrap();
    let log = f.log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|v| v["method"] == "turn/start").count(),
        2
    );
    assert_eq!(
        log.iter().filter(|v| v["method"] == "turn/steer").count(),
        1
    );
    assert!(!log.iter().any(|v| v["method"] == "turn/interrupt"));
}
#[tokio::test]
async fn desktop_owned_turn_is_cancelled_only_by_explicit_phone_stop() {
    let f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("stop-desktop", "inProgress", 2));
    f.wait_attached().await;
    f.call(
        "orchestration.dispatchCommand",
        json!({"type":"thread.turn.interrupt","commandId":"stop","threadId":"shared"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.detail().await["session"]["status"] == "interrupted" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    f.call("native.shutdown", json!({})).await.unwrap();
    let log = f.log.lock().unwrap();
    let stops: Vec<_> = log
        .iter()
        .filter(|v| v["method"] == "turn/interrupt")
        .collect();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0]["params"]["turnId"], "stop-desktop");
}
#[tokio::test]
async fn offline_resolution_clears_stale_approval_only_with_authoritative_flags() {
    let mut f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("offline-approval", "inProgress", 2));
    f.notices.lock().unwrap().push(json!({"id":901,"method":"item/commandExecution/requestApproval","params":{"threadId":"shared-native","turnId":"offline-approval","itemId":"command-1","command":"echo marker"}}));
    let v = f.wait_text("Approval required").await;
    let rid = v["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "approval.requested")
        .unwrap()["id"]
        .clone();
    for flags_known in [false, true] {
        f.call("native.shutdown", json!({})).await.unwrap();
        f.notices.lock().unwrap().clear(); // Another client resolved it while disconnected.
        if flags_known {
            f.turns.lock().unwrap().last_mut().unwrap()["activeFlags"] = json!([]);
        }
        let previous = f
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["id"] == "attach-resume")
            .count();
        f.hub = Chats::start(f.db.clone(), f.config.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                f.watch().await;
                if f.log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|v| v["id"] == "attach-resume")
                    .count()
                    > previous
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let v = f.detail().await;
        assert_eq!(v["hasPendingApprovals"], !flags_known);
        let row = v["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == rid)
            .unwrap();
        assert_eq!(
            row["kind"],
            if flags_known {
                "approval.resolved"
            } else {
                "approval.requested"
            }
        );
        assert_eq!(v["session"]["status"], "running");
    }
    f.call("native.shutdown", json!({})).await.unwrap();
    let log = f.log.lock().unwrap();
    assert!(!log
        .iter()
        .any(|v| v["id"] == 901 || v["method"] == "turn/interrupt"));
}

#[tokio::test]
async fn observer_shutdown_never_cancels_desktop_and_pending_control_recovers_once() {
    let mut f = Fixture::new().await;
    f.turns
        .lock()
        .unwrap()
        .push(external("approval-desktop", "inProgress", 2));
    f.notices.lock().unwrap().push(json!({"id":901,"method":"item/commandExecution/requestApproval","params":{"threadId":"shared-native","turnId":"approval-desktop","itemId":"command-1","command":"echo marker"}}));
    let v = f.wait_text("Approval required").await;
    let rid = v["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "approval.requested")
        .unwrap()["id"]
        .clone();
    f.restart().await;
    f.wait_attached().await;
    let v = f.wait_text("Approval required").await;
    assert_eq!(
        v["activities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "approval.requested")
            .count(),
        1
    );
    assert_eq!(
        v["activities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "approval.requested")
            .unwrap()["id"],
        rid
    );
    f.notices.lock().unwrap().extend([
        json!({"id":902,"method":"item/unknownTool/requestApproval","params":{"threadId":"shared-native","turnId":"approval-desktop"}}),
        json!({"id":903,"method":"item/commandExecution/requestApproval","params":{"threadId":"other-native","turnId":"approval-desktop","command":"FOREIGN_PRIVATE_COMMAND"}}),
        json!({"method":"serverRequest/resolved","params":{"threadId":"shared-native","requestId":901}}),
    ]);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            f.watch().await;
            let v = f.detail().await;
            if v["hasPendingApprovals"] == false
                && v["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["id"] == rid && a["kind"] == "approval.resolved")
            {
                assert!(!v.to_string().contains("FOREIGN_PRIVATE_COMMAND"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    f.call("native.shutdown", json!({})).await.unwrap();
    let log = f.log.lock().unwrap();
    assert!(!log.iter().any(|v| v["method"] == "turn/interrupt"));
    assert!(!log.iter().any(|v| v["id"] == 902 || v["id"] == 903));
}

#[tokio::test]
async fn truncated_shared_image_recovers_from_verified_native_turn_without_raw_history() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let mut f = Fixture::new().await;
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(vec![9; 2048]);
    let data = STANDARD.encode(&bytes);
    let mut turn = external("image-turn", "completed", 2);
    turn["items"].as_array_mut().unwrap().push(json!({"turnId":"image-turn","item":{"id":"truncated-screenshot","type":"mcpToolCall","tool":"screenshot","arguments":{},"status":"completed","result":{"content":[{"type":"text","text":json!({"content":[{"type":"image","mimeType":"image/png","data":format!("{}...[truncated]",&data[..900])}]}).to_string()}]}}}));
    let lines = [
        json!({"type":"session_meta","payload":{"id":"shared-native","base_instructions":"PRIVATE_HISTORY_MUST_STAY_HIDDEN"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"image-turn"}}),
        json!({"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_image","image_url":format!("data:image/png;base64,{data}")},{"type":"input_text","text":"RAW_OUTPUT_MUST_STAY_HIDDEN"}]}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"image-turn"}}),
    ];
    std::fs::create_dir_all(f._dir.path().join("sessions")).unwrap();
    std::fs::write(
        f._dir.path().join("sessions/rollout.jsonl"),
        lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
    f.turns.lock().unwrap().push(turn);
    let detail = f.wait_text("image-turn_ITEM_1").await;
    let activity = detail["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "truncated-screenshot")
        .unwrap();
    let paths = activity["payload"]["data"]["toolImages"]
        .as_array()
        .unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(paths[0].as_str().unwrap()).unwrap(), bytes);
    let galleries: Vec<_> = detail["activities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["payload"]["itemType"] == "image-generation")
        .collect();
    assert_eq!(galleries.len(), 1);
    assert_eq!(
        galleries[0]["payload"]["data"]["generatedImages"],
        json!(paths)
    );
    assert_eq!(
        galleries[0]["payload"]["data"]["sourceToolCallId"],
        "truncated-screenshot"
    );
    let public = detail.to_string();
    assert!(!public.contains("PRIVATE_HISTORY_MUST_STAY_HIDDEN"));
    assert!(!public.contains("RAW_OUTPUT_MUST_STAY_HIDDEN"));
    assert!(!public.contains(&data));
    f.restart().await;
    let after = f.detail().await;
    let recovered = after["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "truncated-screenshot")
        .unwrap();
    assert_eq!(recovered["payload"]["data"]["toolImages"], json!(paths));
    assert_eq!(std::fs::read(paths[0].as_str().unwrap()).unwrap(), bytes);
}
