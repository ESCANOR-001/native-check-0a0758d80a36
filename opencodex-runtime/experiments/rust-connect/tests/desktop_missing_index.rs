//! Real Windows regression: paginated native history can be absent while the
//! compressed rollout still contains verified terminal task records.
use remodex_connect_native::{
    agent::{Connection, Event, SharedServer},
    auth::Auth,
    auth_service::AuthService,
    chat::{AgentConfig, AgentKind, Chats, Config},
    desktop_ipc::Endpoint,
    store::Store,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn endpoint(profile: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        let root = profile.canonicalize().unwrap();
        let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        PathBuf::from(format!(r"\\.\pipe\remodex-desktop-test-{hash}"))
    }
    #[cfg(not(windows))]
    {
        profile.join("offline.sock")
    }
}

#[tokio::test]
async fn desktop_restart_keeps_dormant_history_lazy_and_phone_selection_still_reads_it() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let natives: Vec<_> = (0..72).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let inventory: Vec<_> = natives.iter().map(|id| json!({"id":id,"name":"Saved chat","source":"vscode","cwd":root.path(),"createdAt":1700000000,"updatedAt":1767225700_i64,"status":{"type":"notLoaded"}})).collect();
    std::fs::write(
        profile.join("inventory.json"),
        serde_json::to_vec(&inventory).unwrap(),
    )
    .unwrap();
    let config = Config {
        storage: Default::default(),
        native_installation: None,
        native_profile_handoff: None,
        connection: None,
        max_active_chats: 1,
        workspace: root.path().into(),
        claude: None,
        codex: Some(AgentConfig {
            executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
            profile: profile.clone(),
            model: "test".into(),
            tools: Default::default(),
            remote_control: false,
            shared_server: Some(SharedServer::Desktop {
                desktop_endpoint: Endpoint {
                    path: endpoint(&profile),
                },
                reader_executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
                reader_workspace: root.path().into(),
                activation: None,
            }),
        }),
    };
    let db = AuthService::new(Auth::new(Store::open(&root.path().join("store")).unwrap())).unwrap();
    let ids = natives.clone();
    db.call(move |a| {
        a.store.init_chats()?;
        for (n, native) in ids.iter().enumerate() {
            let id = format!("saved-{n}");
            let state = json!({"agent":"codex","native_id":native,"external_codex":true,"resumed":true,"desktop_read_revision":[native,1767225700_i64],"thread":{
                "id":id,"projectId":"native-test-project","title":"Saved chat","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z",
                "session":{"status":"ready","statusConfidence":if n%2==0{"confirmed"}else{"unknown"},"activeTurnId":null},
                "latestTurn":{"turnId":"completed","state":"completed"},
                "messages":[{"id":"saved-message","role":"assistant","text":"KEEP_SAVED_HISTORY","createdAt":"2026-01-01T00:00:00Z","streaming":false}],"activities":[],"checkpoints":[],"proposedPlans":[]
            }});
            a.store.save_chat(&state, None)?;
        }
        Ok::<_, rusqlite::Error>(())
    }).await.unwrap().unwrap();
    let hub = Chats::start(db.clone(), config).await.unwrap();
    let requests = || -> Vec<Value> {
        std::fs::read_to_string(profile.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|v| serde_json::from_str(v).ok())
            .collect()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if requests()
                .iter()
                .any(|r| r["method"] == "thread/list" && r["params"]["cursor"] == "64")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Metadata scan did not finish");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !requests().iter().any(|r| matches!(
            r["method"].as_str(),
            Some("thread/read" | "thread/turns/list")
        )),
        "Restart must not load dormant histories"
    );
    let shell = hub
        .call(
            AgentKind::Codex,
            "phone",
            "orchestration.subscribeShell",
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(shell["snapshot"]["threads"].as_array().unwrap().len(), 72);
    assert_eq!(
        db.call(|a| a.store.load_chat("saved-71"))
            .await
            .unwrap()
            .unwrap()["thread"]["messages"][0]["text"],
        "KEEP_SAVED_HISTORY"
    );
    hub.call(
        AgentKind::Codex,
        "phone",
        "orchestration.subscribeThread",
        json!({"threadId":"saved-71"}),
    )
    .await
    .unwrap();
    // The WebSocket connection separately renews this viewing lease while
    // Android displays a chat. A cached snapshot alone never owns a follower.
    hub.call(
        AgentKind::Codex,
        "phone",
        "native.watchShared",
        json!({"threadId":"saved-71","refresh":true}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if requests().iter().any(|r| {
                matches!(
                    r["method"].as_str(),
                    Some("thread/read" | "thread/turns/list")
                ) && r["params"]["threadId"] == natives[71]
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Opening the saved chat did not request its native history");
    hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn manual_shell_refresh_checks_newest_native_chats_without_the_poll_delay() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let row = |id: &str| {
        json!({"id":id,"name":"Inventory check","source":"vscode",
        "cwd":dir.path(),"createdAt":1700000000,"updatedAt":1700000010,"status":{"type":"notLoaded"}})
    };
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    let inventory = profile.join("inventory.json");
    std::fs::write(
        &inventory,
        serde_json::to_vec(&json!([row(&first)])).unwrap(),
    )
    .unwrap();
    let db = AuthService::new(Auth::new(Store::open(&dir.path().join("store")).unwrap())).unwrap();
    let hub = Chats::start(
        db,
        Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            claude: None,
            codex: Some(AgentConfig {
                executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
                profile: profile.clone(),
                model: "test".into(),
                tools: Default::default(),
                remote_control: false,
                shared_server: Some(SharedServer::Desktop {
                    desktop_endpoint: Endpoint {
                        path: endpoint(&profile),
                    },
                    reader_executable: env!("CARGO_BIN_EXE_inventory-fixture").into(),
                    reader_workspace: dir.path().into(),
                    activation: None,
                }),
            }),
        },
    )
    .await
    .unwrap();
    async fn wait_for(hub: &Chats, id: &str, timeout: Duration) {
        tokio::time::timeout(timeout, async {
            loop {
                let value = hub
                    .call(
                        AgentKind::Codex,
                        "test",
                        "orchestration.subscribeShell",
                        json!({}),
                    )
                    .await
                    .unwrap();
                if value["snapshot"]["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["id"] == id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("Newest native chat was not refreshed promptly");
    }
    wait_for(&hub, &first, Duration::from_secs(10)).await;
    std::fs::write(
        &inventory,
        serde_json::to_vec(&json!([row(&second), row(&first)])).unwrap(),
    )
    .unwrap();
    hub.call(
        AgentKind::Codex,
        "test",
        "orchestration.subscribeShell",
        json!({"refresh":true}),
    )
    .await
    .unwrap();
    wait_for(&hub, &second, Duration::from_secs(3)).await;
    hub.call(AgentKind::Codex, "test", "native.shutdown", json!({}))
        .await
        .unwrap();
    let logged = std::fs::read_to_string(profile.join("requests.jsonl")).unwrap();
    assert!(logged
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .all(|v| [
            "initialize",
            "initialized",
            "thread/list",
            "model/list",
            "thread/read",
            "thread/turns/list",
            "account/rateLimits/read"
        ]
        .contains(&v["method"].as_str().unwrap())));
}
async fn rpc(c: &mut Connection, method: &str, params: Value) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    c.send(&json!({"id":id,"method":method,"params":params}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::Message(v)) = c.events.recv().await {
                if v["id"] == id {
                    return v;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn missing_index_uses_native_results_without_claiming_a_desktop_owner() {
    for (kind, expected, compressed) in [
        ("task_complete", Some("completed"), true),
        ("turn_aborted", Some("interrupted"), false),
        ("failed", Some("failed"), true),
        ("task_started", None, true),
        ("damaged", None, false),
        ("partial", None, true),
        ("partial-new", None, false),
        ("new-work", None, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile space 测试");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let path = profile.join("sessions/saved.jsonl");
        let mut text = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":native}}),
            json!({"timestamp":"2026-10-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"saved"}})
        );
        if kind != "task_started" {
            text += &format!(
                "{}\n",
                json!({"timestamp":"2026-10-01T00:01:00Z","type":"event_msg","payload":{
                    "type":if kind == "turn_aborted" {kind} else {"task_complete"},"turn_id":"saved",
                    "error":if kind == "failed" {json!({"message":"Saved failure"})} else {Value::Null}
                }})
            );
        }
        if kind == "damaged" {
            text += "broken\n";
        }
        if kind == "partial" {
            text.pop();
        }
        if kind == "partial-new" {
            text += &json!({"timestamp":"2026-10-01T00:02:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"new"}}).to_string();
        }
        if kind == "new-work" {
            text += &format!(
                "{}\n",
                json!({"timestamp":"2026-10-01T00:02:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"new"}})
            );
        }
        let actual = if compressed {
            path.with_extension("jsonl.zst")
        } else {
            path.clone()
        };
        let bytes = if compressed {
            zstd::stream::encode_all(text.as_bytes(), 1).unwrap()
        } else {
            text.as_bytes().to_vec()
        };
        std::fs::write(&actual, &bytes).unwrap();
        std::fs::write(
            profile.join("fixture.json"),
            json!({"native":native,"path":path,"mode":"missing-index"}).to_string(),
        )
        .unwrap();
        let server = SharedServer::Desktop {
            desktop_endpoint: Endpoint {
                path: endpoint(&profile),
            },
            reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
            reader_workspace: root.path().into(),
            activation: None,
        };
        let mut c = server.connect_history(&profile).await.unwrap();
        let response = rpc(
            &mut c,
            "thread/read",
            json!({"threadId":native,"includeTurns":false}),
        )
        .await;
        let thread = &response["result"]["thread"];
        assert_eq!(thread["remodexLiveStatusKnown"], false, "{kind}");
        assert_eq!(
            thread["remodexTaskStatusKnown"],
            expected.is_some(),
            "{kind}"
        );
        if let Some(expected) = expected {
            assert_eq!(thread["remodexSavedTerminal"]["status"], expected);
            assert_eq!(thread["remodexSavedTerminal"]["startedAt"], 1790812800_i64);
            assert_eq!(
                thread["remodexSavedTerminal"]["completedAt"],
                1790812860_i64
            );
        } else {
            assert!(thread["remodexSavedTerminal"].is_null());
        }
        for method in [
            "thread/resume",
            "turn/start",
            "turn/steer",
            "turn/interrupt",
        ] {
            assert!(
                rpc(&mut c, method, json!({"threadId":native,"turnId":"saved"}))
                    .await
                    .get("error")
                    .is_some()
            );
        }
        c.close().await;
        assert_eq!(std::fs::read(&actual).unwrap(), bytes);
        assert!(!std::fs::read_to_string(profile.join("methods.log"))
            .unwrap()
            .lines()
            .any(|m| [
                "thread/resume",
                "turn/start",
                "turn/steer",
                "turn/interrupt"
            ]
            .contains(&m)));

        // Exercise the actual startup and phone shell projection, not just a
        // helper: finished records restore their own result/timestamp.
        if let Some(expected) = expected {
            let agent = AgentConfig {
                executable: env!("CARGO_BIN_EXE_history-fixture").into(),
                profile: profile.clone(),
                model: "test".into(),
                tools: Default::default(),
                remote_control: false,
                shared_server: Some(server),
            };
            let config = Config {
                storage: Default::default(),
                native_installation: None,
                native_profile_handoff: None,
                connection: None,
                max_active_chats: 1,
                workspace: root.path().into(),
                codex: Some(agent),
                claude: None,
            };
            let db = AuthService::new(Auth::new(Store::open(&root.path().join("store")).unwrap()))
                .unwrap();
            let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
            hub.call(AgentKind::Codex,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":"saved-chat","projectId":"native-test-project"})).await.unwrap();
            hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
                .await
                .unwrap();
            // Codex can update its inventory after the last message. Remember
            // that native revision after reading, not the completion timestamp.
            let revision = 1790812865_i64;
            std::fs::write(profile.join("desktop-state.json"), json!({"inventory":[{"id":native,"name":"Saved native chat","source":"vscode","cwd":root.path(),"createdAt":1790812800,"updatedAt":revision,"status":{"type":"notLoaded"}}]}).to_string()).unwrap();
            db.call(move |a| {
                let mut chat = a.store.load_chat("saved-chat")?;
                chat["native_id"] = json!(native);
                chat["external_codex"] = json!(true);
                chat["thread"]["latestTurn"] = Value::Null;
                chat["thread"]["session"]["status"] = json!("disconnected");
                chat["thread"]["session"]["statusConfidence"] = json!("unknown");
                a.store.save_chat(&chat, None)
            })
            .await
            .unwrap()
            .unwrap();
            let hub = Chats::start(db.clone(), config.clone()).await.unwrap();
            let saved_result = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let shell = hub
                        .call(
                            AgentKind::Codex,
                            "phone",
                            "orchestration.subscribeShell",
                            json!({}),
                        )
                        .await
                        .unwrap();
                    let row = shell["snapshot"]["threads"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|r| r["id"] == "saved-chat")
                        .unwrap();
                    if row["session"]["statusConfidence"] == "confirmed"
                        && db
                            .call(|a| a.store.load_chat("saved-chat"))
                            .await
                            .unwrap()
                            .unwrap()["desktop_read_revision"][1]
                            == revision
                    {
                        break row.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await;
            let saved = match saved_result {
                Ok(saved) => saved,
                Err(_) => {
                    let state = db
                        .call(|a| a.store.load_chat("saved-chat"))
                        .await
                        .unwrap()
                        .unwrap();
                    panic!(
                        "{kind}: revision={:?}, session={:?}, methods={}",
                        state["desktop_read_revision"],
                        state["thread"]["session"],
                        std::fs::read_to_string(profile.join("methods.log")).unwrap()
                    );
                }
            };
            assert_eq!(
                saved["latestTurn"]["state"],
                if expected == "failed" {
                    "error"
                } else {
                    expected
                }
            );
            assert_eq!(saved["latestTurn"]["completedAt"], "2026-10-01T00:01:00Z");
            assert!(saved["session"]["activeTurnId"].is_null());
            hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
                .await
                .unwrap();
            let before = std::fs::read_to_string(profile.join("methods.log"))
                .unwrap()
                .lines()
                .count();
            let hub = Chats::start(db.clone(), config).await.unwrap();
            // Windows can take longer to create the isolated reader while the
            // suite is busy. Observe its first request before checking that the
            // retained revision avoids history reads; startup speed is not the
            // invariant under test here.
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let methods =
                        std::fs::read_to_string(profile.join("methods.log")).unwrap_or_default();
                    if methods.lines().skip(before).any(|m| m == "thread/list") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("Restarted reader did not begin its metadata check");
            tokio::time::sleep(Duration::from_millis(600)).await;
            let methods = std::fs::read_to_string(profile.join("methods.log")).unwrap();
            assert!(methods.lines().skip(before).any(|m| m == "thread/list"));
            assert!(
                !methods
                    .lines()
                    .skip(before)
                    .any(|m| matches!(m, "thread/read" | "thread/turns/list")),
                "Already saved native revision must survive restart without rereading history"
            );
            hub.call(AgentKind::Codex, "phone", "native.shutdown", json!({}))
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn canonical_saved_messages_page_through_the_read_only_windows_connection() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("saved profile 测试");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let native = uuid::Uuid::new_v4().to_string();
    let path = profile.join("sessions/saved.jsonl");
    let mut rows =
        vec![json!({"type":"session_meta","payload":{"id":native,"history_mode":"paginated"}})];
    for n in 0..6 {
        let turn = format!("turn-{n}");
        rows.push(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn,"started_at":1790812800+n*10}}));
        rows.push(json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":native,"turn_id":turn,"item":{"type":"UserMessage","id":format!("user-{n}"),"content":[{"type":"text","text":format!("saved question {n}")}]},"started_at_ms":1790812800000_i64+n*10000,"completed_at_ms":1790812800000_i64+n*10000}}));
        rows.push(json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":native,"turn_id":turn,"item":{"type":"AgentMessage","id":format!("agent-{n}"),"phase":"final_answer","content":[{"type":"Text","text":format!("saved answer {n}")}]},"started_at_ms":1790812801000_i64+n*10000,"completed_at_ms":1790812801000_i64+n*10000}}));
        rows.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"started_at":1790812800+n*10,"completed_at":1790812802+n*10}}));
    }
    let mut text = String::new();
    for (ordinal, mut row) in rows.into_iter().enumerate() {
        row["ordinal"] = json!(ordinal);
        text += &row.to_string();
        text.push('\n');
    }
    let bytes = zstd::stream::encode_all(text.as_bytes(), 1).unwrap();
    std::fs::write(path.with_extension("jsonl.zst"), &bytes).unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":native,"path":path,"mode":"missing-index"}).to_string(),
    )
    .unwrap();
    let server = SharedServer::Desktop {
        desktop_endpoint: Endpoint {
            path: endpoint(&profile),
        },
        reader_executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        reader_workspace: root.path().into(),
        activation: None,
    };
    let mut c = server.connect_history(&profile).await.unwrap();
    let mut cursor = Value::Null;
    for n in (0..6).rev() {
        let response=rpc(&mut c,"thread/turns/list",json!({"threadId":native,"limit":1,"cursor":cursor,"sortDirection":"desc","itemsView":"summary"})).await;
        assert!(response.get("error").is_none(), "{response}");
        let page = &response["result"];
        assert_eq!(page["data"][0]["id"], format!("turn-{n}"));
        assert_eq!(
            page["data"][0]["items"][0]["content"][0]["text"],
            format!("saved question {n}")
        );
        assert_eq!(
            page["data"][0]["items"][1]["text"],
            format!("saved answer {n}")
        );
        cursor = page["nextCursor"].clone();
        assert_eq!(cursor.is_null(), n == 0);
    }
    c.close().await;
    // Older work is read through a new private Reader, not the Desktop socket.
    let config = AgentConfig {
        executable: env!("CARGO_BIN_EXE_history-fixture").into(),
        profile: profile.clone(),
        model: "test".into(),
        tools: Default::default(),
        remote_control: false,
        shared_server: None,
    };
    let mut reader =
        remodex_connect_native::codex_history_reader::Reader::open(&config, root.path())
            .await
            .unwrap();
    let page = reader.items(&native, "turn-0", None).await.unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    assert_eq!(page["data"][1]["item"]["text"], "saved answer 0");
    reader.close().await;
    assert_eq!(
        std::fs::read(path.with_extension("jsonl.zst")).unwrap(),
        bytes
    );
    assert!(!std::fs::read_to_string(profile.join("methods.log"))
        .unwrap()
        .lines()
        .any(|m| matches!(
            m,
            "thread/resume" | "turn/start" | "turn/steer" | "turn/interrupt"
        )));
}
