//! Owned quick-tunnel lifecycle. Never forwards the administrator router and
//! never treats a printed URL as proof that the correct bridge is reachable.
use serde::Serialize;
use serde_json::Value;
use std::{net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{watch, Mutex, Notify},
    task::JoinHandle,
    time::Instant,
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(skip)]
    generation: u64,
    pub mode: &'static str,
    pub status: &'static str,
    pub public_url: Option<String>,
    pub error: Option<&'static str>,
}
impl State {
    fn generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
    fn new(status: &'static str, url: Option<String>, error: Option<&'static str>) -> Self {
        Self {
            generation: 0,
            mode: "quick",
            status,
            public_url: url,
            error,
        }
    }
}
#[derive(Clone, Copy)]
struct Desired {
    enabled: bool,
    generation: u64,
}
struct Core {
    desired: watch::Sender<Desired>,
    stop: watch::Sender<bool>,
    state: watch::Receiver<State>,
    check: Arc<Notify>,
    task: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct Manager(Arc<Core>);
#[derive(Clone)]
pub struct Config {
    pub executable: PathBuf,
    /// Private empty config, so quick mode cannot pick up another service's
    /// named tunnel or credentials from the normal cloudflared configuration.
    pub empty_config: PathBuf,
    /// Must be the dedicated phone-only listener, never the dashboard port.
    pub origin: SocketAddr,
    pub environment_id: String,
}

/// The external helper can only reach this phone router. Sharing the local
/// administrator listener would let a forwarded loopback request spoof Host.
pub struct Origin {
    manager: Manager,
    server: Option<JoinHandle<std::io::Result<()>>>,
}
impl Origin {
    pub async fn start(
        app: &mut crate::server::App,
        executable: Option<PathBuf>,
        directory: &std::path::Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use std::io::Write;
        let empty_config = directory.join("native-quick-tunnel.yml");
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&empty_config) {
            Ok(mut file) => {
                file.write_all(b"{}\n")?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let manager = Manager::new(
            Config {
                executable: executable
                    .unwrap_or_else(|| crate::cloudflared::managed_path(directory)),
                empty_config,
                origin: listener.local_addr()?,
                environment_id: app.environment_id.clone(),
            },
            false,
        )?;
        let enabled = app
            .auth
            .call(|a| Ok::<_, rusqlite::Error>(a.control_enabled()? && a.quick_tunnel_enabled()?))
            .await??;
        app.tunnel = Some(manager.clone());
        let mut stopping = app.stopping.subscribe();
        let router = app.clone().phone_router();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopping.wait_for(|v| *v).await;
            })
            .await
        });
        manager.configure(enabled);
        Ok(Self {
            manager,
            server: Some(server),
        })
    }
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.manager.shutdown().await;
        if let Some(mut task) = self.server.take() {
            match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
                Ok(result) => result.map_err(std::io::Error::other)?,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Tunnel origin cleanup did not finish; restart was cancelled",
                    ))
                }
            }
        } else {
            Ok(())
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.manager.stop();
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}
impl Manager {
    pub fn new(config: Config, enabled: bool) -> Result<Self, String> {
        if !config.origin.ip().is_loopback()
            || config.origin.port() == 0
            || config.environment_id.is_empty()
            || config.environment_id.len() > 128
        {
            return Err("A private phone origin and verified environment are required".into());
        }
        if !config.executable.is_absolute()
            || (!config.executable.is_file()
                && !config.empty_config.parent().is_some_and(|directory| {
                    crate::cloudflared::is_managed(&config.executable, directory)
                }))
            || !config.empty_config.is_absolute()
        {
            return Err("Select a native tunnel executable and private configuration".into());
        }
        #[cfg(windows)]
        if !config
            .executable
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("exe"))
        {
            return Err("Select a native tunnel executable".into());
        }
        let meta = std::fs::symlink_metadata(&config.empty_config)
            .map_err(|_| "Tunnel configuration unavailable")?;
        if !meta.is_file()
            || meta.len() > 16
            || std::fs::read(&config.empty_config)
                .map_err(|_| "Tunnel configuration unavailable")?
                != b"{}\n"
        {
            return Err("Tunnel configuration must be a private empty file".into());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| "Tunnel verifier unavailable")?;
        let (desired, changes) = watch::channel(Desired {
            enabled,
            generation: 0,
        });
        let (stop, stopping) = watch::channel(false);
        let (send, state) = watch::channel(State::new(
            if enabled { "starting" } else { "stopped" },
            None,
            None,
        ));
        let check = Arc::new(Notify::new());
        let task = tokio::spawn(run(config, client, changes, stopping, send, check.clone()));
        Ok(Self(Arc::new(Core {
            desired,
            stop,
            state,
            check,
            task: Mutex::new(Some(task)),
        })))
    }
    pub fn state(&self) -> State {
        let desired = self.0.desired.borrow();
        if *self.0.stop.borrow() || !desired.enabled {
            return State::new("stopped", None, None);
        }
        let state = self.0.state.borrow().clone();
        if state.generation != desired.generation {
            State::new("starting", None, None)
        } else {
            state
        }
    }
    pub fn configure(&self, enabled: bool) {
        if *self.0.stop.borrow() {
            return;
        }
        self.0.desired.send_if_modified(|v| {
            if v.enabled == enabled {
                false
            } else {
                v.enabled = enabled;
                v.generation = v.generation.wrapping_add(1);
                true
            }
        });
    }
    pub fn retry(&self) {
        if *self.0.stop.borrow() {
            return;
        }
        self.0
            .desired
            .send_modify(|v| v.generation = v.generation.wrapping_add(1));
    }
    pub async fn shutdown(&self) {
        self.0.stop.send_replace(true);
        if let Some(task) = self.0.task.lock().await.take() {
            let _ = task.await;
        }
    }
    /// Schedule a fresh identity check without replacing a healthy connection.
    pub fn check(&self) {
        self.0.check.notify_one();
    }
    pub fn stop(&self) {
        self.0.stop.send_replace(true);
    }
}
async fn run(
    config: Config,
    client: reqwest::Client,
    mut desired: watch::Receiver<Desired>,
    mut stopping: watch::Receiver<bool>,
    state: watch::Sender<State>,
    check: Arc<Notify>,
) {
    let mut failures = 0usize;
    loop {
        if *stopping.borrow() {
            break;
        }
        let current = *desired.borrow_and_update();
        if !current.enabled {
            failures = 0;
            state.send_replace(State::new("stopped", None, None));
            tokio::select! {_=stopping.changed()=>{},_=desired.changed()=>{}}
            continue;
        }
        state.send_replace(State::new("starting", None, None).generation(current.generation));
        let result = attempt(
            &config,
            &client,
            &mut desired,
            &mut stopping,
            &state,
            AttemptPolicy {
                quic: failures % 2 == 1,
                initial_deadline: Duration::from_secs(45),
            },
            &check,
        )
        .await;
        if *stopping.borrow() {
            break;
        }
        if desired.borrow().generation != current.generation || !desired.borrow().enabled {
            failures = 0;
            continue;
        }
        if let Err(error) = result {
            let public_error = match error {
                "invalid_checksum" | "download_failed" | "unsupported_platform" => {
                    "cloudflared_unavailable"
                }
                other => other,
            };
            state.send_replace(
                State::new("error", None, Some(public_error)).generation(current.generation),
            );
            if matches!(error, "invalid_checksum" | "unsupported_platform") {
                tokio::select! {_=stopping.changed()=>{},_=desired.changed()=>{failures=0;},_=check.notified()=>{}}
                continue;
            }
            let delay = [1, 3, 5, 10, 30][failures.min(4)];
            failures = failures.saturating_add(1);
            tokio::select! {_=tokio::time::sleep(Duration::from_secs(delay))=>{},_=stopping.changed()=>{},_=desired.changed()=>{failures=0;}}
        }
    }
    state.send_replace(State::new("stopped", None, None));
}
struct AttemptPolicy {
    quic: bool,
    initial_deadline: Duration,
}
async fn attempt(
    config: &Config,
    client: &reqwest::Client,
    desired: &mut watch::Receiver<Desired>,
    stopping: &mut watch::Receiver<bool>,
    state: &watch::Sender<State>,
    policy: AttemptPolicy,
    check: &Notify,
) -> Result<(), &'static str> {
    let generation = desired.borrow().generation;
    if !std::fs::symlink_metadata(&config.empty_config).is_ok_and(|m| m.is_file() && m.len() <= 16)
        || !std::fs::read(&config.empty_config).is_ok_and(|b| b == b"{}\n")
    {
        return Err("tunnel_failed");
    }
    let executable = tokio::select! {
        _=desired.changed()=>return Ok(()),
        _=stopping.changed()=>return Ok(()),
        result=crate::cloudflared::resolve(&config.executable, config.empty_config.parent().ok_or("tunnel_failed")?)=>result?,
    };
    let mut command = Command::new(executable);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("TUNNEL_")
            || name.starts_with("CLOUDFLARED_")
            || name.starts_with("CF_TUNNEL_")
        {
            command.env_remove(key);
        }
    }
    command
        .arg("tunnel")
        .arg("--config")
        .arg(&config.empty_config)
        .arg("--origincert")
        .arg(config.empty_config.with_extension("unused-cert"))
        .args([
            "--no-autoupdate",
            "--metrics",
            "127.0.0.1:0",
            "--protocol",
            if policy.quic { "quic" } else { "http2" },
            "--url",
        ])
        .arg(format!("http://{}", config.origin))
        .env_remove("TUNNEL_TOKEN")
        .env_remove("TUNNEL_TOKEN_FILE")
        .env_remove("TUNNEL_CRED_FILE")
        .env_remove("TUNNEL_PIDFILE")
        .env_remove("TUNNEL_LOGFILE")
        .env("TUNNEL_NO_PRECHECKS", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| "cloudflared_unavailable")?;
    let tree = match crate::process_tree::ProcessTree::attach(&child) {
        Ok(v) => v,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("tunnel_failed");
        }
    };
    let mut stdout = child.stdout.take().ok_or("tunnel_failed")?;
    let mut stderr = child.stderr.take().ok_or("tunnel_failed")?;
    let mut out = [0u8; 4096];
    let mut err = [0u8; 4096];
    let (mut out_lines, mut err_lines) = (Lines::default(), Lines::default());
    let (mut out_open, mut err_open) = (true, true);
    let mut url = None;
    let mut registered = false;
    let mut ready = false;
    let mut initial_expired = false;
    let mut misses = 0;
    let start = Instant::now();
    let mut next_check = Instant::now();
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        if *stopping.borrow() || desired.borrow().generation != generation {
            break Ok(());
        }
        tokio::select! {
            _=desired.changed()=>break Ok(()),
            _=stopping.changed()=>break Ok(()),
            _=check.notified()=>{next_check=Instant::now();},
            _=child.wait()=>break Err("tunnel_failed"),
            read=stdout.read(&mut out),if out_open=>match read{Ok(0)=>out_open=false,Ok(n)=>out_lines.feed(&out[..n],&mut url,&mut registered),Err(_)=>break Err("tunnel_failed")},
            read=stderr.read(&mut err),if err_open=>match read{Ok(0)=>err_open=false,Ok(n)=>err_lines.feed(&err[..n],&mut url,&mut registered),Err(_)=>break Err("tunnel_failed")},
            _=timer.tick()=>{
                if !ready && !initial_expired && start.elapsed()>policy.initial_deadline {
                    if !registered || url.is_none() {break Err("tunnel_failed");}
                    // Registration can precede DNS/edge propagation. Keep the
                    // same connector and address while showing the failed check;
                    // rotating URLs here can make propagation fail forever.
                    initial_expired=true;
                    state.send_replace(State::new("error",url.clone(),Some("verification_failed")).generation(generation));
                }
                if registered && Instant::now()>=next_check {
                    if let Some(url)=&url {
                        if !ready && !initial_expired {state.send_replace(State::new("checking",Some(url.clone()),None).generation(generation));}
                        let checked=tokio::select!{_=desired.changed()=>break Ok(()),_=stopping.changed()=>break Ok(()),result=verify(client,url,&config.environment_id)=>result};
                        if checked {ready=true;misses=0;state.send_replace(State::new("ready",Some(url.clone()),None).generation(generation));}
                        else if ready {misses+=1;if misses>=2{break Err("verification_failed");}state.send_replace(State::new("checking",Some(url.clone()),None).generation(generation));}
                        next_check=Instant::now()+Duration::from_secs(if ready || initial_expired{15}else{2});
                    }
                }
            }
        }
    };
    // Keep the old authenticated transport briefly during an explicit retry.
    // Its URL is already hidden by the generation check, while config sockets
    // can deliver the new local fallback addresses before this transport ends.
    // Disable, shutdown and unexpected helper exit never wait for this grace.
    if result.is_ok()
        && !*stopping.borrow()
        && desired.borrow().enabled
        && desired.borrow().generation != generation
    {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if *stopping.borrow() || !desired.borrow().enabled {
                break;
            }
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>break,
                _=stopping.changed()=>break,
                _=desired.changed()=>{},
                _=child.wait()=>break,
            }
        }
    }
    drop(tree);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    discard: bool,
}
impl Lines {
    fn feed(&mut self, chunk: &[u8], url: &mut Option<String>, registered: &mut bool) {
        for byte in chunk {
            if *byte == b'\n' {
                if !self.discard {
                    if let Ok(line) = std::str::from_utf8(&self.pending) {
                        if line.contains("Registered tunnel connection") {
                            *registered = true;
                        }
                        if url.is_none() {
                            *url = quick_url(line);
                        }
                    }
                }
                self.pending.clear();
                self.discard = false;
            } else if !self.discard {
                if self.pending.len() == 8192 {
                    self.pending.clear();
                    self.discard = true;
                } else {
                    self.pending.push(*byte);
                }
            }
        }
    }
}
fn quick_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let token = line[start..]
        .split(|c: char| c.is_ascii_whitespace() || matches!(c, '|' | '"' | '<' | '>'))
        .next()?;
    if token.len() > 100 {
        return None;
    }
    let u = reqwest::Url::parse(token).ok()?;
    let hostname = u.host_str()?;
    let name = hostname.strip_suffix(".trycloudflare.com")?;
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.path() != "/"
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return None;
    }
    Some(format!("https://{hostname}"))
}
async fn verify(client: &reqwest::Client, url: &str, environment: &str) -> bool {
    let result = async {
        let mut response = client
            .get(format!("{url}/.well-known/t3/environment"))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() || response.content_length().is_some_and(|n| n > 16384) {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > 16384 {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body).ok()?;
        (value["environmentId"].as_str() == Some(environment)).then_some(())
    }
    .await;
    result.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn disabled_managed_helper_is_lazy_and_corrupt_cache_reports_a_supported_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty_config = dir.path().join("empty.yml");
        std::fs::write(&empty_config, b"{}\n").unwrap();
        let executable = crate::cloudflared::managed_path(dir.path());
        let config = Config {
            executable: executable.clone(),
            empty_config,
            origin: "127.0.0.1:12345".parse().unwrap(),
            environment_id: "test".into(),
        };
        let manager = Manager::new(config, false).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(manager.state().status, "stopped");
        assert!(!dir.path().join("tools").exists());
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"UNVERIFIED_DO_NOT_EXECUTE").unwrap();
        manager.configure(true);
        tokio::time::timeout(Duration::from_secs(3), async {
            while manager.state().status != "error" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.state().error, Some("cloudflared_unavailable"));
        assert!(manager.state().public_url.is_none());
        assert_eq!(
            std::fs::read(executable).unwrap(),
            b"UNVERIFIED_DO_NOT_EXECUTE"
        );
        manager.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn initial_verification_failure_keeps_the_registered_connector_until_stopped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-cloudflared");
        std::fs::write(&executable,b"#!/bin/sh\nroot=$(dirname \"$0\")\necho $$ > \"$root/pid\"\necho https://fixture.trycloudflare.com\necho Registered tunnel connection\nexec sleep 300\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let empty_config = dir.path().join("empty.yml");
        std::fs::write(&empty_config, b"{}\n").unwrap();
        let config = Config {
            executable,
            empty_config,
            origin: "127.0.0.1:12345".parse().unwrap(),
            environment_id: "test".into(),
        };
        // This fixture never reaches the Internet: HTTPS verification fails at
        // a closed loopback port, simulating incomplete public propagation.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .resolve("fixture.trycloudflare.com", "127.0.0.1:9".parse().unwrap())
            .build()
            .unwrap();
        let (_desired, mut changes) = watch::channel(Desired {
            enabled: true,
            generation: 0,
        });
        let (stop, mut stopping) = watch::channel(false);
        let (send, mut state) = watch::channel(State::new("starting", None, None));
        let task = tokio::spawn(async move {
            attempt(
                &config,
                &client,
                &mut changes,
                &mut stopping,
                &send,
                AttemptPolicy {
                    quic: false,
                    initial_deadline: Duration::from_millis(100),
                },
                &Notify::new(),
            )
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(4),
            state.wait_for(|s| s.error == Some("verification_failed")),
        )
        .await
        .unwrap()
        .unwrap();
        let pid: i32 = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        assert_eq!(
            state.borrow().public_url.as_deref(),
            Some("https://fixture.trycloudflare.com")
        );
        assert!(
            !task.is_finished(),
            "a failed initial check must not rotate the URL"
        );
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }
    #[tokio::test]
    async fn retry_and_disable_hide_old_verified_urls_immediately() {
        let (desired, _changes) = watch::channel(Desired {
            enabled: true,
            generation: 8,
        });
        let (stop, _stopping) = watch::channel(false);
        let (send, state) = watch::channel(
            State::new("ready", Some("https://old.trycloudflare.com".into()), None).generation(8),
        );
        let manager = Manager(Arc::new(Core {
            desired,
            stop,
            state,
            check: Arc::new(Notify::new()),
            task: Mutex::new(None),
        }));
        assert_eq!(manager.state().status, "ready");
        manager.check();
        assert_eq!(manager.state().status, "ready"); // Checking is not restarting.
        manager.retry();
        assert_eq!(manager.state().status, "starting");
        assert!(manager.state().public_url.is_none());
        // A late check from the previous attempt cannot re-advertise its URL.
        send.send_replace(
            State::new("ready", Some("https://old.trycloudflare.com".into()), None).generation(8),
        );
        assert!(manager.state().public_url.is_none());
        manager.configure(false);
        assert_eq!(manager.state().status, "stopped");
        manager.configure(true);
        assert_eq!(manager.state().status, "starting");
        manager.shutdown().await;
        manager.configure(true);
        manager.retry();
        assert_eq!(manager.state().status, "stopped");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn printed_url_is_not_ready_and_retry_disable_and_drop_reap_owned_helpers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-cloudflared");
        std::fs::write(&executable,b"#!/bin/sh\nroot=$(dirname \"$0\")\necho $$ >> \"$root/starts\"\necho https://printed-only.trycloudflare.com\nsleep 300 &\necho $! >> \"$root/descendants\"\nwait\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let empty_config = dir.path().join("empty.yml");
        std::fs::write(&empty_config, b"{}\n").unwrap();
        let config = Config {
            executable,
            empty_config,
            origin: "127.0.0.1:12345".parse().unwrap(),
            environment_id: "test".into(),
        };
        let manager = Manager::new(config.clone(), true).unwrap();
        async fn starts(dir: &std::path::Path, count: usize) -> Vec<i32> {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let pids: Vec<i32> = std::fs::read_to_string(dir.join("starts"))
                        .unwrap_or_default()
                        .lines()
                        .filter_map(|v| v.parse().ok())
                        .collect();
                    let children = std::fs::read_to_string(dir.join("descendants"))
                        .unwrap_or_default()
                        .lines()
                        .count();
                    if pids.len() >= count && children >= count {
                        return pids;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap()
        }
        async fn gone(pid: i32) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if unsafe { libc::kill(pid, 0) } != 0 {
                        break;
                    }
                    // Descendants may briefly remain as reparented zombies.
                    let output = std::process::Command::new("ps")
                        .args(["-o", "stat=", "-p", &pid.to_string()])
                        .output()
                        .unwrap();
                    let state = String::from_utf8_lossy(&output.stdout);
                    if state.trim().is_empty() || state.trim().starts_with('Z') {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
        let first = starts(dir.path(), 1).await[0];
        assert_ne!(manager.state().status, "ready");
        assert!(manager.state().public_url.is_none());
        manager.retry();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            unsafe { libc::kill(first, 0) },
            0,
            "retry must leave time to deliver fallback routes"
        );
        assert!(manager.state().public_url.is_none());
        starts(dir.path(), 2).await;
        gone(first).await;
        manager.retry();
        tokio::time::sleep(Duration::from_millis(100)).await;
        manager.configure(false);
        tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
            .await
            .unwrap();
        let manager = Manager::new(config, true).unwrap();
        let pids = starts(dir.path(), 3).await;
        drop(manager);
        for pid in pids {
            gone(pid).await;
        }
        for pid in std::fs::read_to_string(dir.path().join("descendants"))
            .unwrap()
            .lines()
        {
            gone(pid.parse().unwrap()).await;
        }
    }
    #[test]
    fn quick_logs_are_bounded_and_never_accept_other_origins_or_credentials() {
        let mut lines = Lines::default();
        let mut url = None;
        let mut registered = false;
        for _ in 0..100 {
            lines.feed(&[b'x'; 4096], &mut url, &mut registered);
            assert!(lines.pending.len() <= 8192);
        }
        lines.feed(
            b" https://ignored.trycloudflare.com\n",
            &mut url,
            &mut registered,
        );
        assert!(url.is_none());
        lines.feed(b"INF | https://one-two.trycloud", &mut url, &mut registered);
        assert!(url.is_none());
        lines.feed(
            b"flare.com |\nINF Registered tunnel connection\n",
            &mut url,
            &mut registered,
        );
        assert_eq!(url.as_deref(), Some("https://one-two.trycloudflare.com"));
        assert!(registered);
        for bad in [
            "https://trycloudflare.com",
            "https://one.trycloudflare.com.attacker.test",
            "https://user:pass@one.trycloudflare.com",
            "https://one.trycloudflare.com:1234",
            "https://one.trycloudflare.com/path",
            "https://one.trycloudflare.com?x=1",
            "https://a.b.trycloudflare.com",
            "https://-bad.trycloudflare.com",
            "https://127.0.0.1",
        ] {
            assert!(quick_url(bad).is_none(), "{bad}");
        }
    }
    #[tokio::test]
    async fn verification_requires_exact_identity_bounded_body_and_no_redirect() {
        use axum::{routing::get, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/.well-known/t3/environment",
                        get(|| async { Json(serde_json::json!({"environmentId":"right"})) }),
                    )
                    .route(
                        "/redirect/.well-known/t3/environment",
                        get(|| async {
                            axum::response::Redirect::temporary("/.well-known/t3/environment")
                        }),
                    )
                    .route(
                        "/large/.well-known/t3/environment",
                        get(|| async { "x".repeat(16385) }),
                    ),
            )
            .await
            .unwrap()
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let base = format!("http://{addr}");
        assert!(verify(&client, &base, "right").await);
        assert!(!verify(&client, &base, "wrong").await);
        assert!(!verify(&client, &format!("{base}/redirect"), "right").await);
        assert!(!verify(&client, &format!("{base}/large"), "right").await);
        server.abort();
        let _ = server.await;
    }
}
