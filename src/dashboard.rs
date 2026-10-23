//! Opt-in local dashboard. Phone and dashboard credentials are separate.
//! Uses the existing React bootstrap/header contract; never discovers a normal profile.
#[path = "dashboard_storage.rs"]
mod storage;
use crate::{auth, server::App};
use axum::{
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

const TTL: i64 = 5 * 60 * 1000;
const MAX_SESSIONS: usize = 128;
const MAX_ASSET: u64 = 8 * 1024 * 1024;

struct Session {
    origin: String,
    csrf: String,
    expires: i64,
}
#[derive(Clone)]
pub struct Dashboard {
    root: PathBuf,
    public_url: String,
    port: u16,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    reads: Arc<tokio::sync::Semaphore>,
}
impl Dashboard {
    pub fn new(root: &std::path::Path, public_url: &str) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("Select an absolute dashboard build directory".into());
        }
        let root = root
            .canonicalize()
            .map_err(|_| "Dashboard directory is unavailable")?;
        let index = root
            .join("index.html")
            .canonicalize()
            .map_err(|_| "Dashboard index.html is missing")?;
        if !index.starts_with(&root) || !index.is_file() {
            return Err("Invalid dashboard index.html".into());
        }
        let url = reqwest::Url::parse(public_url).map_err(|_| "Invalid pairing address")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err("Pairing address must be an HTTP origin".into());
        }
        Ok(Self {
            root,
            public_url: url.origin().ascii_serialization(),
            port: url.port_or_known_default().unwrap(),
            sessions: Arc::default(),
            reads: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    fn issue(&self, origin: &str, now: i64) -> Result<String, StatusCode> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let token = format!("ocx_session_{}", secret());
        let csrf = secret();
        let html = format!("<meta name=\"opencodex-session-token\" content=\"{token}\"><meta name=\"opencodex-session-csrf\" content=\"{csrf}\"><meta name=\"opencodex-session-origin\" content=\"{}\">", escape(origin));
        sessions.insert(
            auth::digest(&token),
            Session {
                origin: origin.into(),
                csrf: auth::digest(&csrf),
                expires: now + TTL,
            },
        );
        Ok(html)
    }
    fn allows(&self, origin: &str, headers: &HeaderMap, method: &Method, now: i64) -> bool {
        let Some(raw) = header(headers, "x-opencodex-api-key")
            .filter(|s| s.starts_with("ocx_session_") && s.len() < 128)
        else {
            return false;
        };
        let Ok(mut sessions) = self.sessions.lock() else {
            return false;
        };
        sessions.retain(|_, s| s.expires > now);
        let Some(s) = sessions.get(&auth::digest(raw)) else {
            return false;
        };
        if s.origin != origin || header(headers, "x-opencodex-gui-origin") != Some(origin) {
            return false;
        }
        if *method == Method::GET || *method == Method::HEAD {
            return true;
        }
        header(headers, "origin") == Some(origin)
            && header(headers, "x-opencodex-csrf-token").is_some_and(|v| {
                v.len() < 128 && bool::from(auth::digest(v).as_bytes().ct_eq(s.csrf.as_bytes()))
            })
    }
}
fn secret() -> String {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)?.to_str().ok()
}
fn local_origin(req: &Request) -> Option<String> {
    // Forwarded headers never establish local authority. A LAN client with a
    // forged localhost Host cannot bootstrap a dashboard session.
    if !req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()?
        .0
        .ip()
        .is_loopback()
    {
        return None;
    }
    let host = header(req.headers(), "host")?;
    if req.headers().get_all("host").iter().count() != 1 {
        return None;
    }
    let authority = host.parse::<axum::http::uri::Authority>().ok()?;
    let name = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if name != "localhost"
        && !name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return None;
    }
    let origin = format!("http://{authority}");
    if header(req.headers(), "origin").is_some_and(|v| v != origin) {
        return None;
    }
    if header(req.headers(), "sec-fetch-site").is_some_and(|v| !matches!(v, "same-origin" | "none"))
    {
        return None;
    }
    Some(origin)
}
fn response(status: StatusCode, value: Value) -> Response {
    secure((status, Json(value)).into_response())
}
fn error(status: StatusCode, text: &str) -> Response {
    response(status, json!({"error":text}))
}
fn secure(mut r: Response) -> Response {
    for (k, v) in [
        ("cache-control", "no-store"),
        ("pragma", "no-cache"),
        ("x-frame-options", "DENY"),
        ("content-security-policy", "frame-ancestors 'none'"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
    ] {
        r.headers_mut().insert(
            axum::http::HeaderName::from_static(k),
            axum::http::HeaderValue::from_static(v),
        );
    }
    r
}
pub(crate) fn routes(app: App) -> Router<App> {
    Router::new()
        .route("/api/connect/activity", get(activity))
        .route("/api/storage", get(storage_report))
        .route("/api/storage/cleanup/preview", post(storage_preview))
        .route("/api/storage/trash", get(storage_trash))
        .route(
            "/api/storage/cleanup-policy",
            get(storage::policy_read).put(storage::policy_save),
        )
        .route("/api/storage/cleanup-policy/run", post(storage::policy_run))
        .route("/api/storage/cleanup", post(storage::cleanup))
        .route("/api/storage/trash/restore", post(storage::restore))
        .route(
            "/api/storage/trash/purge/resume",
            post(storage::resume_purge),
        )
        .route("/api/system/memory", get(memory))
        .route("/api/diagnostics/desktop-log", get(diagnostics))
        .route("/api/stop", post(stop))
        .route("/api/restart", post(restart))
        .route("/api/system/restart", post(restart))
        .route("/api/android-remote", get(status))
        .route(
            "/api/android-remote/claude-code",
            get(claude_status).put(claude_settings),
        )
        .route("/api/android-remote/pairing", post(pair))
        .route("/api/android-remote/settings", put(settings))
        .route("/api/android-remote/clients/{id}", delete(revoke))
        .route_layer(axum::middleware::from_fn_with_state(app, guard))
        .route("/opencodex-session", get(bootstrap))
}
async fn diagnostics(State(app): State<App>) -> Response {
    let stopping = *app.stopping.borrow();
    let Some(log) = app.diagnostics else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Native diagnostics are unavailable",
        );
    };
    match log.report(stopping).await {
        Ok(report) => response(StatusCode::OK, report),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}
async fn stop(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Some(raw) = headers.get("x-opencodex-expected-pid") {
        let pid = raw
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0);
        let Some(pid) = pid else {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid expected bridge process ID",
            );
        };
        if pid != std::process::id() {
            return error(
                StatusCode::CONFLICT,
                "The bridge process changed; refresh the dashboard",
            );
        }
    }
    if let Some(restart) = &app.restart {
        restart.store(false, std::sync::atomic::Ordering::Release);
    }
    // Stop only this process through the same durable cleanup as an OS signal.
    // The optional supervisor also exits; never locate or stop another service.
    let already_stopping = app.stopping.send_replace(true);
    response(
        StatusCode::OK,
        json!({"success":true,"message":"Stopping this bridge","alreadyStopping":already_stopping}),
    )
}
async fn restart(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Some(raw) = headers.get("x-opencodex-expected-pid") {
        let Some(pid) = raw
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| *p > 0)
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid expected bridge process ID",
            );
        };
        if pid != std::process::id() {
            return error(
                StatusCode::CONFLICT,
                "The bridge process changed; refresh the dashboard",
            );
        }
    }
    let Some(restart) = &app.restart else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Restart requires starting this isolated candidate with --supervise",
        );
    };
    if *app.stopping.borrow() {
        return if restart.load(std::sync::atomic::Ordering::Acquire) {
            response(
                StatusCode::ACCEPTED,
                json!({"success":true,"alreadyDraining":true,"activeTurnCount":0,"drainTimeoutMs":0,"restartPolicy":"idle-only"}),
            )
        } else {
            error(StatusCode::CONFLICT, "Bridge is already stopping")
        };
    }
    let reservation = if let Some(chats) = &app.chats {
        match chats.prepare_restart().await {
            Ok(reservation) => Some(reservation),
            Err(e) => return error(StatusCode::CONFLICT, &e),
        }
    } else {
        None
    };
    // No await between committing intent and stopping admission. A concurrent
    // Stop can override restart, including while graceful cleanup is running.
    if *app.stopping.borrow() {
        return error(StatusCode::CONFLICT, "Bridge is already stopping");
    }
    restart.store(true, std::sync::atomic::Ordering::Release);
    app.stopping.send_replace(true);
    if let Some(reservation) = reservation {
        reservation.commit();
    }
    response(
        StatusCode::ACCEPTED,
        json!({"success":true,"message":"Restarting this idle bridge through its native supervisor","activeTurnCount":0,"drainTimeoutMs":0,"alreadyDraining":false,"restartPolicy":"idle-only"}),
    )
}
async fn activity(State(app): State<App>) -> Response {
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats.activity.read().await {
        Ok(value) => response(StatusCode::OK, value),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}
async fn memory(State(app): State<App>) -> Response {
    match app.metrics.read() {
        Some(mut value) => {
            if app.restart.is_some() {
                let idle = if let Some(chats) = &app.chats {
                    matches!(chats.call(crate::chat::AgentKind::Codex,"local","native.restartStatus",json!({})).await, Ok(v) if v["ready"]==true)
                } else {
                    true
                };
                // Existing UI treats this field as a restart capability. Until
                // active-task draining is supported, offer it only when idle;
                // the POST still rechecks and atomically reserves admission.
                value["restartPolicy"] = json!("idle-only");
                if idle {
                    value["activeTurnCount"] = json!(0);
                    value["isDraining"] = json!(false);
                }
            }
            response(StatusCode::OK, value)
        }
        None => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Process memory is unavailable",
        ),
    }
}
async fn storage_report(State(app): State<App>) -> Response {
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats.storage_report.read().await {
        Ok(value) => response(StatusCode::OK, value),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}
async fn storage_preview(
    State(app): State<App>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    let Some(percent) = body["percent"]
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_percent");
    };
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats.storage_report.preview(percent).await {
        Ok(value) => response(StatusCode::OK, value),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}
async fn storage_trash(State(app): State<App>) -> Response {
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats.storage_report.trash().await {
        Ok(value) => response(StatusCode::OK, value),
        Err(_) => response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"trash_list_failed","entries":[]}),
        ),
    }
}
async fn guard(State(app): State<App>, req: Request, next: Next) -> Response {
    let Some(d) = &app.dashboard else {
        return error(StatusCode::NOT_FOUND, "Native dashboard is not enabled");
    };
    let Some(origin) = local_origin(&req) else {
        return error(
            StatusCode::FORBIDDEN,
            "Open the dashboard on this PC using localhost",
        );
    };
    if !d.allows(&origin, req.headers(), req.method(), auth::now()) {
        return error(StatusCode::UNAUTHORIZED, "Dashboard session required");
    }
    if *app.stopping.borrow()
        && !matches!(
            req.uri().path(),
            "/api/stop" | "/api/restart" | "/api/system/restart"
        )
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Bridge is stopping");
    }
    secure(next.run(req).await)
}
async fn bootstrap(State(app): State<App>, req: Request) -> Response {
    let Some(d) = &app.dashboard else {
        return error(StatusCode::NOT_FOUND, "Native dashboard is not enabled");
    };
    let Some(origin) = local_origin(&req) else {
        return error(
            StatusCode::FORBIDDEN,
            "Open the dashboard on this PC using localhost",
        );
    };
    match d.issue(&origin, auth::now()) {
        Ok(meta) => secure(
            (
                [("content-type", "text/html; charset=utf-8")],
                format!("<!doctype html><html><head>{meta}</head><body></body></html>"),
            )
                .into_response(),
        ),
        Err(status) => error(
            status,
            "Dashboard session capacity unavailable; retry shortly",
        ),
    }
}
async fn status(State(app): State<App>) -> Response {
    let online = app
        .online
        .lock()
        .map(|v| v.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let state = app.auth.call(move |a| -> rusqlite::Result<(bool, bool, Vec<Value>)> {
        // Expired credentials still need a visible repair/revoke action. Never
        // hide the phone precisely when its authorization needs replacement.
        let mut stmt = a.store.db.prepare("SELECT id,label,os,created_at FROM clients ORDER BY created_at DESC LIMIT 128")?;
        let clients = stmt.query_map([], |r| { let id:String=r.get(0)?;let created:i64=r.get(3)?;
            Ok(json!({"id":id,"label":r.get::<_,String>(1)?,"os":r.get::<_,String>(2)?,"deviceType":"mobile", "createdAt":auth::timestamp(created),"scopes":auth::SCOPES.split(' ').collect::<Vec<_>>(),"online":online.contains(&id)}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((a.control_enabled()?, a.keep_awake()?, clients))
    }).await;
    let Ok(Ok((enabled, keep_awake, clients))) = state else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Pairing store unavailable");
    };
    let d = app.dashboard.as_ref().unwrap();
    let platform = if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    response(
        StatusCode::OK,
        json!({"version":1,"controlEnabled":enabled,"keepAwake":keep_awake,"localNetworkEnabled":true,
        "connectionChoice":"local","pairingAvailable":enabled,"gateway":{"status":if enabled {"ready"} else {"stopped"},"backgroundServer":"current-process","port":d.port},
        "reachableAddresses":[d.public_url],"tunnel":{"configuration":{"mode":"quick","hasNamedTunnelToken":false},"runtime":{"mode":"quick","status":"stopped","publicUrl":null,"error":null}},
        "desktop":{"id":"desktop","label":app.label,"platform":platform,"address":d.public_url,"online":true},"clients":clients}),
    )
}
async fn claude_status(
    State(app): State<App>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats
        .claude_setup(query.get("refresh").is_some_and(|v| v == "1"))
        .await
    {
        Ok(v) => response(StatusCode::OK, v),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}
async fn claude_settings(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let Some(object) = body.as_object().filter(|o| !o.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "Provide Claude setup settings");
    };
    if object.keys().any(|k| {
        !matches!(
            k.as_str(),
            "enabled" | "binary" | "desktopSync" | "autoConnectNewSessions"
        )
    }) {
        return error(StatusCode::BAD_REQUEST, "Unknown Claude setup setting");
    }
    if !object.contains_key("autoConnectNewSessions") {
        if object.iter().any(|(k, v)| {
            if k == "binary" {
                !v.is_string()
            } else {
                !v.is_boolean()
            }
        }) {
            return error(
                StatusCode::BAD_REQUEST,
                "Bridge switches must be boolean and the executable path must be text",
            );
        }
        let Some(chats) = app.chats else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Select the isolated agent configuration first",
            );
        };
        return match chats.configure_claude(body).await {
            Ok(v) => response(StatusCode::OK, v),
            Err(e) => error(StatusCode::CONFLICT, &e),
        };
    }
    if object.len() != 1 || !object.contains_key("autoConnectNewSessions") {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "Change automatic connection separately from other setup settings. No settings changed",
        );
    }
    let Some(enabled) = body["autoConnectNewSessions"].as_bool() else {
        return error(
            StatusCode::BAD_REQUEST,
            "Automatic connection must be true or false",
        );
    };
    let Some(chats) = app.chats else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Select the isolated agent configuration first",
        );
    };
    match chats
        .call(
            crate::chat::AgentKind::Claude,
            "dashboard",
            "claude.setAutoConnect",
            json!({"enabled":enabled}),
        )
        .await
    {
        Ok(_) => match chats.claude_setup(false).await {
            Ok(v) => response(StatusCode::OK, v),
            Err(_) => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Claude preference was saved, but status could not refresh",
            ),
        },
        Err(e) => error(StatusCode::CONFLICT, &e),
    }
}
async fn pair(
    State(app): State<App>,
    Query(query): Query<HashMap<String, String>>,
    bytes: axum::body::Bytes,
) -> Response {
    if query.keys().any(|k| k != "replaceClientId")
        || query
            .get("replaceClientId")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Provide a valid paired device ID only",
        );
    }
    let replace = query.get("replaceClientId").cloned();
    if let Some(id) = replace.clone() {
        match app.auth.call(move |a| a.store.client_exists(&id)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "The phone authorization no longer exists",
                )
            }
            _ => return error(StatusCode::SERVICE_UNAVAILABLE, "Pairing store unavailable"),
        }
    }
    let body = if bytes.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid pairing request"),
        }
    };
    if body.as_object().is_none_or(|v| !v.is_empty()) {
        return error(StatusCode::BAD_REQUEST, "Provide an empty pairing request");
    }
    let url = app.dashboard.as_ref().unwrap().public_url.clone();
    let label = app.label.clone();
    match app
        .auth
        .call(move |a| a.invite_for(&label, &url, replace, auth::now()))
        .await
    {
        Ok(Ok(v)) => response(
            StatusCode::CREATED,
            json!({"version":1,"id":v["pairingLinkId"],"expiresAt":v["expiresAt"],"qrPayload":v.to_string()}),
        ),
        Ok(Err(_)) => error(
            StatusCode::CONFLICT,
            "Pairing is disabled or its invitation capacity is full",
        ),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Pairing store unavailable"),
    }
}
async fn settings(State(app): State<App>, Json(body): Json<Value>) -> Response {
    let Some(object) = body.as_object().filter(|v| !v.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "Provide phone control settings");
    };
    if object.keys().any(|k| {
        !matches!(
            k.as_str(),
            "controlEnabled" | "keepAwake" | "localNetworkEnabled" | "connectionChoice"
        )
    }) || ["controlEnabled", "keepAwake", "localNetworkEnabled"]
        .iter()
        .any(|k| object.get(*k).is_some_and(|v| !v.is_boolean()))
        || object
            .get("connectionChoice")
            .is_some_and(|v| !matches!(v.as_str(), Some("local" | "quick" | "named")))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid phone control settings");
    }
    // Validate the whole update before persisting anything. Unsupported actions
    // must not appear successful or partially disable a working phone.
    if body["localNetworkEnabled"] == false
        || object.get("connectionChoice").is_some_and(|v| v != "local")
    {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "Tunnel and local-network controls are not migrated yet; no settings changed",
        );
    }
    let enabled = body["controlEnabled"].as_bool();
    let keep_awake = body["keepAwake"].as_bool();
    if (enabled.is_some() || keep_awake.is_some())
        && !matches!(
            app.auth
                .call(move |a| a.set_control_settings(enabled, keep_awake))
                .await,
            Ok(Ok(()))
        )
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot save phone control settings",
        );
    }
    status(State(app)).await
}
async fn revoke(State(app): State<App>, Path(id): Path<String>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return error(StatusCode::BAD_REQUEST, "Invalid paired device ID");
    }
    match app.auth.call(move |a| a.revoke(&id)).await {
        Ok(Ok(true)) => status(State(app)).await,
        Ok(Ok(false)) => error(StatusCode::NOT_FOUND, "Paired device not found"),
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot remove paired device",
        ),
    }
}

/// Count live sockets, not cached bearer reads. Dropping a socket/task releases
/// its entry even when the task is canceled. The server limits sockets to eight.
pub(crate) struct Online {
    counts: Arc<Mutex<HashMap<String, usize>>>,
    client: String,
}
impl Online {
    pub(crate) fn new(counts: Arc<Mutex<HashMap<String, usize>>>, client: String) -> Self {
        if let Ok(mut c) = counts.lock() {
            *c.entry(client.clone()).or_default() += 1;
        }
        Self { counts, client }
    }
}
impl Drop for Online {
    fn drop(&mut self) {
        if let Ok(mut c) = self.counts.lock() {
            if let Some(n) = c.get_mut(&self.client) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    c.remove(&self.client);
                }
            }
        }
    }
}

fn decode_path(path: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(path.len());
    let mut it = path.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let a = (it.next()? as char).to_digit(16)?;
            let c = (it.next()? as char).to_digit(16)?;
            bytes.push((a * 16 + c) as u8);
        } else {
            bytes.push(b);
        }
    }
    let s = String::from_utf8(bytes).ok()?;
    (!s.contains(['\\', '\0', ':']) && !s.split('/').any(|s| s == ".." || s == ".")).then_some(s)
}
pub(crate) async fn static_file(State(app): State<App>, req: Request) -> Response {
    let Some(d) = &app.dashboard else {
        return crate::server::not_implemented().await;
    };
    if !matches!(*req.method(), Method::GET | Method::HEAD) || req.uri().path().starts_with("/api/")
    {
        return crate::server::not_implemented().await;
    }
    let Some(origin) = local_origin(&req) else {
        return error(
            StatusCode::FORBIDDEN,
            "Open the dashboard on this PC using localhost",
        );
    };
    let Some(path) = decode_path(req.uri().path()) else {
        return error(StatusCode::NOT_FOUND, "Dashboard asset not found");
    };
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() || std::path::Path::new(relative).extension().is_none() {
        "index.html"
    } else {
        relative
    };
    let ext = std::path::Path::new(relative)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mime = match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => return error(StatusCode::NOT_FOUND, "Dashboard asset not found"),
    };
    let Ok(_permit) = d.reads.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Dashboard asset capacity reached",
        );
    };
    let Ok(path) = tokio::fs::canonicalize(d.root.join(relative)).await else {
        return error(StatusCode::NOT_FOUND, "Dashboard asset not found");
    };
    if !path.starts_with(&d.root) || path != d.root.join(relative) {
        return error(StatusCode::NOT_FOUND, "Dashboard asset not found");
    }
    let Ok(file) = tokio::fs::File::open(path).await else {
        return error(StatusCode::NOT_FOUND, "Dashboard asset not found");
    };
    if !file
        .metadata()
        .await
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_ASSET)
    {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Dashboard asset exceeds limit",
        );
    }
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if file
        .take(MAX_ASSET + 1)
        .read_to_end(&mut bytes)
        .await
        .is_err()
        || bytes.len() > MAX_ASSET as usize
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot read dashboard asset",
        );
    }
    if ext == "html" && *req.method() != Method::HEAD {
        let Ok(html) = String::from_utf8(bytes) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Invalid dashboard document",
            );
        };
        let meta = match d.issue(&origin, auth::now()) {
            Ok(v) => v,
            Err(s) => return error(s, "Dashboard session capacity unavailable"),
        };
        bytes = if html.contains("</head>") {
            html.replacen("</head>", &format!("{meta}</head>"), 1)
        } else {
            format!("{meta}{html}")
        }
        .into_bytes();
    }
    if *req.method() == Method::HEAD {
        bytes.clear();
    }
    secure(
        (
            [("content-type", mime)],
            axum::body::Body::from_stream(futures_util::stream::once(async move {
                let _held = _permit;
                Ok::<_, std::io::Error>(bytes)
            })),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_expire_are_bounded_and_do_not_survive_a_new_server() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), "<head></head>").unwrap();
        let d = Dashboard::new(root.path(), "http://localhost:1").unwrap();
        let doc = d.issue("http://localhost:1", 1000).unwrap();
        let token = doc
            .split("name=\"opencodex-session-token\" content=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-opencodex-api-key", token.parse().unwrap());
        h.insert(
            "x-opencodex-gui-origin",
            "http://localhost:1".parse().unwrap(),
        );
        assert!(d.allows("http://localhost:1", &h, &Method::GET, 1001));
        assert!(!d.allows("http://localhost:2", &h, &Method::GET, 1001));
        for _ in 1..MAX_SESSIONS {
            d.issue("http://localhost:1", 1001).unwrap();
        }
        assert_eq!(
            d.issue("http://localhost:1", 1001).unwrap_err(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(!d.allows("http://localhost:1", &h, &Method::GET, 1000 + TTL));
        d.issue("http://localhost:1", 1000 + TTL + 1).unwrap();
        assert_eq!(d.sessions.lock().unwrap().len(), 1);
        let fresh = Dashboard::new(root.path(), "http://localhost:1").unwrap();
        assert!(!fresh.allows("http://localhost:1", &h, &Method::GET, 1001));
    }
}
