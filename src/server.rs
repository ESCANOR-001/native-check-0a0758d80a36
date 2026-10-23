use crate::{
    auth::{self, Auth, Metadata, SCOPES},
    auth_service::AuthService,
    chat::{AgentKind, Chats},
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Form, Path, Query, Request, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Asset {
    path: std::path::PathBuf,
    mime: String,
    client: String,
    expires: i64,
}
#[derive(Clone)]
pub struct App {
    pub auth: AuthService,
    pub environment_id: String,
    pub label: String,
    pub chats: Option<Chats>,
    pub dashboard: Option<crate::dashboard::Dashboard>,
    pub diagnostics: Option<crate::diagnostics::Log>,
    /// Present only after an inherited native supervisor pipe is verified.
    pub restart: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub tunnel: Option<crate::tunnel::Manager>,
    pub local_url: Option<String>,
    mobile_connection: Arc<std::sync::Mutex<crate::mobile_connection::State>>,
    pub(crate) network: crate::network_access::Access,
    pub(crate) metrics: crate::runtime_metrics::Metrics,
    pub(crate) online: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    pub stopping: tokio::sync::watch::Sender<bool>,
    pub(crate) storage_policy: crate::storage_policy_job::Runner,
    connections: Arc<Semaphore>,
    transfers: Arc<Semaphore>,
    preview_requests: Arc<Semaphore>,
    image_downloads: Arc<Semaphore>,
    image_decodes: Arc<Semaphore>,
    assets: Arc<tokio::sync::Mutex<HashMap<String, Asset>>>,
}
impl App {
    pub fn listener(&self, listener: tokio::net::TcpListener) -> crate::network_access::Listener {
        crate::network_access::Listener::new(listener, self.network.clone())
    }
    pub fn start_storage_scheduler(&self) {
        if let Some(chats) = &self.chats {
            self.storage_policy
                .schedule(chats.clone(), self.stopping.clone());
        }
    }
    pub async fn drain_storage_policy(&self) {
        self.storage_policy.drain().await;
    }
    pub fn new(auth: Auth, label: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let network = crate::network_access::Access::new(auth.local_network_enabled()?);
        let environment_id = auth.store.environment_id()?;
        let auth = AuthService::new(auth)?;
        let storage_policy = crate::storage_policy_job::Runner::new(auth.clone());
        Ok(Self {
            environment_id,
            network,
            auth,
            storage_policy,
            label: label.to_owned(),
            chats: None,
            dashboard: None,
            diagnostics: None,
            restart: None,
            tunnel: None,
            local_url: None,
            mobile_connection: Arc::default(),
            metrics: Default::default(),
            online: Arc::default(),
            stopping: tokio::sync::watch::channel(false).0,
            connections: Arc::new(Semaphore::new(8)),
            transfers: Arc::new(Semaphore::new(1)),
            preview_requests: Arc::new(Semaphore::new(12)),
            image_downloads: Arc::new(Semaphore::new(4)),
            image_decodes: Arc::new(Semaphore::new(1)),
            assets: Arc::default(),
        })
    }
    pub fn router(self) -> Router {
        self.routes(true)
    }
    fn connection_state(&self) -> Value {
        let local = if self.network.enabled() {
            self.local_url.as_ref().cloned().into_iter().collect()
        } else {
            vec![]
        };
        self.mobile_connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(local, self.tunnel.as_ref().map(|t| t.state()))
    }
    /// Dedicated tunnel origin: deliberately contains no browser bootstrap,
    /// dashboard administration or static assets, even for loopback clients.
    /// A local proxy must never inherit the PC administrator's socket authority.
    pub fn phone_router(self) -> Router {
        self.routes(false)
    }
    fn routes(self, administration: bool) -> Router {
        let dashboard = if administration {
            crate::dashboard::routes(self.clone())
        } else {
            Router::new()
        };
        let diagnostics = self.diagnostics.clone();
        let routes = Router::new()
            .route("/.well-known/t3/environment", get(environment))
            .route("/oauth/token", post(exchange))
            .route("/api/auth/session", get(session))
            .route("/api/auth/client-metadata", post(client_metadata))
            .route("/api/auth/websocket-ticket", post(ticket))
            .route("/api/auth/pairing-links/acknowledge", post(acknowledge))
            .route("/api/orchestration/snapshot", get(chat_snapshot))
            .route("/api/orchestration/dispatch", post(chat_dispatch))
            .route("/api/android-remote/assets/{token}/image", get(asset_image))
            .route(
                "/api/android-remote/assets/{token}/image/thumbnail",
                get(asset_thumbnail),
            )
            .route("/ws", get(upgrade))
            .route_layer(axum::middleware::from_fn_with_state(
                self.clone(),
                phone_control_guard,
            ))
            .route("/healthz", get(health))
            .merge(dashboard);
        let routes = if administration {
            routes.fallback(crate::dashboard::static_file)
        } else {
            routes.fallback(|| async { failure(StatusCode::NOT_FOUND, "Route not found") })
        };
        routes
            .layer(axum::middleware::from_fn(
                move |request: Request, next: axum::middleware::Next| {
                    let diagnostics = diagnostics.clone();
                    async move {
                        let path = request.uri().path().to_owned();
                        let response = next.run(request).await;
                        if let Some(log) = diagnostics {
                            log.request(&path, response.status().as_u16());
                        }
                        if std::env::var_os("NATIVE_TRACE_HTTP").is_some() {
                            eprintln!(
                                "http {} {}",
                                response.status().as_u16(),
                                if path.starts_with("/api/android-remote/assets/") {
                                    if path.ends_with("/thumbnail") {
                                        "/api/android-remote/assets/[capability]/thumbnail"
                                    } else {
                                        "/api/android-remote/assets/[capability]/image"
                                    }
                                } else {
                                    &path
                                }
                            );
                        }
                        response
                    }
                },
            ))
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                network_guard,
            ))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .with_state(self)
    }
}
async fn network_guard(
    State(app): State<App>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|p| p.0);
    if !app.network.permits(peer) {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Local network access is disabled. Re-enable it on the PC to reconnect.",
        );
    }
    let response = next.run(request).await;
    if !app.network.permits(peer) {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Local network access was disabled; reconnect after it is enabled.",
        );
    }
    response
}
async fn phone_control_guard(
    State(app): State<App>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    async fn available(app: &App) -> bool {
        !*app.stopping.borrow()
            && matches!(app.auth.call(|a| a.control_enabled()).await, Ok(Ok(true)))
    }
    fn disabled() -> Response {
        // A disabled listener is temporary unavailability, never revocation.
        // Android discards its stored credentials on a genuine 401.
        let mut r = failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Phone control is disabled or unavailable; retry when it is enabled on the PC",
        );
        r.headers_mut()
            .insert("retry-after", axum::http::HeaderValue::from_static("5"));
        r
    }
    if !available(&app).await {
        return disabled();
    }
    let response = next.run(request).await;
    // Cover disable racing a ticket/auth read that was already admitted.
    if !available(&app).await {
        return disabled();
    }
    response
}
fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (kind, token) = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .split_once(' ')?;
    (kind.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then_some(token.trim())
}
fn descriptor() -> Value {
    json!({"policy":"remote-reachable","bootstrapMethods":["one-time-token"],
        "sessionMethods":["bearer-access-token"],"sessionCookieName":"ocx_android_session"})
}
async fn health(State(app): State<App>) -> Response {
    let stopping = *app.stopping.borrow();
    (if stopping { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK }, Json(
        json!({"status":if stopping {"stopping"} else {"running"},"service":"remodex-native-connect-experiment","version":"0.0.0-native-experiment","migrationComplete":false}),
    )).into_response()
}
async fn environment(State(app): State<App>) -> Json<Value> {
    Json(environment_descriptor(&app))
}
fn environment_descriptor(app: &App) -> Value {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "other"
    };
    json!({"environmentId":app.environment_id,"label":app.label,"platform":{"os":os,"arch":arch},
        "serverVersion":"0.0.0-native-experiment", "capabilities":if app.chats.is_some() {json!({"androidRemoteTurnDelivery":true,"androidRemoteAtomicPromptEdit":true,"androidRemoteOptimisticPromptAcknowledgement":true})} else {json!({})}})
}
async fn exchange(State(app): State<App>, Form(form): Form<HashMap<String, String>>) -> Response {
    for (key, expected) in [
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:token-exchange",
        ),
        (
            "subject_token_type",
            "urn:t3:params:oauth:token-type:environment-bootstrap",
        ),
        (
            "requested_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        ),
    ] {
        if form.get(key).map(String::as_str) != Some(expected) {
            return failure(StatusCode::BAD_REQUEST, "Unsupported phone token exchange");
        }
    }
    let Some(token) = form.get("subject_token") else {
        return failure(StatusCode::BAD_REQUEST, "Missing pairing token");
    };
    let now = auth::now();
    let token = token.clone();
    let result = app
        .auth
        .call(move |a| {
            a.exchange(
                &token,
                form.get("client_pairing_proof").map(String::as_str),
                Metadata {
                    label: form.get("client_label").cloned(),
                    os: form.get("client_os").cloned(),
                    installation_id: form.get("client_installation_id").cloned(),
                },
                now,
            )
        })
        .await;
    let result = match result {
        Ok(r) => r,
        Err(_) => {
            return failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authorization service busy",
            )
        }
    };
    match result {
        Ok(Some(r)) => Json(json!({"access_token":r.access_token,"token_type":"Bearer",
            "issued_token_type":"urn:ietf:params:oauth:token-type:access_token",
            "expires_in":(r.expires_at-now)/1000,"scope":SCOPES}))
        .into_response(),
        Ok(None) => failure(
            StatusCode::UNAUTHORIZED,
            "Pairing code is invalid or expired",
        ),
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to save phone authorization",
        ),
    }
}
async fn session(State(app): State<App>, headers: HeaderMap) -> Response {
    let now = auth::now();
    let raw = bearer(&headers).map(str::to_owned);
    let client = app
        .auth
        .call(move |a| match raw {
            Some(raw) => a.authenticate(&raw, now),
            None => Ok(None),
        })
        .await;
    let client = match client {
        Ok(r) => r,
        Err(_) => {
            return failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authorization service busy",
            )
        }
    };
    if std::env::var_os("NATIVE_TRACE_HTTP").is_some() {
        eprintln!("session authenticated={}", matches!(&client, Ok(Some(_))));
    }
    match client {
        Ok(Some(c)) => Json(json!({"authenticated":true,"auth":descriptor(),"scopes":SCOPES.split(' ').collect::<Vec<_>>(),
            "sessionMethod":"bearer-access-token","expiresAt":auth::timestamp(c.expires_at)})).into_response(),
        Ok(None) => Json(json!({"authenticated":false,"auth":descriptor()})).into_response(),
        Err(_) => failure(StatusCode::SERVICE_UNAVAILABLE,"Authorization store unavailable"),
    }
}
async fn ticket(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(raw) = bearer(&headers) else {
        return failure(StatusCode::UNAUTHORIZED, "Phone token required");
    };
    let raw = raw.to_owned();
    match app
        .auth
        .call(move |a| match a.authenticate(&raw, auth::now()) {
            Ok(Some(c)) => match a.ticket(&c.id, auth::now()) {
                Ok(Some(value)) => Json(value).into_response(),
                Ok(None) => failure(StatusCode::TOO_MANY_REQUESTS, "Ticket capacity reached"),
                Err(_) => failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Authorization store unavailable",
                ),
            },
            Ok(None) => failure(StatusCode::UNAUTHORIZED, "Invalid phone token"),
            Err(_) => failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authorization store unavailable",
            ),
        })
        .await
    {
        Ok(r) => r,
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authorization service busy",
        ),
    }
}
async fn acknowledge(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(raw) = bearer(&headers).map(str::to_owned) else {
        return failure(StatusCode::UNAUTHORIZED, "Phone token required");
    };
    match app
        .auth
        .call(move |a| match a.authenticate(&raw, auth::now()) {
            Ok(Some(_)) => {
                let id = body["id"].as_str().unwrap_or("");
                Json(json!({"revoked":!id.is_empty() && !a.has_invitation(id,auth::now())}))
                    .into_response()
            }
            Ok(None) => failure(StatusCode::UNAUTHORIZED, "Invalid phone token"),
            Err(_) => failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Authorization store unavailable",
            ),
        })
        .await
    {
        Ok(r) => r,
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authorization service busy",
        ),
    }
}

async fn client_metadata(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(raw) = bearer(&headers).map(str::to_owned) else {
        return failure(StatusCode::UNAUTHORIZED, "Phone token required");
    };
    let label = body["label"].as_str().unwrap_or("").trim();
    let os = body["os"].as_str().unwrap_or("").trim();
    if label.len() > 160 || os.len() > 64 {
        return failure(StatusCode::BAD_REQUEST, "Device metadata is too long");
    }
    let label = label.to_owned();
    let os = os.to_owned();
    match app
        .auth
        .call(move |a| -> Result<Option<bool>, rusqlite::Error> {
            let Some(client) = a.authenticate(&raw, auth::now())? else {
                return Ok(None);
            };
            // Pairing binds installation identity. Metadata cannot replace that
            // binding or extend the lifetime of an existing credential.
            let label = if label.is_empty() {
                client.label
            } else {
                label
            };
            let os = if os.is_empty() { client.os } else { os };
            Ok(Some(
                a.store.db.execute(
                    "UPDATE clients SET label=?1,os=?2 WHERE id=?3",
                    rusqlite::params![label, os, client.id],
                )? == 1,
            ))
        })
        .await
    {
        Ok(Ok(Some(updated))) => Json(json!({"updated":updated})).into_response(),
        Ok(Ok(None)) => failure(StatusCode::UNAUTHORIZED, "Invalid phone token"),
        _ => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot update device metadata",
        ),
    }
}

async fn upgrade(
    State(app): State<App>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if query.get("agent").is_some_and(|v| v != "claude-code") {
        return failure(StatusCode::BAD_REQUEST, "Unknown agent");
    }
    let Ok(permit) = app.connections.clone().try_acquire_owned() else {
        return failure(StatusCode::TOO_MANY_REQUESTS, "Connection capacity reached");
    };
    let raw = query.get("wsTicket").cloned().unwrap_or_default();
    let agent = if query.contains_key("agent") {
        AgentKind::Claude
    } else {
        AgentKind::Codex
    };
    let client = app
        .auth
        .call(move |a| a.consume_ticket(&raw, auth::now()))
        .await;
    let Ok(Ok(Some(client))) = client else {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Invalid or expired WebSocket ticket",
        );
    };
    let peer = peer.map(|p| p.0 .0);
    ws.protocols(["opencodex-json-v1"])
        .max_frame_size(1024 * 1024)
        .max_message_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            serve_socket(socket, app, client, agent, peer).await
        })
}
async fn serve_socket(
    mut socket: WebSocket,
    app: App,
    client: String,
    agent: AgentKind,
    peer: Option<std::net::SocketAddr>,
) {
    let mut network_changes = app.network.changes();
    let _online = crate::dashboard::Online::new(app.online.clone(), client.clone());
    let mut check = tokio::time::interval(Duration::from_secs(1));
    let mut stopping = app.stopping.subscribe();
    if *stopping.borrow() {
        return;
    }
    let mut changes = app.chats.as_ref().map(Chats::subscribe);
    let mut subscription: Option<(Value, String, Value)> = None;
    let mut connection_revision = None;
    loop {
        let incoming = tokio::select! {
            _=stopping.changed()=>{break;}
            _=network_changes.changed()=>{if !app.network.permits(peer){break;}continue;}
            _=async {match &mut changes {Some(c)=>{let _=c.changed().await;},None=>std::future::pending().await}}=>{
                if !app.network.permits(peer) || !authorized(&app,&client).await {break;}
                if let Some((id,method,params))=&subscription {
                    let reply=native_reply(&app,agent,&client,id,method,params.clone()).await;
                    if !send(&mut socket,reply).await {break;}
                }
                continue;
            }
            _=check.tick() => {
                if !app.network.permits(peer) || !authorized(&app,&client).await {break;}
                if let Some((id,method,_))=&subscription {
                    if method=="subscribeServerConfig" {
                        let connection=app.connection_state();
                        let revision=connection["revision"].as_u64();
                        if revision!=connection_revision {
                            if !send(&mut socket,json!({"id":id,"event":{"version":1,"type":"remodexMobileConnectionUpdated","payload":{"connection":connection}}})).await {break;}
                            connection_revision=revision;
                        }
                    }
                }
                if let (Some(chats),Some((_,method,params)))=(&app.chats,&subscription) {
                    if method=="orchestration.subscribeThread" {let _=chats.call(agent,&client,"native.watchShared",params.clone()).await;}
                }
                continue;
            }
            incoming=socket.recv()=>incoming,
        };
        let Some(Ok(message)) = incoming else { break };
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        if !app.network.permits(peer) || !authorized(&app, &client).await {
            break;
        }
        let request: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => break,
        };
        let Some(id) = request.get("id").filter(|v| v.is_string() || v.is_number()) else {
            break;
        };
        let method = request["method"].as_str().unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        if [
            "orchestration.subscribeShell",
            "orchestration.subscribeThread",
            "subscribeServerConfig",
        ]
        .contains(&method)
        {
            subscription = Some((id.clone(), method.to_owned(), params.clone()));
        }
        let reply = native_reply(&app, agent, &client, id, method, params).await;
        if method == "subscribeServerConfig" {
            connection_revision =
                reply["event"]["config"]["remodexMobileConnection"]["revision"].as_u64();
        }
        if !send(&mut socket, reply).await {
            break;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await;
}
async fn send(socket: &mut WebSocket, value: Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn native_reply(
    app: &App,
    agent: AgentKind,
    client: &str,
    id: &Value,
    method: &str,
    params: Value,
) -> Value {
    // A private lifecycle hook is not exposed to paired phones.
    if method.starts_with("native.") {
        return json!({"id":id,"error":{"code":-32601,"message":"Unsupported method"}});
    }
    let Some(chats) = &app.chats else {
        return json!({"id":id,"error":{"code":-32601,"message":"This operation is not migrated to the native backend yet"}});
    };
    if method == "assets.createUrl" {
        return match chats.call(agent, client, method, params).await {
            Ok(v) => {
                let token = format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                );
                let expires = auth::now() + 120_000;
                let mut tickets = app.assets.lock().await;
                tickets.retain(|_, a| a.expires > auth::now());
                if tickets.len() >= 128 {
                    return json!({"id":id,"error":{"message":"Too many preview requests"}});
                }
                tickets.insert(
                    token.clone(),
                    Asset {
                        path: v["path"].as_str().unwrap().into(),
                        mime: v["mime"].as_str().unwrap().into(),
                        client: client.into(),
                        expires,
                    },
                );
                json!({"id":id,"result":{"relativeUrl":format!("/api/android-remote/assets/{token}/image"),"expiresAt":expires}})
            }
            Err(e) => json!({"id":id,"error":{"message":e}}),
        };
    }
    match chats.call(agent, client, method, params).await {
        Ok(mut value) => {
            if method == "server.getConfig" || method == "subscribeServerConfig" {
                value["environment"] = environment_descriptor(app);
                value["auth"] = descriptor();
                value["remodexMobileConnection"] = app.connection_state();
            }
            if method == "subscribeServerConfig" {
                json!({"id":id,"event":{"version":1,"type":"snapshot","config":value}})
            } else if method.starts_with("orchestration.subscribe") {
                json!({"id":id,"event":value})
            } else {
                json!({"id":id,"result":value})
            }
        }
        Err(error) => json!({"id":id,"error":{"code":-32000,"message":error}}),
    }
}
pub(crate) async fn not_implemented() -> Response {
    failure(
        StatusCode::NOT_FOUND,
        "Endpoint is not implemented in this native experiment",
    )
}

async fn authorized(app: &App, client: &str) -> bool {
    if *app.stopping.borrow() {
        return false;
    }
    let client = client.to_owned();
    matches!(
        app.auth
            .call(move |a| a.authorized(&client, auth::now()))
            .await,
        Ok(Ok(true))
    )
}

async fn chat_http(
    app: App,
    headers: HeaderMap,
    query: HashMap<String, String>,
    command: Option<Value>,
) -> Response {
    if query.get("agent").is_some_and(|v| v != "claude-code") {
        return failure(StatusCode::BAD_REQUEST, "Unknown agent");
    }
    let Some(token) = bearer(&headers).map(str::to_owned) else {
        return failure(StatusCode::UNAUTHORIZED, "Phone token required");
    };
    let client = app
        .auth
        .call(move |a| a.authenticate(&token, auth::now()))
        .await;
    let Ok(Ok(Some(client))) = client else {
        return failure(StatusCode::UNAUTHORIZED, "Invalid phone token");
    };
    let Some(chats) = app.chats else {
        return failure(
            StatusCode::NOT_IMPLEMENTED,
            "Native chat orchestration is not enabled",
        );
    };
    let agent = if query.contains_key("agent") {
        AgentKind::Claude
    } else {
        AgentKind::Codex
    };
    let (method, params) = match command {
        Some(v) => ("orchestration.dispatchCommand", v),
        None => ("orchestration.subscribeShell", json!({})),
    };
    match chats.call(agent, &client.id, method, params).await {
        Ok(v) => Json(if method == "orchestration.subscribeShell" {
            v["snapshot"].clone()
        } else {
            v
        })
        .into_response(),
        Err(e) => failure(StatusCode::BAD_REQUEST, &e),
    }
}
async fn chat_snapshot(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    chat_http(app, headers, query, None).await
}
async fn chat_dispatch(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    request: Request,
) -> Response {
    // Authenticate and reserve the sole upload slot BEFORE buffering any body.
    let Some(raw) = bearer(&headers).map(str::to_owned) else {
        return failure(StatusCode::UNAUTHORIZED, "Phone token required");
    };
    if !matches!(
        app.auth
            .call(move |a| a.authenticate(&raw, auth::now()))
            .await,
        Ok(Ok(Some(_)))
    ) {
        return failure(StatusCode::UNAUTHORIZED, "Invalid phone token");
    }
    let Ok(_permit) = app.transfers.clone().try_acquire_owned() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "Another attachment transfer is in progress",
        );
    };
    let body = match tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(request.into_body(), 56 * 1024 * 1024),
    )
    .await
    {
        Ok(Ok(b)) => b,
        _ => {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload is too large or timed out",
            )
        }
    };
    let command = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v,
        Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid command JSON"),
    };
    drop(body);
    chat_http(app, headers, query, Some(command)).await
}
async fn asset_image(State(app): State<App>, Path(token): Path<String>) -> Response {
    serve_asset(app, token, false).await
}
async fn asset_thumbnail(State(app): State<App>, Path(token): Path<String>) -> Response {
    serve_asset(app, token, true).await
}
async fn serve_asset(app: App, token: String, thumbnail: bool) -> Response {
    let asset = app
        .assets
        .lock()
        .await
        .get(&token)
        .filter(|a| a.expires > auth::now())
        .cloned();
    let Some(asset) = asset else {
        return failure(StatusCode::NOT_FOUND, "Preview expired");
    };
    if !authorized(&app, &asset.client).await {
        return failure(StatusCode::NOT_FOUND, "Preview expired");
    }
    // Android may replace a thumbnail/full-image request while its previous
    // download is still closing. Allow that overlap without sharing the large
    // upload/decode budget. Admission also bounds the number of queued requests.
    let Ok(admission) = app.preview_requests.clone().try_acquire_owned() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many image previews are pending",
        );
    };
    let slots = if thumbnail {
        app.image_decodes.clone()
    } else {
        app.image_downloads.clone()
    };
    let Ok(Ok(permit)) = tokio::time::timeout(Duration::from_secs(5), slots.acquire_owned()).await
    else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "Image preview is busy; retry shortly",
        );
    };
    // Authorization may have changed while waiting for a slot.
    if asset.expires <= auth::now() || !authorized(&app, &asset.client).await {
        return failure(StatusCode::NOT_FOUND, "Preview expired");
    }
    let path = asset.path;
    if !thumbnail {
        let file = tokio::task::spawn_blocking(move || crate::attachments::open_image(&path)).await;
        let Ok(Ok(file)) = file else {
            return failure(StatusCode::NOT_FOUND, "Image unavailable");
        };
        let Ok(length) = file.metadata().map(|m| m.len()) else {
            return failure(StatusCode::NOT_FOUND, "Image unavailable");
        };
        // Only four 64-KiB streaming buffers, never four decoded/full images.
        // Both reservations live until body completion or cancellation.
        let stream = futures_util::stream::unfold(
            (
                tokio::fs::File::from_std(file),
                (permit, admission),
                length as usize,
            ),
            |(mut file, permit, remaining)| async move {
                use tokio::io::AsyncReadExt;
                if remaining == 0 {
                    return None;
                }
                let mut bytes = vec![0; remaining.min(64 * 1024)];
                match file.read(&mut bytes).await {
                    Ok(0) => None,
                    Ok(n) => {
                        bytes.truncate(n);
                        Some((
                            Ok::<_, std::io::Error>(bytes),
                            (file, permit, remaining - n),
                        ))
                    }
                    Err(e) => Some((Err(e), (file, permit, 0))),
                }
            },
        );
        return (
            [
                ("content-type", asset.mime.as_str()),
                ("cache-control", "no-store"),
                ("x-content-type-options", "nosniff"),
                ("content-length", length.to_string().as_str()),
            ],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }
    // A cancelled HTTP handler cannot cancel a running blocking decoder. Keep
    // its permit inside that job so cancellation cannot start parallel decodes.
    let bytes = tokio::task::spawn_blocking(move || {
        let _decode = permit;
        (crate::attachments::thumbnail(&path), admission)
    })
    .await;
    match bytes {
        Ok((Ok(bytes), admission)) => (
            [
                (
                    "content-type",
                    if thumbnail {
                        "image/png"
                    } else {
                        asset.mime.as_str()
                    },
                ),
                ("cache-control", "no-store"),
                ("x-content-type-options", "nosniff"),
            ],
            axum::body::Body::from_stream(futures_util::stream::once(async move {
                let _admission = admission;
                Ok::<_, std::io::Error>(bytes)
            })),
        )
            .into_response(),
        _ => failure(StatusCode::NOT_FOUND, "Image unavailable"),
    }
}
