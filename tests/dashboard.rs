use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use remodex_connect_native::{
    auth::{self, Auth, Metadata},
    dashboard::Dashboard,
    server::App,
    store::Store,
};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    _root: tempfile::TempDir,
    app: App,
    phone: String,
    client: String,
}
fn setup() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let gui = root.path().join("gui");
    std::fs::create_dir(&gui).unwrap();
    std::fs::write(gui.join("index.html"),"<!doctype html><html><head><title>Existing UI</title></head><body>existing React UI</body></html>").unwrap();
    std::fs::write(gui.join("main.js"), "window.originalUI = true").unwrap();
    std::fs::write(gui.join("private.json"), "PRIVATE_TEST_DATA").unwrap();
    let mut auth = Auth::new(Store::open(&root.path().join("state")).unwrap());
    let invite = auth
        .invite("test", "http://127.0.0.1:18900", auth::now())
        .unwrap();
    let phone = auth
        .exchange(
            invite["pairingToken"].as_str().unwrap(),
            None,
            Metadata::default(),
            auth::now(),
        )
        .unwrap()
        .unwrap();
    let mut app = App::new(auth, "test PC").unwrap();
    app.dashboard = Some(Dashboard::new(&gui, "http://127.0.0.1:18900").unwrap());
    Fixture {
        _root: root,
        app,
        phone: phone.access_token,
        client: phone.client_id,
    }
}
async fn call(
    router: Router,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Value,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:18900")
        .header("content-type", "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let res = router
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}
fn meta(html: &str, key: &str) -> String {
    html.split(&format!("name=\"{key}\" content=\""))
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}
async fn session(router: Router) -> (String, String) {
    let (s, html) = call(router, "GET", "/opencodex-session", &[], Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    (
        meta(&html, "opencodex-session-token"),
        meta(&html, "opencodex-session-csrf"),
    )
}

#[tokio::test]
async fn agents_dashboard_uses_real_profile_status_and_isolated_preferences() {
    use remodex_connect_native::chat::{AgentConfig, Chats, Config};
    let mut f = setup();
    let agent = AgentConfig {
        executable: env!("CARGO_BIN_EXE_chat-fixture").into(),
        profile: f._root.path().into(),
        model: "test".into(),
        tools: Default::default(),
        shared_server: None,
        remote_control: false,
    };
    let mut claude = agent.clone();
    claude.remote_control = true;
    let config = Config {
        storage: Default::default(),
        max_active_chats: 1,
        workspace: f._root.path().into(),
        codex: agent,
        claude,
    };
    let hub = Chats::start(f.app.auth.clone(), config).await.unwrap();
    f.app.chats = Some(hub.clone());
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let path = "/api/android-remote/claude-code";
    assert_eq!(
        call(router.clone(), "GET", path, &[], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let started = std::time::Instant::now();
    loop {
        let (code, body) = call(router.clone(), "GET", path, &good, Value::Null).await;
        assert_eq!(code, StatusCode::OK);
        assert!(!body.contains("PRIVATE_FIXTURE"));
        let v: Value = serde_json::from_str(&body).unwrap();
        if v["ready"] == true {
            assert_eq!(v["account"]["subscriptionType"], "pro");
            break;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(8));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let activity_path = "/api/connect/activity";
    assert_eq!(
        call(router.clone(), "GET", activity_path, &[], Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            router.clone(),
            "GET",
            activity_path,
            &[("authorization", &format!("Bearer {}", f.phone))],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let sessions = f._root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let transcript = sessions.join("test.jsonl");
    let source=concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"PRIVATE_THREAD\"}}\n",
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"fixture-model\",\"turn_id\":\"PRIVATE_TURN\"}}\n",
        "{\"type\":\"event_msg\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"PRIVATE_TURN\"}}\n"
    );
    std::fs::write(&transcript, source).unwrap();
    let (code, body) = call(router.clone(), "GET", activity_path, &good, Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains("PRIVATE"));
    let report: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["source"], "native-codex");
    assert_eq!(report["rows"][0]["model"], "fixture-model");
    assert_eq!(report["diagnostics"]["configReadOnly"], true);
    assert_eq!(std::fs::read_to_string(transcript).unwrap(), source);
    assert_eq!(
        call(router.clone(), "POST", activity_path, &good, json!({}))
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    for headers in [
        vec![],
        vec![("authorization", format!("Bearer {}", f.phone))],
    ] {
        let headers: Vec<_> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(
            call(router.clone(), "GET", "/api/storage", &headers, Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (code, body) = call(router.clone(), "GET", "/api/storage", &good, Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    let report: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        report["codexHome"],
        f._root.path().to_string_lossy().as_ref()
    );
    assert_eq!(report["buckets"].as_array().unwrap().len(), 7);
    assert_eq!(report["buckets"][0]["fileCount"], 1);
    assert_eq!(
        std::fs::read_to_string(sessions.join("test.jsonl")).unwrap(),
        source
    );
    assert!(!body.contains("PRIVATE_THREAD"));
    assert_eq!(
        call(router.clone(), "POST", "/api/storage", &good, json!({}))
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let (code, body) = call(
        router.clone(),
        "PUT",
        path,
        &good,
        json!({"autoConnectNewSessions":true}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["autoConnect"]["preference"],
        true
    );
    let settings = f._root.path().join("settings.json");
    let before = std::fs::read(&settings).unwrap();
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            path,
            &good,
            json!({"autoConnectNewSessions":false,"enabled":false})
        )
        .await
        .0,
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(std::fs::read(&settings).unwrap(), before);
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            path,
            &good,
            json!({"autoConnectNewSessions":"true"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            router,
            "PUT",
            path,
            &good[..2],
            json!({"autoConnectNewSessions":false})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for body in [
        json!({"enabled":false}),
        json!({"enabled":true}),
        json!({"desktopSync":false}),
        json!({"desktopSync":true}),
    ] {
        let (code, response) = call(f.app.clone().router(), "PUT", path, &good, body.clone()).await;
        assert_eq!(code, StatusCode::OK, "{response}");
        let value: Value = serde_json::from_str(&response).unwrap();
        for (key, expected) in body.as_object().unwrap() {
            assert_eq!(&value[key], expected);
        }
    }
    assert_eq!(
        call(
            f.app.clone().router(),
            "PUT",
            path,
            &good,
            json!({"enabled":"false"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            f.app.clone().router(),
            "PUT",
            path,
            &good[..2],
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        std::fs::read_to_string(f._root.path().join("setup-auth-probes.log"))
            .unwrap()
            .lines()
            .count(),
        1,
        "status requests must reuse the bounded cache"
    );
    hub.call(
        remodex_connect_native::chat::AgentKind::Codex,
        "local",
        "native.shutdown",
        json!({}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn expired_phone_remains_visible_and_can_be_repaired_without_deleting_history() {
    let f = setup();
    let db =
        rusqlite::Connection::open(f._root.path().join("state/native-connect.sqlite")).unwrap();
    db.execute("UPDATE clients SET expires_at=0 WHERE id=?1", [&f.client])
        .unwrap();
    drop(db);
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let (code, body) = call(
        router.clone(),
        "GET",
        "/api/android-remote",
        &good,
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let status: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status["clients"][0]["id"], f.client);
    assert_eq!(status["clients"][0]["online"], false);
    let (code, body) = call(
        router,
        "POST",
        &format!("/api/android-remote/pairing?replaceClientId={}", f.client),
        &good,
        json!({}),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let value: Value = serde_json::from_str(&body).unwrap();
    let qr: Value = serde_json::from_str(value["qrPayload"].as_str().unwrap()).unwrap();
    f.app
        .auth
        .call(move |a| {
            assert!(a.authenticate(&f.phone, auth::now()).unwrap().is_none());
            let exchanged = a
                .exchange(
                    qr["pairingToken"].as_str().unwrap(),
                    None,
                    Metadata::default(),
                    auth::now(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(exchanged.replaced_client_ids, vec![f.client]);
            assert!(a
                .authenticate(&exchanged.access_token, auth::now())
                .unwrap()
                .is_some());
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn dashboard_requires_local_socket_host_origin_and_distinct_credentials() {
    let f = setup();
    let router = f.app.clone().router();
    for (peer, host, origin) in [
        ("10.1.2.3:321", "localhost:18900", None),
        ("127.0.0.1:321", "attacker.invalid", None),
        (
            "127.0.0.1:321",
            "localhost:18900",
            Some("http://attacker.invalid"),
        ),
        (
            "127.0.0.1:321",
            "localhost:18900",
            Some("http://localhost:18901"),
        ),
    ] {
        let mut req = Request::builder()
            .uri("/opencodex-session")
            .header("host", host)
            .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let res = router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(
            !String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
                .unwrap()
                .contains("ocx_session_")
        );
    }
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    assert_eq!(
        call(
            router.clone(),
            "GET",
            "/api/android-remote",
            &[],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            router.clone(),
            "GET",
            "/api/android-remote",
            &[("x-opencodex-api-key", &f.phone)],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (s, text) = call(
        router.clone(),
        "GET",
        "/api/android-remote",
        &good,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let status: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["clients"][0]["id"], f.client);
    assert_eq!(status["clients"][0]["online"], false);
    assert!(!text.contains("digest"));
    assert!(!text.contains(&f.phone));
    for bad in [&good[..1], &good[..2], &good[..3]] {
        assert_eq!(
            call(
                router.clone(),
                "POST",
                "/api/android-remote/pairing",
                bad,
                json!({})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (s, text) = call(
        router.clone(),
        "POST",
        "/api/android-remote/pairing",
        &good,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let pairing: Value = serde_json::from_str(&text).unwrap();
    // The existing React client sends no body and no Content-Type here.
    let mut empty = Request::builder()
        .method("POST")
        .uri("/api/android-remote/pairing")
        .header("host", "localhost:18900")
        .extension(ConnectInfo(
            "127.0.0.1:321".parse::<std::net::SocketAddr>().unwrap(),
        ));
    for (k, v) in good {
        empty = empty.header(k, v);
    }
    assert_eq!(
        router
            .clone()
            .oneshot(empty.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        call(
            router.clone(),
            "POST",
            "/api/android-remote/pairing?replaceClientId=existing",
            &good,
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (s, repair) = call(
        router.clone(),
        "POST",
        &format!("/api/android-remote/pairing?replaceClientId={}", f.client),
        &good,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let repair: Value = serde_json::from_str(&repair).unwrap();
    let repair_id = repair["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(
            router.clone(),
            "POST",
            &format!(
                "/api/android-remote/pairing?replaceClientId={}",
                uuid::Uuid::new_v4()
            ),
            &good,
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let qr: Value = serde_json::from_str(pairing["qrPayload"].as_str().unwrap()).unwrap();
    assert_eq!(qr["directUrl"], "http://127.0.0.1:18900");
    assert_eq!(
        call(
            router.clone(),
            "POST",
            "/api/android-remote/pairing",
            &good,
            json!({"extra":true})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let session = call(
        router.clone(),
        "GET",
        "/api/auth/session",
        &[("authorization", &format!("Bearer {token}"))],
        Value::Null,
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&session.1).unwrap()["authenticated"],
        false
    );
    assert_eq!(
        call(
            router.clone(),
            "DELETE",
            &format!("/api/android-remote/clients/{}", f.client),
            &good,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let old = f.phone;
    assert!(!f
        .app
        .auth
        .call(move |a| a.has_invitation(&repair_id, auth::now()))
        .await
        .unwrap());
    assert!(f
        .app
        .auth
        .call(move |a| a.authenticate(&old, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn persisted_disable_revokes_pending_capabilities_without_erasing_paired_phones() {
    let f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let client = f.client.clone();
    let ticket = f
        .app
        .auth
        .call(move |a| a.ticket(&client, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    let invitation = f
        .app
        .auth
        .call(|a| a.invite("test", "http://127.0.0.1", auth::now()))
        .await
        .unwrap()
        .unwrap();
    for bad in [
        json!({"controlEnabled":false,"keepAwake":true,"connectionChoice":"quick"}),
        json!({"controlEnabled":false,"connectionChoice":"named"}),
        json!({"controlEnabled":false,"localNetworkEnabled":false}),
    ] {
        assert_eq!(
            call(
                router.clone(),
                "PUT",
                "/api/android-remote/settings",
                &good,
                bad
            )
            .await
            .0,
            StatusCode::NOT_IMPLEMENTED
        );
        assert!(f
            .app
            .auth
            .call(|a| a.control_enabled())
            .await
            .unwrap()
            .unwrap());
    }
    for bad in [
        json!({}),
        json!({"controlEnabled":"false"}),
        json!({"unknown":false}),
    ] {
        assert_eq!(
            call(
                router.clone(),
                "PUT",
                "/api/android-remote/settings",
                &good,
                bad
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            "/api/android-remote/settings",
            &good,
            json!({"controlEnabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    let phone = f.phone.clone();
    for (method, path) in [
        ("GET", "/.well-known/t3/environment"),
        ("GET", "/api/auth/session"),
        ("POST", "/api/auth/websocket-ticket"),
        ("POST", "/oauth/token"),
        ("GET", "/api/orchestration/snapshot"),
        ("GET", "/ws"),
    ] {
        assert_eq!(
            call(
                router.clone(),
                method,
                path,
                &[("authorization", &format!("Bearer {phone}"))],
                Value::Null
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    assert!(f
        .app
        .auth
        .call(move |a| a.authenticate(&phone, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert!(f
        .app
        .auth
        .call(move |a| a.consume_ticket(&ticket, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert_eq!(
        call(
            router.clone(),
            "POST",
            "/api/android-remote/pairing",
            &good,
            json!({})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let reopened = Auth::new(Store::open(&f._root.path().join("state")).unwrap());
    assert!(!reopened.control_enabled().unwrap());
    drop(reopened);
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            "/api/android-remote/settings",
            &good,
            json!({"controlEnabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let phone = f.phone;
    assert!(f
        .app
        .auth
        .call(move |a| a.authenticate(&phone, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .is_some());
    assert!(f
        .app
        .auth
        .call(move |a| a.exchange(
            invitation["pairingToken"].as_str().unwrap(),
            None,
            Metadata::default(),
            auth::now()
        ))
        .await
        .unwrap()
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn serves_unchanged_gui_assets_with_private_bootstrap_and_confined_paths() {
    let f = setup();
    let router = f.app.clone().router();
    let req = Request::builder()
        .uri("/")
        .header("host", "localhost:18900")
        .extension(ConnectInfo(
            "127.0.0.1:321".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["x-frame-options"], "DENY");
    let html =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(html.contains("existing React UI"));
    assert!(meta(&html, "opencodex-session-token").starts_with("ocx_session_"));
    let (s, text) = call(router.clone(), "GET", "/main.js", &[], Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(text, "window.originalUI = true");
    for path in [
        "/private.json",
        "/%2e%2e/private.json",
        "/bad%xx.js",
        "/assets/..%5cprivate.js",
        "/C:/secret.js",
        "/api/missing",
    ] {
        assert_eq!(
            call(router.clone(), "GET", path, &[], Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            f._root.path().join("gui/private.json"),
            f._root.path().join("outside.js"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            f._root.path().join("outside.js"),
            f._root.path().join("gui/escape.js"),
        )
        .unwrap();
        assert_eq!(
            call(router.clone(), "GET", "/escape.js", &[], Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn disabling_phone_control_closes_live_socket_and_status_tracks_real_presence() {
    use futures_util::StreamExt;
    let f = setup();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = f.app.clone().router();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = f.client.clone();
    let ticket = f
        .app
        .auth
        .call(move |a| a.ticket(&client, auth::now()))
        .await
        .unwrap()
        .unwrap()
        .unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?wsTicket={ticket}"))
            .await
            .unwrap();
    let r = f.app.clone().router();
    let (token, csrf) = session(r.clone()).await;
    let h = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let (_, s) = call(r.clone(), "GET", "/api/android-remote", &h, Value::Null).await;
    assert_eq!(
        serde_json::from_str::<Value>(&s).unwrap()["clients"][0]["online"],
        true
    );
    call(
        r.clone(),
        "PUT",
        "/api/android-remote/settings",
        &h,
        json!({"controlEnabled":false}),
    )
    .await;
    let closed = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .unwrap();
    assert!(closed.is_none() || closed.unwrap().unwrap().is_close());
    let (_, s) = call(r, "GET", "/api/android-remote", &h, Value::Null).await;
    assert_eq!(
        serde_json::from_str::<Value>(&s).unwrap()["clients"][0]["online"],
        false
    );
    server.abort();
}

#[tokio::test]
async fn stored_awake_preference_survives_restart_without_changing_phone_control() {
    let f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let headers = [
        ("x-opencodex-api-key", token),
        ("x-opencodex-gui-origin", "http://localhost:18900".into()),
        ("x-opencodex-csrf-token", csrf),
        ("origin", "http://localhost:18900".into()),
    ];
    let h = headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect::<Vec<_>>();
    let before = call(
        router.clone(),
        "GET",
        "/api/android-remote",
        &h,
        Value::Null,
    )
    .await;
    assert_eq!(before.0, StatusCode::OK);
    let before: Value = serde_json::from_str(&before.1).unwrap();
    assert_eq!(before["keepAwake"], false);
    let changed = call(
        router.clone(),
        "PUT",
        "/api/android-remote/settings",
        &h,
        json!({"keepAwake":true}),
    )
    .await;
    assert_eq!(changed.0, StatusCode::OK);
    let changed: Value = serde_json::from_str(&changed.1).unwrap();
    assert_eq!(changed["keepAwake"], true);
    assert_eq!(changed["clients"], before["clients"]);
    assert_eq!(changed["controlEnabled"], true);
    let reopened = Auth::new(Store::open(&f._root.path().join("state")).unwrap());
    assert!(reopened.keep_awake().unwrap());
    assert!(reopened.control_enabled().unwrap());
    // A storage failure must roll back the whole update, including auth state.
    let disk =
        rusqlite::Connection::open(f._root.path().join("state/native-connect.sqlite")).unwrap();
    disk.execute_batch("CREATE TRIGGER fail_awake BEFORE UPDATE ON metadata WHEN NEW.key='keep_awake' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let failed = call(
        router.clone(),
        "PUT",
        "/api/android-remote/settings",
        &h,
        json!({"controlEnabled":false,"keepAwake":false}),
    )
    .await;
    assert_eq!(failed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(f
        .app
        .auth
        .call(|a| a.control_enabled())
        .await
        .unwrap()
        .unwrap());
    assert!(f.app.auth.call(|a| a.keep_awake()).await.unwrap().unwrap());
    disk.execute_batch("DROP TRIGGER fail_awake").unwrap();
    let reset = call(
        router,
        "PUT",
        "/api/android-remote/settings",
        &h,
        json!({"keepAwake":false}),
    )
    .await;
    assert_eq!(reset.0, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&reset.1).unwrap()["keepAwake"],
        false
    );
}
