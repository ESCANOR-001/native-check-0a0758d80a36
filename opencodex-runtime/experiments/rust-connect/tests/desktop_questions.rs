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
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct Fixture {
    _dir: tempfile::TempDir,
    hub: Chats,
    log: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(running: bool, reject: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-capability-token-not-production").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        let turns = Arc::new(Mutex::new(Vec::<Value>::new()));
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                let turns = turns.clone();
                clients.spawn(async move {
                    let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
                    while let Some(Ok(Message::Text(raw)))=ws.next().await {
                        let v:Value=serde_json::from_str(&raw).unwrap(); seen.lock().unwrap().push(v.clone());
                        let mut events=vec![]; let mut error=false;
                        let result={ let mut turns=turns.lock().unwrap();
                            match v["method"].as_str().unwrap_or("") {
                                "initialize"=>json!({}), "initialized"=>continue,
                                "model/list"=>json!({"data":[{"model":"test","displayName":"Test","supportedReasoningEfforts":[]}]}),
                                "skills/list" => json!({"data":[]}),
                                "plugin/installed" => json!({"marketplaces":[]}),
                                "config/read" => json!({"config":{"mcp_servers":{}}}),
                                "account/rateLimits/read"=>json!({}),
                                "thread/read"|"thread/resume"|"thread/start"=>json!({"thread":{"id":"question-native","status":{"type":if turns.last().is_some_and(|t|t["status"]=="inProgress"){"active"}else{"idle"}}}}),
                                "thread/turns/list"=>{
                                    // Match the installed CLI's requested page size and direction.
                                    let mut page=turns.clone();
                                    if v["params"]["sortDirection"]!="asc" {page.reverse();}
                                    let start=v["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
                                    let limit=v["params"]["limit"].as_u64().unwrap_or(20) as usize;
                                    let end=start.saturating_add(limit).min(page.len());
                                    json!({"data":page.into_iter().skip(start).take(limit).map(|mut t|{t.as_object_mut().unwrap().remove("items");t}).collect::<Vec<_>>(),"nextCursor":if end<turns.len(){json!(end.to_string())}else{Value::Null}})
                                },
                                "thread/items/list"=>{
                                    let t=turns.iter().find(|t| t["id"]==v["params"]["turnId"]).unwrap();
                                    json!({"data":t["items"].as_array().unwrap().iter().map(|item|json!({"turnId":t["id"],"item":item,"startedAtMs":1700000000000i64,"completedAtMs":1700000001000i64})).collect::<Vec<_>>(),"nextCursor":null})
                                }
                                "turn/start"=>{
                                    let n=turns.len(); let id=format!("turn-{n}");
                                    let question=json!({"id":"async-question","type":"agentMessage","delivery":"async","text":"Pick a color","questions":[{"title":"Pick a color","options":["Blue","Green"]}]});
                                    let user=json!({"id":format!("user-{n}"),"clientId":v["params"]["clientUserMessageId"],"type":"userMessage","content":v["params"]["input"]});
                                    let status=if n==0 && running {"inProgress"}else{"completed"};
                                    let item=if n==0 {question}else{json!({"id":"response","type":"agentMessage","text":"ANSWER_RECEIVED"})};
                                    turns.push(json!({"id":id,"status":status,"startedAt":1700000000+n as i64*10,"completedAt":if status=="completed"{json!(1700000001+n as i64*10)}else{Value::Null},"items":[user.clone(),item.clone()]}));
                                    events.push(json!({"method":"item/completed","params":{"threadId":"question-native","turnId":id,"item":user}}));
                                    events.push(json!({"method":"item/completed","params":{"threadId":"question-native","turnId":id,"item":item}}));
                                    if status=="completed" {events.push(json!({"method":"turn/completed","params":{"threadId":"question-native","turn":{"id":id,"status":"completed"}}}));}
                                    json!({"turn":{"id":id,"status":"inProgress"}})
                                }
                                "turn/steer"=>{
                                    if reject {error=true;json!({"code":-32600,"message":"Rejected test answer"})}else{
                                        let turn=turns.last_mut().unwrap();
                                        assert_eq!(turn["id"],v["params"]["expectedTurnId"]);
                                        let item=json!({"id":"steered-answer","clientId":v["params"]["clientUserMessageId"],"type":"userMessage","content":v["params"]["input"]});
                                        turn["items"].as_array_mut().unwrap().push(item.clone());
                                        events.push(json!({"method":"item/completed","params":{"threadId":"question-native","turnId":turn["id"],"item":item}}));
                                        json!({"turnId":turn["id"]})
                                    }
                                }
                                "turn/interrupt"=>json!({}),
                                other=>panic!("Unexpected request {other}")
                            }
                        };
                        let response=if error {json!({"id":v["id"],"error":result})}else{json!({"id":v["id"],"result":result})};
                        if ws.send(Message::Text(response.to_string().into())).await.is_err(){break;}
                        for event in events { if ws.send(Message::Text(event.to_string().into())).await.is_err(){break;} }
                    }
                });
            }
        });
        let config = Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 2,
            workspace: dir.path().into(),
            claude: None,
            codex: Some(AgentConfig {
                executable: PathBuf::from(env!("CARGO_BIN_EXE_chat-fixture")),
                profile: dir.path().into(),
                model: "test".into(),
                tools: Default::default(),
                remote_control: false,
                shared_server: Some(SharedServer::Capability {
                    address,
                    token_file: token,
                }),
            }),
        };
        let db = AuthService::new(Auth::new(Store::open(dir.path()).unwrap())).unwrap();
        let hub = Chats::start(db, config).await.unwrap();
        let fixture = Self {
            _dir: dir,
            hub,
            log,
            server,
        };
        fixture.dispatch(json!({"type":"thread.create","threadId":"questions","commandId":"create","projectId":"native-test-project"})).await.unwrap();
        fixture.dispatch(json!({"type":"thread.turn.start","threadId":"questions","commandId":"ask","message":{"messageId":"ask","text":"Ask a question","attachments":[]}})).await.unwrap();
        fixture.wait_pending(true).await;
        fixture
    }
    async fn dispatch(&self, c: Value) -> Result<Value, String> {
        self.hub
            .call(
                AgentKind::Codex,
                "phone",
                "orchestration.dispatchCommand",
                c,
            )
            .await
    }
    async fn detail(&self) -> Value {
        self.hub
            .call(
                AgentKind::Codex,
                "phone",
                "orchestration.subscribeThread",
                json!({"threadId":"questions"}),
            )
            .await
            .unwrap()["snapshot"]["thread"]
            .clone()
    }
    async fn wait_pending(&self, pending: bool) -> Value {
        let mut last = Value::Null;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let d = self.detail().await;
                if d["hasPendingUserInput"] == pending {
                    return d;
                }
                last = d;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "question state: {last}; requests: {:?}",
                self.log.lock().unwrap()
            )
        })
    }
    fn answer() -> Value {
        json!({"type":"thread.user-input.respond","threadId":"questions","commandId":"answer","requestId":"desktop-question:async-question","answers":{"[\"request_user_input_async\",\"async-question\",0]":{"answers":["Custom blue"]}}})
    }
    async fn stop(&self) {
        self.hub
            .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn skipping_async_question_never_sends_and_survives_native_replay() {
    for running in [false, true] {
        let f = Fixture::new(running, false).await;
        let before = f.detail().await;
        let mut c = Fixture::answer();
        c["answers"] = json!({"[\"request_user_input_async\",\"async-question\",0]":[]});
        f.dispatch(c.clone()).await.unwrap();
        f.dispatch(c).await.unwrap(); // An acknowledged retry is idempotent.
        let after = f.wait_pending(false).await;
        assert_eq!(
            after["latestTurn"]["turnId"],
            before["latestTurn"]["turnId"]
        );
        assert_eq!(after["latestTurn"]["state"], before["latestTurn"]["state"]);
        assert_eq!(after["messages"], before["messages"]);
        f.hub
            .call(
                AgentKind::Codex,
                "phone",
                "native.watchShared",
                json!({"threadId":"questions"}),
            )
            .await
            .unwrap();
        let replayed = f.wait_pending(false).await;
        assert!(replayed["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "user-input.resolved"
                && a["payload"]["requestId"] == "desktop-question:async-question"));
        let log = f.log.lock().unwrap().clone();
        assert_eq!(
            log.iter().filter(|v| v["method"] == "turn/start").count(),
            1
        );
        assert!(!log
            .iter()
            .any(|v| v["method"] == "turn/steer" || v["method"] == "turn/interrupt"));
        f.stop().await;
    }
}

#[tokio::test]
async fn historical_question_keeps_native_item_time_after_recovery() {
    let f = Fixture::new(false, false).await;
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            f.hub
                .call(
                    AgentKind::Codex,
                    "phone",
                    "native.watchShared",
                    json!({"threadId":"questions"}),
                )
                .await
                .unwrap();
            let d = f.detail().await;
            if let Some(q) = d["activities"].as_array().unwrap().iter().find(|a| {
                a["kind"] == "user-input.requested"
                    && a["payload"]["data"]["nativeCreatedAt"].is_string()
            }) {
                assert_eq!(q["turnId"], d["latestTurn"]["turnId"]);
                break q.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("historical question was not projected with native timing");
    assert_eq!(
        question["createdAt"],
        remodex_connect_native::auth::timestamp(1700000000000)
    );
    assert_eq!(question["payload"]["status"], "pending");
    f.stop().await;
}

#[tokio::test]
async fn desktop_async_answer_starts_when_idle_and_steers_the_same_running_turn_once() {
    for running in [false, true] {
        let f = Fixture::new(running, false).await;
        f.dispatch(Fixture::answer()).await.unwrap();
        // Starting an answer creates a newer turn, which clears the old
        // attention badge before delivery. Verify the actual receipt instead
        // of treating that presentation flag as an acknowledgement.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let d = f.detail().await;
                if d["activities"].as_array().unwrap().iter().any(|a| {
                    a["id"] == "desktop-question:async-question"
                        && a["kind"] == "user-input.resolved"
                }) {
                    assert_eq!(d["hasPendingUserInput"], false);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("answer was not acknowledged");
        f.dispatch(Fixture::answer()).await.unwrap();
        let log = f.log.lock().unwrap().clone();
        let answers: Vec<_> = log
            .iter()
            .filter(|v| {
                v["params"]["input"][0]["text"]
                    .as_str()
                    .is_some_and(|s| s.contains("send_user_message_question_reply"))
            })
            .collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(
            answers[0]["method"],
            if running { "turn/steer" } else { "turn/start" }
        );
        if running {
            assert_eq!(answers[0]["params"]["expectedTurnId"], "turn-0");
        }
        let mut stale = Fixture::answer();
        stale["commandId"] = json!("stale");
        assert!(f.dispatch(stale).await.is_err());
        f.stop().await;
    }
}

#[tokio::test]
async fn newer_prompt_clears_attention_without_answering_the_older_question() {
    let f = Fixture::new(false, false).await;
    let old = f.wait_pending(true).await;
    let question = old["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "desktop-question:async-question")
        .unwrap()
        .clone();
    f.dispatch(json!({"type":"thread.turn.start","threadId":"questions","commandId":"new-prompt","message":{"messageId":"new-prompt","text":"Continue without answering","attachments":[]}})).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let d = f.detail().await;
            if d["latestTurn"]["turnId"] != old["latestTurn"]["turnId"]
                && d["latestTurn"]["state"] == "completed"
            {
                break d;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("new response did not complete");
    assert_eq!(d["hasPendingUserInput"], false);
    assert_eq!(d["hasPendingNonBlockingUserInput"], false);
    let retained = d["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == question["id"])
        .expect("old question history was removed");
    assert_eq!(retained["kind"], "user-input.requested");
    assert_eq!(retained["turnId"], question["turnId"]);
    assert_eq!(
        retained["payload"]["questions"],
        question["payload"]["questions"]
    );
    assert_eq!(retained["payload"]["status"], "pending");
    assert!(!f
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["params"]["input"][0]["text"]
            .as_str()
            .is_some_and(|s| s.contains("send_user_message_question_reply"))));
    f.stop().await;
}

#[tokio::test]
async fn rejected_async_answer_keeps_the_question_and_does_not_claim_it_was_answered() {
    let f = Fixture::new(true, true).await;
    f.dispatch(Fixture::answer()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let d = f.wait_pending(true).await;
    assert!(!d["messages"].as_array().unwrap().iter().any(|m| m["text"]
        .as_str()
        .is_some_and(|s| s.contains("send_user_message_question_reply"))));
    f.stop().await;
}
