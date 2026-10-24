//! Opt-in native orchestration. Bounded owned CLI workers; no Desktop
//! discovery, normal-profile fallback, or automatic replay after a crash.
#[path = "chat_metadata.rs"]
mod metadata;
#[path = "chat_native_control.rs"]
mod native_control;
#[path = "chat_observer.rs"]
mod observer;
#[path = "chat_send_recovery.rs"]
mod send_recovery;
#[path = "chat_setup.rs"]
mod setup;
#[path = "chat_sync_recovery.rs"]
mod sync_recovery;
pub(crate) use observer::StopIntent;

use crate::{
    agent::{Agent, Connection, Event, Launch},
    auth_service::AuthService,
    chat_projection::{self as projection, now, s},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Codex,
    Claude,
}
impl AgentKind {
    pub fn instance(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude-code",
        }
    }
    pub(crate) fn driver(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub executable: PathBuf,
    pub profile: PathBuf,
    pub model: String,
    #[serde(default)]
    pub tools: crate::capabilities::ToolConfig,
    #[serde(default)]
    pub shared_server: Option<crate::agent::SharedServer>,
    #[serde(default)]
    pub remote_control: bool,
}
impl AgentConfig {
    pub(crate) async fn connect(&self, launch: Launch) -> Result<Connection, String> {
        if let Some(server) = &self.shared_server {
            if !matches!(launch.agent, Agent::Codex) {
                return Err("Shared transport is only supported for Codex".into());
            }
            server.connect(&self.profile).await
        } else {
            Connection::start(launch).map_err(|_| "Could not launch isolated CLI".into())
        }
    }
}
fn default_workers() -> usize {
    4
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub storage: crate::storage::Limits,
    #[serde(default = "default_workers")]
    pub max_active_chats: usize,
    pub workspace: PathBuf,
    pub codex: AgentConfig,
    pub claude: AgentConfig,
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        self.storage.validate()?;
        if !(1..=8).contains(&self.max_active_chats) {
            return Err("Choose between one and eight active chats".into());
        }
        for p in [
            &self.workspace,
            &self.codex.executable,
            &self.claude.executable,
            &self.codex.profile,
            &self.claude.profile,
        ] {
            if !p.is_absolute() || !p.exists() {
                return Err("Existing absolute test paths required".into());
            }
        }
        for p in [&self.codex.profile, &self.claude.profile] {
            let path = p
                .canonicalize()
                .map_err(|_| "Cannot resolve test profile")?;
            for key in ["HOME", "USERPROFILE"] {
                if let Some(home) = std::env::var_os(key) {
                    if crate::profile_lock::standard_agent_profile(&path, &PathBuf::from(home)) {
                        return Err("Normal agent profiles are forbidden in this experiment".into());
                    }
                }
            }
        }
        if self.codex.model.is_empty() || self.claude.model.is_empty() {
            return Err("Explicit test models required".into());
        }
        if self.claude.shared_server.is_some() {
            return Err("Shared transport is only supported for Codex".into());
        }
        if self.codex.remote_control {
            return Err("Claude Remote Control cannot be enabled for Codex".into());
        }
        if let Some(server) = &self.codex.shared_server {
            server.validate(&self.codex.profile)?;
        }
        if !self.codex.tools.is_empty() {
            return Err("Explicit plugin overrides currently apply to Claude only".into());
        }
        self.claude
            .tools
            .validate(&self.workspace, &self.claude.profile)?;
        Ok(())
    }
    fn provider(&self, agent: AgentKind) -> &AgentConfig {
        if agent == AgentKind::Codex {
            &self.codex
        } else {
            &self.claude
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Chat {
    pub agent: AgentKind,
    pub thread: Value,
    pub native_id: String,
    pub resumed: bool,
    #[serde(default)]
    pub claude_plan_access: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_permission_rules: Vec<String>,
    #[serde(default)]
    pub remote_identity: Option<crate::claude_host::Identity>,
    #[serde(default)]
    pub external_claude: bool,
    #[serde(default)]
    pub external_stop: Option<StopIntent>,
    #[serde(default)]
    pub native_control: Option<crate::native_control::Intent>,
    #[serde(default)]
    pub fork_from: Option<crate::branches::Fork>,
    /// An explicit owned-link replacement keeps its pinned fork intent until
    /// Claude durably creates the new native transcript. Never for observers.
    #[serde(default)]
    pub sync_replacement: Option<crate::branches::Fork>,
    #[serde(default)]
    pub queue: Vec<Value>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub disabled_mcp: Vec<String>,
    #[serde(skip)]
    pub stream_id: String,
    #[serde(default)]
    pub(crate) pending_message: Value,
    #[serde(skip)]
    pub(crate) stream_model: String,
}
impl Chat {
    /// A new chat derived from this one (branch or worktree) owns a fresh
    /// native session. It must never inherit the source's shared-session
    /// identity, ownership, pending controls or queued prompts.
    pub(crate) fn detach_native_session(&mut self) {
        self.resumed = false;
        self.claude_plan_access = None;
        self.claude_permission_rules.clear();
        self.remote_identity = None;
        self.external_claude = false;
        self.external_stop = None;
        self.native_control = None;
        self.fork_from = None;
        self.sync_replacement = None;
        self.queue.clear();
        self.paused = false;
        self.stream_id.clear();
        self.stream_model.clear();
        self.pending_message = Value::Null;
    }
}
struct Request {
    agent: AgentKind,
    client: String,
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value, String>>,
}
pub struct RestartReservation {
    gate: Arc<AtomicU8>,
    committed: bool,
}
impl RestartReservation {
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for RestartReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.gate.store(0, Ordering::Release);
        }
    }
}

#[derive(Clone)]
pub struct Chats {
    restart_gate: Arc<AtomicU8>,
    send: mpsc::Sender<Request>,
    changes: watch::Receiver<u64>,
    setup: crate::agent_setup::ClaudeSetup,
    pub(crate) activity: crate::activity::Reader,
    pub(crate) storage_report: crate::storage_report::Reader,
}
impl Chats {
    pub async fn start(db: AuthService, mut config: Config) -> Result<Self, String> {
        db.call(|a| a.store.assert_migration_ready()).await??;
        config.validate()?;
        let switches = setup::Settings::load(&db, config.claude.remote_control).await?;
        let claude_default_binary = config.claude.executable.clone();
        if let Some(binary) = &switches.binary {
            if !binary.is_absolute() || binary.as_os_str().len() > 2048 {
                return Err("Saved Claude executable path is invalid".into());
            }
            config.claude.executable = binary.clone();
        }
        config.claude.remote_control = switches.desktop_sync;
        let setup =
            crate::agent_setup::ClaudeSetup::new(config.claude.clone(), config.workspace.clone())?;
        let activity = crate::activity::Reader::new(config.codex.profile.clone());
        let storage_report = crate::storage_report::Reader::new(config.codex.profile.clone());
        let limits = config.storage.clone();
        db.call(move |a| a.store.configure_storage(limits))
            .await??;
        let saved = db
            .call(|a| {
                a.store.init_chats()?;
                a.store.chat_headers()
            })
            .await?
            .map_err(|_| "Chat store unavailable")?;
        if saved.len() > 10000 {
            return Err("This profile exceeds the 10,000-chat metadata budget".into());
        }
        let mut chats = HashMap::new();
        let mut shared_recovery = Vec::new();
        for value in saved {
            let mut chat: Chat =
                serde_json::from_value(value.clone()).map_err(|_| "Invalid saved native chat")?;
            if chat.thread["workspaceKind"] == "projectless" {
                chat.thread["projectId"] = json!("codex-project-chats");
            }
            // Prior delivery may have reached the model. Never replay it.
            let recover_shared = chat.agent == AgentKind::Codex
                && config.codex.shared_server.is_some()
                && !chat.native_id.is_empty()
                && chat.thread["deletedAt"].is_null()
                && chat.thread["archivedAt"].is_null()
                && matches!(
                    s(&chat.thread["session"]["status"]),
                    "starting" | "running" | "stopping"
                );
            if recover_shared {
                shared_recovery.push((
                    s(&chat.thread["id"]).to_owned(),
                    s(&chat.thread["updatedAt"]).to_owned(),
                ));
                chat.thread["session"]["statusConfidence"] = json!("unknown");
                chat.thread["session"]["lastError"] = json!(
                    "Reconnecting to the shared Codex session. Saved prompts will not be resent."
                );
            }
            if matches!(s(&chat.thread["session"]["status"]), "starting" | "running")
                && !chat.external_claude
                && !recover_shared
            {
                finish(
                    &mut chat,
                    "interrupted",
                    Some("Bridge restarted. Review the last response before resending."),
                );
            }
            chat.paused = true;

            if chat.agent == AgentKind::Claude {
                let id = s(&chat.thread["id"]).to_owned();
                let native = chat.native_id.clone();
                db.call(move |a| a.store.mark_claude_delivery_uncertain(&id, &native))
                    .await?
                    .map_err(|_| "Cannot retain uncertain Claude delivery records")?;
            }
            if switches.enabled && config.claude.remote_control && claude_recoverable(&chat) {
                projection::activity(
                    &mut chat,
                    "claude-native-sync",
                    "claude.desktop-sync.updated",
                    "Claude Desktop sync",
                    "info",
                    json!({"status":"disconnected","detail":"Reconnecting automatically. Saved messages will not be resent."}),
                );
            }
            let data = serde_json::to_value(&chat).unwrap();
            if data != value {
                db.call(move |a| a.store.save_chat(&data, None))
                    .await?
                    .map_err(|_| "Chat recovery save failed")?;
            }
            chats.insert(s(&chat.thread["id"]).to_owned(), chat);
        }
        let root = config.workspace.clone();
        let projects = db
            .call(move |a| a.store.projects(&root))
            .await?
            .map_err(|_| "Project store unavailable")?;
        let models = db
            .call(|a| -> rusqlite::Result<HashMap<String, Value>> {
                use rusqlite::OptionalExtension;
                let value: Option<String> = a
                    .store
                    .db
                    .query_row(
                        "SELECT value FROM metadata WHERE key='native.models'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(value
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or_default())
            })
            .await?
            .map_err(|_| "Cannot load model catalog")?;
        let (send, receive) = mpsc::channel(16);
        let (changes_tx, changes) = watch::channel(0);
        let mut claude_recovery = crate::claude_reconnect::Recovery::default();
        if switches.enabled && config.claude.remote_control {
            let mut owned: Vec<_> = chats
                .iter()
                .filter(|(_, c)| claude_recoverable(c))
                .collect();
            owned.sort_by(|(aid, a), (bid, b)| {
                s(&b.thread["updatedAt"])
                    .cmp(s(&a.thread["updatedAt"]))
                    .then(aid.cmp(bid))
            });
            for (id, _) in owned.into_iter().take(crate::claude_reconnect::HOST_LIMIT) {
                claude_recovery.remember(id, Instant::now());
            }
        }
        shared_recovery.sort_by(|(aid, a), (bid, b)| b.cmp(a).then(aid.cmp(bid)));
        let shared_watches = shared_recovery
            .into_iter()
            .take(64)
            .map(|(id, _)| {
                (
                    id,
                    (Instant::now() + Duration::from_secs(60), Instant::now()),
                )
            })
            .collect();
        let restart_gate = Arc::new(AtomicU8::new(0));
        tokio::spawn(
            Hub {
                db,
                config,
                claude_enabled: switches.enabled,
                claude_default_binary,
                claude_binary_override: switches.binary,
                chats,
                projects,
                models,
                discovery_errors: HashMap::new(),
                usage_task: None,
                usage_cache: None,
                usage_reads: Vec::new(),
                active: HashMap::new(),
                claude_recovery,
                shared_watches,
                tool_controls: HashSet::new(),
                recovery_checks: HashMap::new(),
                prepared_recoveries: HashMap::new(),
                confirmed_settings: None,
                prepared_edits: HashMap::new(),
                prepared_bootstraps: HashMap::new(),
                seq: 0,
                changes: changes_tx,
                dirty: false,
            }
            .run(receive, restart_gate.clone()),
        );
        Ok(Self {
            restart_gate,
            send,
            changes,
            setup,
            activity,
            storage_report,
        })
    }
    /// Reserve an idle hub until the caller commits shutdown or drops the guard.
    /// The guard also releases admission when a dashboard request is cancelled.
    pub async fn prepare_restart(&self) -> Result<RestartReservation, String> {
        self.restart_gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A bridge restart is already being prepared")?;
        let reservation = RestartReservation {
            gate: self.restart_gate.clone(),
            committed: false,
        };
        self.send_request(
            AgentKind::Codex,
            "local",
            "native.prepareRestart",
            json!({}),
        )
        .await?;
        Ok(reservation)
    }
    /// Reserve admission while existing work drains. No new prompt, Steer,
    /// setup or storage mutation may start; approvals and Stop remain usable.
    pub async fn prepare_drain(&self) -> Result<(RestartReservation, Value), String> {
        self.restart_gate
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A bridge restart or storage operation is already being prepared")?;
        let reservation = RestartReservation {
            gate: self.restart_gate.clone(),
            committed: false,
        };
        let status = self
            .call(AgentKind::Codex, "local", "native.restartStatus", json!({}))
            .await?;
        Ok((reservation, status))
    }
    /// Hold admission through the blocking storage transaction, even if its HTTP
    /// caller goes away. Known chat histories are protected from archive cleanup.
    pub(crate) async fn prepare_storage(
        &self,
    ) -> Result<(RestartReservation, Vec<String>), String> {
        self.restart_gate
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "storage_mutation_busy")?;
        let guard = RestartReservation {
            gate: self.restart_gate.clone(),
            committed: false,
        };
        let value = self
            .send_request(
                AgentKind::Codex,
                "local",
                "native.prepareStorage",
                json!({}),
            )
            .await?;
        let ids = serde_json::from_value(value).map_err(|_| "storage_mutation_busy")?;
        Ok((guard, ids))
    }
    pub async fn call(
        &self,
        agent: AgentKind,
        client: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        if method == "claude.recoverDesktopSync" {
            return self.retry_claude_sync(agent, client, params).await;
        }
        if matches!(method, "claude.getSyncSettings" | "claude.setAutoConnect") {
            if agent != AgentKind::Claude {
                return Err("Select Claude Code before changing its settings".into());
            }
            let enabled = if method == "claude.setAutoConnect" {
                Some(
                    params
                        .as_object()
                        .filter(|o| o.len() == 1)
                        .and_then(|o| o.get("enabled"))
                        .and_then(Value::as_bool)
                        .ok_or("Provide only the automatic sync enabled boolean")?,
                )
            } else {
                None
            };
            let mut state = self
                .send_request(agent, client, "native.claudeSetupSnapshot", json!({}))
                .await?;
            if enabled == Some(true) && (state["enabled"] != true || state["desktopSync"] != true) {
                return Err("Enable Claude Code and Desktop sharing in this bridge before automatic connection".into());
            }
            state["autoConnect"] = self.setup.preference(enabled).await?;
            return Ok(state);
        }
        self.send_request(agent, client, method, params).await
    }
    pub(crate) async fn claude_setup(&self, refresh: bool) -> Result<Value, String> {
        let mut state = self
            .call(
                AgentKind::Claude,
                "dashboard",
                "claude.getSyncSettings",
                json!({}),
            )
            .await?;
        if state["enabled"] != true {
            return Ok(state);
        }
        let executable = PathBuf::from(s(&state["binary"]));
        let account = self.setup.account(refresh, &executable).await;
        state["ready"] = json!(account["ready"] == true && state["catalogReady"] == true);
        state["account"] = account["account"].clone();
        if account["error"].is_string() {
            state["error"] = account["error"].clone();
        }
        if let Some(o) = state.as_object_mut() {
            o.remove("catalogReady");
        }
        Ok(state)
    }
    async fn send_request(
        &self,
        agent: AgentKind,
        client: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let (reply, receive) = oneshot::channel();
        self.send
            .try_send(Request {
                agent,
                client: client.into(),
                method: method.into(),
                params,
                reply,
            })
            .map_err(|_| "Native chat service busy")?;
        receive
            .await
            .map_err(|_| "Native chat service stopped".to_owned())?
    }
    pub async fn configure_claude(&self, settings: Value) -> Result<Value, String> {
        self.send_request(
            AgentKind::Claude,
            "dashboard",
            "native.configureClaude",
            settings,
        )
        .await?;
        self.claude_setup(false).await
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.clone()
    }
}
struct Worker {
    connection: Connection,
    chat_id: String,
    stage: &'static str,
    native_turn: String,
    owns_turn: bool,
    pre_turn_events: std::collections::VecDeque<(Value, usize)>,
    pre_turn_bytes: usize,
    requests: HashMap<String, Value>,
    steers: HashMap<String, bool>,
    last_prompt: String,
    first_prompt: String,
    deadline: Option<Instant>,
    stopping: bool,
    metadata: metadata::Reads,
    control_reply: Option<native_control::Waiter>,
    startup_tools: Vec<String>,
    checkpoint_labels: HashMap<String, String>,
    unsupported_controls: u8,
    claude_router: Option<crate::claude_turns::Router>,
    claude_background: crate::claude_background::Tasks,
    claude_deferred_result: Option<Value>,
    claude_stop: Option<crate::claude_background::StopSequence>,
    local_turn: Option<ClaudeLocalTurn>,
    recovering: bool,
    observer: Option<observer::State>,
}
struct ClaudeLocalTurn {
    latest: Value,
    message: Value,
    native: String,
    delivered: bool,
}
enum PreparedTool {
    Cached(Value),
    Run(String, String, crate::capabilities::Control),
}
enum FileJob {
    Preview(Result<(Option<crate::checkpoints::Preview>, Value), String>),
    Restore(String, String, Result<Value, String>),
}
struct PreparedBootstrap {
    key: String,
    digest: String,
    source: Chat,
    setup: crate::worktrees::Setup,
    target: crate::worktrees::Target,
}
struct Hub {
    tool_controls: HashSet<String>,
    recovery_checks: HashMap<String, sync_recovery::HistoryCheck>,
    prepared_recoveries: HashMap<String, String>,
    /// Chat whose model/effort the shared worker has just acknowledged for
    /// the command now being dispatched (set only around that dispatch).
    confirmed_settings: Option<String>,
    prepared_edits: HashMap<String, crate::editing::Prepared>,
    prepared_bootstraps: HashMap<String, (Chat, PathBuf)>,
    db: AuthService,
    config: Config,
    claude_enabled: bool,
    claude_default_binary: PathBuf,
    claude_binary_override: Option<PathBuf>,
    chats: HashMap<String, Chat>,
    projects: Value,
    models: HashMap<String, Value>,
    discovery_errors: HashMap<String, String>,
    usage_task: Option<tokio::task::JoinHandle<Option<Value>>>,
    usage_cache: Option<(Instant, Option<Value>)>,
    usage_reads: Vec<String>,
    active: HashMap<String, Worker>,
    claude_recovery: crate::claude_reconnect::Recovery,
    shared_watches: HashMap<String, (Instant, Instant)>,
    seq: u64,
    changes: watch::Sender<u64>,
    dirty: bool,
}
pub(crate) fn finish(chat: &mut Chat, state: &str, error: Option<&str>) {
    chat.pending_message = Value::Null;
    chat.external_stop = None;
    let t = &mut chat.thread;
    t["session"]["status"] = json!(if state == "completed" { "ready" } else { state });
    t["session"]["activeTurnId"] = Value::Null;
    // A provider error can itself be a large protocol frame. Keep diagnostics
    // bounded too, rather than retaining it outside the transcript size guard.
    t["session"]["lastError"] = json!(error.map(|e| e.chars().take(1024).collect::<String>()));
    t["session"]["updatedAt"] = json!(now());
    if !t["latestTurn"].is_null() {
        t["latestTurn"]["state"] = json!(state);
        t["latestTurn"]["completedAt"] = json!(now());
    }
    t["hasPendingApprovals"] = json!(false);
    t["hasPendingUserInput"] = json!(false);
    t["updatedAt"] = json!(now());
    for m in t["messages"].as_array_mut().unwrap() {
        m["streaming"] = json!(false);
    }
    for a in t["activities"].as_array_mut().unwrap() {
        if a["kind"] == "steer.updated" && a["payload"]["status"] == "pending" {
            a["payload"]["status"] = json!("failed");
            a["payload"]["detail"] =
                json!("Delivery was not confirmed. Review the chat before resending.");
        }
        if a["payload"]["status"] == "running"
            || ["approval.requested", "user-input.requested"]
                .iter()
                .any(|k| a["kind"] == *k)
        {
            a["payload"]["status"] = json!(if state == "completed" {
                "completed"
            } else {
                "stopped"
            });
            if a["kind"] == "approval.requested" {
                a["kind"] = json!("approval.resolved");
            } else if a["kind"] == "user-input.requested" {
                a["kind"] = json!("user-input.resolved");
            }
        }
    }
    crate::turn_timing::complete(&mut chat.thread);
    if state != "completed" {
        chat.paused = true;
    }
}
impl Hub {
    async fn reconnect_claude(&mut self) {
        if !self.claude_enabled || !self.config.claude.remote_control {
            return;
        }
        // One launch per tick and at most two handshakes; never compete with
        // active user work, metadata controls, or more than eight live hosts.
        if self.busy_count() >= self.config.max_active_chats
            || self
                .active
                .values()
                .filter(|w| w.recovering && w.stage != "idle")
                .count()
                >= 2
            || self
                .active
                .values()
                .filter(|w| w.claude_router.is_some() || w.observer.is_some())
                .count()
                >= crate::claude_reconnect::HOST_LIMIT
        {
            return;
        }
        for id in self.claude_recovery.ready(Instant::now()) {
            if !self.chats.get(&id).is_some_and(claude_recoverable) {
                self.claude_recovery.forget(&id);
                continue;
            }
            if self.active.contains_key(&id) || self.tool_controls.contains(&id) {
                continue;
            }
            let result = match self.db.call(|a| a.store.check_history(0)).await {
                Ok(Ok(())) => self.launch_worker(&id, true).await,
                _ => Err("Claude reconnect is waiting for available history storage".into()),
            };
            if let Err(error) = result {
                self.claude_recovery.failed(&id, Instant::now());
                projection::activity(
                    self.chats.get_mut(&id).unwrap(),
                    "claude-native-sync",
                    "claude.desktop-sync.updated",
                    "Claude Desktop sync",
                    "info",
                    json!({"status":"disconnected","detail":error}),
                );
                let _ = self.save(&id, None).await;
                self.dirty = true;
            }
            break;
        }
    }
    fn busy(&self, id: &str) -> bool {
        self.active.get(id).is_some_and(|w| w.stage != "idle")
    }
    fn busy_count(&self) -> usize {
        self.recovery_checks.len()
            + self.active.values().filter(|w| w.stage != "idle").count()
            + self
                .chats
                .keys()
                .filter(|id| self.detached_shared_task(id))
                .count()
    }
    fn detached_shared_task(&self, id: &str) -> bool {
        self.config.codex.shared_server.is_some()
            && !self.active.contains_key(id)
            && self.chats.get(id).is_some_and(|chat| {
                chat.agent == AgentKind::Codex
                    && !chat.native_id.is_empty()
                    && matches!(
                        s(&chat.thread["session"]["status"]),
                        "starting" | "running" | "stopping"
                    )
            })
    }
    async fn attach_shared(&mut self, id: &str, turn: String) -> Result<(), String> {
        // Reattaching replaces this task's existing reservation; it must not
        // require a second slot or let a disconnected CLI exceed the limit.
        let other_busy = self
            .busy_count()
            .saturating_sub(usize::from(self.detached_shared_task(id)));
        if self.busy(id) || other_busy >= self.config.max_active_chats {
            return Ok(());
        }
        let config = &self.config.codex;
        let connection = config
            .shared_server
            .as_ref()
            .ok_or("Shared server unavailable")?
            .connect(&config.profile)
            .await?;
        connection.send(&json!({"id":"attach-init","method":"initialize","params":{"clientInfo":{"name":"remodex_shared_controls","version":"0.0.0"},"capabilities":{"experimentalApi":true}}})).await.map_err(|_|"Shared control connection unavailable")?;
        self.active.insert(
            id.into(),
            Worker {
                connection,
                chat_id: id.into(),
                stage: "attach-init",
                native_turn: turn,
                owns_turn: false,
                pre_turn_events: Default::default(),
                pre_turn_bytes: 0,
                requests: HashMap::new(),
                steers: HashMap::new(),
                last_prompt: String::new(),
                first_prompt: String::new(),
                deadline: Some(Instant::now() + Duration::from_secs(15)),
                stopping: false,
                metadata: metadata::Reads::default(),
                control_reply: None,
                startup_tools: vec![],
                checkpoint_labels: HashMap::new(),
                unsupported_controls: 0,
                claude_router: None,
                claude_background: Default::default(),
                claude_deferred_result: None,
                claude_stop: None,
                local_turn: None,
                recovering: false,
                observer: None,
            },
        );
        Ok(())
    }
    async fn apply_shared(
        &mut self,
        id: &str,
        batch: crate::shared_history::Batch,
    ) -> Result<(), String> {
        if batch.missing_empty_session {
            let mut chat = self.chats[id].clone();
            chat.native_id.clear();
            chat.resumed = false;
            projection::activity(
                &mut chat,
                "empty-session-recovered",
                "task.progress",
                "Reconnected after the first send failed",
                "info",
                json!({"status":"completed","detail":"The empty native session expired. The failed prompt is retained; only your new message will be sent."}),
            );
            let state =
                serde_json::to_value(&chat).map_err(|_| "Cannot retain the failed prompt")?;
            let cid = id.to_owned();
            let native = batch.native;
            self.db
                .call(move |a| {
                    if !a.store.empty_shared_session(&cid, &native)? {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    a.store.save_chat(&state, None)
                })
                .await?
                .map_err(|_| "Cannot retain the failed session; no new prompt was sent")?;
            self.chats.insert(id.to_owned(), chat);
            self.dirty = true;
            return Ok(());
        }
        let mut chat = self.chats[id].clone();
        // A page never brings an unrelated conversation or raw tool output
        // into the live projection. Older owned turns keep their original IDs.
        for kind in ["messages", "activities", "proposedPlans"] {
            chat.thread[kind] = json!([]);
        }
        let original = chat.clone();
        if let Some(turn) = &batch.turn {
            let public = batch
                .public_turn
                .clone()
                .ok_or("Shared turn has no verified public identity")?;
            let owned = public != format!("desktop-{}", s(&turn["id"]));
            let started = turn["startedAt"]
                .as_i64()
                .filter(|v| *v > 0)
                .map(|v| crate::auth::timestamp(v.saturating_mul(1000)))
                .ok_or("Shared turn has no start timestamp")?;
            let completed = turn["completedAt"]
                .as_i64()
                .filter(|v| *v > 0)
                .map(|v| crate::auth::timestamp(v.saturating_mul(1000)));
            let latest = chat.thread["latestTurn"].clone();
            let is_latest =
                latest["turnId"] == public || s(&latest["requestedAt"]) <= started.as_str();
            chat.thread["latestTurn"] = json!({"turnId":public,"state":"running","requestedAt":started,"startedAt":started,"completedAt":null,"assistantMessageId":null});
            for row in &batch.items {
                let item = &row["item"];
                let item_id = s(&item["id"]);
                if item["type"] == "userMessage" {
                    if let Some(client_id) = item["clientId"].as_str() {
                        let cid = id.to_owned();
                        let mid = client_id.to_owned();
                        if let Some(receipt) = self
                            .db
                            .call(move |a| a.store.history_row(&cid, "messages", &mid))
                            .await?
                            .map_err(|_| "Shared prompt receipt unavailable")?
                        {
                            if receipt["role"] != "user" || receipt["turnId"] != public {
                                return Err("Shared prompt receipt does not match this turn; history is intact".into());
                            }
                            continue;
                        }
                    }
                    let text = item["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|v| v["type"] == "text")
                        .map(|v| s(&v["text"]))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.len() > crate::chat_store::ROW_BYTES / 2 {
                        return Err(
                            "A Desktop prompt exceeds the display limit; history is intact".into(),
                        );
                    }
                    projection::message(&mut chat, item_id, &text, "user", false, false);
                } else {
                    projection::project(
                        &mut chat,
                        &json!({"method":if row["completedAtMs"].is_number() || batch.final_page {"item/completed"}else{"item/started"},"params":{"threadId":batch.native,"turnId":turn["id"],"item":item}}),
                    );
                }
                if (row["completedAtMs"].is_number() || batch.final_page)
                    && ["mcpToolCall", "dynamicToolCall", "imageView"].contains(&s(&item["type"]))
                {
                    let cid = id.to_owned();
                    let item = item.clone();
                    let workspace = PathBuf::from(s(&chat.thread["worktreePath"]));
                    let mut paths = self
                        .db
                        .call(move |a| a.store.codex_tool_images(&cid, &workspace, &item))
                        .await??;
                    if let Some(recovered) = batch.recovered_images.get(item_id) {
                        for path in recovered {
                            if !paths.contains(path) {
                                paths.push(path.clone());
                            }
                        }
                    }
                    if !paths.is_empty() {
                        if let Some(out) = chat.thread["activities"]
                            .as_array_mut()
                            .unwrap()
                            .iter_mut()
                            .find(|r| r["id"] == item_id)
                        {
                            out["payload"]["data"]["toolImages"] = json!(paths);
                        }
                    }
                }
                for kind in ["messages", "activities", "proposedPlans"] {
                    if let Some(out) = chat.thread[kind]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|v| v["id"] == item_id)
                    {
                        let cid = id.to_owned();
                        let key = item_id.to_owned();
                        let old = self
                            .db
                            .call(move |a| a.store.history_row(&cid, kind, &key))
                            .await?
                            .map_err(|_| "Shared history row unavailable")?;
                        if kind == "activities" && out["payload"]["data"]["toolImages"].is_null() {
                            if let Some(paths) = old
                                .as_ref()
                                .and_then(|v| v["payload"]["data"]["toolImages"].as_array())
                            {
                                out["payload"]["data"]["toolImages"] = json!(paths);
                            }
                        }
                        let at = row["startedAtMs"]
                            .as_i64()
                            .map(crate::auth::timestamp)
                            .unwrap_or_else(|| started.clone());
                        out["createdAt"] = old
                            .as_ref()
                            .and_then(|v| v.get("createdAt"))
                            .cloned()
                            .unwrap_or(json!(at));
                        if out.get("updatedAt").is_some() {
                            out["updatedAt"] = json!(row["completedAtMs"]
                                .as_i64()
                                .map(crate::auth::timestamp)
                                .unwrap_or(at));
                        }
                        if kind == "proposedPlans" {
                            if let Some(old) = &old {
                                for key in ["implementedAt", "implementationThreadId"] {
                                    if let Some(v) = old.get(key) {
                                        out[key] = v.clone();
                                    }
                                }
                            }
                        }
                        if out.to_string().len() > crate::chat_store::ROW_BYTES {
                            return Err("A Desktop output item exceeds the display limit; history is intact".into());
                        }
                    }
                }
                projection::tool_image_gallery(&mut chat, item_id);
                for kind in ["messages", "activities", "proposedPlans"] {
                    let cid = id.to_owned();
                    let key = item_id.to_owned();
                    let old = self
                        .db
                        .call(move |a| a.store.history_row(&cid, kind, &key))
                        .await?
                        .map_err(|_| "Shared history row unavailable")?;
                    if let Some(old) = old {
                        chat.thread[kind]
                            .as_array_mut()
                            .unwrap()
                            .retain(|r| r["id"] != item_id || *r != old);
                    }
                }
                // The eight-item page is additionally bounded by transport and
                // projection limits, never by the size of the full transcript.
            }
            if is_latest {
                chat.thread["sharedTurnOwner"] = json!(if owned { "android" } else { "desktop" });
                chat.thread["session"]["status"] = json!("running");
                chat.thread["session"]["statusConfidence"] = json!("confirmed");
                chat.thread["session"]["activeTurnId"] = json!(public);
                chat.thread["session"]["lastError"] = Value::Null;
                chat.thread["latestUserMessageAt"] = json!(started);
                chat.thread["updatedAt"] = json!(completed.as_ref().unwrap_or(&started));
                if batch.final_page {
                    let state = match s(&turn["status"]) {
                        "interrupted" => "interrupted",
                        "failed" => "error",
                        _ => "completed",
                    };
                    finish(
                        &mut chat,
                        state,
                        if state == "error" {
                            Some("The shared CLI reported an unsuccessful turn")
                        } else {
                            None
                        },
                    );
                    chat.thread["latestTurn"]["completedAt"] =
                        json!(completed.as_ref().unwrap_or(&started));
                    chat.thread["session"]["updatedAt"] =
                        json!(completed.as_ref().unwrap_or(&started));
                    chat.thread["updatedAt"] = json!(completed.as_ref().unwrap_or(&started));
                }
            } else {
                chat.thread["latestTurn"] = latest;
            }
        }
        if batch.before == batch.after
            && serde_json::to_value(&chat).unwrap() == serde_json::to_value(&original).unwrap()
        {
            return Ok(());
        }
        let data = serde_json::to_value(&chat).map_err(|_| "Invalid shared history")?;
        self.db
            .call(move |a| {
                a.store
                    .check_history(crate::chat_store::ROW_BYTES as u64 * 8)
                    .map_err(rusqlite::Error::InvalidParameterName)?;
                a.store.save_shared_history(&data, &batch)
            })
            .await?
            .map_err(|_| "Cannot commit shared history; its cursor was retained")?;
        self.chats.insert(id.to_owned(), chat);
        self.dirty = true;
        Ok(())
    }
    async fn run(mut self, mut receive: mpsc::Receiver<Request>, restart_gate: Arc<AtomicU8>) {
        let mut tick = tokio::time::interval(Duration::from_millis(120));
        let mut inspections = tokio::task::JoinSet::new();
        let mut clone_jobs = tokio::task::JoinSet::new();
        let mut discovery = tokio::task::JoinSet::new();
        let mut capability_jobs = tokio::task::JoinSet::new();
        let mut tool_jobs = tokio::task::JoinSet::new();
        let mut worktree_jobs = tokio::task::JoinSet::new();
        let mut file_jobs = tokio::task::JoinSet::new();
        let mut edit_jobs = tokio::task::JoinSet::new();
        let mut bootstrap_jobs = tokio::task::JoinSet::new();
        let mut shared_jobs = tokio::task::JoinSet::new();
        let mut shared_reads = HashSet::new();
        let mut shared_sends: HashMap<String, (Instant, u64, Request)> = HashMap::new();
        let mut claude_sends = send_recovery::Sends::default();
        let mut shared_send_epoch = 0u64;
        let mut previews: HashMap<String, crate::checkpoints::Preview> = HashMap::new();
        let mut capability_waiters: HashMap<String, Vec<oneshot::Sender<Result<Value, String>>>> =
            HashMap::new();
        let mut idle_capabilities: HashMap<String, (Instant, Value)> = HashMap::new();
        let mut to_discover = vec![AgentKind::Claude, AgentKind::Codex];
        let mut poll_cursor = 0usize;
        let mut retention = crate::retention::Scanner::default();
        let mut retention_due = Instant::now() + Duration::from_secs(30);
        let mut storage_checked = Instant::now();
        loop {
            let restart_pending = restart_gate.load(Ordering::Acquire) != 0;
            self.shared_watches.retain(|id, (until, _)| {
                *until > Instant::now()
                    || shared_sends.contains_key(id)
                    || self.chats.get(id).is_some_and(|c| {
                        c.thread["deletedAt"].is_null()
                            && c.thread["archivedAt"].is_null()
                            && matches!(
                                s(&c.thread["session"]["status"]),
                                "starting" | "running" | "stopping"
                            )
                    })
            });
            let ready: Vec<_> = self
                .shared_watches
                .iter()
                .filter(|(id, (_, due))| {
                    restart_gate.load(Ordering::Acquire) != 1
                        && restart_gate.load(Ordering::Acquire) != 2
                        && *due <= Instant::now()
                        && !self.active.contains_key(*id)
                        && !shared_reads.contains(*id)
                        && (!self.tool_controls.contains(*id) || shared_sends.contains_key(*id))
                })
                .map(|(id, (_, due))| (id.clone(), *due))
                .collect();
            let mut ready = ready;
            ready.sort_by_key(|(_, due)| *due);
            for (id, _) in ready
                .into_iter()
                .take(2usize.saturating_sub(shared_jobs.len()))
            {
                let Some(chat) = self.chats.get(&id).filter(|c| {
                    c.thread["deletedAt"].is_null()
                        && c.thread["archivedAt"].is_null()
                        && !c.native_id.is_empty()
                }) else {
                    continue;
                };
                let native = chat.native_id.clone();
                let config = self.config.codex.clone();
                let db = self.db.clone();
                let send_epoch = shared_sends.get(&id).map(|(_, epoch, _)| *epoch);
                shared_reads.insert(id.clone());
                shared_jobs.spawn(async move {
                    let result = crate::shared_history::read(
                        config,
                        db,
                        id.clone(),
                        native.clone(),
                        send_epoch.is_some(),
                    )
                    .await;
                    (id, native, send_epoch, result)
                });
            }
            // Metadata-only and isolated: idle shared connections must not keep
            // a stale catalog (or its alias-to-wire mapping) forever.
            if !restart_pending && discovery.is_empty() && self.busy_count() == 0 {
                if let Some(agent) = to_discover
                    .pop()
                    .filter(|a| *a != AgentKind::Claude || self.claude_enabled)
                {
                    let config = self.config.provider(agent).clone();
                    let cwd = self.config.workspace.clone();
                    discovery.spawn(async move {
                        (agent, crate::discovery::discover(agent, config, &cwd).await)
                    });
                }
            }
            claude_sends.refresh(&self);
            tokio::select! {
                request=async { match claude_sends.pop() { Some(r)=>Some(r), None=>receive.recv().await.map(|r|(r,None)) } }=>{
                    let Some((r,waiting_token))=request else {break};
                    if waiting_token.as_ref().is_some_and(|token| !send_recovery::Sends::valid(&self,&r,token)) {
                        let _=r.reply.send(Err("Connection wait ended or the session changed; your prompt was not sent".into()));continue;
                    }
                    if r.method=="native.shutdown" {self.shutdown().await; let _=r.reply.send(Ok(json!({})));break;}
                    if r.method=="native.prepareStorage" {
                        let busy=self.busy_count()>0 || !self.tool_controls.is_empty() || !clone_jobs.is_empty() || !worktree_jobs.is_empty() || !file_jobs.is_empty() || !edit_jobs.is_empty() || !bootstrap_jobs.is_empty() || !tool_jobs.is_empty() || !claude_sends.is_empty() || !shared_sends.is_empty() || !shared_jobs.is_empty() || !discovery.is_empty() || !capability_jobs.is_empty() || !inspections.is_empty() || self.usage_task.is_some() || self.chats.values().any(|c|!c.queue.is_empty() || matches!(s(&c.thread["session"]["status"]),"starting"|"running"|"stopping"));
                        let result=if restart_gate.load(Ordering::Acquire)!=2 {Err("storage_mutation_busy".into())}else if busy {Err("codex_busy".into())}else {Ok(json!(self.chats.values().filter(|c|c.agent==AgentKind::Codex && !c.native_id.is_empty() && c.thread["deletedAt"].is_null()).map(|c|c.native_id.clone()).collect::<Vec<_>>()))};
                        let _=r.reply.send(result);continue;
                    }
                    if matches!(r.method.as_str(),"native.prepareRestart"|"native.restartStatus") {
                        let busy=self.busy_count()>0 || !self.tool_controls.is_empty() || !clone_jobs.is_empty() || !worktree_jobs.is_empty() || !file_jobs.is_empty() || !edit_jobs.is_empty() || !bootstrap_jobs.is_empty() || !tool_jobs.is_empty() || !claude_sends.is_empty() || !shared_sends.is_empty() || self.chats.values().any(|c|!c.queue.is_empty() || matches!(s(&c.thread["session"]["status"]),"starting"|"running"|"stopping"));
                        if r.method=="native.restartStatus" {
                            let drain_busy=self.busy_count()>0 || !self.tool_controls.is_empty() || !clone_jobs.is_empty() || !worktree_jobs.is_empty() || !file_jobs.is_empty() || !edit_jobs.is_empty() || !bootstrap_jobs.is_empty() || !tool_jobs.is_empty() || !claude_sends.is_empty() || !shared_sends.is_empty() || self.chats.values().any(|c|!c.paused && !c.queue.is_empty());
                            let _=r.reply.send(Ok(json!({"ready":!busy && restart_gate.load(Ordering::Acquire)==0,"drainReady":!drain_busy,"isDraining":restart_gate.load(Ordering::Acquire)==3,"activeTurnCount":self.busy_count()})));continue;
                        }
                        let result=if restart_gate.load(Ordering::Acquire)==0 {Err("Restart reservation is no longer active".into())}else if busy {Err("Finish or stop active tasks and clear queued messages before restarting this bridge".into())}else {Ok(json!({"reserved":true}))};
                        let _=r.reply.send(result);continue;
                    }
                    if restart_gate.load(Ordering::Acquire)!=0 && !(restart_gate.load(Ordering::Acquire)==3 && crate::restart_drain::allowed(&r.method,&r.params)) {let message=if restart_gate.load(Ordering::Acquire)==2 {"Storage maintenance is in progress; retry shortly"}else{"Bridge is restarting; retry after it reconnects"};let _=r.reply.send(Err(message.into()));continue;}
                    if r.method=="native.configureClaude" {
                        // Wait for bounded metadata/jobs rather than racing a configuration
                        // change against a CLI launched with the previous setup.
                        let result = if !claude_sends.is_empty() || !discovery.is_empty() || !capability_jobs.is_empty() || !tool_jobs.is_empty() || !file_jobs.is_empty() || !edit_jobs.is_empty() || !bootstrap_jobs.is_empty() || !worktree_jobs.is_empty() || self.usage_task.is_some() {
                            Err("Wait for agent setup or pending background controls to finish".into())
                        } else {self.configure_claude(&r.params).await};
                        if result.is_ok() && self.claude_enabled && !self.models.contains_key(AgentKind::Claude.instance()) && !to_discover.contains(&AgentKind::Claude) {to_discover.push(AgentKind::Claude);}
                        let _=r.reply.send(result);continue;
                    }
                    if r.agent==AgentKind::Claude && !self.claude_enabled && (r.method=="orchestration.dispatchCommand" || r.method=="native.createChat" || r.method.starts_with("claude.")) {
                        let _=r.reply.send(Err("Enable Claude Code in the Remodex dashboard first".into()));continue;
                    }
                    if r.method=="native.watchShared" {
                        let id=s(&r.params["threadId"]).to_owned();
                        if r.agent==AgentKind::Codex && self.config.codex.shared_server.is_some() && self.checked(r.agent,&id).is_ok() && (self.shared_watches.len()<64 || self.shared_watches.contains_key(&id)) {
                            let watch=self.shared_watches.entry(id).or_insert((Instant::now(),Instant::now()));watch.0=Instant::now()+Duration::from_secs(3);
                        }
                        let _=r.reply.send(Ok(json!({})));continue;
                    }
                    if r.method=="native.retryClaudeSync" {
                        let result=self.request_sync_retry(r.agent,&r.client,&r.params).await;
                        let _=r.reply.send(result);continue;
                    }
                    if r.method=="native.claudeSyncStatus" {
                        let result=self.sync_retry_status(r.agent,s(&r.params["threadId"]));
                        let _=r.reply.send(result);continue;
                    }
                    // Check native receipts before any bootstrap/edit/job can
                    // create side effects under a reused dispatch command ID.
                    if r.method=="orchestration.dispatchCommand" && r.agent==AgentKind::Claude {
                        match self.cached_native_dispatch(&r).await {
                            Ok(Some(value))=>{let _=r.reply.send(Ok(value));continue;},
                            Err(error)=>{let _=r.reply.send(Err(error));continue;},
                            Ok(None)=>{},
                        }
                    }
                    claude_sends.cancel_for(&r);
                    if send_recovery::Sends::needs_wait(&self,&r) {
                        claude_sends.hold(&self,r,waiting_token);continue;
                    }
                    if self.chats.get(s(&r.params["threadId"])).is_some_and(|c|c.external_claude) && ((r.method=="claude.sessionAction" && !["connect","newWorktree"].contains(&s(&r.params["action"])) && !native_control::supported(&r.params["action"])) || r.params.get("promptEdit").is_some()) {
                        // Same outcomes as the TypeScript bridge for Desktop-owned chats.
                        let action=if r.method=="claude.sessionAction" {s(&r.params["action"])} else {""};
                        let _=r.reply.send(match action {
                            "checkpoints"=>Ok(json!({"points":[],"external":true})),
                            "branchChat"=>Err("Branching is available for chats started by Remodex. Continue this Desktop-owned chat through its existing connection.".into()),
                            "previewRestore"|"restoreFiles"=>Err("File restore is available for chats started by Remodex with checkpointing enabled.".into()),
                            _=>Err("This control is not available for a Desktop-owned Claude chat.".into()),
                        });
                        continue;
                    }
                    if r.method=="orchestration.dispatchCommand" && r.params["type"]=="thread.turn.start" && r.params.get("promptEdit").is_none() && r.params.get("bootstrap").is_none() && r.agent==AgentKind::Codex && self.config.codex.shared_server.is_some() {
                        let id=s(&r.params["threadId"]).to_owned();
                        if self.checked(r.agent,&id).is_ok_and(|c|!c.native_id.is_empty()) && !self.active.contains_key(&id) {
                            if self.tool_controls.contains(&id) || shared_sends.contains_key(&id) || shared_sends.len()>=4 {let _=r.reply.send(Err("Wait for this chat's current history check".into()));continue;}
                            self.tool_controls.insert(id.clone());
                            self.shared_watches.insert(id.clone(),(Instant::now()+Duration::from_secs(35),Instant::now()));
                            shared_send_epoch=shared_send_epoch.wrapping_add(1);
                            shared_sends.insert(id,(Instant::now(),shared_send_epoch,r));continue;
                        }
                    }
                    if r.method=="orchestration.dispatchCommand" {
                        previews.retain(|_,p|p.chat!=s(&r.params["threadId"]));
                    }
                    if r.method=="orchestration.dispatchCommand" && r.agent==AgentKind::Claude
                        && ["thread.runtime-mode.set","thread.interaction-mode.set"].contains(&s(&r.params["type"]))
                        && (self.chats.get(s(&r.params["threadId"])).is_some_and(|c|c.external_claude)
                            || self.active.get(s(&r.params["threadId"])).is_some_and(|w|w.claude_router.is_some())) {
                        match self.prepare_access_control(&r).await {
                            Ok(PreparedTool::Run(key,digest,action))=>self.start_native_control(r,key,digest,action).await,
                            Ok(PreparedTool::Cached(value))=>{let result=crate::native_control::response(value).map(|_|json!({"sequence":self.seq}));let _=r.reply.send(result);},
                            Err(error)=>{let _=r.reply.send(Err(error));},
                        }
                        continue;
                    }
                    // A shared session's model/effort is confirmed by its own
                    // worker before the unchanged prompt path may deliver.
                    if r.method=="orchestration.dispatchCommand" && r.agent==AgentKind::Claude && ["thread.turn.start","thread.turn.queue.steer"].contains(&s(&r.params["type"]))
                        && self.chats.get(s(&r.params["threadId"])).is_some_and(|c|c.external_claude) {
                        match self.prepare_shared_settings(&r).await {
                            Ok(Some(plan))=>{self.start_shared_settings(r,plan).await;continue;},
                            Ok(None)=>{},
                            Err(error)=>{let _=r.reply.send(Err(error));continue;},
                        }
                    }
                    if r.method=="orchestration.dispatchCommand" && r.params["type"]=="thread.turn.start" && (r.params["bootstrap"].get("prepareWorktree").is_some() || r.params["bootstrap"]["createThread"]["worktreePath"].is_string() || r.params["bootstrap"]["createThread"]["branch"].is_string()) {
                        let id=s(&r.params["threadId"]).to_owned();
                        match self.prepare_bootstrap(&r).await {
                            Ok(None)=>{let _=r.reply.send(Ok(json!({"sequence":self.seq})));},
                            Ok(Some(p))=>{
                                if bootstrap_jobs.len()+worktree_jobs.len()>=2 {let _=r.reply.send(Err("Worktree preparation is busy; try again shortly".into()));continue;}
                                self.tool_controls.insert(id);let db=self.db.clone();let base=self.config.workspace.clone();
                                bootstrap_jobs.spawn(async move {let root=p.setup.project_root.clone();let result=crate::worktrees::prepare(db,base,p.source,p.key,p.digest,p.setup,p.target).await.map(|chat|(chat,root));(r,result)});
                            },
                            Err(e)=>{let _=r.reply.send(Err(e));},
                        }
                        continue;
                    }
                    if r.method=="orchestration.dispatchCommand" && r.params["type"]=="thread.turn.start" && r.params.get("promptEdit").is_some() {
                        let id=s(&r.params["threadId"]).to_owned();
                        match self.prepare_edit(&r).await {
                            Ok(None)=>{let _=r.reply.send(Ok(json!({"sequence":self.seq})));},
                            Ok(Some(target))=>{
                                if edit_jobs.len()>=2 {let _=r.reply.send(Err("Two edits are already being prepared; try again shortly".into()));continue;}
                                self.tool_controls.insert(id.clone());let chat=self.chats[&id].clone();let config=self.config.provider(r.agent).clone();
                                edit_jobs.spawn(async move {let result=crate::editing::prepare(config,chat,target).await;(r,result)});
                            },
                            Err(e)=>{let _=r.reply.send(Err(e));},
                        }
                        continue;
                    }
                    if r.method=="claude.sessionAction" && ["checkpoints","previewRestore","restoreFiles"].contains(&s(&r.params["action"])) {
                        let id=s(&r.params["threadId"]).to_owned();
                        if r.agent!=AgentKind::Claude || self.checked(r.agent,&id).is_err() {let _=r.reply.send(Err("Select a Claude chat".into()));continue;}
                        if r.params["action"]=="checkpoints" {
                            let result=self.db.call(move|a|a.store.checkpoints(&id)).await.map_err(str::to_owned).and_then(|r|r.map_err(|_|"Checkpoint list unavailable".into()));
                            let _=r.reply.send(result);continue;
                        }
                        let command=s(&r.params["commandId"]);
                        if command.is_empty() || command.len()>128 {let _=r.reply.send(Err("A command ID is required".into()));continue;}
                        let key=format!("claude-restore:{}:{command}",r.client);let digest=format!("{:x}",Sha256::digest(r.params.to_string().as_bytes()));
                        if r.params["action"]=="restoreFiles" {
                            let lookup=key.clone();
                            match self.db.call(move|a|a.store.tool_action(&lookup)).await {
                                Ok(Ok(Some((old,value))))=>{let _=r.reply.send(if old==digest {Ok(value)}else{Err("Command ID was already used for different content".into())});continue;},
                                Ok(Ok(None))=>{},
                                _=>{let _=r.reply.send(Err("Restore receipt store unavailable".into()));continue;},
                            }
                        }
                        let chat=&self.chats[&id];
                        if self.active.contains_key(&id) || self.tool_controls.contains(&id) || !chat.queue.is_empty() || chat.thread["archivedAt"].is_string() {
                            let _=r.reply.send(Err("Finish the task and clear queued messages before using this control".into()));continue;
                        }
                        if file_jobs.len()>=2 {let _=r.reply.send(Err("File controls are busy; try again shortly".into()));continue;}
                        if r.params["action"]=="previewRestore" {
                            previews.retain(|_,p|p.chat!=id && p.created.elapsed()<Duration::from_secs(60));
                            if previews.len()+file_jobs.len()>=2 {let _=r.reply.send(Err("Close or finish another file preview before opening one".into()));continue;}
                            let point=s(&r.params["point"]).to_owned();let lookup=id.clone();let pid=point.clone();
                            let valid=self.db.call(move|a|a.store.has_checkpoint(&lookup,&pid)).await;
                            if !matches!(valid,Ok(Ok(true))) {let _=r.reply.send(Err("This checkpoint does not belong to the chat".into()));continue;}
                            let chat=chat.clone();let config=self.config.claude.clone();let client=r.client.clone();self.tool_controls.insert(id);
                            file_jobs.spawn(async move {let result=crate::checkpoints::preview(config,chat,client,point).await;(r,FileJob::Preview(result))});
                        } else {
                            let token=s(&r.params["token"]);
                            let valid=previews.get(token).is_some_and(|p|p.chat==id && p.client==r.client && p.created.elapsed()<Duration::from_secs(60) && p.version==crate::checkpoints::version(chat));
                            if !valid {let _=r.reply.send(Err("The restore preview expired or this chat changed. Preview it again".into()));continue;}
                            let preview=previews.remove(token).unwrap();self.tool_controls.insert(id);
                            file_jobs.spawn(async move {let result=crate::checkpoints::restore(preview).await;(r,FileJob::Restore(key,digest,result))});
                        }
                        continue;
                    }
                    if r.method=="claude.sessionAction" && ["newWorktree","branchChat"].contains(&s(&r.params["action"])) {
                        match self.prepare_worktree(&r).await {
                            Ok((key,digest,None))=>{
                                if worktree_jobs.len()+bootstrap_jobs.len()>=2 || self.chats.len()+worktree_jobs.len()+bootstrap_jobs.len()>=10000 {let _=r.reply.send(Err("Worktree creation is busy; try again shortly".into()));continue;}
                                let id=s(&r.params["threadId"]).to_owned();let source=self.chats[&id].clone();
                                let setup=self.project_setup(&source,true);
                                let base=self.config.workspace.clone();let profile=self.config.claude.profile.clone();let db=self.db.clone();self.tool_controls.insert(id);
                                worktree_jobs.spawn(async move {let result=if r.params["action"]=="branchChat" {crate::branches::create(db,profile,source,key,digest).await} else {crate::worktrees::create(db,base,source,key,digest,setup).await};(r,result)});
                            },
                            Ok((_,_,Some(value)))=>{let _=r.reply.send(Ok(value));},
                            Err(error)=>{let _=r.reply.send(Err(error));},
                        }
                        continue;
                    }
                    if r.method=="claude.sessionAction" && r.params["action"]=="connect" {
                        let result=self.connect_external_claude(r.agent,&r.params).await;
                        let _=r.reply.send(result); continue;
                    }
                    if r.method=="claude.sessionAction" {
                        if capability_waiters.contains_key(s(&r.params["threadId"])) {let _=r.reply.send(Err("Wait for the current tool check to finish".into()));continue;}
                        match self.prepare_tool_control(&r).await {
                            Ok(PreparedTool::Run(key,digest,control)) => {
                                if self.active.contains_key(s(&r.params["threadId"])) {
                                    self.start_native_control(r,key,digest,control).await;continue;
                                }
                                if tool_jobs.len()+capability_jobs.len()>=2 {let _=r.reply.send(Err("Tool controls are busy; try again shortly".into()));continue;}
                                let id=s(&r.params["threadId"]).to_owned();
                                let chat=&self.chats[&id];let cwd=PathBuf::from(s(&chat.thread["worktreePath"]));let disabled=chat.disabled_mcp.clone();
                                let config=self.config.claude.clone();self.tool_controls.insert(id.clone());idle_capabilities.remove(&id);
                                tool_jobs.spawn(async move {let result=crate::capabilities::probe(config,cwd,disabled,Some(control.clone())).await;(r,key,digest,control,result)});
                            },
                            Ok(PreparedTool::Cached(value))=>{let _=r.reply.send(crate::native_control::response(value));},
                            Err(error)=>{let _=r.reply.send(Err(error));},
                        }
                        continue;
                    }
                    if r.method=="claude.getCapabilities" {
                        let id=s(&r.params["threadId"]).to_owned();
                        if r.agent!=AgentKind::Claude || self.checked(r.agent,&id).is_err() {
                            let _=r.reply.send(Err("Select a Claude chat".into()));continue;
                        }
                        if self.tool_controls.contains(&id) {let _=r.reply.send(Err("Wait for this chat's tool control to finish".into()));continue;}
                        if self.active.contains_key(&id) {idle_capabilities.remove(&id);self.read_capabilities(r).await;continue;}
                        if self.chats[&id].external_claude {let _=r.reply.send(Ok(crate::capabilities::unavailable()));continue;}
                        if let Some((_,v))=idle_capabilities.get(&id).filter(|(at,_)|at.elapsed()<Duration::from_secs(10)) {
                            let _=r.reply.send(Ok(v.clone()));continue;
                        }
                        if let Some(waiters)=capability_waiters.get_mut(&id) {
                            waiters.retain(|r|!r.is_closed());
                            if waiters.len()<4 {waiters.push(r.reply);}else{let _=r.reply.send(Err("Claude tools are already being checked".into()));}
                            continue;
                        }
                        if capability_jobs.len()+tool_jobs.len()>=2 {let _=r.reply.send(Err("Claude tools are busy; try again shortly".into()));continue;}
                        let cwd=PathBuf::from(s(&self.chats[&id].thread["worktreePath"]));
                        let config=self.config.claude.clone();
                        let disabled=self.chats[&id].disabled_mcp.clone();
                        capability_waiters.insert(id.clone(),vec![r.reply]);
                        capability_jobs.spawn(async move {(id,crate::capabilities::inspect(config,cwd,disabled).await)});
                        continue;
                    }
                    if r.method=="sourceControl.cloneRepository" {
                        if r.agent!=AgentKind::Codex {let _=r.reply.send(Err("Repository cloning is not available for managed Claude chats".into()));continue;}
                        if !clone_jobs.is_empty() {let _=r.reply.send(Err("A repository clone is already running; retry shortly".into()));continue;}
                        let base=self.config.workspace.clone();let db=self.db.clone();
                        clone_jobs.spawn(async move {
                            let result=crate::repository_clone::run(db,&base,r.params).await;
                            let _=r.reply.send(result);
                        });continue;
                    }
                    if ["vcs.refreshStatus","vcs.listRefs","review.getDiffPreview"].contains(&r.method.as_str()) {
                        if inspections.len()>=2 {let _=r.reply.send(Err("Git inspection busy; try again shortly".into()));continue;}
                        let base=self.config.workspace.clone();
                        inspections.spawn(async move {
                            let result=tokio::time::timeout(Duration::from_secs(30),crate::vcs::inspect(&base,&r.method,&r.params)).await.unwrap_or_else(|_|Err("Git inspection timed out".into()));
                            let _=r.reply.send(result);
                        });
                        continue;
                    }
                    let result=self.request(r.agent,&r.client,&r.method,r.params).await;
                    let _=r.reply.send(result);
                }
                Some(result)=shared_jobs.join_next(), if !shared_jobs.is_empty()=>{
                    if let Ok((id,native,send_epoch,result))=result {
                        // A background read started before a send cannot authorize it.
                        // Match the exact queued request, including after timeout/retry.
                        let current_send=send_epoch.is_some_and(|epoch|shared_sends.get(&id).is_some_and(|(_,current,_)|epoch==*current));
                        shared_reads.remove(&id);
                        let valid=self.chats.get(&id).is_some_and(|c|c.native_id==native && c.thread["deletedAt"].is_null() && c.thread["archivedAt"].is_null()) && !self.active.contains_key(&id) && (!self.tool_controls.contains(&id) || shared_sends.contains_key(&id));
                        if !valid {continue;}
                        let result=match result {Ok(batch)=>{let tail=batch.caught_up;let busy=batch.busy;let external=batch.turn.as_ref().filter(|t|t["status"]=="inProgress").map(|t|s(&t["id"]).to_owned());
                            if batch.missing_empty_session && !current_send {continue;}
                            match self.apply_shared(&id,batch).await {Ok(())=>{if tail && busy && (current_send || !shared_sends.contains_key(&id)) {if let Some(turn)=external {let _=self.attach_shared(&id,turn).await;}}Ok((tail,busy))},Err(e)=>Err(e)}},Err(e)=>Err(e)};
                        if let Some((_,due))=self.shared_watches.get_mut(&id){*due=Instant::now()+Duration::from_millis(if (shared_sends.contains_key(&id) && !current_send) || matches!(result,Ok((false,_))) {10} else if result.is_ok(){750}else{5000});}
                        let finish=match &result {Ok((tail,_))=>*tail,Err(_)=>true};
                        if finish && current_send {
                            if let Some((_,_,r))=shared_sends.remove(&id) {
                                self.tool_controls.remove(&id);
                                let result=match result {Ok((_,false))=>self.dispatch(r.agent,&r.client,r.params).await,Ok((_,true)) if self.active.contains_key(&id) && r.params["deliveryMode"]=="queue"=>self.dispatch(r.agent,&r.client,r.params).await,Ok((_,true))=>Err("This shared chat is running in Desktop. Wait for it to finish before sending another prompt.".into()),Err(e)=>Err(e)};
                                let _=r.reply.send(result);
                            }
                        }
                    }
                },
                _=inspections.join_next(), if !inspections.is_empty()=>{},
                _=clone_jobs.join_next(), if !clone_jobs.is_empty()=>{},
                Some(result)=bootstrap_jobs.join_next(), if !bootstrap_jobs.is_empty()=>{
                    if let Ok((r,result))=result {
                        let id=s(&r.params["threadId"]).to_owned();self.tool_controls.remove(&id);
                        let result=match result {
                            Ok(chat)=>{self.prepared_bootstraps.insert(id.clone(),chat);self.dispatch(r.agent,&r.client,r.params).await},
                            Err(e)=>Err(e),
                        };
                        self.prepared_bootstraps.remove(&id);let _=r.reply.send(result);
                    }
                },
                Some(result)=edit_jobs.join_next(), if !edit_jobs.is_empty()=>{
                    if let Ok((r,result))=result {
                        let id=s(&r.params["threadId"]).to_owned();self.tool_controls.remove(&id);
                        let result=match result {
                            Ok(prepared)=>{self.prepared_edits.insert(id.clone(),prepared);self.dispatch(r.agent,&r.client,r.params).await},
                            Err(e)=>Err(e),
                        };
                        self.prepared_edits.remove(&id);
                        let _=r.reply.send(result);
                    }
                },
                Some(result)=file_jobs.join_next(), if !file_jobs.is_empty()=>{
                    if let Ok((r,result))=result {
                        let id=s(&r.params["threadId"]).to_owned();self.tool_controls.remove(&id);
                        let result=match result {
                            FileJob::Preview(result)=>result.map(|(preview,mut value)|{
                                if let Some(preview)=preview {let token=uuid::Uuid::new_v4().to_string();value["token"]=json!(token);previews.insert(token,preview);}value
                            }),
                            FileJob::Restore(key,digest,result)=>match result {Ok(value)=>self.finish_restore(&id,&key,&digest,value).await,Err(e)=>Err(e)},
                        };
                        let _=r.reply.send(result);
                    }
                },
                Some(result)=worktree_jobs.join_next(), if !worktree_jobs.is_empty()=>{
                    if let Ok((r,result))=result {
                        self.tool_controls.remove(s(&r.params["threadId"]));
                        let result=result.map(|(chat,value)|{self.chats.insert(s(&chat.thread["id"]).into(),chat);self.dirty=true;value});
                        let _=r.reply.send(result);
                    }
                },
                Some(result)=tool_jobs.join_next(), if !tool_jobs.is_empty()=>{
                    if let Ok((r,key,digest,control,result))=result {
                        let id=s(&r.params["threadId"]).to_owned();self.tool_controls.remove(&id);idle_capabilities.remove(&id);
                        let result=self.finish_tool_control(&id,&key,&digest,&control,result).await;
                        let _=r.reply.send(result);
                    }
                },
                Some(result)=capability_jobs.join_next(), if !capability_jobs.is_empty()=>{
                    if let Ok((id, mut result))=result {
                        if self.active.contains_key(&id) {result=Ok(crate::capabilities::unavailable());}
                        else if let Ok(v)=&result {
                            idle_capabilities.retain(|_,(at,_)|at.elapsed()<Duration::from_secs(10));
                            if idle_capabilities.len()<64 {idle_capabilities.insert(id.clone(),(Instant::now(),v.clone()));}
                        }
                        for waiter in capability_waiters.remove(&id).unwrap_or_default(){let _=waiter.send(result.clone());}
                    }
                },
                Some(result)=discovery.join_next(), if !discovery.is_empty()=>{
                    if let Ok((agent, result)) = result {
                        match result {
                            Ok(models) => {
                                self.models.insert(agent.instance().into(), models);
                                self.discovery_errors.remove(agent.instance());
                                let data=serde_json::to_string(&self.models).unwrap();
                                let _=self.db.call(move |a|a.store.db.execute("INSERT INTO metadata(key,value) VALUES('native.models',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[data])).await;
                            },
                            Err(error) => {self.discovery_errors.insert(agent.instance().into(),error);}
                        }
                        self.dirty=true;
                    }
                },
                result=async {match self.usage_task.as_mut(){Some(task)=>task.await,None=>std::future::pending().await}}=>{
                    self.usage_task=None;
                    let value=result.ok().flatten();
                    self.usage_cache=Some((Instant::now(),value.clone()));
                    for id in std::mem::take(&mut self.usage_reads){
                        let _=self.report_usage(&id,false,value.clone()).await;
                    }
                },
                (id,event)=async {
                    if self.active.is_empty(){std::future::pending().await} else {
                        let mut waits: Vec<_>=self.active.iter_mut().map(|(id,w)|Box::pin(async move {
                            if !w.claude_background.busy() {
                                if let Some(v)=w.claude_deferred_result.take(){return (id.clone(),Some(Event::Message(v)));}
                            }
                            if (w.stage=="running" && !w.native_turn.is_empty()) || w.stage=="prompt" {
                                if let Some((v,bytes))=w.pre_turn_events.pop_front(){w.pre_turn_bytes-=bytes;return (id.clone(),Some(Event::Message(v)));}
                            }
                            (id.clone(),w.connection.events.recv().await)
                        })).collect();
                        let offset=poll_cursor % waits.len();waits.rotate_left(offset);
                        futures_util::future::select_all(waits).await.0
                    }
                }=>{
                    poll_cursor=poll_cursor.wrapping_add(1);
                    let result=self.event(&id,event).await;
                    if let Err(error)=result {self.end(&id,"error",Some(&error)).await;}
                }
                _=tick.tick()=>{
                    if matches!(restart_gate.load(Ordering::Acquire),1|2) {continue;}
                    self.finish_recovery_checks().await;
                    self.expire_native_controls().await;
                    if !restart_pending {self.reconnect_claude().await;}
                    previews.retain(|_,p|p.created.elapsed()<Duration::from_secs(60));
                    let expired:Vec<_>=shared_sends.iter().filter(|(_, (at,_,r))|at.elapsed()>Duration::from_secs(30) || r.reply.is_closed()).map(|(id,_)|id.clone()).collect();
                    for id in expired {if let Some((_,_,r))=shared_sends.remove(&id){self.tool_controls.remove(&id);let _=r.reply.send(Err("Shared history is still catching up. Your prompt was not sent; try again shortly.".into()));}}
                    if !self.active.is_empty() && storage_checked.elapsed() >= Duration::from_secs(2) {
                        storage_checked=Instant::now();
                        if let Err(e)=self.db.call(|a|a.store.check_history(0)).await.map_err(str::to_owned).and_then(|r|r) {
                            let ids:Vec<_>=self.active.keys().cloned().collect();
                            for id in ids {self.end(&id,"error",Some(&e)).await;}
                        }
                    }
                    let mut expired_context = vec![];
                    for (id,w) in self.active.iter_mut() {
                        let (ids,context) = w.metadata.expire(Instant::now());
                        if context {expired_context.push(id.clone());}
                        if w.observer.is_some() {
                            for request in ids {
                                let _=w.connection.send(&json!({"type":"remodex_observer_forget_read","request_id":request})).await;
                            }
                        }
                    }
                    for id in expired_context {let _=self.report_usage(&id,true,None).await;}
                    let detached:Vec<_>=self.active.iter().filter(|(id,w)|!w.owns_turn && w.observer.is_none() && !self.shared_watches.contains_key(*id) && self.chats[*id].queue.is_empty() && w.steers.values().all(|v|*v)).map(|(id,_)|id.clone()).collect();
                    for id in detached {self.end(&id,"interrupted",Some("Shared viewer disconnected")).await;}
                    if !restart_pending && self.active.is_empty() && Instant::now() >= retention_due {
                        let mut scan=std::mem::take(&mut retention);
                        if let Ok((scan,result))=self.db.call(move |a| {
                            let result=scan.step(&a.store,std::time::SystemTime::now());(scan,result)
                        }).await {
                            let complete=result.is_ok_and(|r|r.complete);
                            retention=if complete {crate::retention::Scanner::default()}else{scan};
                            retention_due=Instant::now()+Duration::from_secs(if complete{3600}else{30});
                        } else {retention_due=Instant::now()+Duration::from_secs(60);}
                    }
                    let expired:Vec<String>=self.active.iter().filter(|(_,w)|w.deadline.is_some_and(|d|Instant::now()>d)).map(|(id,_)|id.clone()).collect();
                    for id in expired {
                        self.end(&id,"error",Some("The CLI did not acknowledge its control request. The prompt was not retried.")).await;
                    }
                    if self.dirty {self.publish();}
                }
            }
        }
        claude_sends.shutdown();
        shared_jobs.abort_all();
        while shared_jobs.join_next().await.is_some() {}
        for (_, (_, _, r)) in shared_sends {
            let _ = r.reply.send(Err(
                "Native chat service stopped; prompt was not sent".into()
            ));
        }
        inspections.abort_all();
        while inspections.join_next().await.is_some() {}
        clone_jobs.abort_all();
        while clone_jobs.join_next().await.is_some() {}
        capability_jobs.abort_all();
        while capability_jobs.join_next().await.is_some() {}
        tool_jobs.abort_all();
        while tool_jobs.join_next().await.is_some() {}
        worktree_jobs.abort_all();
        while worktree_jobs.join_next().await.is_some() {}
        bootstrap_jobs.abort_all();
        while bootstrap_jobs.join_next().await.is_some() {}
        edit_jobs.abort_all();
        while edit_jobs.join_next().await.is_some() {}
        file_jobs.abort_all();
        while file_jobs.join_next().await.is_some() {}
        previews.clear();
        discovery.abort_all();
        while discovery.join_next().await.is_some() {}
        if let Some(task) = self.usage_task.take() {
            task.abort();
            let _ = task.await;
        }
        self.shutdown().await;
    }
    async fn shutdown(&mut self) {
        // A blocking history copy must finish before releasing the profile
        // lease. It can commit a resumable file, but never launch a CLI here.
        for (id, check) in std::mem::take(&mut self.recovery_checks) {
            let _ = check.task.await;
            self.tool_controls.remove(&id);
        }
        for id in self.active.keys().cloned().collect::<Vec<_>>() {
            self.end(&id, "interrupted", Some("Test bridge stopped"))
                .await;
        }
    }
    async fn prepare_tool_control(&self, r: &Request) -> Result<PreparedTool, String> {
        if r.agent != AgentKind::Claude {
            return Err("Select a Claude chat".into());
        }
        let id = s(&r.params["threadId"]);
        let chat = self.checked(r.agent, id)?;
        let command = s(&r.params["commandId"]);
        if command.is_empty() || command.len() > 128 {
            return Err("A command ID is required".into());
        }
        let control = crate::capabilities::Control::parse(&r.params)?;
        let key = format!("claude-tool:{}:{command}", r.client);
        let digest = format!("{:x}", Sha256::digest(r.params.to_string().as_bytes()));
        let lookup = key.clone();
        if let Some((prior, value)) = self
            .db
            .call(move |a| a.store.tool_action(&lookup))
            .await?
            .map_err(|_| "Tool control store unavailable")?
        {
            return if prior == digest {
                Ok(PreparedTool::Cached(value))
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        if self
            .active
            .get(id)
            .is_some_and(|w| w.stage != "idle" || !w.metadata_ready())
            || self.tool_controls.contains(id)
            || !chat.queue.is_empty()
        {
            return Err(
                "Finish the task and clear queued messages before using this control".into(),
            );
        }
        if chat.external_claude && !self.active.contains_key(id) {
            return Err("Reconnect the original Claude session before using this control".into());
        }
        if chat.thread["deletedAt"].is_string() || chat.thread["archivedAt"].is_string() {
            return Err("Select an active chat".into());
        }
        if let crate::capabilities::Control::Toggle {
            name,
            enabled: false,
        } = &control
        {
            if chat.disabled_mcp.len() >= 128 && !chat.disabled_mcp.contains(name) {
                return Err("Too many per-chat MCP overrides".into());
            }
        }
        Ok(PreparedTool::Run(key, digest, control))
    }
    async fn finish_restore(
        &mut self,
        id: &str,
        key: &str,
        digest: &str,
        value: Value,
    ) -> Result<Value, String> {
        let mut chat = self
            .chats
            .get(id)
            .ok_or("Chat unavailable after file restore")?
            .clone();
        projection::activity(
            &mut chat,
            &uuid::Uuid::new_v4().to_string(),
            "session.notice",
            "Tracked files restored",
            "info",
            json!({"detail":"Restored Claude-tracked changes. Conversation history and Bash changes are kept.","skippedLinks":value["skippedLinks"]}),
        );
        let (key, digest, saved) = (key.to_owned(), digest.to_owned(), value.clone());
        let state = serde_json::to_value(&chat).unwrap();
        self.db.call(move|a|a.store.save_tool_action(&state,&key,&digest,&saved)).await?.map_err(|_|"Files were restored, but the receipt could not be saved. Inspect files before retrying")?;
        self.chats.insert(id.into(), chat);
        self.dirty = true;
        Ok(value)
    }
    async fn prepare_worktree(
        &self,
        r: &Request,
    ) -> Result<(String, String, Option<Value>), String> {
        if r.agent != AgentKind::Claude {
            return Err("Select a Claude chat".into());
        }
        let id = s(&r.params["threadId"]);
        let chat = self.checked(r.agent, id)?;
        let command = s(&r.params["commandId"]);
        if command.is_empty() || command.len() > 128 {
            return Err("A command ID is required".into());
        }
        let key = format!(
            "{}:{}:{command}",
            if r.params["action"] == "branchChat" {
                "claude-branch"
            } else {
                "claude-worktree"
            },
            r.client
        );
        let digest = format!("{:x}", Sha256::digest(r.params.to_string().as_bytes()));
        let lookup = key.clone();
        if let Some((old, value)) = self
            .db
            .call(move |a| a.store.tool_action(&lookup))
            .await?
            .map_err(|_| "Worktree store unavailable")?
        {
            return if old == digest {
                Ok((key, digest, Some(value)))
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        // A Desktop-owned chat's idle connection is not a task. A new isolated
        // chat starts from its repository and never touches that session.
        let busy = if chat.external_claude {
            self.busy(id)
                || chat.thread["session"]["status"] == "running"
                || chat.thread["hasPendingApprovals"] == true
                || chat.thread["hasPendingUserInput"] == true
        } else {
            self.active.contains_key(id)
        };
        if busy || self.tool_controls.contains(id) || !chat.queue.is_empty() {
            return Err(
                "Finish the task and clear queued messages before using this control".into(),
            );
        }
        if chat.thread["deletedAt"].is_string() || chat.thread["archivedAt"].is_string() {
            return Err("Select an active chat".into());
        }
        if r.params["action"] == "branchChat"
            && (!chat.resumed || chat.thread["latestUserMessageAt"].is_null())
        {
            return Err("Send a message in an active chat before branching it".into());
        }
        self.db
            .call(|a| a.store.check_history(1024 * 1024))
            .await??;
        Ok((key, digest, None))
    }
    async fn finish_tool_control(
        &mut self,
        id: &str,
        key: &str,
        digest: &str,
        control: &crate::capabilities::Control,
        result: Result<crate::capabilities::Probe, String>,
    ) -> Result<Value, String> {
        let probe = result?;
        let mut chat = self
            .chats
            .get(id)
            .ok_or("This chat is no longer available")?
            .clone();
        if let crate::capabilities::Control::Toggle { name, enabled } = control {
            chat.disabled_mcp.retain(|s| s != name);
            if !enabled {
                chat.disabled_mcp.push(name.clone());
                chat.disabled_mcp.sort();
            }
        }
        let state = serde_json::to_value(&chat).map_err(|_| "Cannot save tool state")?;
        let key = key.to_owned();
        let digest = digest.to_owned();
        let value = probe.result.clone();
        self.db
            .call(move |a| a.store.save_tool_action(&state, &key, &digest, &value))
            .await?
            .map_err(|_| "Could not save this chat's tool settings")?;
        self.chats.insert(id.into(), chat);
        self.dirty = true;
        Ok(probe.result)
    }
    fn project_setup(&self, chat: &Chat, enabled: bool) -> crate::worktrees::Setup {
        let project = self
            .projects
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == chat.thread["projectId"]);
        // The existing runtime selects the first script marked for setup.
        let scripts = if enabled {
            project
                .and_then(|p| p["scripts"].as_array())
                .into_iter()
                .flatten()
                .filter(|s| s["runOnWorktreeCreate"] == true)
                .take(1)
                .cloned()
                .collect()
        } else {
            vec![]
        };
        let project_root = PathBuf::from(
            project
                .and_then(|p| p["workspaceRoot"].as_str())
                .unwrap_or_else(|| s(&chat.thread["worktreePath"])),
        );
        crate::worktrees::Setup {
            project_root,
            scripts,
        }
    }
    async fn prepare_bootstrap(&self, r: &Request) -> Result<Option<PreparedBootstrap>, String> {
        let c = &r.params;
        let id = s(&c["threadId"]);
        let command = s(&c["commandId"]);
        if id.is_empty() || id.len() > 128 || command.is_empty() || command.len() > 128 {
            return Err("Command and chat IDs required".into());
        }
        let key = format!("{}:{}:{command}", r.agent.instance(), r.client);
        let digest = format!("{:x}", Sha256::digest(c.to_string().as_bytes()));
        let lookup = key.clone();
        if let Some(old) = self
            .db
            .call(move |a| a.store.command_digest(&lookup))
            .await?
            .map_err(|_| "Command store unavailable")?
        {
            return if old == digest {
                Ok(None)
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        if self.tool_controls.contains(id) {
            return Err("This chat is already being prepared".into());
        }
        if self.busy_count() >= self.config.max_active_chats {
            return Err("Wait for a free chat worker before preparing this chat".into());
        }
        let b = &c["bootstrap"];
        if !b.is_object()
            || !b["createThread"].is_object()
            || b.get("prepareWorktree").is_some_and(|v| !v.is_object())
            || b.as_object().unwrap().keys().any(|k| {
                !["createThread", "prepareWorktree", "runSetupScript"].contains(&k.as_str())
            })
            || b.get("runSetupScript").is_some_and(|v| !v.is_boolean())
            || c.get("promptEdit").is_some()
            || c.get("sourceProposedPlan").is_some()
            || c["deliveryMode"].as_str().is_some_and(|m| m != "normal")
        {
            return Err("Create a worktree with a normal first prompt".into());
        }
        let mut create = b["createThread"].clone();
        let target = if let Some(spec) = b.get("prepareWorktree") {
            if create["worktreePath"].is_string() || create["branch"].is_string() {
                return Err("Choose either a new or an existing worktree".into());
            }
            crate::worktrees::Target::Create(Some(spec.clone()))
        } else {
            for field in ["worktreePath", "branch"] {
                if create.get(field).is_some_and(|v| {
                    !v.is_null()
                        && v.as_str().is_none_or(|s| {
                            s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)
                        })
                }) {
                    return Err("Invalid existing worktree selection".into());
                }
            }
            let path = create["worktreePath"].as_str().unwrap_or("").to_owned();
            let branch = create["branch"].as_str().map(str::to_owned);
            create["worktreePath"] = Value::Null;
            create["branch"] = Value::Null;
            crate::worktrees::Target::Existing { path, branch }
        };
        let source = self.new_chat(r.agent, id, &create).await?;
        if source.thread["workspaceKind"] == "projectless" {
            return Err("Select a registered Git project for a worktree".into());
        }
        let mid = s(&c["message"]["messageId"]);
        if c["message"]["text"].as_str().is_none_or(|t| {
            t.len() > 32000
                || (t.trim().is_empty()
                    && c["message"]["attachments"]
                        .as_array()
                        .is_none_or(|a| a.is_empty()))
        }) || mid.is_empty()
            || mid.len() > 128
            || c.get("titleSeed").is_some_and(|v| {
                !v.is_null() && v.as_str().is_none_or(|t| t.is_empty() || t.len() > 256)
            })
        {
            return Err("A bounded first prompt and message ID are required".into());
        }
        if c["runtimeMode"].as_str().is_some_and(|v| {
            !["approval-required", "full-access", "auto-accept-edits"].contains(&v)
        }) || c["interactionMode"]
            .as_str()
            .is_some_and(|v| !["default", "plan"].contains(&v))
        {
            return Err("Unsupported chat settings".into());
        }
        let catalog = self
            .models
            .get(r.agent.instance())
            .cloned()
            .unwrap_or_else(|| crate::models::fallback(&self.config.provider(r.agent).model));
        crate::models::selection(
            &catalog,
            &c["modelSelection"],
            &source.thread["modelSelection"],
            r.agent.instance(),
        )?;
        let uploads = c["message"]
            .get("attachments")
            .cloned()
            .unwrap_or(json!([]));
        self.db
            .call(move |a| {
                a.store.check_history(1024 * 1024)?;
                a.store.preflight_uploads(&uploads)
            })
            .await??;
        let setup = self.project_setup(&source, b["runSetupScript"] == true);
        let target = match target {
            crate::worktrees::Target::Existing { path, branch } if path.is_empty() => {
                crate::worktrees::Target::Existing {
                    path: s(&source.thread["worktreePath"]).to_owned(),
                    branch,
                }
            }
            target => target,
        };
        Ok(Some(PreparedBootstrap {
            key,
            digest,
            source,
            setup,
            target,
        }))
    }
    async fn prepare_edit(&self, r: &Request) -> Result<Option<crate::editing::Target>, String> {
        let c = &r.params;
        let id = s(&c["threadId"]);
        let command = s(&c["commandId"]);
        if command.is_empty() || command.len() > 128 || id.is_empty() || id.len() > 128 {
            return Err("Command and chat IDs required".into());
        }
        let key = format!("{}:{}:{command}", r.agent.instance(), r.client);
        let digest = format!("{:x}", Sha256::digest(c.to_string().as_bytes()));
        if let Some(saved) = self
            .db
            .call(move |a| a.store.command_digest(&key))
            .await?
            .map_err(|_| "Command store unavailable")?
        {
            return if saved == digest {
                Ok(None)
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        let chat = self.checked(r.agent, id)?;
        if self.active.contains_key(id)
            || self.tool_controls.contains(id)
            || !chat.queue.is_empty()
            || chat.thread["archivedAt"].is_string()
        {
            return Err("Stop the task and clear queued messages before editing a prompt".into());
        }
        if c["deliveryMode"].as_str().is_some_and(|m| m != "normal")
            || c.get("bootstrap").is_some()
            || c.get("sourceProposedPlan").is_some()
        {
            return Err("Send prompt edits as a normal message in the original chat".into());
        }
        if c["message"]["text"]
            .as_str()
            .is_none_or(|s| s.len() > 32000)
            || s(&c["message"]["messageId"]).is_empty()
        {
            return Err("A bounded replacement prompt and message ID are required".into());
        }
        if self.busy_count() >= self.config.max_active_chats {
            return Err("Wait for a free chat worker before editing".into());
        }
        if s(&c["message"]["messageId"]).len() > 128
            || (s(&c["message"]["text"]).trim().is_empty()
                && c["message"]["attachments"]
                    .as_array()
                    .is_none_or(|a| a.is_empty()))
        {
            return Err("A replacement prompt and message ID are required".into());
        }
        if c["runtimeMode"].as_str().is_some_and(|m| {
            !["approval-required", "full-access", "auto-accept-edits"].contains(&m)
        }) || c["interactionMode"]
            .as_str()
            .is_some_and(|m| !["default", "plan"].contains(&m))
        {
            return Err("Unsupported chat settings".into());
        }
        let catalog = self
            .models
            .get(r.agent.instance())
            .cloned()
            .unwrap_or_else(|| crate::models::fallback(&self.config.provider(r.agent).model));
        crate::models::selection(
            &catalog,
            &c["modelSelection"],
            &chat.thread["modelSelection"],
            r.agent.instance(),
        )?;
        let uploads = c["message"]
            .get("attachments")
            .cloned()
            .unwrap_or(json!([]));
        self.db
            .call(move |a| a.store.preflight_uploads(&uploads))
            .await??;
        let lookup = id.to_owned();
        let mid = s(&c["message"]["messageId"]).to_owned();
        if self
            .db
            .call(move |a| a.store.history_row(&lookup, "messages", &mid))
            .await?
            .map_err(|_| "History unavailable")?
            .is_some()
        {
            return Err("Message ID already accepted".into());
        }
        let target = s(&c["promptEdit"]["targetTurnId"]);
        if target.is_empty()
            || target.len() > 128
            || c["promptEdit"]["turnCount"].as_u64().is_none()
        {
            return Err("Select a prompt with a valid history position".into());
        }
        let latest_only = r.agent == AgentKind::Codex;
        if latest_only && chat.thread["latestTurn"]["turnId"] != target {
            return Err(
                "Only the latest Codex prompt can be edited. History was not changed.".into(),
            );
        }
        let id = id.to_owned();
        let target = target.to_owned();
        self.db
            .call(move |a| {
                if latest_only {
                    a.store.require_latest_editable_prompt(&id, &target)?;
                }
                a.store.edit_target(&id, &target)
            })
            .await?
            .map(Some)
    }
    async fn read_capabilities(&mut self, r: Request) {
        let id = s(&r.params["threadId"]);
        if r.agent != AgentKind::Claude || self.checked(r.agent, id).is_err() {
            let _ = r.reply.send(Err("Select a Claude chat".into()));
            return;
        }
        let Some(w) = self.active.get_mut(id).filter(|w| w.metadata_ready()) else {
            let _ = r.reply.send(Ok(crate::capabilities::unavailable()));
            return;
        };
        if let Some((_, cache)) = w
            .metadata
            .capability_cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(2))
        {
            let _ = r.reply.send(Ok(cache.clone()));
            return;
        }
        if let Some(read) = w.metadata.capability_read.as_mut() {
            read.replies.retain(|r| !r.is_closed());
            if read.deadline.is_none() || read.replies.len() >= 4 {
                let _ = r.reply.send(Err(
                    "Claude tools are still being checked; try again shortly".into(),
                ));
            } else {
                read.replies.push(r.reply);
            }
            return;
        }
        let rid = format!("capabilities-{}", uuid::Uuid::new_v4());
        // Remote Control currently returns a placeholder empty mcp_status and
        // redacts MCP/plugin lists from init. Context reports actual tool names,
        // but not connection status. Never substitute another CLI's catalog.
        let context_catalog = w.observer.is_some();
        let request = if context_catalog {
            json!({"subtype":"get_context_usage","detail":"summary"})
        } else {
            json!({"subtype":"mcp_status"})
        };
        if w.connection
            .send(&json!({"type":"control_request","request_id":rid,"request":request}))
            .await
            .is_err()
        {
            let _ = r
                .reply
                .send(Err("Claude tools are temporarily unavailable".into()));
            return;
        }
        w.metadata.capability_read = Some(metadata::CapabilityRead {
            id: rid,
            context_catalog,
            deadline: Some(Instant::now() + Duration::from_secs(5)),
            replies: vec![r.reply],
        });
    }
    fn publish(&mut self) {
        // Idle chats retain metadata and queue state, not a transcript page per
        // conversation. Opening one loads only that chat's bounded recent page.
        for (id, chat) in &mut self.chats {
            if self.active.get(id).is_none_or(|w| w.stage == "idle") {
                for kind in ["messages", "activities", "proposedPlans"] {
                    chat.thread[kind] = json!([]);
                }
                chat.stream_id.clear();
                chat.stream_model.clear();
            }
        }
        self.seq += 1;
        self.changes.send_replace(self.seq);
        self.dirty = false;
    }
    async fn save(&mut self, id: &str, receipt: Option<(String, String)>) -> Result<(), String> {
        let data = serde_json::to_value(&self.chats[id]).map_err(|_| "Invalid chat state")?;
        let key = id.to_owned();
        let saved = self
            .db
            .call(move |a| {
                a.store.save_chat(
                    &data,
                    receipt.as_ref().map(|(k, d)| (k.as_str(), d.as_str())),
                )?;
                a.store.load_chat(&key)
            })
            .await?
            .map_err(|_| "Could not save chat history")?;
        let chat = self.chats.get_mut(id).unwrap();
        chat.thread = saved["thread"].clone();
        Ok(())
    }
    fn checked(&self, agent: AgentKind, id: &str) -> Result<&Chat, String> {
        self.chats
            .get(id)
            .filter(|c| c.agent == agent && c.thread["deletedAt"].is_null())
            .ok_or_else(|| "Chat not found for this agent".into())
    }
    fn config(&self, agent: AgentKind) -> Value {
        let p = self.config.provider(agent);
        let title = if agent == AgentKind::Codex {
            "Codex"
        } else {
            "Claude Code"
        };
        let error = self.discovery_errors.get(agent.instance());
        let known = self.models.contains_key(agent.instance());
        let enabled = agent != AgentKind::Claude || self.claude_enabled;
        json!({"cwd":self.config.workspace,"keybindingsConfigPath":"","keybindings":[],"issues":[],"availableEditors":[],
        "providers":[{"instanceId":agent.instance(),"driver":agent.driver(),"displayName":title,"enabled":enabled,"installed":true,"status":if !enabled{"disabled"}else if error.is_some(){"error"}else if known{"ready"}else{"unknown"},"version":null,"auth":{"status":"unknown"},"checkedAt":now(),"availability":if enabled{"available"}else{"unavailable"},"showInteractionModeToggle":true,"message":error.map(String::as_str).unwrap_or(if known {"Model list loaded. Account access is checked when a request is sent."}else{"Checking the isolated CLI's available models..."}),"models":self.models.get(agent.instance()).cloned().unwrap_or_else(||crate::models::fallback(&p.model)),"skills":[],"plugins":[],"slashCommands":[]}],
        "settings":{"addProjectBaseDirectory":self.config.workspace,"providerInstances":{agent.instance():{"driver":agent.driver(),"displayName":title,"enabled":enabled}},"providers":{agent.driver():{"enabled":enabled}}},
        "environment":{"capabilities":{"androidRemoteAtomicPromptEdit":true,"androidRemoteOptimisticPromptAcknowledgement":true,"androidRemoteLatestPromptEditOnly":agent==AgentKind::Codex}},
        "observability":{"logsDirectoryPath":"","localTracingEnabled":false,"otlpTracesEnabled":false,"otlpMetricsEnabled":false}})
    }
    fn shell(&self, agent: AgentKind) -> Value {
        self.shell_archive(agent, false)
    }
    fn shell_archive(&self, agent: AgentKind, archived: bool) -> Value {
        let threads: Vec<Value> = self
            .chats
            .values()
            .filter(|c| {
                c.agent == agent
                    && c.thread["deletedAt"].is_null()
                    && c.thread["archivedAt"].is_null() != archived
            })
            .map(|c| {
                Value::Object(
                    c.thread
                        .as_object()
                        .unwrap()
                        .iter()
                        .filter(|(k, _)| {
                            !["messages", "activities", "checkpoints", "proposedPlans"]
                                .contains(&k.as_str())
                        })
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            })
            .collect();
        let mut projects = self.projects.clone();
        if let Some(chat) = self.chats.values().find(|c| {
            c.agent == agent
                && c.thread["deletedAt"].is_null()
                && c.thread["workspaceKind"] == "projectless"
        }) {
            projects.as_array_mut().unwrap().insert(0,json!({"id":"codex-project-chats","title":"No Project","workspaceRoot":self.config.workspace.join(".remodex-chats"),"repositoryIdentity":null,"defaultModelSelection":null,"scripts":[],"createdAt":chat.thread["createdAt"],"updatedAt":chat.thread["createdAt"]}));
        }
        json!({"snapshotSequence":self.seq,"projects":projects,"threads":threads,"nativeProjects":[],"nativeThreads":[],"updatedAt":now()})
    }
    async fn detail(&self, agent: AgentKind, id: &str) -> Result<Value, String> {
        let c = self.checked(agent, id)?;
        let key = id.to_owned();
        // Pending controls must stay actionable even if parallel tool output
        // pushes their original rows outside the newest history page.
        let pending: Vec<_> = self
            .active
            .get(id)
            .into_iter()
            .flat_map(|w| w.requests.keys().cloned())
            .collect();
        let saved = self
            .db
            .call(move |a| a.store.load_chat_display(&key, &pending))
            .await?
            .map_err(|_| "Chat history unavailable")?;
        let mut t = saved["thread"].clone();
        for (index, q) in c.queue.iter().enumerate() {
            t["messages"].as_array_mut().unwrap().push(json!({"id":q["message"]["messageId"],"role":"user","text":crate::attachments::display_message(&q["message"]),"turnId":null,"streaming":false,"phase":"queued","queuePosition":index,"queueStatus":if c.paused {"paused"} else {"queued"},"createdAt":q["createdAt"],"updatedAt":q["createdAt"]}));
        }
        Ok(json!({"snapshotSequence":self.seq,"historyPage":t["historyPage"],"thread":t}))
    }
    async fn request(
        &mut self,
        agent: AgentKind,
        client: &str,
        method: &str,
        p: Value,
    ) -> Result<Value, String> {
        match method {
            "native.claudeSetupSnapshot" => {
                let connected = self
                    .active
                    .iter()
                    .filter(|(id, w)| {
                        self.chats.get(*id).is_some_and(|c| {
                            c.agent == AgentKind::Claude
                                && c.thread["deletedAt"].is_null()
                                && c.thread["archivedAt"].is_null()
                        }) && (w.claude_router.is_some() || w.observer.is_some())
                            && w.metadata_ready()
                    })
                    .count();
                let active = self
                    .active
                    .iter()
                    .filter(|(id, w)| {
                        self.chats
                            .get(*id)
                            .is_some_and(|c| c.agent == AgentKind::Claude)
                            && w.stage != "idle"
                    })
                    .count();
                let known = self.models.contains_key(AgentKind::Claude.instance());
                let error = self.discovery_errors.get(AgentKind::Claude.instance());
                let ids: Vec<_> = self
                    .chats
                    .iter()
                    .filter(|(_, c)| {
                        c.agent == AgentKind::Claude
                            && c.thread["deletedAt"].is_null()
                            && c.thread["archivedAt"].is_null()
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let (verification_count, mut issue) = self.db.call(move |a| -> rusqlite::Result<(usize, Option<&'static str>)> {
                    use rusqlite::OptionalExtension;
                    let mut statement = a.store.db.prepare("SELECT substr(json_extract(data,'$.payload.detail'),1,2048) FROM chat_rows WHERE chat=?1 AND kind='activities' AND id='claude-native-sync' AND json_extract(data,'$.payload.status')!='connected'")?;
                    let mut count = 0;let mut issue = None;
                    for id in ids {
                        let detail: Option<Option<String>> = statement.query_row([id], |r| r.get(0)).optional()?;
                        if let Some(found) = detail.flatten().as_deref().and_then(crate::agent_setup::verification_issue) { count += 1; if issue != Some("device-verification") {issue = Some(found);} }
                    }
                    Ok((count, issue))
                }).await?.map_err(|_| "Claude connection status is unavailable")?;
                if issue.is_none() {
                    issue = error.and_then(|e| crate::agent_setup::verification_issue(e));
                }
                Ok(
                    json!({"enabled":self.claude_enabled,"desktopSync":self.config.claude.remote_control,"installed":self.config.claude.executable.is_file(),"catalogReady":known && error.is_none(),"ready":false,"error":error,"account":{},"binary":self.config.claude.executable,"activeSessions":active,"connectedSessions":connected,"connection":{"connectedSessions":connected,"verificationRequiredSessions":verification_count,"verificationIssue":issue,"desktopVerification":"unconfirmed"},"models":self.models.get(AgentKind::Claude.instance()).cloned().unwrap_or_else(||json!([]))}),
                )
            }
            "server.getConfig" | "subscribeServerConfig" => Ok(self.config(agent)),
            "orchestration.getArchivedShellSnapshot" => Ok(self.shell_archive(agent, true)),
            "server.getStorageStatus" => self.db.call(|a| a.store.storage_status()).await?,
            "claude.getUsage" => {
                if agent != AgentKind::Claude {
                    return Err("Claude usage is only available for Claude chats".into());
                }
                let id = s(&p["threadId"]).to_owned();
                self.checked(agent, &id)?;
                let mut context_available = false;
                if let Some(w) = self.active.get_mut(&id).filter(|w| w.metadata_ready()) {
                    context_available = true;
                    if w.metadata.context_request.is_none()
                        && w.metadata
                            .context_checked
                            .is_none_or(|at| at.elapsed() > Duration::from_secs(15))
                    {
                        let rid = format!("context-{}", uuid::Uuid::new_v4());
                        w.connection.send(&json!({"type":"control_request","request_id":rid,"request":{"subtype":"get_context_usage","detail":"summary"}})).await.map_err(|_|"Context information is unavailable")?;
                        w.metadata.context_request =
                            Some((rid, Some(Instant::now() + Duration::from_secs(8))));
                        w.metadata.context_checked = Some(Instant::now());
                    }
                }
                if !context_available {
                    self.report_usage(&id, true, None).await?;
                }
                if let Some((_, value)) = self
                    .usage_cache
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < Duration::from_secs(60))
                {
                    self.report_usage(&id, false, value.clone()).await?;
                } else {
                    if !self.usage_reads.contains(&id) {
                        self.usage_reads.push(id);
                    }
                    if self.usage_task.is_none() {
                        let config = self.config.claude.clone();
                        let cwd = self.config.workspace.clone();
                        self.usage_task = Some(tokio::spawn(async move {
                            crate::discovery::claude_usage(config, &cwd).await
                        }));
                    }
                }
                Ok(json!({"status":if context_available{"ready"}else{"unavailable"}}))
            }
            "filesystem.readTextFile" => {
                let id = s(&p["threadId"]).to_owned();
                let chat = self.checked(agent, &id)?;
                let workspace = PathBuf::from(s(&chat.thread["worktreePath"]));
                let path = s(&p["path"]).to_owned();
                let old_path = path.clone();
                let migrated = self
                    .db
                    .call(move |a| a.store.read_legacy_upload_text(&id, &old_path))
                    .await??;
                // An existing Desktop chat may be outside the bridge's default
                // new-project directory. Use its saved, verified workspace,
                // never a cwd supplied by the phone.
                tokio::task::spawn_blocking(move || {
                    if let Some(result) = migrated {
                        Ok(result)
                    } else {
                        crate::workspace::read_chat(&workspace, &path)
                    }
                })
                .await
                .map_err(|_| "File service stopped")?
            }
            "projects.listEntries" | "projects.searchEntries" | "projects.readFile" => {
                // Read-only browsing may follow a registered project or a
                // verified chat workspace outside the new-chat default root.
                // Never turn an arbitrary cwd from the phone into a root.
                let mut roots = vec![self.config.workspace.clone()];
                roots.extend(
                    self.projects
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|project| {
                            project["workspaceRoot"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(PathBuf::from)
                        }),
                );
                roots.extend(
                    self.chats
                        .values()
                        .filter(|chat| chat.agent == agent && chat.thread["deletedAt"].is_null())
                        .filter_map(|chat| {
                            chat.thread["worktreePath"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(PathBuf::from)
                        }),
                );
                let method = method.to_owned();
                tokio::task::spawn_blocking(move || {
                    let root = roots
                        .iter()
                        .find_map(|base| crate::workspace::resolve(base, s(&p["cwd"])).ok())
                        .ok_or("Folder is not part of a registered project or chat")?;
                    let mut p = p;
                    p["cwd"] = json!(root);
                    match method.as_str() {
                        "projects.listEntries" => crate::workspace::entries(&root, &p, false),
                        "projects.searchEntries" => crate::workspace::entries(&root, &p, true),
                        _ => crate::workspace::read(&root, &p),
                    }
                })
                .await
                .map_err(|_| "File service stopped")?
            }
            "filesystem.browse" | "projects.writeFile" => {
                let base = self.config.workspace.clone();
                let method = method.to_owned();
                tokio::task::spawn_blocking(move || match method.as_str() {
                    "filesystem.browse" => crate::workspace::browse(&base, &p),
                    _ => crate::workspace::write(&base, &p),
                })
                .await
                .map_err(|_| "File service stopped")?
            }
            "orchestration.subscribeShell" => {
                Ok(json!({"kind":"snapshot","snapshot":self.shell(agent)}))
            }
            "orchestration.subscribeThread" => Ok(
                json!({"kind":"snapshot","snapshot":self.detail(agent,s(&p["threadId"])).await?}),
            ),
            "orchestration.getThreadPage" | "orchestration.getTurnWork" => {
                let id = s(&p["threadId"]).to_owned();
                self.checked(agent, &id)?;
                let cursor = s(&p["cursor"]);
                let before = if cursor.is_empty() {
                    None
                } else {
                    Some(
                        cursor
                            .strip_prefix(&format!("v1:{id}:"))
                            .and_then(|v| v.parse::<i64>().ok())
                            .filter(|v| *v > 0)
                            .ok_or("Invalid history cursor")?,
                    )
                };
                if method == "orchestration.getThreadPage" && before.is_none() {
                    return Err("Older history cursor required".into());
                }
                let turn = if method == "orchestration.getTurnWork" {
                    Some(s(&p["turnId"]).to_owned())
                } else {
                    None
                };
                let mut page = self
                    .db
                    .call(move |a| a.store.history_page(&id, before, turn.as_deref()))
                    .await?
                    .map_err(|_| "History unavailable")?;
                if method == "orchestration.getTurnWork" {
                    page["turnId"] = p["turnId"].clone();
                    page["revision"] = json!(self.seq.to_string());
                    page["nextCursor"] = page["pageInfo"]["olderCursor"].clone();
                }
                Ok(page)
            }
            "assets.createUrl" => {
                let r = &p["resource"];
                let id = s(&r["threadId"]).to_owned();
                self.checked(agent, &id)?;
                if r["_tag"] != "workspace-file" {
                    return Err("Only workspace image resources are supported".into());
                }
                let path = PathBuf::from(s(&r["path"]));
                let workspace = PathBuf::from(s(&self.chats[&id].thread["worktreePath"]));
                self.db.call(move |a| -> Result<Value,String> {
                    let migrated=a.store.resolve_legacy_upload(&id,path.to_str().ok_or("Image unavailable")?)?;
                    let base=if migrated.is_some() {workspace} else {workspace.canonicalize().map_err(|_|"Workspace unavailable")?};
                    let is_migrated=migrated.is_some();
                    let file=if let Some(file)=migrated {file} else {if path.is_absolute() {path} else {base.join(path)}.canonicalize().map_err(|_|"Image unavailable")?};
                    let uploaded=file.starts_with(a.store.directory.join("uploads"));
                    let referenced=uploaded && a.store.db.query_row("SELECT EXISTS(SELECT 1 FROM chat_rows WHERE chat=?1 AND kind='messages' AND instr(data,?2)>0) OR EXISTS(SELECT 1 FROM chats WHERE id=?1 AND instr(data,?2)>0)",rusqlite::params![id,file.to_string_lossy()],|r|r.get::<_,bool>(0)).map_err(|_|"History unavailable")?;
                    if !is_migrated && !file.starts_with(&base) && !referenced && !a.store.owns_tool_image(&id,&file) {return Err("Image is outside this chat's workspace and uploads".into());}
                    let mut handle=crate::attachments::open_image(&file)?;
                    use std::io::Read;
                    let mut header=[0u8;16];let count=handle.read(&mut header).map_err(|_|"Image unavailable")?;
                    let mime=crate::attachments::image_mime(&header[..count]).ok_or("Unsupported image format")?;
                    Ok(json!({"path":file,"mime":mime}))
                }).await?
            }
            "orchestration.dispatchCommand" => self.dispatch(agent, client, p).await,
            _ => Err("This operation is not migrated to the native backend yet".into()),
        }
    }
    async fn report_usage(
        &mut self,
        id: &str,
        context: bool,
        value: Option<Value>,
    ) -> Result<(), String> {
        let Some(chat) = self.chats.get_mut(id) else {
            return Ok(());
        };
        let mut payload = json!({"providerName":"claude-code","status":if value.is_some(){"ready"}else{"unavailable"},"checkedAt":now()});
        if context {
            if let Some(value) = value {
                for (k, v) in value.as_object().unwrap() {
                    payload[k] = v.clone();
                }
            }
        } else {
            payload["providerLabel"] = json!("Claude");
            payload["windows"] = value.unwrap_or(json!([]));
        }
        projection::activity(
            chat,
            &format!("{}-{id}", if context { "context" } else { "usage" }),
            if context {
                "context-window.updated"
            } else {
                "provider.usage.updated"
            },
            if context {
                "Context window updated"
            } else {
                "Claude usage limits updated"
            },
            "info",
            payload,
        );
        self.save(id, None).await?;
        self.dirty = true;
        Ok(())
    }
    async fn new_chat(&self, agent: AgentKind, id: &str, c: &Value) -> Result<Chat, String> {
        self.db
            .call(|a| a.store.check_history(1024 * 1024))
            .await??;
        if self.chats.contains_key(id) {
            return Err("Chat already exists".into());
        }
        if self.chats.len() >= 10000 {
            return Err("This profile has reached its 10,000-chat metadata budget".into());
        }
        let projectless =
            c["workspaceKind"] == "projectless" || c["projectId"] == "codex-project-chats";
        if c["branch"].is_string() || c["worktreePath"].is_string() {
            return Err("Use the worktree action to create a separate checkout".into());
        }
        let title = c["title"].as_str().unwrap_or("New task");
        if title.trim().is_empty() || title.len() > 256 {
            return Err("A chat title of up to 256 bytes is required".into());
        }
        if c.get("workspaceKind").is_some() && !projectless {
            return Err("Unsupported workspace kind".into());
        }
        let model = self.config.provider(agent).model.clone();
        let catalog = self
            .models
            .get(agent.instance())
            .cloned()
            .unwrap_or_else(|| crate::models::fallback(&model));
        let fallback = json!({"model":model});
        let project_default = self
            .projects
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == c["projectId"])
            .map(|p| &p["defaultModelSelection"])
            .filter(|v| v["instanceId"] == agent.instance())
            .unwrap_or(&fallback);
        let selection = crate::models::selection(
            &catalog,
            &c["modelSelection"],
            project_default,
            agent.instance(),
        )?;
        let runtime = c["runtimeMode"].as_str().unwrap_or("approval-required");
        let interaction = c["interactionMode"].as_str().unwrap_or("default");
        if !["approval-required", "full-access", "auto-accept-edits"].contains(&runtime)
            || !["default", "plan"].contains(&interaction)
        {
            return Err("Unsupported chat settings".into());
        }
        let root = if projectless {
            let base = self.config.workspace.clone();
            let name = format!(".remodex-chats/{:x}", Sha256::digest(id.as_bytes()));
            tokio::task::spawn_blocking(move || crate::workspace::directory(&base, &name, true))
                .await
                .map_err(|_| "Workspace service stopped")??
        } else {
            let project = self
                .projects
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == c["projectId"])
                .ok_or("Select a registered project")?;
            crate::workspace::directory(
                &self.config.workspace,
                s(&project["workspaceRoot"]),
                false,
            )?
        };
        let timestamp = now();
        let mut thread = json!({"id":id,"projectId":c["projectId"],"title":title,"modelSelection":{"instanceId":agent.instance(),"model":model},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":root,"latestTurn":null,"createdAt":timestamp,"updatedAt":timestamp,"archivedAt":null,"deletedAt":null,"latestUserMessageAt":null,"hasPendingApprovals":false,"hasPendingUserInput":false,"hasActionableProposedPlan":false,"session":{"threadId":id,"providerName":agent.driver(),"providerInstanceId":agent.instance(),"status":"idle","statusConfidence":"confirmed","runtimeMode":"approval-required","activeTurnId":null,"lastError":null,"updatedAt":timestamp},"messages":[],"activities":[],"checkpoints":[],"proposedPlans":[]});
        thread["modelSelection"] = selection;
        thread["runtimeMode"] = json!(runtime);
        thread["session"]["runtimeMode"] = json!(runtime);
        thread["interactionMode"] = json!(interaction);
        if projectless {
            thread["projectId"] = json!("codex-project-chats");
            thread["workspaceKind"] = json!("projectless");
        }
        Ok(Chat {
            agent,
            thread,
            native_id: if agent == AgentKind::Claude {
                uuid::Uuid::new_v4().to_string()
            } else {
                String::new()
            },
            resumed: false,
            claude_plan_access: None,
            claude_permission_rules: vec![],
            remote_identity: None,
            external_claude: false,
            external_stop: None,
            native_control: None,
            fork_from: None,
            sync_replacement: None,
            queue: vec![],
            paused: false,
            disabled_mcp: vec![],
            stream_id: String::new(),
            stream_model: String::new(),
            pending_message: Value::Null,
        })
    }
    async fn dispatch(
        &mut self,
        agent: AgentKind,
        client: &str,
        mut c: Value,
    ) -> Result<Value, String> {
        let command = s(&c["commandId"]).to_owned();
        let id = s(&c["threadId"]).to_owned();
        if command.is_empty()
            || command.len() > 128
            || ((!s(&c["type"]).starts_with("project.")) && (id.is_empty() || id.len() > 128))
        {
            return Err("Command and thread IDs required".into());
        }
        let key = format!("{}:{client}:{command}", agent.instance());
        let mut digest = Sha256::new();
        struct HashWriter<'a>(&'a mut Sha256);
        impl std::io::Write for HashWriter<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.update(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(HashWriter(&mut digest), &c).map_err(|_| "Invalid command")?;
        let digest = format!("{:x}", digest.finalize());
        let lookup = key.clone();
        if let Some(old) = self
            .db
            .call(move |a| a.store.command_digest(&lookup))
            .await?
            .map_err(|_| "Command store unavailable")?
        {
            return if old == digest {
                Ok(json!({"sequence":self.seq}))
            } else {
                Err("Command ID was already used for different content".into())
            };
        }
        let command_receipt = (key.clone(), digest);
        let receipt = Some(command_receipt.clone());
        let bootstrapped = if c["type"] == "thread.turn.start" && c.get("bootstrap").is_some() {
            let b = &c["bootstrap"];
            if let Some((chat, project_root)) = self.prepared_bootstraps.remove(&id) {
                if self.chats.len() >= 10000 {
                    return Err(
                        "The chat capacity was reached during setup; the checkout was preserved"
                            .into(),
                    );
                }
                let project = self
                    .projects
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["id"] == chat.thread["projectId"])
                    .ok_or(
                        "The selected project was removed during setup; the checkout was preserved",
                    )?;
                if crate::workspace::resolve(&self.config.workspace, s(&project["workspaceRoot"]))?
                    != project_root
                {
                    return Err("The selected project folder changed during setup; the checkout was preserved".into());
                }
                self.chats.insert(id.clone(), chat);
            } else {
                if !b.is_object()
                    || b.get("prepareWorktree").is_some()
                    || b["runSetupScript"] == true
                {
                    return Err("Worktree and setup-script bootstrap are not migrated yet".into());
                }
                if b.get("runSetupScript").is_some_and(|v| !v.is_boolean())
                    || b.as_object()
                        .unwrap()
                        .keys()
                        .any(|k| !["createThread", "runSetupScript"].contains(&k.as_str()))
                    || !b["createThread"].is_object()
                    || c.get("promptEdit").is_some()
                    || c.get("sourceProposedPlan").is_some()
                    || c["deliveryMode"].as_str().is_some_and(|m| m != "normal")
                {
                    return Err("Create a new chat with a normal first prompt".into());
                }
                if b["createThread"]["branch"].is_string()
                    || b["createThread"]["worktreePath"].is_string()
                {
                    return Err(
                        "Select a registered project; worktree bootstrap is not migrated yet"
                            .into(),
                    );
                }
                if self.busy_count() >= self.config.max_active_chats {
                    return Err("Wait for a free chat worker before creating this chat".into());
                }
                let chat = self.new_chat(agent, &id, &b["createThread"]).await?;
                self.chats.insert(id.clone(), chat);
            }
            c.as_object_mut().unwrap().remove("bootstrap");
            true
        } else {
            false
        };
        let cleanup_key = key.clone();
        let result = async {
        if self.recovery_checks.contains_key(&id)
            && matches!(s(&c["type"]), "thread.turn.interrupt" | "thread.session.stop") {
            self.checked(agent, &id)?;
            let old = self.chats[&id].clone();
            let chat = self.chats.get_mut(&id).unwrap();
            if chat.thread["latestTurn"]["state"] == "running" {
                finish(chat, "interrupted", None);
                chat.pending_message = Value::Null;
            }
            projection::activity(chat, "claude-native-sync", "claude.desktop-sync.updated", "Claude Desktop sync", "info", json!({"status":"disconnected","detail":"Connection attempt cancelled. Saved history is intact."}));
            chat.paused = true;
            if let Err(e) = self.save(&id, receipt).await {
                self.chats.insert(id.clone(), old);
                return Err(e);
            }
            self.recovery_checks.get_mut(&id).unwrap().cancelled = true;
            self.claude_recovery.forget(&id);
            self.publish();
            return Ok(json!({"sequence":self.seq}));
        }
        if self.tool_controls.contains(&id) {
            return Err("Wait for this chat's control to finish".into());
        }
        if s(&c["type"]).starts_with("project.") {
            if c["type"] == "project.delete"
                && self.chats.values().any(|chat| {
                    chat.thread["projectId"] == c["projectId"] && chat.thread["deletedAt"].is_null()
                })
            {
                return Err("Move or delete this project's chats before removing it".into());
            }
            if let Some(value)=c.get("defaultModelSelection").filter(|v|!v.is_null()) {
                if !value.is_object() || !value["model"].is_string() {
                    return Err("Select a default model for this agent".into());
                }
                let catalog=self.models.get(agent.instance()).cloned().unwrap_or_else(|| crate::models::fallback(&self.config.provider(agent).model));
                c["defaultModelSelection"]=crate::models::project_selection(&catalog,value,agent.instance(),c["type"]=="project.create")?;
            }
            let base = self.config.workspace.clone();
            let old = self.projects.clone();
            let cmd = c.clone();
            let next =
                tokio::task::spawn_blocking(move || crate::projects::change(&base, &old, &cmd))
                    .await
                    .map_err(|_| "Project service stopped")??;
            let data = next.clone();
            let receipt = command_receipt;
            self.db
                .call(move |a| a.store.save_projects(&data, (&receipt.0, &receipt.1)))
                .await?
                .map_err(|_| "Cannot save projects")?;
            self.projects = next;
        } else if c["type"] == "thread.create" {
            let chat = self.new_chat(agent, &id, &c).await?;
            self.chats.insert(id.clone(), chat);
            if let Err(e) = self.save(&id, receipt).await {
                self.chats.remove(&id);
                return Err(e);
            }
        } else {
            self.checked(agent, &id)?;
            let own = self.busy(&id);
            match s(&c["type"]) {
                "thread.checkpoint.revert" if agent == AgentKind::Codex => {
                    return Err("Codex supports editing the latest prompt; Revert is not available.".into());
                }
                "thread.turn.start" => {
                    if c["deliveryMode"]
                        .as_str()
                        .is_some_and(|v| !["normal", "queue", "steer"].contains(&v))
                    {
                        return Err("Unknown prompt delivery mode".into());
                    }
                    if c.get("bootstrap").is_some()
                        || (c.get("promptEdit").is_some() && !self.prepared_edits.contains_key(&id))
                    {
                        return Err("Bootstrap is not migrated; prompt edits require prepared native history".into());
                    }
                    if let Some(source) = c.get("sourceProposedPlan") {
                        if source["threadId"] != id
                            || s(&source["planId"]).is_empty()
                            || s(&source["planId"]).len() > 128
                        {
                            return Err("Implement the plan in its original chat".into());
                        }
                        if own || c["deliveryMode"] == "steer" || c["deliveryMode"] == "queue" {
                            return Err(
                                "Wait for the active turn before implementing a plan".into()
                            );
                        }
                        let cid = id.clone();
                        let pid = s(&source["planId"]).to_owned();
                        let plan = self
                            .db
                            .call(move |a| a.store.history_row(&cid, "proposedPlans", &pid))
                            .await?
                            .map_err(|_| "Plan history unavailable")?
                            .ok_or("Plan not found")?;
                        if !plan["implementedAt"].is_null() || plan["completed"] != true {
                            return Err("Select an unimplemented, completed plan".into());
                        }
                        let plans = self.chats.get_mut(&id).unwrap().thread["proposedPlans"]
                            .as_array_mut()
                            .unwrap();
                        if !plans.iter().any(|p| p["id"] == plan["id"]) {
                            plans.push(plan);
                        }
                    }
                    if c["interactionMode"]
                        .as_str()
                        .is_some_and(|v| !["default", "plan"].contains(&v))
                    {
                        return Err("Unsupported interaction mode".into());
                    }
                    if c.get("titleSeed").is_some_and(|v| v.as_str().is_none_or(|t| t.trim().is_empty() || t.len()>256)) {
                        return Err("A title seed of up to 256 bytes is required".into());
                    }
                    let text = s(&c["message"]["text"]);
                    let mid = s(&c["message"]["messageId"]);
                    if (text.trim().is_empty()
                        && c["message"]["attachments"]
                            .as_array()
                            .is_none_or(|a| a.is_empty()))
                        || text.len() > 32000
                        || mid.is_empty()
                        || mid.len() > 128
                    {
                        return Err(
                            "A prompt of up to 32,000 bytes and a message ID are required".into(),
                        );
                    }
                    let chat = &self.chats[&id];
                    if chat.thread["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["id"] == mid)
                        || chat.queue.iter().any(|q| q["message"]["messageId"] == mid)
                    {
                        return Err("Message ID already accepted".into());
                    }
                    let lookup_id = id.clone();
                    let lookup_mid = mid.to_owned();
                    if self
                        .db
                        .call(move |a| a.store.history_row(&lookup_id, "messages", &lookup_mid))
                        .await?
                        .map_err(|_| "History unavailable")?
                        .is_some()
                    {
                        return Err("Message ID already accepted".into());
                    }
                    if c["runtimeMode"].as_str().is_some_and(|m| {
                        !["approval-required", "full-access", "auto-accept-edits"].contains(&m)
                    }) {
                        return Err("Unsupported access mode".into());
                    }
                    let catalog = self
                        .models
                        .get(agent.instance())
                        .cloned()
                        .unwrap_or_else(|| {
                            crate::models::fallback(&self.config.provider(agent).model)
                        });
                    let selection = if self.confirmed_settings.as_deref() == Some(id.as_str()) {
                        // Acknowledged by the worker, which reports wire IDs for aliases.
                        self.chats[&id].thread["modelSelection"].clone()
                    } else if self.chats[&id].external_claude {
                        crate::native_control::shared_selection(&catalog,&c["modelSelection"],&self.chats[&id].thread["modelSelection"])?
                    } else {crate::models::selection(
                        &catalog,
                        &c["modelSelection"],
                        &self.chats[&id].thread["modelSelection"],
                        agent.instance(),
                    )?};
                    if own
                        && c["deliveryMode"] == "steer"
                        && (selection != self.chats[&id].thread["modelSelection"]
                            || c["runtimeMode"]
                                .as_str()
                                .is_some_and(|v| self.chats[&id].thread["runtimeMode"] != v)
                            || c["interactionMode"]
                                .as_str()
                                .is_some_and(|v| self.chats[&id].thread["interactionMode"] != v))
                    {
                        return Err("Model settings apply to the next turn; queue this message to change them".into());
                    }
                    self.db
                        .call(|a| a.store.check_history(1024 * 1024))
                        .await??;
                    let raw = c["message"]
                        .as_object_mut()
                        .unwrap()
                        .remove("attachments")
                        .unwrap_or(json!([]));
                    let staged = self.db.call(move |a| a.store.stage_uploads(raw)).await??;
                    // Keep only the accepted wire fields. Unknown JSON must not
                    // turn a bounded queue into a retained upload-sized object.
                    c = json!({"type":"thread.turn.start","threadId":id,"commandId":command,
                        "message":{"messageId":c["message"]["messageId"],"text":c["message"]["text"],"attachments":staged},
                        "deliveryMode":c["deliveryMode"],"runtimeMode":c["runtimeMode"].as_str().unwrap_or_else(||s(&self.chats[&id].thread["runtimeMode"])),
                        "interactionMode":c["interactionMode"].as_str().unwrap_or_else(||s(&self.chats[&id].thread["interactionMode"])),
                        "modelSelection":selection,"titleSeed":c["titleSeed"],
                        "sourceProposedPlan":c["sourceProposedPlan"],
                        "createdAt":now()});
                    let receipt_key = key.clone();
                    let delivery = async {
                    if own && c["deliveryMode"] == "steer" {
                        if self.active.get(&id).unwrap().stage != "running"
                            || self.active.get(&id).unwrap().stopping
                        {
                            return Err("Wait for a running turn before steering".into());
                        }
                        self.steer(&id, c, receipt).await?;
                    } else if own && c["deliveryMode"] == "queue" {
                        if self.chats.values().map(|chat|chat.queue.len()).sum::<usize>() >= 64 {
                            return Err("The PC has 64 queued prompts. Finish or remove queued messages before adding more.".into());
                        }
                        if self.chats[&id].queue.len() >= 8 {
                            return Err("Queue holds at most eight prompts".into());
                        }
                        let old = self.chats[&id].clone();
                        let chat = self.chats.get_mut(&id).unwrap();
                        // The phone's queue action carries a message ID, not a
                        // turn ID. Bind conversion to the task the user queued
                        // it during, so a late tap cannot steer its successor.
                        c["steerTurnId"] = chat.thread["session"]["activeTurnId"].clone();
                        chat.queue.push(c);
                        chat.paused = false;
                        if let Err(e) = self.save(&id, receipt).await {
                            self.chats.insert(id.clone(), old);
                            return Err(e);
                        }
                    } else {
                        if own || self.busy_count() >= self.config.max_active_chats {
                            return Err("Active chat limit reached. Queue in a running chat or wait for a free worker.".into());
                        }
                        if !self.chats[&id].queue.is_empty() {
                            return Err(
                                "Remove the paused queued messages before sending another prompt"
                                    .into(),
                            );
                        }
                        self.begin(&id, c, receipt).await?;
                    }
                    Ok::<(),String>(())
                    }.await;
                    if delivery.is_err() {
                        let accepted = self
                            .db
                            .call(move |a| a.store.command_digest(&receipt_key))
                            .await?
                            .map_err(|_| "Command store unavailable")?
                            .is_some();
                        if !accepted {
                            let _ = self
                                .db
                                .call(move |a| a.store.discard_uploads(&staged))
                                .await;
                        }
                    }
                    delivery?;
                }
                "thread.turn.queue.cancel"
                | "thread.turn.queue.update"
                | "thread.turn.queue.move"
                | "thread.turn.queue.steer" => {
                    let mid = s(&c["messageId"]);
                    let chat = self.chats.get_mut(&id).unwrap();
                    let index = chat
                        .queue
                        .iter()
                        .position(|q| q["message"]["messageId"] == mid)
                        .ok_or("Queued message not found")?;
                    let old = chat.clone();
                    let result = match s(&c["type"]) {
                        "thread.turn.queue.update" => {
                            let text = s(&c["text"]);
                            if text.trim().is_empty() || text.len() > 32000 {
                                return Err("Enter a queued prompt of up to 32,000 bytes".into());
                            }
                            chat.queue[index]["message"]["text"] = json!(text);
                            self.save(&id, receipt).await
                        }
                        "thread.turn.queue.move" => {
                            let target = match s(&c["direction"]) {
                                "up" => index.saturating_sub(1),
                                "down" => (index + 1).min(chat.queue.len() - 1),
                                _ => return Err("Choose up or down".into()),
                            };
                            chat.queue.swap(index, target);
                            self.save(&id, receipt).await
                        }
                        "thread.turn.queue.steer" => {
                            // On an idle shared Claude connection this sends the
                            // message now as a new turn; the rest of the queue
                            // resumes after it, as with the TypeScript bridge.
                            let send_now = |w: &Worker| {
                                !own && (w.claude_router.is_some() || w.observer.is_some())
                            };
                            if let Some(w) = self.active.get(&id).filter(|w| !send_now(w)) {
                                if !own || w.stage != "running" || w.stopping || w.steers.len() >= 8
                                {
                                    return Err("Wait for a running turn before steering".into());
                                }
                                if !chat.queue[index]["steerTurnId"].is_string()
                                    || chat.queue[index]["steerTurnId"]
                                        != chat.thread["session"]["activeTurnId"]
                                {
                                    return Err("The original task has finished. This message remains queued; remove it and send a new steering message to change the current task.".into());
                                }
                            }
                            if !own && self.active.values().filter(|w|w.stage!="idle").count() >= self.config.max_active_chats {
                                return Err(
                                    "Active chat limit reached; queued message was kept".into()
                                );
                            }
                            let entry = chat.queue.remove(index);
                            if own {
                                self.steer(&id, entry, receipt).await
                            } else {
                                self.begin(&id, entry, receipt).await
                            }
                        }
                        _ => {
                            chat.queue.remove(index);
                            self.save(&id, receipt).await
                        }
                    };
                    if let Err(e) = result {
                        self.chats.insert(id.clone(), old);
                        return Err(e);
                    }
                }
                "thread.turn.interrupt" | "thread.session.stop" => {
                    if !own {
                        return Err("This test chat is not running".into());
                    }
                    // Receipt precedes control delivery, so reconnect cannot repeat a stop.
                    self.chats.get_mut(&id).unwrap().paused = true;
                    let background = self.background_tasks(&id)?;
                    let stop_id = observer::record_stop(self.chats.get_mut(&id).unwrap(), background.stop_targets());
                    self.save(&id, receipt).await?;
                    let w = self.active.get_mut(&id).unwrap();
                    w.stopping = true;
                    w.deadline = Some(Instant::now() + Duration::from_secs(10));
                    let request = if agent == AgentKind::Codex {
                        json!({"id":"stop","method":"turn/interrupt","params":{"threadId":self.chats[&id].native_id,"turnId":w.native_turn}})
                    } else {
                        let interrupt = json!({"type":"control_request","request_id":stop_id,"request":{"subtype":"interrupt","cancel_queued":w.claude_router.is_some()}});
                        let sequence = crate::claude_background::StopSequence::new(self.chats[&id].external_stop.as_ref().unwrap().background.clone(), interrupt);
                        let request = sequence.first(&background)?;
                        w.claude_stop = Some(sequence);
                        request
                    };
                    if w.stage != "running" {
                        self.end(&id, "interrupted", None).await;
                    } else if w.connection.send(&request).await.is_err() {
                        self.end(&id, "interrupted", Some("CLI disconnected while stopping"))
                            .await;
                    }
                }
                "thread.meta.update"
                | "thread.archive"
                | "thread.unarchive"
                | "thread.delete"
                | "thread.runtime-mode.set"
                | "thread.interaction-mode.set" => {
                    if own {
                        return Err("Wait for the active turn before changing chat settings".into());
                    }
                    if self.active.get(&id).is_some_and(|w|w.claude_router.is_some()) && ["thread.archive","thread.delete"].contains(&s(&c["type"])) {
                        return Err("Disconnect this shared Claude session before archiving or deleting it".into());
                    }
                    if self.chats[&id].external_claude && (c.get("modelSelection").is_some() || ["thread.runtime-mode.set","thread.interaction-mode.set"].contains(&s(&c["type"]))) {
                        return Err("Changing settings on an externally owned Claude session is not migrated yet".into());
                    }
                    let old = self.chats[&id].clone();
                    let mut next = old.clone();
                    match s(&c["type"]) {
                        "thread.meta.update" => {
                            if c.get("worktreePath").is_some() || c.get("branch").is_some() {
                                return Err(
                                    "Workspace and branch changes require their dedicated controls"
                                        .into(),
                                );
                            }
                            if let Some(title) = c.get("title") {
                                let title = title
                                    .as_str()
                                    .filter(|v| !v.trim().is_empty() && v.len() <= 256)
                                    .ok_or("Enter a title of up to 256 bytes")?;
                                next.thread["title"] = json!(title);
                            }
                            if c.get("modelSelection").is_some() {
                                let catalog = self
                                    .models
                                    .get(agent.instance())
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        crate::models::fallback(&self.config.provider(agent).model)
                                    });
                                next.thread["modelSelection"] = crate::models::selection(
                                    &catalog,
                                    &c["modelSelection"],
                                    &old.thread["modelSelection"],
                                    agent.instance(),
                                )?;
                            }
                        }
                        "thread.runtime-mode.set" => {
                            if !["approval-required", "full-access", "auto-accept-edits"]
                                .contains(&s(&c["runtimeMode"]))
                            {
                                return Err("Unsupported access mode".into());
                            }
                            next.thread["runtimeMode"] = c["runtimeMode"].clone();
                            next.thread["session"]["runtimeMode"] = c["runtimeMode"].clone();
                        }
                        "thread.interaction-mode.set" => {
                            if !["default", "plan"].contains(&s(&c["interactionMode"])) {
                                return Err("Unsupported interaction mode".into());
                            }
                            next.thread["interactionMode"] = c["interactionMode"].clone();
                        }
                        "thread.archive" => next.thread["archivedAt"] = json!(now()),
                        "thread.unarchive" => next.thread["archivedAt"] = Value::Null,
                        _ => {
                            next.thread["deletedAt"] = json!(now());
                            next.paused = true;
                            next.queue.clear();
                        }
                    }
                    next.thread["updatedAt"] = json!(now());
                    self.chats.insert(id.clone(), next);
                    if let Err(e) = self.save(&id, receipt).await {
                        self.chats.insert(id.clone(), old);
                        return Err(e);
                    }
                }
                "thread.user-input.respond" => {
                    self.answer(&id, &c, receipt).await?;
                }
                "thread.user-input.draft.update" => {
                    // Match the Android gateway: drafts stay on the phone.
                    // Typing must never refresh a native request, acknowledge a
                    // question or send partial/private answers to the CLI.
                    let answers=c["answers"].as_object().ok_or("Invalid draft answers")?;
                    if s(&c["requestId"]).is_empty() || s(&c["requestId"]).len()>128
                        || c["questionIndex"].as_u64().is_none_or(|n|n>15)
                        || answers.len()>16 || c["answers"].to_string().len()>32000 {
                        return Err("Question draft exceeds the supported limits".into());
                    }
                    for (key,answer) in answers {
                        if key.len()>256 || !answer.is_object()
                            || answer.get("customAnswer").is_some_and(|v|v.as_str().is_none_or(|s|s.len()>8192))
                            || answer.get("selectedOptionLabels").is_some_and(|v|v.as_array().is_none_or(|a|a.len()>32 || a.iter().any(|v|v.as_str().is_none_or(|s|s.len()>1024)))) {
                            return Err("Invalid draft answer".into());
                        }
                    }
                    return Ok(json!({"sequence":self.seq}));
                }
                "thread.approval.respond" => {
                    self.approve(&id, &c, receipt).await?;
                }
                _ => return Err("This chat command is not migrated yet".into()),
            }
        }
        self.publish();
        Ok(json!({"sequence":self.seq}))
        }.await;
        if bootstrapped && result.is_err() {
            // The first prompt and new chat share one commit and receipt. A
            // rejected request must not leave an empty chat in the live map.
            let accepted = self
                .db
                .call(move |a| a.store.command_digest(&cleanup_key))
                .await?
                .map_err(|_| "Command store unavailable")?
                .is_some();
            if !accepted {
                self.chats.remove(&id);
            }
        }
        result
    }
    async fn begin(
        &mut self,
        id: &str,
        mut c: Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        if self.tool_controls.contains(id) {
            return Err("Wait for this chat's tool control to finish".into());
        }
        if self.busy(id) || self.busy_count() >= self.config.max_active_chats {
            return Err("Active chat limit reached; wait for a free worker".into());
        }
        if self.chats[id].agent == AgentKind::Claude
            && self.config.claude.remote_control
            && !self.active.contains_key(id)
            && self
                .active
                .values()
                .filter(|w| w.claude_router.is_some() || w.observer.is_some())
                .count()
                >= 8
        {
            return Err("Eight Claude shared connections are open in this experiment. Disconnect an idle test connection before opening another.".into());
        }
        if self.chats[id].external_claude {
            // Settings just acknowledged for this dispatch, or an alias of the
            // confirmed wire model, are the confirmed settings. Anything else
            // is refused below: no prompt runs under unconfirmed settings.
            if c["modelSelection"].is_object()
                && (self.confirmed_settings.as_deref() == Some(id)
                    || self
                        .shared_actions(id, &c["modelSelection"])
                        .is_ok_and(|a| a.is_empty()))
            {
                c["modelSelection"] = self.chats[id].thread["modelSelection"].clone();
            }
            self.external_send_ready(id, &c, false)?;
        }
        self.db
            .call(|a| a.store.check_history(1024 * 1024))
            .await??;
        let previous = self.chats[id].clone();
        let chat = self.chats.get_mut(id).unwrap();
        if let Some(edit) = self.prepared_edits.get(id) {
            edit.apply(chat);
        }
        chat.thread["sharedTurnOwner"] = Value::Null;
        let first_prompt = chat.thread["latestUserMessageAt"].is_null();
        let mode = s(&c["runtimeMode"]);
        if !["approval-required", "full-access", "auto-accept-edits", ""].contains(&mode) {
            return Err("Unsupported access mode".into());
        }
        let mode = if mode.is_empty() {
            "approval-required"
        } else {
            mode
        };
        if c["modelSelection"].is_object() {
            chat.thread["modelSelection"] = c["modelSelection"].clone();
        }
        if c["interactionMode"].is_string() {
            chat.thread["interactionMode"] = c["interactionMode"].clone();
        }
        chat.thread["runtimeMode"] = json!(mode);
        chat.thread["session"]["runtimeMode"] = json!(mode);
        let turn = uuid::Uuid::new_v4().to_string();
        chat.thread["latestTurn"] = json!({"turnId":turn,"state":"running","requestedAt":now(),"startedAt":null,"completedAt":null,"assistantMessageId":null});
        chat.thread["session"]["status"] = json!("starting");
        chat.thread["session"]["activeTurnId"] = json!(turn);
        chat.thread["session"]["lastError"] = Value::Null;
        chat.thread["latestUserMessageAt"] = json!(now());
        chat.thread["updatedAt"] = json!(now());
        if c["sourceProposedPlan"].is_object() {
            let plan = chat.thread["proposedPlans"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|p| p["id"] == c["sourceProposedPlan"]["planId"])
                .ok_or("Plan no longer available")?;
            plan["implementedAt"] = json!(now());
            plan["implementationThreadId"] = json!(id);
            plan["updatedAt"] = json!(now());
            chat.thread["interactionMode"] = json!("default");
            chat.thread["hasActionableProposedPlan"] = json!(false);
        }
        if first_prompt {
            if let Some(title) = c["titleSeed"].as_str() {
                chat.thread["title"] = json!(title);
            } else if chat.thread["title"] == "New task" {
                chat.thread["title"] = json!(s(&c["message"]["text"])
                    .chars()
                    .take(90)
                    .collect::<String>());
            }
        }
        projection::message(
            chat,
            s(&c["message"]["messageId"]),
            &crate::attachments::display_message(&c["message"]),
            "user",
            false,
            false,
        );
        chat.pending_message = c["message"].clone();
        chat.paused = false;
        let save = if let Some(edit) = self.prepared_edits.remove(id) {
            let data = serde_json::to_value(&self.chats[id]).map_err(|_| "Invalid edited chat")?;
            self.db
                .call(move |a| {
                    a.store.save_edit(
                        &data,
                        receipt.as_ref().map(|(k, d)| (k.as_str(), d.as_str())),
                        &edit.target,
                    )
                })
                .await
                .map_err(str::to_owned)
                .and_then(|r| {
                    r.map_err(|_| {
                        "Could not save the edited history; the original is intact".into()
                    })
                })
        } else {
            self.save(id, receipt).await
        };
        if let Err(e) = save {
            self.chats.insert(id.into(), previous);
            return Err(e);
        }
        // No prompt retries after this point. Any startup/write failure becomes
        // an explicit saved error; the user can decide whether to resend.
        if let Err(e) = self.launch(id).await {
            finish(self.chats.get_mut(id).unwrap(), "error", Some(&e));
            self.save(id, None).await?;
        }
        Ok(())
    }
    async fn launch(&mut self, id: &str) -> Result<(), String> {
        self.launch_worker(id, false).await
    }
    async fn launch_worker(&mut self, id: &str, recovering: bool) -> Result<(), String> {
        if self.chats[id].agent == AgentKind::Claude && !self.claude_enabled {
            return Err("Enable Claude Code in the Remodex dashboard first".into());
        }
        if self.start_recovery_check(id, recovering)? {
            return Ok(());
        }
        let chat = &self.chats[id];
        if chat.external_claude {
            if !recovering {
                return self.send_external_claude(id).await;
            }
            return self.launch_external_claude(id).await;
        }
        if recovering && (!self.config.claude.remote_control || !claude_recoverable(chat)) {
            return Err("This chat has no owned Claude connection to recover".into());
        }
        if let Some(w) = self.active.get_mut(id) {
            if w.stage != "idle" || w.claude_router.is_none() {
                return Err("The chat already has an active worker".into());
            }
            prepare_claude_local(w, chat)?;
            w.stage = "reuse-model";
            w.deadline = Some(Instant::now() + Duration::from_secs(15));
            w.connection.send(&json!({"type":"control_request","request_id":"reuse-model","request":{"subtype":"set_model","model":chat.thread["modelSelection"]["model"]}})).await.map_err(|_|"Claude settings could not be applied")?;
            return Ok(());
        }
        // History checks finish asynchronously. Recheck admission here so two
        // completed copies cannot both take the last available shared worker.
        if chat.agent == AgentKind::Claude
            && (self.busy_count() >= self.config.max_active_chats
                || (self.config.claude.remote_control
                    && self
                        .active
                        .values()
                        .filter(|w| w.claude_router.is_some() || w.observer.is_some())
                        .count()
                        >= crate::claude_reconnect::HOST_LIMIT))
        {
            return Err("Claude connection is waiting for an available worker".into());
        }
        let p = self.config.provider(chat.agent);
        let mut env = crate::discovery::environment(chat.agent, &p.profile);
        let mut args = crate::discovery::isolated_args(chat.agent);
        if chat.agent == AgentKind::Claude {
            crate::claude_permissions::apply(&mut args, &chat.claude_permission_rules)?;
            p.tools.apply(&mut args);
            env.insert(
                "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".into(),
                "true".into(),
            );
            args.push("--replay-user-messages".into());
        }
        if chat.agent == AgentKind::Claude {
            args.extend([
                "--tools".into(),
                "Read,Write,Edit,Bash,Glob,Grep,AskUserQuestion,EnterPlanMode,ExitPlanMode".into(),
                "--model".into(),
                s(&chat.thread["modelSelection"]["model"]).to_owned(),
                "--thinking-display".into(),
                "summarized".into(),
                "--permission-mode".into(),
                match (
                    s(&chat.thread["interactionMode"]),
                    s(&chat.thread["runtimeMode"]),
                ) {
                    ("plan", _) => "plan",
                    (_, "full-access") => "bypassPermissions",
                    (_, "auto-accept-edits") => "acceptEdits",
                    _ => "default",
                }
                .into(),
                if chat.resumed || chat.fork_from.is_some() || chat.remote_identity.is_some() {
                    "--resume"
                } else {
                    "--session-id"
                }
                .into(),
                chat.fork_from
                    .as_ref()
                    .map(|f| f.session.clone())
                    .unwrap_or_else(|| chat.native_id.clone()),
            ]);
            if let Some(fork) = &chat.fork_from {
                args.extend([
                    "--fork-session".into(),
                    "--session-id".into(),
                    chat.native_id.clone(),
                    "--resume-session-at".into(),
                    fork.point.clone(),
                    "--name".into(),
                    s(&chat.thread["title"]).into(),
                ]);
            }
        }
        if chat.agent == AgentKind::Claude {
            if let Some(effort) = crate::models::effort(&chat.thread["modelSelection"]) {
                args.extend(["--effort".into(), effort.into()]);
            }
        }
        let mut connection = p
            .connect(Launch {
                agent: if chat.agent == AgentKind::Codex {
                    Agent::Codex
                } else {
                    Agent::Claude
                },
                executable: p.executable.clone(),
                cwd: PathBuf::from(s(&chat.thread["worktreePath"])),
                env,
                extra_args: args,
            })
            .await?;
        let hosted = chat.agent == AgentKind::Claude && p.remote_control;
        if hosted {
            connection = Connection::host_claude_configured(
                connection,
                chat.native_id.clone(),
                chat.remote_identity.clone(),
                s(&chat.thread["title"]).chars().take(120).collect(),
                chat.disabled_mcp.clone(),
            );
        }
        let init = if chat.agent == AgentKind::Codex {
            json!({"id":"init","method":"initialize","params":{"clientInfo":{"name":"remodex_native_test","version":"0.0.0"},"capabilities":{"experimentalApi":true}}})
        } else {
            json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}})
        };
        connection
            .send(&init)
            .await
            .map_err(|_| "CLI initialization write failed")?;
        self.active.insert(
            id.into(),
            Worker {
                connection,
                chat_id: id.into(),
                stage: "init",
                native_turn: String::new(),
                owns_turn: true,
                pre_turn_events: Default::default(),
                pre_turn_bytes: 0,
                requests: HashMap::new(),
                steers: HashMap::new(),
                last_prompt: uuid::Uuid::new_v4().to_string(),
                first_prompt: String::new(),
                deadline: Some(Instant::now() + Duration::from_secs(if hosted { 35 } else { 25 })),
                stopping: false,
                metadata: metadata::Reads::default(),
                control_reply: None,
                startup_tools: if hosted {
                    vec![]
                } else {
                    chat.disabled_mcp.clone()
                },
                checkpoint_labels: HashMap::new(),
                unsupported_controls: 0,
                claude_router: hosted.then(Default::default),
                claude_background: Default::default(),
                claude_deferred_result: None,
                claude_stop: None,
                local_turn: None,
                recovering,
                observer: None,
            },
        );
        if hosted && !recovering {
            prepare_claude_local(self.active.get_mut(id).unwrap(), chat)?;
        }
        Ok(())
    }
    async fn steer(
        &mut self,
        id: &str,
        c: Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        if self.chats[id].external_claude {
            return self.steer_external_claude(id, c, receipt).await;
        }
        if self.active.get(id).unwrap().steers.len() >= 8 {
            return Err("Wait for pending steering messages".into());
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let routed = if let Some(router) = self.active[id].claude_router.as_ref() {
            let mut router = router.clone();
            router.register(crate::claude_turns::Input {
                native: uuid.clone(),
                message: s(&c["message"]["messageId"]).into(),
                public_turn: s(&self.chats[id].thread["session"]["activeTurnId"]).into(),
                local: true,
                steer: true,
            })?;
            Some(router)
        } else {
            None
        };
        let old = self.chats[id].clone();
        let chat = self.chats.get_mut(id).unwrap();
        projection::message(
            chat,
            s(&c["message"]["messageId"]),
            &crate::attachments::display_message(&c["message"]),
            "user",
            false,
            false,
        );
        projection::activity(
            chat,
            &uuid,
            "steer.updated",
            "Steering message sent",
            "info",
            json!({"status":"pending","detail":"Waiting for CLI confirmation"}),
        );
        if let Err(e) = self.save(id, receipt).await {
            self.chats.insert(id.into(), old);
            return Err(e);
        }
        let w = self.active.get_mut(id).unwrap();
        if let Some(router) = routed {
            w.claude_router = Some(router);
        }
        w.steers.insert(uuid.clone(), false);
        w.last_prompt = uuid.clone();
        if chat_agent(&self.chats, id) == AgentKind::Claude && w.checkpoint_labels.len() < 256 {
            w.checkpoint_labels.insert(
                uuid.clone(),
                s(&c["message"]["text"]).chars().take(120).collect(),
            );
        }
        let request = if chat_agent(&self.chats, id) == AgentKind::Codex {
            json!({"id":uuid,"method":"turn/steer","params":{"threadId":self.chats[id].native_id,"expectedTurnId":w.native_turn,"clientUserMessageId":c["message"]["messageId"],"input":crate::attachments::input(&c["message"],true)?}})
        } else {
            json!({"type":"user","uuid":uuid,"priority":"now","session_id":self.chats[id].native_id,"parent_tool_use_id":null,"message":{"role":"user","content":crate::attachments::input(&c["message"],false)?}})
        };
        if self.chats[id].agent == AgentKind::Claude {
            let cid = id.to_owned();
            let session = self.chats[id].native_id.clone();
            let input = crate::claude_turns::Input {
                native: uuid.clone(),
                message: s(&c["message"]["messageId"]).into(),
                public_turn: s(&self.chats[id].thread["session"]["activeTurnId"]).into(),
                local: true,
                steer: true,
            };
            let payload = request.clone();
            if !self
                .db
                .call(move |a| a.store.offer_claude_input(&cid, &session, &input, &payload))
                .await??
            {
                return Err("Steering delivery was already offered; it was not resent".into());
            }
        }
        if w.connection.send(&request).await.is_err() {
            self.end(
                id,
                "error",
                Some("Steer delivery is uncertain. It was not retried."),
            )
            .await;
        }
        Ok(())
    }
    async fn answer(
        &mut self,
        id: &str,
        c: &Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        let w = self.active.get(id).ok_or("No active question")?;
        let rid = s(&c["requestId"]);
        let original = w.requests.get(rid).ok_or("Question expired")?;
        let tool_call_id = original["request"]["tool_use_id"].clone();
        let response = crate::questions::answer(original, &c["answers"])?;
        // Reserve the receipt before delivery, so a lost response cannot resubmit.
        self.save(id, receipt).await?;
        let w = self.active.get_mut(id).unwrap();
        if w.connection.send(&response).await.is_err() {
            self.end(
                id,
                "error",
                Some("Answer delivery is uncertain. It was not retried."),
            )
            .await;
            return Err("Answer delivery is uncertain".into());
        }
        w.requests.remove(rid);
        let chat = self.chats.get_mut(id).unwrap();
        chat.thread["hasPendingUserInput"] =
            json!(w.requests.values().any(crate::questions::is_input));
        projection::activity(
            chat,
            rid,
            "user-input.resolved",
            "Answer sent",
            "info",
            json!({"requestId":rid,"status":"completed","data":{"toolCallId":tool_call_id}}),
        );
        self.save(id, None).await
    }
    async fn approve(
        &mut self,
        id: &str,
        c: &Value,
        receipt: Option<(String, String)>,
    ) -> Result<(), String> {
        let w = self.active.get(id).ok_or("No active approval")?;
        let rid = s(&c["requestId"]);
        let original = w.requests.get(rid).cloned().ok_or("Approval expired")?;
        if crate::questions::is_input(&original) {
            return Err("Answer this question using its answer controls".into());
        }
        let decision = s(&c["decision"]);
        let remember = ["approved_for_session", "acceptForSession"].contains(&decision);
        let grant = if remember {
            if self.chats[id].agent != AgentKind::Claude {
                return Err("Choose Approve once, Decline or Cancel turn for this request".into());
            }
            Some(crate::claude_permissions::suggested(&original).ok_or(
                "Claude did not provide a reusable permission for this action. Choose Approve once.",
            )?)
        } else {
            None
        };
        if !remember && !["approved", "declined", "accept", "decline", "cancel"].contains(&decision)
        {
            return Err("Choose an available approval action".into());
        }
        let cancel = decision == "cancel";
        let background = self.background_tasks(id)?;
        let before = self.chats[id].clone();
        if let Some(grant) = &grant {
            if !before.external_claude {
                self.chats.get_mut(id).unwrap().claude_permission_rules =
                    crate::claude_permissions::merged(
                        &before.claude_permission_rules,
                        &grant.rules,
                    )?;
            }
        }
        let stop_id = if cancel {
            observer::record_stop(self.chats.get_mut(id).unwrap(), background.stop_targets())
        } else {
            String::new()
        };
        if cancel {
            self.chats.get_mut(id).unwrap().paused = true;
        }
        if let Err(error) = self.save(id, receipt).await {
            self.chats.insert(id.into(), before);
            return Err(error);
        }
        let allowed = remember || ["approved", "accept"].contains(&decision);
        let mut claude_response =
            json!({"behavior":"allow","updatedInput":original["request"]["input"]});
        if let Some(grant) = grant {
            claude_response["updatedPermissions"] = json!(grant.updates);
        }
        let request = if chat_agent(&self.chats, id) == AgentKind::Codex {
            json!({"id":original["id"],"result":{"decision":if allowed {"accept"}else if cancel {"cancel"}else{"decline"}}})
        } else {
            json!({"type":"control_response","response":{"subtype":"success","request_id":original["request_id"],"response":if allowed {claude_response}else{json!({"behavior":"deny","message":"User declined this action"})}}})
        };
        let w = self.active.get_mut(id).unwrap();
        if cancel {
            w.stopping = true;
            w.deadline = Some(Instant::now() + Duration::from_secs(10));
        }
        w.connection
            .send(&request)
            .await
            .map_err(|_| "Approval response could not be delivered; it was not retried")?;
        w.requests.remove(rid);
        if cancel && self.chats[id].agent == AgentKind::Claude {
            let interrupt = json!({"type":"control_request","request_id":stop_id,"request":{"subtype":"interrupt","cancel_queued":w.claude_router.is_some()}});
            let sequence = crate::claude_background::StopSequence::new(
                self.chats[id]
                    .external_stop
                    .as_ref()
                    .unwrap()
                    .background
                    .clone(),
                interrupt,
            );
            let request = sequence.first(&background)?;
            w.claude_stop = Some(sequence);
            w.connection
                .send(&request)
                .await
                .map_err(|_| "CLI disconnected while cancelling")?;
        }
        let chat = self.chats.get_mut(id).unwrap();
        if allowed && original["request"]["tool_name"] == "ExitPlanMode" {
            chat.thread["interactionMode"] = json!("default");
        }
        chat.thread["hasPendingApprovals"] =
            json!(w.requests.values().any(|r| !crate::questions::is_input(r)));
        let mut resolved = json!({"requestId":rid,"status":"completed","data":{"toolCallId":if chat.agent == AgentKind::Codex {&original["params"]["itemId"]}else{&original["request"]["tool_use_id"]}}});
        if chat.agent == AgentKind::Claude {
            resolved["data"]["nativeRequestId"] = original["request_id"].clone();
        }
        projection::activity(
            chat,
            rid,
            "approval.resolved",
            if allowed {
                "Approved"
            } else if cancel {
                "Cancelled"
            } else {
                "Declined"
            },
            "info",
            resolved,
        );
        self.save(id, None).await
    }
    async fn event(&mut self, id: &str, event: Option<Event>) -> Result<(), String> {
        if self.chats.get(id).is_some_and(|c| c.external_claude) {
            return self.event_external_claude(id, event).await;
        }
        let Some(w) = self.active.get(id) else {
            return Ok(());
        };
        let id = w.chat_id.clone();
        let mut v = match event {
            Some(Event::Message(v)) => v,
            Some(Event::ClaudeRemote(_)) => {
                return Err("External Claude projection is not attached to this worker".into())
            }
            _ => {
                if self.chats[&id].agent == AgentKind::Codex
                    && self.config.codex.shared_server.is_some()
                    && !w.native_turn.is_empty()
                {
                    self.detach_shared_connection(&id).await?;
                    return Ok(());
                }
                self.end(
                    &id,
                    "error",
                    Some("CLI disconnected before confirming completion. No prompt was replayed."),
                )
                .await;
                return Ok(());
            }
        };
        if let Some(w) = self.active.get_mut(&id) {
            if let Some(router) = w.claude_router.as_mut() {
                router.filter_interruption_notice(&mut v, w.stopping);
            }
        }
        if self.chats[&id].agent == AgentKind::Claude {
            let native = self.chats[&id].native_id.clone();
            if v["session_id"].as_str().is_some_and(|s| s != native) {
                return Err("Claude event belongs to another session".into());
            }
            if crate::native_control::matches(&self.chats[&id], &v) {
                let before_control = self.chats[&id].clone();
                crate::native_control::apply(self.chats.get_mut(&id).unwrap(), &v);
                if let Err(error) = self.save(&id, None).await {
                    self.chats.insert(id.clone(), before_control);
                    return Err(error);
                }
                let action = &self.chats[&id].native_control.as_ref().unwrap().action;
                crate::capabilities::control_seed(
                    &mut self.active.get_mut(&id).unwrap().metadata.capability_seed,
                    action,
                    &v,
                );
                self.complete_native_control(&id).await;
                return Ok(());
            }
            if let Some(stop) = &self.chats[&id].external_stop {
                let w = self.active.get_mut(&id).unwrap();
                if let Some(router) = &mut w.claude_router {
                    router.acknowledge_task_stop(&v, &stop.background);
                } else {
                    w.claude_background.acknowledge_stop(&v, &stop.background);
                }
            }
            let cid = id.clone();
            let frame = v.clone();
            self.db
                .call(move |a| a.store.acknowledge_claude_input(&cid, &native, &frame))
                .await?
                .map_err(|_| "Could not save Claude delivery confirmation")?;
        }
        if self.chats[&id].agent == AgentKind::Claude && self.active[&id].claude_router.is_none() {
            let w = self.active.get_mut(&id).unwrap();
            w.claude_background.observe(&v)?;
            if v["type"] == "result"
                && v["parent_tool_use_id"].is_null()
                && w.claude_background.busy()
            {
                if v.to_string().len() > 256 * 1024 {
                    return Err(
                        "Claude completion exceeds its bounded background-task budget".into(),
                    );
                }
                w.claude_deferred_result = Some(v);
                return Ok(());
            }
        }
        let hosted_change = if self.active[&id].claude_router.is_some() {
            Some(self.route_claude(&id, &v).await?)
        } else {
            None
        };
        let w = self.active.get_mut(&id).unwrap();
        let codex = self.chats[&id].agent == AgentKind::Codex;
        // Shared servers may notify several clients. Never project another
        // chat's response, approve its tools or finish this worker for its turn.
        if codex && v["method"].is_string() {
            let native = &self.chats[&id].native_id;
            let thread = v["params"]["threadId"]
                .as_str()
                .or_else(|| v["params"]["thread"]["id"].as_str());
            if thread.is_some_and(|thread| thread != native.as_str()) {
                return Ok(());
            }
            let turn = v["params"]["turnId"]
                .as_str()
                .or_else(|| v["params"]["turn"]["id"].as_str());
            if !w.native_turn.is_empty() && turn.is_some_and(|turn| turn != w.native_turn) {
                return Ok(());
            }
            if self.config.codex.shared_server.is_some()
                && (s(&v["method"]).starts_with("item/")
                    || s(&v["method"]).starts_with("turn/")
                    || s(&v["method"]).starts_with("mcpServer/elicitation/"))
            {
                if w.stage != "running" && w.stage != "attach-resume" {
                    return Ok(());
                }
                if w.native_turn.is_empty() || w.stage == "attach-resume" {
                    // Only our turn/start RPC response establishes ownership.
                    // Hold early notifications until its ID arrives, then apply
                    // the same thread/turn filters without losing early output.
                    let bytes = v.to_string().len();
                    if w.pre_turn_events.len() >= 64
                        || w.pre_turn_bytes.saturating_add(bytes) > crate::agent::MAX_FRAME
                    {
                        return Err("Shared Codex sent too much output before confirming the turn. Prompt delivery is uncertain; no retry was made.".into());
                    }
                    w.pre_turn_bytes += bytes;
                    w.pre_turn_events.push_back((v, bytes));
                    return Ok(());
                }
            }
        }
        if !w.owns_turn && (v["id"] == "attach-init" || v["id"] == "attach-resume") {
            if v.get("error").is_some() {
                return Err(
                    "Could not attach to the shared task; Desktop is still in control".into(),
                );
            }
            if v["id"] == "attach-init" && w.stage == "attach-init" {
                w.connection
                    .send(&json!({"method":"initialized","params":{}}))
                    .await
                    .map_err(|_| "Shared controls disconnected")?;
                // No model, sandbox, approval or workspace overrides: join the
                // existing task exactly as its originating client configured it.
                w.connection.send(&json!({"id":"attach-resume","method":"thread/resume","params":{"threadId":self.chats[&id].native_id,"excludeTurns":true,"initialTurnsPage":{"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}}})).await.map_err(|_|"Shared controls disconnected")?;
                w.stage = "attach-resume";
                return Ok(());
            }
            if v["id"] == "attach-resume" && w.stage == "attach-resume" {
                let result = &v["result"];
                let turn = &result["initialTurnsPage"]["data"][0];
                if result["thread"]["id"] != self.chats[&id].native_id
                    || turn["id"] != w.native_turn
                {
                    return Err(
                        "The shared task changed while connecting; no control was sent".into(),
                    );
                }
                w.stage = "running";
                w.deadline = None;
                // A pending request may have been answered while this observer
                // was offline. Only an explicit native flag snapshot can clear
                // it; absence of status metadata is not evidence of resolution.
                let flags = result["thread"]["status"]["activeFlags"]
                    .as_array()
                    .filter(|flags| {
                        result["thread"]["status"]["type"] == "active"
                            && flags.iter().all(|f| {
                                matches!(
                                    f.as_str(),
                                    Some("waitingOnApproval" | "waitingOnUserInput")
                                )
                            })
                    });
                let stale = if let Some(flags) = flags {
                    let cid = id.clone();
                    let public = s(&self.chats[&id].thread["latestTurn"]["turnId"]).to_owned();
                    self.db
                        .call(move |a| a.store.pending_shared_controls(&cid, &public))
                        .await?
                        .map_err(|_| "Shared pending requests unavailable")?
                        .into_iter()
                        .filter(|r| {
                            let flag = if r["kind"] == "approval.requested" {
                                "waitingOnApproval"
                            } else {
                                "waitingOnUserInput"
                            };
                            !flags.iter().any(|f| f == flag)
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };
                let chat = self.chats.get_mut(&id).unwrap();
                for mut row in stale {
                    row["kind"] = json!(if row["kind"] == "approval.requested" {
                        "approval.resolved"
                    } else {
                        "user-input.resolved"
                    });
                    row["summary"] = json!("Handled in another client");
                    row["payload"]["status"] = json!("completed");
                    let rows = chat.thread["activities"].as_array_mut().unwrap();
                    if let Some(old) = rows.iter_mut().find(|r| r["id"] == row["id"]) {
                        *old = row;
                    } else {
                        rows.push(row);
                    }
                }
                if let Some(flags) = flags {
                    if !flags.iter().any(|f| f == "waitingOnApproval") {
                        chat.thread["hasPendingApprovals"] = json!(false);
                    }
                    if !flags.iter().any(|f| f == "waitingOnUserInput") {
                        chat.thread["hasPendingUserInput"] = json!(false);
                    }
                }
                if result["model"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() < 256)
                    .is_some()
                {
                    chat.thread["modelSelection"] =
                        crate::models::codex_ack(&chat.thread["modelSelection"], result);
                }
                if let Some(mode) = result["collaborationMode"]["mode"]
                    .as_str()
                    .filter(|m| ["default", "plan"].contains(m))
                {
                    chat.thread["interactionMode"] = json!(mode);
                }
                if result["approvalPolicy"] == "never" {
                    chat.thread["runtimeMode"] = json!("full-access");
                } else if result["approvalPolicy"] == "on-request" {
                    chat.thread["runtimeMode"] = json!("approval-required");
                }
                chat.thread["session"]["runtimeMode"] = chat.thread["runtimeMode"].clone();
                self.save(&id, None).await?;
                self.dirty = true;
                match s(&turn["status"]) {
                    "inProgress" => {}
                    "completed" => self.end(&id, "completed", None).await,
                    "interrupted" => self.end(&id, "interrupted", None).await,
                    "failed" => self.end(&id, "error", None).await,
                    _ => return Err("Shared task status is unknown; no control was sent".into()),
                }
                return Ok(());
            }
        }
        if codex && v["method"] == "serverRequest/resolved" {
            let resolved: Vec<_> = w
                .requests
                .iter()
                .filter(|(_, r)| r["id"] == v["params"]["requestId"])
                .map(|(rid, _)| rid.clone())
                .collect();
            for rid in resolved {
                w.requests.remove(&rid);
                let cid = id.clone();
                let row_id = rid.clone();
                let saved = self
                    .db
                    .call(move |a| a.store.history_row(&cid, "activities", &row_id))
                    .await?
                    .map_err(|_| "Shared approval history unavailable")?;
                let chat = self.chats.get_mut(&id).unwrap();
                if !chat.thread["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == rid)
                {
                    if let Some(saved) = saved {
                        chat.thread["activities"]
                            .as_array_mut()
                            .unwrap()
                            .push(saved);
                    }
                }
                if let Some(row) = chat.thread["activities"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|r| r["id"] == rid)
                {
                    row["kind"] = json!(if row["kind"] == "user-input.requested" {
                        "user-input.resolved"
                    } else {
                        "approval.resolved"
                    });
                    row["payload"]["status"] = json!("completed");
                }
            }
            let chat = self.chats.get_mut(&id).unwrap();
            chat.thread["hasPendingUserInput"] =
                json!(w.requests.values().any(crate::questions::is_input));
            chat.thread["hasPendingApprovals"] =
                json!(w.requests.values().any(|v| !crate::questions::is_input(v)));
            self.save(&id, None).await?;
            self.dirty = true;
            return Ok(());
        }
        if let Some((response, notice)) = crate::controls::automatic(&v, codex) {
            if !w.owns_turn && notice.is_some() {
                return Ok(());
            }
            w.connection
                .send(&response)
                .await
                .map_err(|_| "CLI control response could not be delivered")?;
            if let Some(message) = notice {
                w.unsupported_controls += 1;
                let exhausted = w.unsupported_controls >= 8;
                projection::activity(
                    self.chats.get_mut(&id).unwrap(),
                    &uuid::Uuid::new_v4().to_string(),
                    "session.notice",
                    "CLI request could not continue",
                    "warning",
                    json!({"detail":message}),
                );
                self.save(&id, None).await?;
                self.dirty = true;
                if exhausted {
                    return Err("The CLI repeatedly requested unsupported controls. The turn was stopped; history is retained.".into());
                }
            }
            return Ok(());
        }
        if self.chats[&id].agent == AgentKind::Claude
            && v["type"] == "user"
            && v["isSynthetic"] != true
            && v["parent_tool_use_id"].is_null()
        {
            let point = s(&v["uuid"]).to_owned();
            if point == w.first_prompt {
                let cid = id.clone();
                let public = s(&self.chats[&id].thread["latestTurn"]["turnId"]).to_owned();
                let native = point.clone();
                self.db
                    .call(move |a| a.store.native_turn(&cid, &public, &native))
                    .await?
                    .map_err(|_| "Could not save the verified native prompt")?;
            }
            if let Some(label) = w.checkpoint_labels.remove(&point) {
                let cid = id.clone();
                self.db
                    .call(move |a| a.store.checkpoint(&cid, &point, &label))
                    .await?
                    .map_err(|_| "Cannot save file checkpoint")?;
            }
        }
        if self.chats[&id].agent == AgentKind::Claude {
            match w.metadata.event(&v) {
                metadata::Reply::Capabilities => return Ok(()),
                metadata::Reply::Context(value) => {
                    self.report_usage(&id, true, value).await?;
                    return Ok(());
                }
                metadata::Reply::Other => {}
            }
        }
        let mut images = HashMap::new();
        if self.chats[&id].agent == AgentKind::Codex
            && crate::usage::project_codex(self.chats.get_mut(&id).unwrap(), &v)
        {
            self.save(&id, None).await?;
            self.dirty = true;
            return Ok(());
        }
        if self.chats[&id].agent == AgentKind::Claude
            && v["type"] == "user"
            && v["parent_tool_use_id"].is_null()
        {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for block in blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_result")
                    .take(32)
                {
                    let content = block["content"].clone();
                    let chat_id = id.clone();
                    let paths = self
                        .db
                        .call(move |a| a.store.capture_tool_images(&chat_id, &content))
                        .await??;
                    if !paths.is_empty() {
                        images.insert(s(&block["tool_use_id"]).to_owned(), paths);
                    }
                }
            }
        }
        if self.chats[&id].agent == AgentKind::Codex && v["method"] == "item/completed" {
            let item = &v["params"]["item"];
            if ["mcpToolCall", "dynamicToolCall", "imageView"].contains(&s(&item["type"])) {
                let native_item = item.clone();
                let chat_id = id.clone();
                let workspace = PathBuf::from(s(&self.chats[&id].thread["worktreePath"]));
                let paths = self
                    .db
                    .call(move |a| {
                        a.store
                            .codex_tool_images(&chat_id, &workspace, &native_item)
                    })
                    .await??;
                if !paths.is_empty() {
                    images.insert(s(&item["id"]).to_owned(), paths);
                }
            }
        }
        let agent = self.chats[&id].agent;
        let model_rows = if agent == AgentKind::Codex && v["id"] == "models" {
            Some(&v["result"]["data"])
        } else if agent == AgentKind::Claude && v["response"]["request_id"] == "init" {
            Some(&v["response"]["response"]["models"])
        } else {
            None
        };
        if let Some(rows) = model_rows.filter(|r| r.is_array()) {
            self.models.insert(
                agent.instance().into(),
                crate::models::catalog(
                    rows,
                    agent == AgentKind::Codex,
                    &self.config.provider(agent).model,
                ),
            );
        }
        if model_rows.is_some_and(|rows| rows.is_array()) {
            let data = serde_json::to_string(&self.models).map_err(|_| "Invalid model catalog")?;
            self.db.call(move |a|a.store.db.execute("INSERT INTO metadata(key,value) VALUES('native.models',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[data])).await?.map_err(|_|"Cannot save model catalog")?;
        }
        if agent == AgentKind::Codex && v["id"] == "models" {
            self.dirty = true;
            return Ok(());
        }
        // An update may target an older spilled tool/message. Fetch only its
        // exact row before applying the delta; never reload the whole turn.
        let mut ids = vec![
            s(&v["params"]["itemId"]).to_owned(),
            s(&v["params"]["item"]["id"]).to_owned(),
            s(&v["message"]["id"]).to_owned(),
            self.chats[&id].stream_id.clone(),
            s(&v["tool_use_id"]).to_owned(),
            format!(
                "claude-thinking-{}-{}",
                self.chats[&id].stream_id,
                v["event"]["index"].as_u64().unwrap_or(0)
            ),
        ];
        if let Some(blocks) = v["message"]["content"].as_array() {
            for b in blocks {
                ids.push(s(&b["tool_use_id"]).to_owned());
                ids.push(s(&b["id"]).to_owned());
            }
        }
        for key in ids.into_iter().filter(|k| !k.is_empty()) {
            for kind in ["messages", "activities", "proposedPlans"] {
                if !self.chats[&id].thread[kind]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == key)
                {
                    let cid = id.clone();
                    let row_id = key.clone();
                    if let Some(row) = self
                        .db
                        .call(move |a| a.store.history_row(&cid, kind, &row_id))
                        .await?
                        .map_err(|_| "History unavailable")?
                    {
                        self.chats.get_mut(&id).unwrap().thread[kind]
                            .as_array_mut()
                            .unwrap()
                            .push(row);
                    }
                }
            }
        }
        let chat = self.chats.get_mut(&id).unwrap();
        let codex = chat.agent == AgentKind::Codex;
        let mut control_updated = false;
        if !codex && v["type"] == "system" && v["subtype"] == "init" {
            if v["session_id"] != chat.native_id {
                return Err("Claude returned an unexpected session identity".into());
            }
            chat.resumed = true;
            if chat.sync_replacement.is_none() {
                chat.fork_from = None;
            }
            let data = serde_json::to_value(&*chat).unwrap();
            self.db
                .call(move |a| a.store.save_chat(&data, None))
                .await?
                .map_err(|_| "Could not persist Claude session identity")?;
        }
        let response = if codex {
            s(&v["id"])
        } else {
            s(&v["response"]["request_id"])
        };
        if !response.is_empty() && (v.get("error").is_some() || v["response"]["subtype"] == "error")
        {
            if w.steers.remove(response).is_some() {
                projection::activity(
                    chat,
                    response,
                    "steer.updated",
                    "Steering message was rejected",
                    "error",
                    json!({"status":"failed","detail":"The CLI rejected the steering message. The original task is still running."}),
                );
                self.save(&id, None).await?;
                self.dirty = true;
                return Ok(());
            }
            return Err(format!(
                "CLI rejected {response}: {}",
                if codex {
                    s(&v["error"]["message"])
                } else {
                    s(&v["response"]["error"])
                }
            ));
        }
        if response == "init" && w.stage == "init" {
            if codex {
                w.connection
                    .send(&json!({"method":"initialized","params":{}}))
                    .await
                    .map_err(|_| "CLI disconnected")?;
                w.connection
                    .send(&json!({"id":"models","method":"model/list","params":{"limit":100}}))
                    .await
                    .map_err(|_| "CLI disconnected")?;
                w.connection
                    .send(&json!({"id":"quota","method":"account/rateLimits/read","params":{}}))
                    .await
                    .map_err(|_| "CLI disconnected")?;
                if self.config.codex.shared_server.is_some() && !chat.native_id.is_empty() {
                    w.connection.send(&json!({"id":"shared-check","method":"thread/read","params":{"threadId":chat.native_id,"includeTurns":false}})).await.map_err(|_| "Shared Codex state check disconnected")?;
                    w.stage = "shared-check";
                } else {
                    w.connection
                        .send(&codex_thread_request(chat))
                        .await
                        .map_err(|_| "CLI disconnected")?;
                    w.stage = "thread";
                }
            } else if w.claude_router.is_some() {
                let identity: crate::claude_host::Identity = serde_json::from_value(
                    v["response"]["response"]["remodexRemoteIdentity"].clone(),
                )
                .map_err(|_| "Claude shared identity was not confirmed")?;
                identity.validate()?;
                if identity.native_session != chat.native_id
                    || chat
                        .remote_identity
                        .as_ref()
                        .is_some_and(|prior| prior != &identity)
                {
                    return Err("Claude shared identity changed; no prompt was sent".into());
                }
                chat.remote_identity = Some(identity.clone());
                let data = serde_json::to_value(&*chat).unwrap();
                self.db
                    .call(move |a| a.store.save_chat(&data, None))
                    .await?
                    .map_err(|_| "Cannot save Claude shared identity; no prompt was sent")?;
                w.connection
                    .send(&json!({"type":"remodex_identity_commit","identity":identity}))
                    .await
                    .map_err(|_| "Cannot confirm Claude shared identity")?;
                w.stage = "remote-commit";
            } else if !w.startup_tools.is_empty() {
                w.connection.send(&json!({"type":"control_request","request_id":"startup-mcp-list","request":{"subtype":"mcp_status"}})).await.map_err(|_|"Could not check this chat's tool settings")?;
                w.stage = "tool-list";
            } else {
                w.stage = "prompt";
            }
        }
        if !codex && response == "remote-commit" && w.stage == "remote-commit" {
            self.claude_recovery.connected(&id, Instant::now());
            if chat.thread["session"]["lastError"]
                == "Connecting a replacement Claude link; saved history is retained"
            {
                chat.thread["session"]["lastError"] = Value::Null;
                chat.thread["session"]["statusConfidence"] = json!("confirmed");
            }
            projection::activity(
                chat,
                "claude-native-sync",
                "claude.desktop-sync.updated",
                "Claude Desktop sync",
                "info",
                json!({"status":"connected"}),
            );
            if w.recovering {
                // Reattachment alone is never a turn or a queued prompt.
                // Preserve the saved completion/error and all pending drafts.
                w.stage = if w.claude_router.as_ref().unwrap().active().is_some() {
                    "running"
                } else {
                    "idle"
                };
                w.deadline = None;
            } else if !w.startup_tools.is_empty() {
                w.connection.send(&json!({"type":"control_request","request_id":"startup-mcp-list","request":{"subtype":"mcp_status"}})).await.map_err(|_|"Could not check this chat's tool settings")?;
                w.stage = "tool-list";
            } else {
                w.stage = "prompt";
            }
        }
        if !codex && response == "reuse-model" && w.stage == "reuse-model" {
            w.connection.send(&json!({"type":"control_request","request_id":"reuse-access","request":{"subtype":"set_permission_mode","mode":claude_permission(chat)}})).await.map_err(|_|"Could not apply Claude access mode")?;
            w.stage = "reuse-access";
        }
        if !codex && response == "reuse-access" && w.stage == "reuse-access" {
            w.connection.send(&json!({"type":"control_request","request_id":"reuse-effort","request":{"subtype":"apply_flag_settings","settings":{"effortLevel":crate::models::effort(&chat.thread["modelSelection"])}}})).await.map_err(|_|"Could not apply Claude effort")?;
            w.stage = "reuse-effort";
        }
        if !codex && response == "reuse-effort" && w.stage == "reuse-effort" {
            w.stage = "prompt";
        }
        if codex && response == "shared-check" && w.stage == "shared-check" {
            if !["idle", "notLoaded"].contains(&s(&v["result"]["thread"]["status"]["type"])) {
                return Err("Shared Codex chat is not idle. The new prompt was not delivered; its existing task was left running.".into());
            }
            w.connection
                .send(&codex_thread_request(chat))
                .await
                .map_err(|_| "CLI disconnected")?;
            w.stage = "thread";
        }
        if !codex && response == "startup-mcp-list" && w.stage == "tool-list" {
            let servers = v["response"]["response"]["mcpServers"]
                .as_array()
                .ok_or("Claude did not report configured MCP servers")?;
            w.startup_tools
                .retain(|name| servers.iter().any(|s| s["name"] == *name));
            w.stage = "tools";
        }
        if !codex
            && w.stage == "tools"
            && (response == "startup-mcp-list" || response.starts_with("startup-mcp-toggle-"))
        {
            if let Some(name) = w.startup_tools.pop() {
                w.connection.send(&json!({"type":"control_request","request_id":format!("startup-mcp-toggle-{}",w.startup_tools.len()),"request":{"subtype":"mcp_toggle","serverName":name,"enabled":false}})).await.map_err(|_|"Could not apply this chat's tool settings")?;
            } else {
                w.stage = "prompt";
            }
        }
        if codex && response == "thread" && w.stage == "thread" {
            if self.config.codex.shared_server.is_some()
                && v["result"]["thread"]["status"]["type"] != "idle"
            {
                return Err("Shared Codex chat is not idle. The new prompt was not delivered; its existing task was left running.".into());
            }
            chat.native_id = s(&v["result"]["thread"]["id"]).into();
            if chat.native_id.is_empty() {
                return Err("CLI did not return a thread ID".into());
            }
            chat.resumed = true;
            w.stage = "prompt";
        }
        if w.stage == "prompt" {
            if w.claude_router
                .as_ref()
                .is_some_and(|r| r.active().is_some())
            {
                w.stage = "waiting-external";
                w.deadline = None;
                return Ok(());
            }
            w.first_prompt = w.last_prompt.clone();
            let input = crate::attachments::input(&chat.pending_message, codex)?;
            if !codex {
                w.checkpoint_labels.insert(
                    w.last_prompt.clone(),
                    s(&chat.pending_message["text"]).chars().take(120).collect(),
                );
            }
            let mut request = if codex {
                json!({"id":"turn","method":"turn/start","params":{"threadId":chat.native_id,"clientUserMessageId":chat.pending_message["messageId"],"input":input,"model":chat.thread["modelSelection"]["model"],"effort":crate::models::effort(&chat.thread["modelSelection"]),"collaborationMode":{"mode":chat.thread["interactionMode"],"settings":{"model":chat.thread["modelSelection"]["model"],"reasoning_effort":crate::models::effort(&chat.thread["modelSelection"]),"developer_instructions":null}}}})
            } else {
                json!({"type":"user","uuid":w.last_prompt,"session_id":chat.native_id,"parent_tool_use_id":null,"message":{"role":"user","content":input}})
            };
            if codex {
                crate::models::codex_tier(&mut request["params"], &chat.thread["modelSelection"]);
            }
            // Persist the native session identity before offering a prompt.
            let data = serde_json::to_value(&*chat).unwrap();
            self.db
                .call(move |a| a.store.save_chat(&data, None))
                .await?
                .map_err(|_| "Cannot persist native session")?;
            if !codex {
                let cid = id.clone();
                let session = chat.native_id.clone();
                let input = crate::claude_turns::Input {
                    native: w.last_prompt.clone(),
                    message: s(&chat.pending_message["messageId"]).into(),
                    public_turn: s(&chat.thread["latestTurn"]["turnId"]).into(),
                    local: true,
                    steer: false,
                };
                let payload = request.clone();
                if !self
                    .db
                    .call(move |a| a.store.offer_claude_input(&cid, &session, &input, &payload))
                    .await??
                {
                    return Err("Prompt delivery was already offered; it was not resent".into());
                }
            }
            w.connection
                .send(&request)
                .await
                .map_err(|_| "Prompt delivery uncertain; no automatic retry")?;
            if let Some(local) = w.local_turn.as_mut() {
                local.delivered = true;
            }
            w.stage = "running";
            w.deadline = if codex {
                Some(Instant::now() + Duration::from_secs(25))
            } else {
                None
            };
            chat.thread["session"]["status"] = json!("running");
            chat.thread["latestTurn"]["startedAt"] = json!(now());
        }
        if codex && (response == "turn" || v["method"] == "turn/started") {
            let turn = if response == "turn" {
                &v["result"]["turn"]
            } else {
                &v["params"]["turn"]
            };
            w.native_turn = s(&turn["id"]).into();
            let cid = id.clone();
            let public = s(&chat.thread["latestTurn"]["turnId"]).to_owned();
            let native = w.native_turn.clone();
            self.db
                .call(move |a| a.store.native_turn(&cid, &public, &native))
                .await?
                .map_err(|_| "Could not save the verified native turn")?;
            if !w.stopping {
                w.deadline = None;
            }
        }
        if codex && w.steers.contains_key(response) {
            if v["result"]["turnId"] != w.native_turn || w.native_turn.is_empty() {
                w.steers.remove(response);
                projection::activity(
                    chat,
                    response,
                    "steer.updated",
                    "Steering confirmation did not match this turn",
                    "error",
                    json!({"status":"failed","detail":"The CLI did not confirm delivery to the current turn. The original question or task is still active."}),
                );
                self.save(&id, None).await?;
                self.dirty = true;
                return Ok(());
            }
            control_updated = w.steers.get(response) == Some(&false);
            w.steers.insert(response.into(), true);
            if control_updated {
                // Codex queues steer input behind an outstanding user-input
                // tool. Release only those questions, after same-turn acceptance,
                // with no invented answer and without approving any tool action.
                let questions: Vec<_> = w
                    .requests
                    .iter()
                    .filter(|(_, r)| crate::questions::is_question(r))
                    .map(|(key, r)| (key.clone(), r["id"].clone()))
                    .collect();
                for (key, rid) in questions {
                    w.connection
                        .send(&json!({"id":rid,"result":{"answers":{}}}))
                        .await
                        .map_err(|_| {
                            "Steer was accepted, but the pending question could not be released"
                        })?;
                    w.requests.remove(&key);
                    projection::activity(
                        chat,
                        &key,
                        "user-input.resolved",
                        "Question superseded by steering",
                        "info",
                        json!({"requestId":key,"status":"cancelled"}),
                    );
                }
                chat.thread["hasPendingUserInput"] =
                    json!(w.requests.values().any(crate::questions::is_input));
            }
            projection::activity(
                chat,
                response,
                "steer.updated",
                "Steering message received",
                "info",
                json!({"status":"completed"}),
            );
        }
        if !codex {
            // Only native consumption evidence confirms priority input.
            for uuid in consumed(&v) {
                if w.steers.contains_key(&uuid) {
                    control_updated |= w.steers.get(&uuid) == Some(&false);
                    w.steers.insert(uuid.clone(), true);
                    projection::activity(
                        chat,
                        &uuid,
                        "steer.updated",
                        "Steering message received",
                        "info",
                        json!({"status":"completed"}),
                    );
                }
            }
        }
        let is_approval = crate::controls::is_approval(&v, codex);
        let is_question = crate::questions::is_input(&v);
        if is_approval || is_question {
            if v.to_string().len() > 64 * 1024 {
                return Err(
                    "Approval details exceed the native test limit; the action was not approved"
                        .into(),
                );
            }
            if w.requests.len() >= 16 {
                return Err("CLI approval capacity exceeded".into());
            }
            let native_id = if codex {
                v["id"].to_string()
            } else {
                s(&v["request_id"]).to_owned()
            };
            if native_id.is_empty() || native_id.len() > 256 || native_id == "null" {
                return Err("CLI returned an invalid control request ID".into());
            }
            if let Some(previous) = w.requests.values().find(|r| {
                if codex {
                    r["id"] == v["id"]
                } else {
                    r["request_id"] == v["request_id"]
                }
            }) {
                if previous == &v {
                    return Ok(());
                }
                return Err(
                    "CLI reused a pending control request ID with different details".into(),
                );
            }
            // Native IDs restart with each worker. Public IDs must remain unique
            // across the durable transcript, including older pages.
            let prior = if codex && self.config.codex.shared_server.is_some() {
                let cid = id.clone();
                let turn = s(&chat.thread["latestTurn"]["turnId"]).to_owned();
                let native = v["id"].clone();
                self.db
                    .call(move |a| a.store.pending_native_control(&cid, &turn, &native))
                    .await?
                    .map_err(|_| "Shared approval receipt unavailable")?
            } else {
                None
            };
            let rid = prior
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("request-{}", uuid::Uuid::new_v4()));
            if let Some(prior) = prior {
                if !chat.thread["activities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == rid)
                {
                    chat.thread["activities"]
                        .as_array_mut()
                        .unwrap()
                        .push(prior);
                }
            }
            let detail = if codex {
                v["params"].clone()
            } else {
                v["request"]["input"].clone()
            };
            w.requests.insert(rid.clone(), v.clone());
            control_updated = true;
            if is_question {
                chat.thread["hasPendingUserInput"] = json!(true);
            } else {
                chat.thread["hasPendingApprovals"] = json!(true);
            }
            let tool = s(&v["request"]["tool_name"]);
            let kind = if ["Write", "Edit", "MultiEdit"].contains(&tool)
                || s(&v["method"]).contains("fileChange")
            {
                "file-change"
            } else if tool == "Read" {
                "file-read"
            } else {
                "command"
            };
            if is_question {
                projection::activity(
                    chat,
                    &rid,
                    "user-input.requested",
                    "Your input is needed",
                    "info",
                    json!({"requestId":rid,"status":"pending","questions":crate::questions::questions(&v)?,"data":{"toolCallId":if codex{&v["params"]["itemId"]}else{&v["request"]["tool_use_id"]},"nativeRequestId":if codex{v["id"].clone()}else{Value::Null}}}),
                );
            } else {
                projection::activity(
                    chat,
                    &rid,
                    "approval.requested",
                    "Approval required",
                    "approval",
                    json!({"requestId":rid,"status":"pending","requestKind":kind,"providerName":chat.agent.instance(),"canApproveForSession":!codex && crate::claude_permissions::suggested(&v).is_some(),"detail":projection::approval_detail(&v,codex),"command":detail["command"],"availableDecisions":crate::claude_permissions::decisions(!codex && crate::claude_permissions::suggested(&v).is_some()),"data":{"toolCallId":if codex{&v["params"]["itemId"]}else{&v["request"]["tool_use_id"]},"nativeRequestId":if codex{v["id"].clone()}else{Value::Null}}}),
                );
            }
        }
        // Bound the retained projection even when a single CLI frame is huge.
        // The CLI's own transcript remains in its isolated profile; the app
        // reports the unsupported size explicitly instead of silently clipping.
        let mut projected = chat.clone();
        if codex
            && self.config.codex.shared_server.is_some()
            && ["item/started", "item/completed"].contains(&s(&v["method"]))
            && v["params"]["item"]["type"] == "userMessage"
        {
            let item = &v["params"]["item"];
            let cid = id.clone();
            let mid = s(&item["clientId"]).to_owned();
            let accepted = !mid.is_empty()
                && self
                    .db
                    .call(move |a| a.store.history_row(&cid, "messages", &mid))
                    .await?
                    .map_err(|_| "Shared prompt receipt unavailable")?
                    .is_some();
            if !accepted {
                let text = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|v| v["type"] == "text")
                    .map(|v| s(&v["text"]))
                    .collect::<Vec<_>>()
                    .join("\n");
                projection::message(&mut projected, s(&item["id"]), &text, "user", false, false);
            }
        }
        projection::project(&mut projected, &v);
        projection::interrupted_tools(&mut projected, &v, w.stopping);
        for (tool_id, paths) in images {
            if let Some(row) = projected.thread["activities"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == tool_id)
            {
                row["payload"]["data"]["toolImages"] = json!(paths);
            }
            projection::tool_image_gallery(&mut projected, &tool_id);
        }
        if ["messages", "activities", "proposedPlans"]
            .iter()
            .any(|kind| {
                projected.thread[kind]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r.to_string().len() > crate::chat_store::ROW_BYTES)
            })
        {
            return Err("A single output item exceeds the 384 KiB display limit. Earlier history is retained; full output remains in the isolated CLI profile.".into());
        }
        *chat = projected;
        self.dirty = true;
        let _ = control_updated;
        let data = serde_json::to_value(&*chat).unwrap();
        let cid = id.clone();
        let saved = self
            .db
            .call(move |a| {
                a.store.save_chat(&data, None)?;
                a.store.load_chat(&cid)
            })
            .await?
            .map_err(|_| "Could not save streamed history")?;
        chat.thread = saved["thread"].clone();
        self.db.call(|a| a.store.check_history(0)).await??;
        if !codex {
            self.advance_task_stop(&id, &v).await?;
        }
        let w = self.active.get_mut(&id).unwrap();
        let chat = self.chats.get_mut(&id).unwrap();
        let completed = if codex {
            v["method"] == "turn/completed"
        } else {
            v["type"] == "result"
        };
        if completed
            || hosted_change
                .as_ref()
                .is_some_and(|change| change.completed)
        {
            if let Some(change) = hosted_change {
                if change.completed {
                    let state = if w.stopping
                        || ["aborted_tools", "aborted_streaming"]
                            .contains(&s(&v["terminal_reason"]))
                    {
                        "interrupted"
                    } else if let Some(state) = change.outcome.as_deref() {
                        state
                    } else if v["is_error"] == true {
                        "error"
                    } else {
                        "completed"
                    };
                    let state = state.to_owned();
                    self.complete_claude_host(&id, change, &state).await?;
                }
                return Ok(());
            }
            if !codex && !w.stopping && !w.steers.is_empty() && v["is_error"] != true {
                let consumed = consumed(&v);
                if w.steers.values().any(|confirmed| !*confirmed)
                    || !consumed.contains(&w.last_prompt)
                {
                    return Ok(());
                }
            }
            chat.resumed = true;
            let state = if w.stopping {
                "interrupted"
            } else if codex {
                match s(&v["params"]["turn"]["status"]) {
                    "interrupted" => "interrupted",
                    "failed" => "error",
                    _ => "completed",
                }
            } else if ["aborted_tools", "aborted_streaming"].contains(&s(&v["terminal_reason"])) {
                "interrupted"
            } else if v["is_error"] == true {
                "error"
            } else {
                "completed"
            };
            let error = if state == "error" {
                Some("The CLI reported an unsuccessful turn")
            } else {
                None
            };
            self.end(&id, state, error).await;
        }
        Ok(())
    }
    async fn route_claude(
        &mut self,
        id: &str,
        v: &Value,
    ) -> Result<crate::claude_turns::Change, String> {
        let w = self.active.get_mut(id).unwrap();
        let router = w.claude_router.as_mut().unwrap();
        let chat = self.chats.get_mut(id).unwrap();
        if let Some(native) = crate::claude_turns::input_identity(v)? {
            let known = router.pending().any(|i| i.native == native);
            if !known {
                let public = format!("claude-{native}");
                if router.register(crate::claude_turns::Input {
                    native: native.clone(),
                    message: native.clone(),
                    public_turn: public.clone(),
                    local: false,
                    steer: false,
                })? && w.stage == "idle"
                {
                    w.stage = "awaiting-native";
                }
            }
        }
        if let Some((native, text)) = crate::claude_turns::user_input(v)? {
            if let Some(input) = router.pending().find(|i| i.native == native && !i.local) {
                // The native start may have already registered this input.
                // Local echoes retain the original Android message identity.
                if !chat.thread["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == native)
                {
                    let cid = id.to_owned();
                    let mid = native.clone();
                    if let Some(saved) = self
                        .db
                        .call(move |a| a.store.history_row(&cid, "messages", &mid))
                        .await?
                        .map_err(|_| "Claude input history unavailable")?
                    {
                        chat.thread["messages"].as_array_mut().unwrap().push(saved);
                    }
                }
                projection::message(chat, &native, &text, "user", false, false);
                if let Some(row) = chat.thread["messages"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|m| m["id"] == native)
                {
                    row["turnId"] = json!(input.public_turn);
                }
            }
        }
        let change = router.observe(v)?;
        if let Some(turn) = &change.turn {
            w.recovering = false;
            if chat.thread["session"]["activeTurnId"] != turn.public_turn {
                if let Some(local) = w
                    .local_turn
                    .as_ref()
                    .filter(|l| l.latest["turnId"] == turn.public_turn)
                {
                    chat.thread["latestTurn"] = local.latest.clone();
                } else {
                    chat.thread["latestTurn"] = json!({"turnId":turn.public_turn,"state":"running","requestedAt":now(),"startedAt":now(),"completedAt":null,"assistantMessageId":null});
                }
                chat.stream_id.clear();
                chat.stream_model.clear();
            }
            chat.thread["session"]["status"] = json!("running");
            chat.thread["session"]["activeTurnId"] = json!(turn.public_turn);
            chat.thread["sharedTurnOwner"] = json!(if turn.local { "android" } else { "desktop" });
            chat.thread["latestTurn"]["startedAt"] = chat.thread["latestTurn"]["startedAt"]
                .as_str()
                .map(|s| json!(s))
                .unwrap_or(json!(now()));
            if ["idle", "awaiting-native"].contains(&w.stage) {
                w.stage = "running";
            }
            for message in &change.messages {
                let cid = id.to_owned();
                let mid = message.clone();
                let saved = self
                    .db
                    .call(move |a| a.store.history_row(&cid, "messages", &mid))
                    .await?
                    .map_err(|_| "Claude input history unavailable")?;
                if !chat.thread["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == *message)
                {
                    if let Some(row) = saved {
                        chat.thread["messages"].as_array_mut().unwrap().push(row);
                    }
                }
                if let Some(row) = chat.thread["messages"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|m| m["id"] == *message)
                {
                    row["turnId"] = json!(turn.public_turn);
                }
            }
            let cid = id.to_owned();
            let public = turn.public_turn.clone();
            let native = turn.inputs[0].clone();
            self.db
                .call(move |a| a.store.native_turn(&cid, &public, &native))
                .await?
                .map_err(|_| "Claude consumed input mapping could not be saved")?;
        }
        if let Some(local) = w.local_turn.as_ref() {
            if change
                .cancelled
                .iter()
                .any(|m| *m == s(&local.message["messageId"]))
                && router
                    .active()
                    .is_none_or(|turn| !turn.inputs.contains(&local.native))
            {
                // A prompt cancelled in the native pre-start queue has no
                // result frame. Finish only its own visible turn; an already
                // running Desktop turn still waits for its native result.
                if chat.thread["session"]["activeTurnId"] == local.latest["turnId"] {
                    finish(chat, "interrupted", None);
                }
                w.local_turn = None;
            }
        }
        if !change.cancelled.is_empty() && router.active().is_none() && w.local_turn.is_none() {
            w.stage = if router.pending().next().is_some() {
                "awaiting-native"
            } else {
                "idle"
            };
            if w.stage == "idle" {
                w.stopping = false;
                w.deadline = None;
                w.requests.clear();
                w.steers.clear();
            }
        }
        Ok(change)
    }
    async fn complete_claude_host(
        &mut self,
        id: &str,
        change: crate::claude_turns::Change,
        state: &str,
    ) -> Result<(), String> {
        let w = self.active.get_mut(id).unwrap();
        let chat = self.chats.get_mut(id).unwrap();
        let previous_stage = w.stage;
        chat.resumed = true;
        finish(
            chat,
            state,
            if state == "error" {
                Some("The CLI reported an unsuccessful turn")
            } else {
                None
            },
        );
        if let Some(local) = w.local_turn.as_ref() {
            if change
                .turn
                .as_ref()
                .is_some_and(|t| t.inputs.contains(&local.native))
            {
                w.local_turn = None;
            }
        }
        w.requests.clear();
        w.steers.clear();
        w.stopping = false;
        w.claude_stop = None;
        w.deadline = None;
        if let Some(local) = &w.local_turn {
            chat.thread["latestTurn"] = local.latest.clone();
            chat.thread["session"]["status"] = json!("starting");
            chat.thread["session"]["activeTurnId"] = local.latest["turnId"].clone();
            chat.pending_message = local.message.clone();
            w.last_prompt = local.native.clone();
            w.first_prompt = local.native.clone();
            w.stage = if local.delivered {
                "running"
            } else if [
                "running",
                "idle",
                "awaiting-native",
                "waiting-external",
                "prompt",
            ]
            .contains(&previous_stage)
            {
                "prompt"
            } else {
                previous_stage
            };
            if !local.delivered && w.stage == "prompt" {
                let frame = json!({"type":"system","subtype":"remodex_continue_pending"});
                w.pre_turn_events.push_back((frame, 0));
            }
        } else {
            w.stage = if w.claude_router.as_ref().unwrap().pending().next().is_some() {
                "awaiting-native"
            } else {
                "idle"
            };
        }
        self.save(id, None).await?;
        self.dirty = true;
        if self.active[id].stage == "idle"
            && state == "completed"
            && !self.chats[id].paused
            && !self.chats[id].queue.is_empty()
        {
            let next = self.chats.get_mut(id).unwrap().queue.remove(0);
            if let Err(error) = self.begin(id, next.clone(), None).await {
                let chat = self.chats.get_mut(id).unwrap();
                chat.queue.insert(0, next);
                chat.paused = true;
                chat.thread["session"]["lastError"] = json!(error);
                self.save(id, None).await?;
            }
        }
        Ok(())
    }
    async fn detach_shared_connection(&mut self, id: &str) -> Result<(), String> {
        let Some(mut worker) = self.active.remove(id) else {
            return Ok(());
        };
        // A transport failure is not a turn failure. The shared daemon owns the
        // process and continues its tools; do not send an implicit interrupt.
        let _ = native_control::disconnect(&mut worker);
        self.tool_controls.remove(id);
        worker.connection.close().await;
        let chat = self.chats.get_mut(id).unwrap();
        chat.paused |= !chat.queue.is_empty();
        chat.thread["session"]["statusConfidence"] = json!("unknown");
        chat.thread["session"]["lastError"] =
            json!("Connection lost. Reconnecting to the original task; no prompt was resent.");
        chat.thread["session"]["updatedAt"] = json!(now());
        // Retain the active native/public turn and all delivery receipts. The
        // authoritative reader can reconcile completion even after the phone
        // leaves. Its existing two-read concurrency limit still applies.
        self.shared_watches.insert(
            id.into(),
            (Instant::now() + Duration::from_secs(3), Instant::now()),
        );
        self.dirty = true;
        self.save(id, None).await
    }
    async fn end(&mut self, id: &str, state: &str, error: Option<&str>) {
        if self.chats.get(id).is_some_and(|c| c.external_claude) {
            self.detach_external_claude(id, error).await;
            return;
        }
        let Some(mut w) = self.active.remove(id) else {
            return;
        };
        let _ = native_control::disconnect(&mut w);
        self.tool_controls.remove(id);
        if let Some(read) = w.metadata.capability_read.take() {
            for reply in read.replies {
                let _ = reply.send(Ok(crate::capabilities::unavailable()));
            }
        }
        let id = w.chat_id.clone();
        if self.chats[&id].agent == AgentKind::Claude {
            let cid = id.clone();
            let native = self.chats[&id].native_id.clone();
            let _ = self
                .db
                .call(move |a| a.store.mark_claude_delivery_uncertain(&cid, &native))
                .await;
        }
        if w.claude_router.is_some() {
            if claude_recoverable(&self.chats[&id]) {
                self.claude_recovery.failed(&id, Instant::now());
            } else {
                self.claude_recovery.forget(&id);
            }
            projection::activity(
                self.chats.get_mut(&id).unwrap(),
                "claude-native-sync",
                "claude.desktop-sync.updated",
                "Claude Desktop sync",
                "info",
                json!({"status":"disconnected","detail":error.unwrap_or("The isolated Claude connection was closed. History is retained.")}),
            );
        }
        if w.claude_router.is_some()
            && (w.stage == "idle"
                || (w.recovering && w.claude_router.as_ref().unwrap().active().is_none()))
        {
            w.connection.close().await;
            let _ = self.save(&id, None).await;
            self.dirty = true;
            return;
        }
        if self.chats[&id].agent == AgentKind::Codex
            && self.config.codex.shared_server.is_some()
            && state != "completed"
            && w.owns_turn
            && !w.native_turn.is_empty()
        {
            // The socket does not own the server. Cancel only the verified
            // turn we launched, never the process or another client's turn.
            let _ = w.connection.send(&json!({"id":"release-owned-turn","method":"turn/interrupt","params":{"threadId":self.chats[&id].native_id,"turnId":w.native_turn}})).await;
        }
        // Reap before launching the next queued turn for this chat: no
        // retirement race where input is written to a closing CLI.
        w.connection.close().await;
        let chat = self.chats.get_mut(&id).unwrap();
        if !w.owns_turn && error.is_some() {
            // Losing an observer does not stop or claim completion of the
            // Desktop-owned task. Catch-up resumes when its reader reconnects.
            chat.paused = !chat.queue.is_empty();
            let _ = self.save(&id, None).await;
            self.dirty = true;
            return;
        }
        finish(chat, state, error);
        if self.save(&id, None).await.is_err() {
            self.chats.get_mut(&id).unwrap().paused = true;
            self.publish();
            return;
        }
        self.publish();
        let chat = self.chats.get_mut(&id).unwrap();
        if state == "completed" && !chat.paused && !chat.queue.is_empty() {
            let next = chat.queue.remove(0);
            // begin persists dequeue + new turn atomically before launching.
            if let Err(e) = self.begin(&id, next.clone(), None).await {
                let chat = self.chats.get_mut(&id).unwrap();
                chat.queue.insert(0, next);
                chat.paused = true;
                finish(chat, "error", Some(&e));
                let _ = self.save(&id, None).await;
            }
            self.publish();
        }
    }
}
fn codex_thread_request(chat: &Chat) -> Value {
    let mut request = json!({"id":"thread","method":if chat.native_id.is_empty(){"thread/start"}else{"thread/resume"},"params":{"cwd":chat.thread["worktreePath"],"model":chat.thread["modelSelection"]["model"],"approvalPolicy":if chat.thread["runtimeMode"]=="full-access" {"never"}else{"on-request"},"sandbox":if chat.thread["runtimeMode"]=="full-access" {"danger-full-access"}else{"workspace-write"},"threadId":chat.native_id,"excludeTurns":true}});
    crate::models::codex_tier(&mut request["params"], &chat.thread["modelSelection"]);
    request
}
fn claude_permission(chat: &Chat) -> &'static str {
    match (
        s(&chat.thread["interactionMode"]),
        s(&chat.thread["runtimeMode"]),
    ) {
        ("plan", _) => "plan",
        (_, "full-access") => "bypassPermissions",
        (_, "auto-accept-edits") => "acceptEdits",
        _ => "default",
    }
}
fn claude_recoverable(chat: &Chat) -> bool {
    chat.agent == AgentKind::Claude
        && chat.thread["deletedAt"].is_null()
        && chat.thread["archivedAt"].is_null()
        && ((chat.fork_from.is_none()
            && chat.remote_identity.as_ref().is_some_and(|identity| {
                identity.native_session == chat.native_id && identity.validate().is_ok()
            }))
            || (!chat.external_claude
                && chat.sync_replacement.is_some()
                && (chat.fork_from.is_some() || chat.resumed)))
}
fn prepare_claude_local(w: &mut Worker, chat: &Chat) -> Result<(), String> {
    w.recovering = false;
    let native = uuid::Uuid::new_v4().to_string();
    w.claude_router
        .as_mut()
        .ok_or("Claude shared router unavailable")?
        .register(crate::claude_turns::Input {
            native: native.clone(),
            message: s(&chat.pending_message["messageId"]).into(),
            public_turn: s(&chat.thread["latestTurn"]["turnId"]).into(),
            local: true,
            steer: false,
        })?;
    w.local_turn = Some(ClaudeLocalTurn {
        latest: chat.thread["latestTurn"].clone(),
        message: chat.pending_message.clone(),
        native: native.clone(),
        delivered: false,
    });
    w.last_prompt = native.clone();
    w.first_prompt = native;
    w.stopping = false;
    w.steers.clear();
    w.requests.clear();
    Ok(())
}
fn chat_agent(chats: &HashMap<String, Chat>, id: &str) -> AgentKind {
    chats[id].agent
}
fn consumed(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = v["user_message_uuids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(id) = v["user_message_uuid"].as_str() {
        ids.push(id.into());
    }
    if v["type"] == "command_lifecycle" && v["state"] == "started" {
        if let Some(id) = v["command_uuid"].as_str() {
            ids.push(id.into());
        }
    }
    ids
}
