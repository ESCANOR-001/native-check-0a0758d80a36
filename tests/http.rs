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
            native_profile_handoff: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            codex: provider.clone(),
            claude: provider,
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
        event["event"]["config"]["environment"]["capabilities"]
            ["androidRemoteLatestPromptEditOnly"],
        true
    );
    ws.close(None).await.unwrap();
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
            native_profile_handoff: None,
            max_active_chats: 1,
            workspace: dir.path().into(),
            codex: agent.clone(),
            claude: agent,
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
            native_profile_handoff: None,
            max_active_chats: 1,
            codex: a.clone(),
            claude: a,
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
