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
    call_from(router, method, path, extra, body, "127.0.0.1:12345").await
}
async fn call_from(
    router: Router,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Value,
    peer: &str,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:18900")
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
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
async fn diagnostics_requires_dashboard_authority_and_excludes_request_content() {
    let mut f = setup();
    let log = remodex_connect_native::diagnostics::Log::new(&f._root.path().join("state")).unwrap();
    log.lifecycle(remodex_connect_native::diagnostics::Lifecycle::Ready);
    log.flush().await;
    f.app.diagnostics = Some(log.clone());
    let router = f.app.router();
    let path = "/api/diagnostics/desktop-log";
    assert_eq!(
        call(router.clone(), "GET", path, &[], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            router.clone(),
            "GET",
            path,
            &[("authorization", &format!("Bearer {}", f.phone))],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (token, csrf) = session(router.clone()).await;
    let headers = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let (code, body) = call(
        router.clone(),
        "GET",
        "/api/diagnostics/desktop-log?token=PRIVATE_QUERY",
        &headers,
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["reportVersion"], 1);
    assert!(value["log"].as_str().unwrap().contains("runtime.ready"));
    log.flush().await;
    let report = log.report(false).await.unwrap().to_string();
    let private_path = f._root.path().to_string_lossy().into_owned();
    for private in [
        token.as_str(),
        f.phone.as_str(),
        private_path.as_str(),
        "PRIVATE_QUERY",
    ] {
        assert!(!report.contains(private));
    }
    assert_eq!(
        call(
            router,
            "POST",
            path,
            &headers,
            json!({"prompt":"PRIVATE_BODY"})
        )
        .await
        .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn restart_requires_supervisor_and_local_csrf_authority_and_stop_overrides_it() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let mut f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let headers = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    assert_eq!(
        call(router, "POST", "/api/restart", &headers, json!({}))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(!*f.app.stopping.borrow());
    let restart = Arc::new(AtomicBool::new(false));
    f.app.restart = Some(restart.clone());
    let router = f.app.clone().router();
    let (code, body) = call(
        router.clone(),
        "GET",
        "/api/system/memory",
        &headers,
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["activeTurnCount"],
        0
    );
    for path in ["/api/restart", "/api/system/restart"] {
        for extra in [
            vec![],
            vec![("authorization", format!("Bearer {}", f.phone))],
            vec![("x-opencodex-api-key", token.clone())],
        ] {
            let extra: Vec<_> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(
                call(router.clone(), "POST", path, &extra, json!({}))
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        let mut stale = headers.to_vec();
        stale.push(("x-opencodex-expected-pid", "0"));
        assert_eq!(
            call(router.clone(), "POST", path, &stale, json!({}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(!restart.load(Ordering::Acquire));
    assert!(!*f.app.stopping.borrow());
    assert_eq!(
        call(router.clone(), "POST", "/api/restart", &headers, json!({}))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert!(restart.load(Ordering::Acquire));
    let (code, body) = call(
        router.clone(),
        "POST",
        "/api/system/restart",
        &headers,
        json!({}),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["alreadyDraining"],
        true
    );
    assert_eq!(
        call(router.clone(), "POST", "/api/stop", &headers, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(!restart.load(Ordering::Acquire));
    assert_eq!(
        call(router, "POST", "/api/restart", &headers, json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn process_memory_requires_dashboard_session_and_reports_only_native_counters() {
    let f = setup();
    let router = f.app.router();
    let path = "/api/system/memory";
    assert_eq!(
        call(router.clone(), "GET", path, &[], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            router.clone(),
            "GET",
            path,
            &[("authorization", &format!("Bearer {}", f.phone))],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (token, csrf) = session(router.clone()).await;
    let headers = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let (code, body) = call(router.clone(), "GET", path, &headers, Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["runtime"], "rust");
    assert!(v["rss"].as_u64().unwrap() > 0);
    assert!(v.get("activeTurnCount").is_none());
    assert!(v.get("bunVersion").is_none());
    assert!(!body.contains(&token));
    assert!(!body.contains(&f.phone));
    assert!(!body.contains(f._root.path().to_str().unwrap()));
    assert_eq!(
        call(router, "POST", path, &headers, json!({})).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
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
    let preview_path = "/api/storage/cleanup/preview";
    for headers in [
        vec![],
        vec![("authorization", format!("Bearer {}", f.phone))],
        vec![("x-opencodex-api-key", token.clone())],
    ] {
        let headers: Vec<_> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_ne!(
            call(
                router.clone(),
                "POST",
                preview_path,
                &headers,
                json!({"percent":50})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for body in [
        json!({}),
        json!({"percent":"50"}),
        json!({"percent":-1}),
        json!({"percent":101}),
    ] {
        assert_eq!(
            call(router.clone(), "POST", preview_path, &good, body)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    std::fs::create_dir(f._root.path().join("archived_sessions")).unwrap();
    let archived = f._root.path().join("archived_sessions/test.jsonl");
    std::fs::write(&archived, "PRIVATE_ARCHIVED").unwrap();
    let (code, body) = call(
        router.clone(),
        "POST",
        preview_path,
        &good,
        json!({"percent":50}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let p: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(p["count"], 1);
    assert_eq!(
        p["candidates"][0]["relPath"],
        "archived_sessions/test.jsonl"
    );
    assert!(!body.contains("PRIVATE_ARCHIVED"));
    assert!(!body.contains(f._root.path().to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(&archived).unwrap(),
        "PRIVATE_ARCHIVED"
    );
    let trash_path = "/api/storage/trash";
    assert_eq!(
        call(router.clone(), "GET", trash_path, &[], Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            router.clone(),
            "GET",
            trash_path,
            &[("authorization", &format!("Bearer {}", f.phone))],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let stage = f._root.path().join(".trash/1");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("test.jsonl"), "PRIVATE_QUARANTINE").unwrap();
    let (code, body) = call(router.clone(), "GET", trash_path, &good, Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["entries"][0]["id"], ".trash/1");
    assert_eq!(listed["entries"][0]["fileCount"], 1);
    assert!(!body.contains("PRIVATE_QUARANTINE"));
    assert!(!body.contains(f._root.path().to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(stage.join("test.jsonl")).unwrap(),
        "PRIVATE_QUARANTINE"
    );
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
        json!({"controlEnabled":false,"localNetworkEnabled":false,"connectionChoice":"quick"}),
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

#[tokio::test]
async fn lan_control_preserves_pairing_admin_and_history_and_rejects_forged_local_headers() {
    let f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let h = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let before = call(
        router.clone(),
        "GET",
        "/api/android-remote",
        &h,
        Value::Null,
    )
    .await;
    let before: Value = serde_json::from_str(&before.1).unwrap();
    let disk =
        rusqlite::Connection::open(f._root.path().join("state/native-connect.sqlite")).unwrap();
    disk.execute(
        "INSERT INTO metadata(key,value) VALUES('history-fixture','preserve')",
        [],
    )
    .unwrap();
    let changed = call(
        router.clone(),
        "PUT",
        "/api/android-remote/settings",
        &h,
        json!({"localNetworkEnabled":false}),
    )
    .await;
    assert_eq!(changed.0, StatusCode::OK);
    let changed: Value = serde_json::from_str(&changed.1).unwrap();
    assert_eq!(changed["localNetworkEnabled"], false);
    assert_eq!(changed["pairingAvailable"], false);
    assert_eq!(changed["reachableAddresses"], json!([]));
    assert_eq!(changed["clients"], before["clients"]);
    assert_eq!(changed["controlEnabled"], true);
    let remote = call_from(
        router.clone(),
        "GET",
        "/healthz",
        &[
            ("x-forwarded-for", "127.0.0.1"),
            ("forwarded", "for=127.0.0.1"),
        ],
        Value::Null,
        "203.0.113.8:1000",
    )
    .await;
    assert_eq!(remote.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        call(
            router.clone(),
            "POST",
            "/api/android-remote/pairing",
            &h,
            json!({})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let reopened = Auth::new(Store::open(&f._root.path().join("state")).unwrap());
    assert!(!reopened.local_network_enabled().unwrap());
    assert!(reopened
        .authenticate(&f.phone, auth::now())
        .unwrap()
        .is_some());
    let restarted = App::new(reopened, "test PC").unwrap().router();
    assert_eq!(
        call_from(
            restarted,
            "GET",
            "/healthz",
            &[],
            Value::Null,
            "203.0.113.8:1000"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            "/api/android-remote/settings",
            &h,
            json!({"localNetworkEnabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call_from(
            router,
            "GET",
            "/healthz",
            &[],
            Value::Null,
            "203.0.113.8:1000"
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        disk.query_row(
            "SELECT value FROM metadata WHERE key='history-fixture'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "preserve"
    );
}

#[tokio::test]
async fn failed_lan_save_rolls_back_other_settings_and_does_not_close_access() {
    let f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let h = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let disk =
        rusqlite::Connection::open(f._root.path().join("state/native-connect.sqlite")).unwrap();
    disk.execute_batch("CREATE TRIGGER fail_lan BEFORE INSERT ON metadata WHEN NEW.key='local_network_enabled' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            "/api/android-remote/settings",
            &h,
            json!({"controlEnabled":false,"keepAwake":true,"localNetworkEnabled":false})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        call_from(
            router,
            "GET",
            "/healthz",
            &[],
            Value::Null,
            "203.0.113.8:1000"
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.app
            .auth
            .call(|a| (
                a.control_enabled().unwrap(),
                a.keep_awake().unwrap(),
                a.local_network_enabled().unwrap()
            ))
            .await
            .unwrap(),
        (true, false, true)
    );
    disk.execute_batch("DROP TRIGGER fail_lan; INSERT INTO metadata(key,value) VALUES('local_network_enabled','invalid');").unwrap();
    assert!(App::new(
        Auth::new(Store::open(&f._root.path().join("state")).unwrap()),
        "test PC"
    )
    .is_err());
}

#[tokio::test]
async fn stop_requires_local_csrf_rejects_stale_process_and_preserves_pairing() {
    let f = setup();
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let headers = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    for h in [
        vec![],
        vec![("authorization", f.phone.as_str())],
        headers[..3].to_vec(),
    ] {
        assert_eq!(
            call(router.clone(), "POST", "/api/stop", &h, json!({}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert!(!*f.app.stopping.borrow());
    }
    for (pid, code) in [
        ("invalid", StatusCode::BAD_REQUEST),
        ("0", StatusCode::BAD_REQUEST),
        ("4294967295", StatusCode::CONFLICT),
    ] {
        let mut h = headers.to_vec();
        h.push(("x-opencodex-expected-pid", pid));
        assert_eq!(
            call(router.clone(), "POST", "/api/stop", &h, json!({}))
                .await
                .0,
            code
        );
        assert!(!*f.app.stopping.borrow());
    }
    assert_eq!(
        call(router.clone(), "GET", "/api/stop", &headers, Value::Null)
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    for already in [false, true] {
        let (code, body) = call(router.clone(), "POST", "/api/stop", &headers, json!({})).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["alreadyStopping"], already);
        assert!(*f.app.stopping.borrow());
    }
    for path in [
        "/api/android-remote",
        "/api/auth/session",
        "/healthz",
        "/api/system/memory",
    ] {
        assert_eq!(
            call(router.clone(), "GET", path, &headers, Value::Null)
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let client = f.client;
    assert!(f
        .app
        .auth
        .call(move |a| a.authorized(&client, auth::now()))
        .await
        .unwrap()
        .unwrap());
}

async fn storage_fixture() -> (Fixture, remodex_connect_native::chat::Chats) {
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
    let config = Config {
        storage: Default::default(),
        max_active_chats: 1,
        workspace: f._root.path().into(),
        codex: agent.clone(),
        claude: agent,
    };
    let hub = Chats::start(f.app.auth.clone(), config).await.unwrap();
    f.app.chats = Some(hub.clone());
    std::fs::create_dir(f._root.path().join("archived_sessions")).unwrap();
    std::fs::write(
        f._root.path().join("archived_sessions/retired.jsonl"),
        b"archived fixture\n",
    )
    .unwrap();
    let db = rusqlite::Connection::open(f._root.path().join("state_1.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,archived INTEGER);INSERT INTO threads VALUES('retired','archived_sessions/retired.jsonl',1);").unwrap();
    (f, hub)
}
async fn wait_policy(router: Router, headers: &[(&str, &str)]) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let (code, text) = call(
                router.clone(),
                "GET",
                "/api/storage/cleanup-policy",
                headers,
                json!(null),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{text}");
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["job"]["status"] != "running" {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn storage_policy_is_opt_in_local_only_and_runs_and_restores_in_existing_ui_contract() {
    use remodex_connect_native::chat::AgentKind;
    let (f, hub) = storage_fixture().await;
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    let path = "/api/storage/cleanup-policy";
    for (method, path) in [
        ("GET", path),
        ("PUT", path),
        ("POST", "/api/storage/cleanup-policy/run"),
    ] {
        assert_eq!(
            call(router.clone(), method, path, &[], json!({"enabled":true}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            router.clone(),
            "PUT",
            path,
            &good,
            json!({"target":{"reduceToBytes":0,"removeOldestPercent":10}})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let initial = wait_policy(router.clone(), &good).await;
    assert_eq!(initial["enabled"], false);
    let (code, text) = call(
        router.clone(),
        "POST",
        "/api/storage/cleanup-policy/run",
        &good,
        json!(null),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["started"],
        true
    );
    let disabled = wait_policy(router.clone(), &good).await;
    assert_eq!(disabled["job"]["lastOutcome"]["skipped"], "disabled");
    assert!(f
        ._root
        .path()
        .join("archived_sessions/retired.jsonl")
        .exists());
    assert_eq!(call(router.clone(),"PUT",path,&good,json!({"enabled":true,"trigger":{"archivedBytesOver":0},"target":{"reduceToBytes":0},"schedule":"manual","mode":"quarantine"})).await.0,StatusCode::OK);
    // Startup discovery can briefly reserve the CLI; only retry an explicit
    // deferred outcome. Successful mutations are never replayed.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let done = loop {
        assert_eq!(
            call(
                router.clone(),
                "POST",
                "/api/storage/cleanup-policy/run",
                &good,
                json!(null)
            )
            .await
            .0,
            StatusCode::OK
        );
        let v = wait_policy(router.clone(), &good).await;
        if v["job"]["lastOutcome"]["deferred"] != "codex_busy" {
            break v;
        }
        assert!(std::time::Instant::now() < end);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(done["job"]["lastOutcome"]["ok"], true, "{done}");
    assert_eq!(done["lastRun"]["removed"], 1);
    assert!(!f
        ._root
        .path()
        .join("archived_sessions/retired.jsonl")
        .exists());
    let (code, v) = storage_idle_call(
        router.clone(),
        "/api/storage/trash/restore",
        &good,
        json!({"id":done["job"]["lastOutcome"]["trashDir"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert!(f
        ._root
        .path()
        .join("archived_sessions/retired.jsonl")
        .exists());
    hub.call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}
#[tokio::test]
async fn storage_policy_defers_both_agents_and_shutdown_prevents_late_scheduler_work() {
    use remodex_connect_native::chat::AgentKind;
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (f, hub) = storage_fixture().await;
        let router = f.app.clone().router();
        let (token, csrf) = session(router.clone()).await;
        let good = [
            ("x-opencodex-api-key", token.as_str()),
            ("x-opencodex-gui-origin", "http://localhost:18900"),
            ("origin", "http://localhost:18900"),
            ("x-opencodex-csrf-token", csrf.as_str()),
        ];
        assert_eq!(call(router.clone(),"PUT","/api/storage/cleanup-policy",&good,json!({"enabled":true,"trigger":{"archivedBytesOver":0},"schedule":"startup","target":{"removeOldestPercent":100}})).await.0,StatusCode::OK);
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","threadId":"policy-busy","projectId":"native-test-project","commandId":"policy-create"})).await.unwrap();
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.turn.start","threadId":"policy-busy","commandId":"policy-hold","message":{"messageId":"hold","text":"hold","attachments":[]}})).await.unwrap();
        f.app.start_storage_scheduler();
        let result = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let v = wait_policy(router.clone(), &good).await;
                if v["job"]["lastOutcome"].is_object() {
                    break v;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(result["job"]["lastOutcome"]["deferred"], "codex_busy");
        assert!(
            result["nextRun"].as_u64().unwrap() > result["job"]["finishedAt"].as_u64().unwrap()
        );
        assert!(f
            ._root
            .path()
            .join("archived_sessions/retired.jsonl")
            .exists());
        f.app.stopping.send_replace(true);
        f.app.drain_storage_policy().await;
        assert_eq!(
            call(
                router,
                "POST",
                "/api/storage/cleanup-policy/run",
                &good,
                json!(null)
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        hub.call(agent, "local", "native.shutdown", json!({}))
            .await
            .unwrap();
    }
}
async fn storage_idle_call(
    router: Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Value) {
    let started = std::time::Instant::now();
    loop {
        let (code, text) = call(router.clone(), "POST", path, headers, body.clone()).await;
        let value: Value = serde_json::from_str(&text).unwrap();
        if value["error"] != "codex_busy" || started.elapsed() > std::time::Duration::from_secs(5) {
            return (code, value);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
#[tokio::test]
async fn storage_mutations_require_local_authority_bind_preview_and_restore_exact_archive() {
    use remodex_connect_native::chat::AgentKind;
    let (f, hub) = storage_fixture().await;
    let router = f.app.clone().router();
    let (token, csrf) = session(router.clone()).await;
    let good = [
        ("x-opencodex-api-key", token.as_str()),
        ("x-opencodex-gui-origin", "http://localhost:18900"),
        ("origin", "http://localhost:18900"),
        ("x-opencodex-csrf-token", csrf.as_str()),
    ];
    for path in [
        "/api/storage/cleanup",
        "/api/storage/trash/restore",
        "/api/storage/trash/purge/resume",
    ] {
        for bad in [
            vec![],
            vec![("authorization", format!("Bearer {}", f.phone))],
            vec![
                ("x-opencodex-api-key", token.clone()),
                ("x-opencodex-gui-origin", "http://localhost:18900".into()),
            ],
        ] {
            let headers: Vec<_> = bad.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(
                call(router.clone(), "POST", path, &headers, json!({}))
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            call(
                router.clone(),
                "POST",
                path,
                &good,
                json!({"id":"../escape","extra":true})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, text) = call(
        router.clone(),
        "POST",
        "/api/storage/cleanup/preview",
        &good,
        json!({"percent":100}),
    )
    .await;
    let preview: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(preview["count"], 1);
    let archived = f._root.path().join("archived_sessions/retired.jsonl");
    let before = std::fs::read(&archived).unwrap();
    let modified = std::fs::metadata(&archived).unwrap().modified().unwrap();
    let request = json!({"percent":100,"mode":"quarantine","digest":preview["digest"]});
    let (_, text) = call(router.clone(), "GET", "/api/storage", &good, Value::Null).await;
    let report: Value = serde_json::from_str(&text).unwrap();
    let count = |v: &Value| {
        v["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == "archived_sessions")
            .unwrap()["fileCount"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(count(&report), 1);
    let (code, result) = storage_idle_call(
        router.clone(),
        "/api/storage/cleanup",
        &good,
        request.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert_eq!(result["count"], 1);
    assert!(!archived.exists());
    let id = result["trashDir"].as_str().unwrap();
    let db = rusqlite::Connection::open(f._root.path().join("state_1.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let (_, text) = call(router.clone(), "GET", "/api/storage", &good, Value::Null).await;
    assert_eq!(
        count(&serde_json::from_str(&text).unwrap()),
        0,
        "cache must invalidate immediately"
    );
    let (_, text) = call(
        router.clone(),
        "GET",
        "/api/storage/trash",
        &good,
        Value::Null,
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["entries"][0]["id"],
        id
    );
    let (code, result) =
        storage_idle_call(router.clone(), "/api/storage/cleanup", &good, request).await;
    assert_eq!(code, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["error"], "stale_preview");
    let (code, result) = storage_idle_call(
        router.clone(),
        "/api/storage/trash/purge/resume",
        &good,
        json!({"id":id}),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["error"], "purge_not_started");
    let (code, result) = storage_idle_call(
        router.clone(),
        "/api/storage/trash/restore",
        &good,
        json!({"id":id}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert_eq!(std::fs::read(&archived).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&archived).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(!f._root.path().join(id).exists());
    let (code, result) = storage_idle_call(
        router.clone(),
        "/api/storage/trash/restore",
        &good,
        json!({"id":id}),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{result}");
    let (_, text) = call(
        router.clone(),
        "POST",
        "/api/storage/cleanup/preview",
        &good,
        json!({"percent":100}),
    )
    .await;
    let preview: Value = serde_json::from_str(&text).unwrap();
    let (code, result) = storage_idle_call(
        router.clone(),
        "/api/storage/cleanup",
        &good,
        json!({"percent":100,"mode":"permanent","digest":preview["digest"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert!(!archived.exists());
    assert!(result.get("trashDir").is_none());
    assert_eq!(
        db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    hub.call(AgentKind::Codex, "local", "native.shutdown", json!({}))
        .await
        .unwrap();
}
#[tokio::test]
async fn storage_cleanup_refuses_active_and_queued_tasks_without_stopping_either_agent() {
    use remodex_connect_native::chat::AgentKind;
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        let (f, hub) = storage_fixture().await;
        let router = f.app.clone().router();
        let (token, csrf) = session(router.clone()).await;
        let good = [
            ("x-opencodex-api-key", token.as_str()),
            ("x-opencodex-gui-origin", "http://localhost:18900"),
            ("origin", "http://localhost:18900"),
            ("x-opencodex-csrf-token", csrf.as_str()),
        ];
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.create","threadId":"busy-storage","projectId":"native-test-project","commandId":"create-storage"})).await.unwrap();
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.turn.start","threadId":"busy-storage","commandId":"hold-storage","message":{"messageId":"hold-message","text":"hold","attachments":[]}})).await.unwrap();
        let (_, text) = call(
            router.clone(),
            "POST",
            "/api/storage/cleanup/preview",
            &good,
            json!({"percent":100}),
        )
        .await;
        let preview: Value = serde_json::from_str(&text).unwrap();
        let (code, text) = call(
            router.clone(),
            "POST",
            "/api/storage/cleanup",
            &good,
            json!({"percent":100,"mode":"permanent","digest":preview["digest"]}),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["error"],
            "codex_busy"
        );
        assert!(f
            ._root
            .path()
            .join("archived_sessions/retired.jsonl")
            .exists());
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.turn.start","threadId":"busy-storage","commandId":"queue-storage","deliveryMode":"queue","message":{"messageId":"queued-storage","text":"queued after hold","attachments":[]}})).await.unwrap();
        // Admission must reopen after refusal so the user's Stop still works.
        hub.call(agent,"phone","orchestration.dispatchCommand",json!({"type":"thread.turn.interrupt","threadId":"busy-storage","commandId":"stop-storage"})).await.unwrap();
        let (code, text) = call(
            router.clone(),
            "POST",
            "/api/storage/cleanup",
            &good,
            json!({"percent":100,"mode":"permanent","digest":preview["digest"]}),
        )
        .await;
        assert_eq!(
            code,
            StatusCode::CONFLICT,
            "paused queued work must be retained: {text}"
        );
        assert!(f
            ._root
            .path()
            .join("archived_sessions/retired.jsonl")
            .exists());

        hub.call(agent, "local", "native.shutdown", json!({}))
            .await
            .unwrap();
    }
}
