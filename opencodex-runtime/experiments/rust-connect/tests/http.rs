use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use remodex_connect_native::{
    auth::{self, Auth, Metadata},
    server::App,
    store::Store,
};
use serde_json::{json, Value};
use tower::ServiceExt;

fn setup() -> (tempfile::TempDir, App, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Auth::new(Store::open(dir.path()).unwrap());
    let token = a.invite("test", "http://127.0.0.1", auth::now()).unwrap()["pairingToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let pair = a
        .exchange(&token, None, Metadata::default(), auth::now())
        .unwrap()
        .unwrap();
    (
        dir,
        App::new(a, "native test").unwrap(),
        pair.access_token,
        pair.client_id,
    )
}

#[tokio::test]
async fn shell_socket_sends_small_live_changes_and_complete_refreshes() {
    use remodex_connect_native::chat::{AgentConfig, AgentKind, Chats, Config};
    use std::time::Duration;
    let (dir, mut app, token, _) = setup();
    let chats = Chats::start(
        app.auth.clone(),
        Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            claude: None,
            codex: Some(AgentConfig {
                executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
                profile: dir.path().into(),
                model: "test".into(),
                tools: Default::default(),
                shared_server: None,
                remote_control: false,
            }),
        },
    )
    .await
    .unwrap();
    for id in ["first", "second"] {
        chats.call(AgentKind::Codex,"test","orchestration.dispatchCommand",
            json!({"type":"thread.create","commandId":id,"threadId":id,"projectId":"native-test-project","title":id,"modelSelection":{"instanceId":"codex","model":"test"},"runtimeMode":"full-access","interactionMode":"default"})).await.unwrap();
    }
    app.chats = Some(chats.clone());
    let router = app.clone().router();
    let (_, ticket) = request(
        router.clone(),
        "/api/auth/websocket-ticket",
        Some(&token),
        Some(String::new()),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/ws?wsTicket={}",
        ticket["ticket"].as_str().unwrap()
    ))
    .await
    .unwrap();
    let subscribe =
        json!({"id":"shell","method":"orchestration.subscribeShell","params":{"refresh":true}})
            .to_string();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        subscribe.clone().into(),
    ))
    .await
    .unwrap();
    let first: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(first["event"]["kind"], "snapshot");
    assert_eq!(
        first["event"]["snapshot"]["threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Provider discovery/publish ticks must not resend unchanged lists.
    assert!(tokio::time::timeout(Duration::from_millis(600), ws.next())
        .await
        .is_err());
    chats.call(AgentKind::Codex,"test","orchestration.dispatchCommand",
        json!({"type":"thread.meta.update","threadId":"first","commandId":"rename","title":"Renamed"})).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let update: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(update["event"]["kind"], "thread-upserted");
    assert_eq!(update["event"]["thread"]["id"], "first");
    assert_eq!(update["event"]["thread"]["title"], "Renamed");
    assert!(
        update["event"]["sequence"].as_u64().unwrap()
            > first["event"]["snapshot"]["snapshotSequence"]
                .as_u64()
                .unwrap()
    );
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        subscribe.into(),
    ))
    .await
    .unwrap();
    let refreshed: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(refreshed["event"]["kind"], "snapshot");
    assert_eq!(
        refreshed["event"]["snapshot"]["threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let watch_thread = json!({"id":"detail","method":"orchestration.subscribeThread","params":{"threadId":"first"}}).to_string();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        watch_thread.clone().into(),
    ))
    .await
    .unwrap();
    let detail: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(detail["event"]["snapshot"]["thread"]["title"], "Renamed");
    chats.call(AgentKind::Codex,"test","orchestration.dispatchCommand",
        json!({"type":"thread.meta.update","threadId":"second","commandId":"unrelated","title":"Other chat changed"})).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(600), ws.next())
        .await
        .is_err());
    chats.call(AgentKind::Codex,"test","orchestration.dispatchCommand",
        json!({"type":"thread.meta.update","threadId":"first","commandId":"selected","title":"Selected chat changed"})).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let detail: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(
        detail["event"]["snapshot"]["thread"]["title"],
        "Selected chat changed"
    );
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        watch_thread.into(),
    ))
    .await
    .unwrap();
    let refreshed: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(refreshed["event"]["kind"], "snapshot");
    ws.close(None).await.unwrap();
    chats
        .call(AgentKind::Codex, "test", "native.shutdown", json!({}))
        .await
        .unwrap();
    server.abort();
}
async fn request(
    router: Router,
    path: &str,
    token: Option<&str>,
    body: Option<String>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" });
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        req = req.header("content-type", "application/x-www-form-urlencoded");
    }
    let res = router
        .oneshot(
            req.body(body.map(Body::from).unwrap_or_else(Body::empty))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn native_exchange_uses_android_wire_contract_and_requires_authentication() {
    let (_dir, app, token, _) = setup();
    let r = app.clone().router();
    let (_, v) = request(r.clone(), "/.well-known/t3/environment", None, None).await;
    assert_eq!(v["capabilities"], json!({}));
    let (_, v) = request(r.clone(), "/api/auth/session", None, None).await;
    assert_eq!(v["authenticated"], false);
    let (_, v) = request(r.clone(), "/api/auth/session", Some(&token), None).await;
    assert_eq!(v["authenticated"], true);
    assert_eq!(v["sessionMethod"], "bearer-access-token");
    let (status, _) = request(
        r.clone(),
        "/api/auth/websocket-ticket",
        None,
        Some(String::new()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, v) = request(
        r.clone(),
        "/api/auth/websocket-ticket",
        Some(&token),
        Some(String::new()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["protocol"], "opencodex-json-v1");
    assert!(v["ticket"].as_str().unwrap().starts_with("ocx_ws_"));
    let invite = app
        .auth
        .call(|a| a.invite("test", "http://127.0.0.1", auth::now()))
        .await
        .unwrap()
        .unwrap();
    let form=format!("grant_type=urn:ietf:params:oauth:grant-type:token-exchange&subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={}&client_pairing_proof={}",invite["pairingToken"].as_str().unwrap(),"a".repeat(64));
    let (status, v) = request(r.clone(), "/oauth/token", None, Some(form.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["token_type"], "Bearer");
    let (_, retry) = request(r.clone(), "/oauth/token", None, Some(form)).await;
    assert_eq!(retry["access_token"], v["access_token"]);
    let (status, _) = request(r, "/oauth/token", None, Some("x".repeat(65537))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
#[tokio::test]
async fn paired_phones_have_independent_bounded_live_connections() {
    let (_dir, app, first_token, _) = setup();
    let router = app.clone().router();
    let mut tokens = vec![first_token];
    for _ in 0..4 {
        tokens.push(
            app.auth
                .call(|auth| {
                    let invitation = auth
                        .invite("test", "http://127.0.0.1", auth::now())
                        .unwrap();
                    let paired = auth
                        .exchange(
                            invitation["pairingToken"].as_str().unwrap(),
                            None,
                            Metadata::default(),
                            auth::now(),
                        )
                        .unwrap()
                        .unwrap();
                    paired.access_token
                })
                .await
                .unwrap(),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_router = router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_router).await.unwrap() });
    let connect = |token: String| {
        let router = router.clone();
        async move {
            let (status, ticket) = request(
                router,
                "/api/auth/websocket-ticket",
                Some(&token),
                Some(String::new()),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            tokio_tungstenite::connect_async(format!(
                "ws://127.0.0.1:{port}/ws?wsTicket={}",
                ticket["ticket"].as_str().unwrap()
            ))
            .await
        }
    };
    let mut sockets = Vec::new();
    for token in tokens.iter().take(4) {
        for _ in 0..8 {
            sockets.push(connect(token.clone()).await.unwrap().0);
        }
        // A full client cannot consume another phone's allowance.
        let error = connect(token.clone()).await.unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(ref response)
            if response.status() == StatusCode::TOO_MANY_REQUESTS)
        );
    }
    // Global admission stays bounded even with additional authenticated phones.
    let error = connect(tokens[4].clone()).await.unwrap_err();
    assert!(
        matches!(error, tokio_tungstenite::tungstenite::Error::Http(ref response)
        if response.status() == StatusCode::TOO_MANY_REQUESTS)
    );
    // A completed RPC releases both allowances, allowing the same client back.
    let mut closing = sockets.pop().unwrap();
    closing.close(None).await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), closing.next())
        .await
        .unwrap();
    let (mut replacement, _) = connect(tokens[3].clone()).await.unwrap();
    replacement
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"id":"after-capacity", "method":"server.getConfig", "params":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply: Value = serde_json::from_str(
        replacement
            .next()
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply["id"], "after-capacity");
    server.abort();
}

#[tokio::test]
async fn silent_disconnected_sockets_release_capacity_but_live_phones_remain_connected() {
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;
    let (_dir, app, token, _) = setup();
    let router = app.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_router = router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_router).await.unwrap() });
    let connect = || {
        let router = router.clone();
        let token = token.clone();
        async move {
            let (_, ticket) = request(router, "/api/auth/websocket-ticket", Some(&token), Some(String::new())).await;
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?wsTicket={}", ticket["ticket"].as_str().unwrap())).await
        }
    };
    let (mut live, _) = connect().await.unwrap();
    let mut silent = Vec::new();
    for _ in 0..7 { silent.push(connect().await.unwrap().0); }
    assert!(connect().await.is_err());
    // These sockets deliberately stop reading, like a phone whose network has
    // disappeared without a TCP close. The healthy but idle phone answers only
    // pings: ordinary requests must not hide a broken pong keepalive path.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(24);
    let mut pongs = 0;
    while let Ok(frame) = tokio::time::timeout_at(deadline, live.next()).await {
        match frame.unwrap().unwrap() {
            Message::Ping(payload) => {
                live.send(Message::Pong(payload)).await.unwrap();
                pongs += 1;
            }
            _ => panic!("An idle healthy phone received an unexpected frame"),
        }
    }
    assert!(pongs >= 3, "An idle phone must receive periodic liveness checks");
    live.send(Message::Text(json!({"id":"still-live","method":"server.getConfig","params":{}}).to_string().into())).await.unwrap();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), live.next()).await.unwrap().unwrap().unwrap();
        if let Message::Ping(payload) = frame {
            live.send(Message::Pong(payload)).await.unwrap();
        } else if let Message::Text(text) = frame {
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["id"], "still-live");
            break;
        } else { panic!("A healthy phone was disconnected"); }
    }
    let mut replacements = Vec::new();
    for _ in 0..7 { replacements.push(connect().await.expect("Silent sockets must release their allowances").0); }
    // Cleanup must preserve the original per-phone bound.
    assert!(connect().await.is_err());
    server.abort();
}

#[tokio::test]
async fn websocket_ticket_is_one_use_preserves_request_ids_and_revokes_live_connection() {
    let (_dir, app, token, client) = setup();
    let router = app.clone().router();
    let (_, v) = request(
        router.clone(),
        "/api/auth/websocket-ticket",
        Some(&token),
        Some(String::new()),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let url = format!(
        "ws://127.0.0.1:{port}/ws?wsTicket={}",
        v["ticket"].as_str().unwrap()
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    // Readiness gate: these real Android contracts are still missing. Preserve
    // request identity and fail explicitly rather than fabricating success.
    for method in [
        "server.getConfig",
        "orchestration.subscribeShell",
        "orchestration.subscribeThread",
        "orchestration.getThreadPage",
        "orchestration.getTurnWork",
        "orchestration.dispatchCommand",
        "projects.searchEntries",
        "filesystem.browse",
        "assets.createUrl",
        "claude.getCapabilities",
        "claude.recoverDesktopSync",
    ] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"id":method,"method":method,"params":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let frame = ws.next().await.unwrap().unwrap();
        let response: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(response["id"], method);
        assert_eq!(response["error"]["code"], -32601);
    }
    app.auth
        .call(move |a| a.revoke(&client))
        .await
        .unwrap()
        .unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .unwrap();
    assert!(closed.is_none() || closed.unwrap().unwrap().is_close());
    server.abort();
}

#[tokio::test]
async fn first_pairing_recovers_a_lost_exchange_and_opens_its_first_socket() {
    use remodex_connect_native::chat::{AgentConfig, AgentKind, Chats, Config};
    let dir = tempfile::tempdir().unwrap();
    let mut auth = Auth::new(Store::open(dir.path()).unwrap());
    let invite = auth
        .invite("fresh PC", "http://127.0.0.1", auth::now())
        .unwrap();
    let mut app = App::new(auth, "fresh PC").unwrap();
    let provider = AgentConfig {
        executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
        profile: dir.path().into(),
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let chats = Chats::start(
        app.auth.clone(),
        Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            codex: Some(provider.clone()),
            claude: Some(provider),
        },
    )
    .await
    .unwrap();
    app.chats = Some(chats.clone());
    let router = app.clone().router();
    let form = format!("grant_type=urn:ietf:params:oauth:grant-type:token-exchange&subject_token_type=urn:t3:params:oauth:token-type:environment-bootstrap&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={}&client_pairing_proof={}&client_installation_id=phone-one", invite["pairingToken"].as_str().unwrap(), "a".repeat(64));

    // The server commits, but the phone loses the response before saving it.
    let (status, lost) = request(router.clone(), "/oauth/token", None, Some(form.clone())).await;
    assert_eq!(status, StatusCode::OK);
    let wrong_proof = form.replace(&"a".repeat(64), &"b".repeat(64));
    assert_eq!(
        request(router.clone(), "/oauth/token", None, Some(wrong_proof))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, recovered) = request(router.clone(), "/oauth/token", None, Some(form)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recovered["access_token"], lost["access_token"]);
    let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM clients", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let token = recovered["access_token"].as_str().unwrap();
    assert_eq!(
        request(router.clone(), "/api/auth/session", Some(token), None)
            .await
            .1["authenticated"],
        true
    );
    let (_, ticket) = request(
        router.clone(),
        "/api/auth/websocket-ticket",
        Some(token),
        Some(String::new()),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/ws?wsTicket={}",
        ticket["ticket"].as_str().unwrap()
    ))
    .await
    .unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"id":"first","method":"server.getConfig"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let reply: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(reply["id"], "first");
    assert_eq!(
        reply["result"]["environment"]["capabilities"]["androidRemoteFileAttachments"],
        true
    );
    assert_eq!(
        reply["result"]["environment"]["environmentId"],
        app.environment_id
    );
    assert_eq!(
        reply["result"]["environment"]["capabilities"]["androidRemoteLatestPromptEditOnly"],
        true
    );
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"id":"subscribe","method":"subscribeServerConfig"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let event: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(
        event["event"]["config"]["environment"]["capabilities"]["androidRemoteFileAttachments"],
        true
    );
    assert_eq!(
        event["event"]["config"]["environment"]["capabilities"]
            ["androidRemoteLatestPromptEditOnly"],
        true
    );
    ws.close(None).await.unwrap();
    let (_, ticket) = request(
        app.clone().router(),
        "/api/auth/websocket-ticket",
        Some(token),
        Some(String::new()),
    )
    .await;
    let (mut claude_ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/ws?agent=claude-code&wsTicket={}",
        ticket["ticket"].as_str().unwrap()
    ))
    .await
    .unwrap();
    for method in ["server.getConfig", "subscribeServerConfig"] {
        claude_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":method,"method":method}).to_string().into(),
            ))
            .await
            .unwrap();
        let reply: Value =
            serde_json::from_str(claude_ws.next().await.unwrap().unwrap().to_text().unwrap())
                .unwrap();
        let config = if method == "server.getConfig" {
            &reply["result"]
        } else {
            &reply["event"]["config"]
        };
        assert_eq!(
            config["environment"]["capabilities"]["androidRemoteFileAttachments"],
            true
        );
        assert_eq!(
            config["environment"]["capabilities"]["androidRemoteResumableUploads"],
            true
        );
    }
    claude_ws.close(None).await.unwrap();
    server.abort();
    chats
        .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}
#[tokio::test]
async fn sqlite_contention_does_not_block_health_or_network_runtime() {
    let (dir, app, _token, _) = setup();
    let router = app.clone().router();
    let invite = app
        .auth
        .call(|a| a.invite("test", "http://127.0.0.1", auth::now()))
        .await
        .unwrap()
        .unwrap();
    let database = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    database.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let service = app.auth.clone();
    let blocked = tokio::spawn(async move {
        service
            .call(move |a| {
                a.exchange(
                    invite["pairingToken"].as_str().unwrap(),
                    None,
                    Metadata::default(),
                    auth::now(),
                )
            })
            .await
    });
    tokio::task::yield_now().await;
    let (status, v) = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        request(router, "/healthz", None, None),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["migrationComplete"], false);
    database.execute_batch("ROLLBACK").unwrap();
    let _ = blocked.await.unwrap();
}

#[tokio::test]
async fn android_http_chat_creation_and_websocket_reads_share_native_state() {
    use remodex_connect_native::chat::{AgentConfig, AgentKind, Chats, Config};
    let (dir, mut app, token, _) = setup();
    let agent = AgentConfig {
        executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
        profile: dir.path().into(),
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let chats = Chats::start(
        app.auth.clone(),
        Config {
            storage: Default::default(),
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            codex: Some(agent.clone()),
            claude: Some(agent),
        },
    )
    .await
    .unwrap();
    app.chats = Some(chats.clone());
    for suffix in ["", "?agent=claude-code"] {
        let id = if suffix.is_empty() {
            "codex-http"
        } else {
            "claude-http"
        };
        let command = json!({"type":"thread.create","threadId":id,"commandId":id,"projectId":"native-test-project"});
        let request = |auth: bool| {
            let mut r = Request::builder()
                .uri(format!("/api/orchestration/dispatch{suffix}"))
                .method("POST")
                .header("content-type", "application/json");
            if auth {
                r = r.header("authorization", format!("Bearer {token}"));
            }
            r.body(Body::from(command.to_string())).unwrap()
        };
        assert_eq!(
            app.clone()
                .router()
                .oneshot(request(false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .router()
                .oneshot(request(true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let a = if suffix.is_empty() {
            AgentKind::Codex
        } else {
            AgentKind::Claude
        };
        let v = chats
            .call(
                a,
                "phone",
                "orchestration.subscribeThread",
                json!({"threadId":id}),
            )
            .await
            .unwrap();
        assert_eq!(v["snapshot"]["thread"]["id"], id);
        let (status, v) = request_snapshot(
            app.clone().router(),
            &format!("/api/orchestration/snapshot{suffix}"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["threads"][0]["id"], id);
    }
    let bad = app.clone().router().oneshot(Request::builder()
        .uri("/api/orchestration/dispatch").method("POST")
        .header("authorization",format!("Bearer {token}"))
        .header("content-type","application/json")
        .body(Body::from(json!({"type":"thread.turn.start","threadId":"missing-chat","commandId":"bad","message":{"messageId":"bad","text":"hello","attachments":[]}}).to_string())).unwrap()).await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(bad.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(body["message"], body["error"]);
    assert!(!body["message"].as_str().unwrap().is_empty());
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; 128 * 1024]);
    let command = json!({"type":"thread.turn.start","commandId":"upload","threadId":"codex-http","message":{"messageId":"uploaded","text":"read file","attachments":[{"type":"file","name":"large.txt","mimeType":"text/plain","dataUrl":format!("data:text/plain;base64,{data}")}]}});
    let res = app
        .clone()
        .router()
        .oneshot(
            Request::builder()
                .uri("/api/orchestration/dispatch")
                .method("POST")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(command.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    chats
        .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}
async fn request_snapshot(router: Router, path: &str, token: &str) -> (StatusCode, Value) {
    let r = router
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let b = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&b).unwrap())
}

#[tokio::test]
async fn previews_require_an_authorized_exact_file_and_stop_after_phone_revocation() {
    use remodex_connect_native::chat::{AgentConfig, AgentKind, Chats, Config};
    let (dir, mut app, token, client) = setup();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(800, 400)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let image = encoded.into_inner();
    std::fs::write(workspace.join("image.png"), &image).unwrap();
    std::fs::write(dir.path().join("outside.png"), &image).unwrap();
    let a = AgentConfig {
        executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
        profile: dir.path().into(),
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let chats = Chats::start(
        app.auth.clone(),
        Config {
            storage: Default::default(),
            workspace,
            native_installation: None,
            native_profile_handoff: None,
            connection: None,
            max_active_chats: 1,
            codex: Some(a.clone()),
            claude: Some(a),
        },
    )
    .await
    .unwrap();
    chats.call(AgentKind::Codex,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","commandId":"create","threadId":"preview","projectId":"native-test-project"})).await.unwrap();
    app.chats = Some(chats.clone());
    let (_, ticket) = request(
        app.clone().router(),
        "/api/auth/websocket-ticket",
        Some(&token),
        Some(String::new()),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.clone().router();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/ws?wsTicket={}",
        ticket["ticket"].as_str().unwrap()
    ))
    .await
    .unwrap();
    // Assistant-shared artifacts can live outside the initial chat directory.
    // They must work through the same signed HTTP path as workspace previews.
    let report = dir.path().join("report + notes.txt");
    std::fs::write(&report, "Verification passed\n").unwrap();
    let mut fixtures = Vec::new();
    for (extension, format) in [
        ("png", image::ImageFormat::Png),
        ("jpg", image::ImageFormat::Jpeg),
        ("gif", image::ImageFormat::Gif),
        ("webp", image::ImageFormat::WebP),
    ] {
        let path = dir.path().join(format!("comparison (final).{extension}"));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(64, 32)
            .write_to(&mut encoded, format)
            .unwrap();
        let bytes = encoded.into_inner();
        std::fs::write(&path, &bytes).unwrap();
        fixtures.push((path, bytes));
    }
    let links = fixtures
        .iter()
        .map(|(path, _)| format!("![Proof](<{}>)", path.to_str().unwrap().replace('\\', "/")))
        .chain(std::iter::once(format!(
            "[Report](<{}>)",
            report.to_str().unwrap().replace('\\', "/")
        )))
        .collect::<Vec<_>>()
        .join("\n\n");
    app.auth
        .call(move |auth| {
            let mut state = auth.store.load_chat("preview").unwrap();
            state["thread"]["messages"] =
                json!([{"id":"shared-proof", "role":"assistant", "text":links}]);
            auth.store.save_chat(&state, None).unwrap();
        })
        .await
        .unwrap();
    for (i, (path, bytes)) in fixtures.iter().enumerate() {
        let id = 100 + i;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":id,"method":"assets.createUrl","params":{"resource":{"_tag":"workspace-file","threadId":"preview","path":path}}}).to_string().into())).await.unwrap();
        let reply: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        let url = reply["result"]["relativeUrl"].as_str().unwrap();
        let full = app
            .clone()
            .router()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(
            full.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            bytes
        );
        let thumbnail = app
            .clone()
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("{url}/thumbnail"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(thumbnail.status(), StatusCode::OK);
        let thumb_bytes = thumbnail.into_body().collect().await.unwrap().to_bytes();
        assert!(image::load_from_memory(&thumb_bytes).is_ok());
    }
    let text = chats
        .call(
            AgentKind::Codex,
            "phone",
            "filesystem.readTextFile",
            json!({"threadId":"preview","path":report}),
        )
        .await
        .unwrap();
    assert_eq!(text["content"], "Verification passed\n");
    assert!(chats
        .call(
            AgentKind::Claude,
            "phone",
            "filesystem.readTextFile",
            json!({"threadId":"preview","path":report})
        )
        .await
        .is_err());
    for (i, path) in ["../outside.png", "image.png"].into_iter().enumerate() {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":i,"method":"assets.createUrl","params":{"resource":{"_tag":"workspace-file","threadId":"preview","path":path}}}).to_string().into())).await.unwrap();
        let reply: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if i == 0 {
            assert!(reply.get("error").is_some());
            continue;
        }
        let url = reply["result"]["relativeUrl"].as_str().unwrap();
        let res = app
            .clone()
            .router()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert_eq!(res.headers()["content-length"], image.len().to_string());
        // The gallery overlaps/replaces downloads. Four bounded streams can
        // coexist, and a thumbnail does not contend for their transfer slots.
        let mut held = Vec::new();
        for _ in 0..3 {
            let r = app
                .clone()
                .router()
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            held.push(r);
        }
        let thumb = app
            .clone()
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("{url}/thumbnail"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(thumb.status(), StatusCode::OK);
        drop(thumb);
        let waiting = tokio::spawn(
            app.clone()
                .router()
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap()),
        );
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !waiting.is_finished(),
            "overlap waits rather than returning 429"
        );
        let mut queued = Vec::new();
        for _ in 0..7 {
            queued.push(tokio::spawn(
                app.clone()
                    .router()
                    .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap()),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(queued.iter().all(|q| !q.is_finished()));
        let excess = app
            .clone()
            .router()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(excess.status(), StatusCode::TOO_MANY_REQUESTS);
        for q in queued {
            q.abort();
            assert!(q.await.unwrap_err().is_cancelled());
        }
        // Cancelling an older body immediately admits the replacement.
        drop(held.pop());
        let replacement = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(replacement.status(), StatusCode::OK);
        assert_eq!(
            &replacement.into_body().collect().await.unwrap().to_bytes()[..],
            image
        );
        drop(held);
        assert_eq!(
            &res.into_body().collect().await.unwrap().to_bytes()[..],
            image
        );
        let res = app
            .clone()
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("{url}/thumbnail"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let small = image::load_from_memory(&bytes).unwrap();
        assert_eq!((small.width(), small.height()), (256, 128));
        assert!(bytes.len() < image.len());
        let client = client.clone();
        app.auth
            .call(move |a| a.store.revoke(&client))
            .await
            .unwrap()
            .unwrap();
        let res = app
            .clone()
            .router()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("{url}/thumbnail"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
    ws.close(None).await.unwrap();
    task.abort();
    chats
        .call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn device_metadata_requires_auth_and_cannot_replace_installation_identity() {
    let (_dir, app, token, _) = setup();
    let before_token = token.clone();
    let before = app
        .auth
        .call(move |a| a.authenticate(&before_token, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for authenticated in [false, true] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/auth/client-metadata")
            .header("content-type", "application/json");
        if authenticated {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response=app.clone().router().oneshot(req.body(Body::from(json!({"label":"Realme test","os":"Android","installationId":"other-id","expiresAt":9999999999999i64}).to_string())).unwrap()).await.unwrap();
        assert_eq!(
            response.status(),
            if authenticated {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
    }
    let after = app
        .auth
        .call(move |a| a.authenticate(&token, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(after.label, "Realme test");
    assert_eq!(after.os, "Android");
    assert_eq!(after.installation_digest, before.installation_digest);
    assert_eq!(after.expires_at, before.expires_at);
    assert_eq!(after.id, before.id);
}

#[tokio::test]
async fn phone_chunks_require_authentication_and_accept_more_than_default_json_limit() {
    use base64::Engine;
    let (_dir, app, token, _) = setup();
    let r = app.phone_router();
    let begin=json!({"action":"begin","id":"phone-file","file":{"type":"file","name":"test.txt","mimeType":"text/plain","sizeBytes":131072}}).to_string();
    assert_eq!(
        request(
            r.clone(),
            "/api/android-remote/uploads",
            None,
            Some(begin.clone())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            r.clone(),
            "/api/android-remote/uploads",
            Some(&token),
            Some(begin)
        )
        .await
        .0,
        StatusCode::OK
    );
    let data = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; 131072]);
    let (status, value) = request(
        r.clone(),
        "/api/android-remote/uploads",
        Some(&token),
        Some(json!({"action":"chunk","id":"phone-file","offset":0,"data":data}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["offset"], 131072);
    let (status, value) = request(
        r,
        "/api/android-remote/uploads",
        Some(&token),
        Some(json!({"action":"finish","id":"phone-file"}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["ready"], true);
}
