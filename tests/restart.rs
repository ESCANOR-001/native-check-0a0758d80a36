use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
};

struct Fixture {
    root: tempfile::TempDir,
    state: PathBuf,
    config: PathBuf,
    gui: PathBuf,
    http: reqwest::Client,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let config = root.path().join("config.json");
        let gui = root.path().join("gui");
        std::fs::create_dir(&gui).unwrap();
        std::fs::write(
            gui.join("index.html"),
            "<html><head></head><body>fixture</body></html>",
        )
        .unwrap();
        let agent = json!({"executable":env!("CARGO_BIN_EXE_chat-fixture"),"profile":root.path(),"model":"test"});
        std::fs::write(
            &config,
            json!({"workspace":root.path(),"codex":agent,"claude":agent}).to_string(),
        )
        .unwrap();
        Self {
            root,
            state,
            config,
            gui,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }
    fn launch(&self, mode: &str) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remodex-connect-native"));
        command
            .arg(mode)
            .arg(&self.state)
            .args(["127.0.0.1:0", "http://127.0.0.1"])
            .arg(&self.config)
            .arg(&self.gui)
            .current_dir(self.root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        command.spawn().unwrap()
    }
    async fn dashboard(&self, ready: &Value) -> (String, String, String) {
        let base = format!("http://{}", ready["address"].as_str().unwrap());
        let html = self
            .http
            .get(format!("{base}/opencodex-session"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        let meta = |name: &str| {
            html.split(&format!("name=\"{name}\" content=\""))
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_owned()
        };
        (
            base,
            meta("opencodex-session-token"),
            meta("opencodex-session-csrf"),
        )
    }
    fn action(
        &self,
        session: &(String, String, String),
        path: &str,
        pid: u64,
    ) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", session.0))
            .header("x-opencodex-api-key", &session.1)
            .header("x-opencodex-csrf-token", &session.2)
            .header("x-opencodex-gui-origin", &session.0)
            .header("origin", &session.0)
            .header("x-opencodex-expected-pid", pid)
            .json(&json!({}))
    }
}
async fn ready(reader: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    let bytes = tokio::time::timeout(Duration::from_secs(12), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes > 0, "child exited before readiness");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ready"], true);
    v
}
async fn exit(child: &mut Child, success: bool) {
    let result = tokio::time::timeout(Duration::from_secs(12), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.success(), success, "unexpected child exit {result}");
}

#[tokio::test]
async fn supervised_restart_keeps_pairing_identity_and_history_and_stop_exits_supervisor() {
    let f = Fixture::new();
    let mut child = f.launch("--supervise");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let first = ready(&mut reader).await;
    assert_ne!(first["pid"], child.id().unwrap());
    let session = f.dashboard(&first).await;
    let invite: Value =
        serde_json::from_slice(&std::fs::read(first["invitationFile"].as_str().unwrap()).unwrap())
            .unwrap();
    let paired: Value = f
        .http
        .post(format!("{}/oauth/token", session.0))
        .form(&[
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
            ("subject_token", invite["pairingToken"].as_str().unwrap()),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = paired["access_token"].as_str().unwrap();
    let environment: Value = f
        .http
        .get(format!("{}/.well-known/t3/environment", session.0))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for agent in ["", "?agent=claude-code"] {
        let url = format!("{}/api/orchestration/dispatch{agent}", session.0);
        for command in [
            json!({"type":"thread.create","commandId":format!("create{agent}"),"threadId":format!("test{agent}"),"projectId":"native-test-project"}),
            json!({"type":"thread.turn.start","commandId":format!("send{agent}"),"threadId":format!("test{agent}"),"message":{"messageId":format!("message{agent}"),"text":"RESTART_HISTORY","attachments":[]},"deliveryMode":"normal"}),
        ] {
            f.http
                .post(&url)
                .bearer_auth(token)
                .json(&command)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
    }
    // Poll durable completion, not just the HTTP dispatch acknowledgement.
    tokio::time::timeout(Duration::from_secs(12),async {
        loop {
            let db=rusqlite::Connection::open(f.state.join("native-connect.sqlite")).unwrap();
            let count:i64=db.query_row("SELECT count(*) FROM chats WHERE json_extract(data,'$.thread.session.status')='ready'",[],|r|r.get(0)).unwrap();
            if count==2 {break;}
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }).await.unwrap();
    let rows = || {
        let db = rusqlite::Connection::open(f.state.join("native-connect.sqlite")).unwrap();
        let mut q = db
            .prepare("SELECT data FROM chat_rows WHERE kind='messages' ORDER BY seq")
            .unwrap();
        q.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = rows();
    assert_eq!(before.len(), 4);
    let stale = f
        .action(&session, "/api/restart", first["pid"].as_u64().unwrap() + 1)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    assert!(child.try_wait().unwrap().is_none());
    let res = f
        .action(&session, "/api/restart", first["pid"].as_u64().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    assert_eq!(
        res.json::<Value>().await.unwrap()["restartPolicy"],
        "idle-only"
    );
    let second = ready(&mut reader).await;
    assert_ne!(first["pid"], second["pid"]);
    assert!(!PathBuf::from(first["invitationFile"].as_str().unwrap()).exists());
    let next = f.dashboard(&second).await;
    let descriptor: Value = f
        .http
        .get(format!("{}/.well-known/t3/environment", next.0))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(descriptor["environmentId"], environment["environmentId"]);
    assert_eq!(
        f.http
            .get(format!("{}/api/auth/session", next.0))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        f.action(&next, "/api/restart", first["pid"].as_u64().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(rows(), before);
    assert_eq!(
        f.action(&next, "/api/stop", second["pid"].as_u64().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    exit(&mut child, true).await;
    assert!(!PathBuf::from(second["invitationFile"].as_str().unwrap()).exists());
    assert_eq!(rows(), before);
}

#[tokio::test]
async fn supervised_child_requires_pipe_handshake_and_stops_when_parent_pipe_closes() {
    let f = Fixture::new();
    let mut invalid = f.launch("--supervised-child");
    invalid
        .stdin
        .take()
        .unwrap()
        .write_all(b"invalid\n")
        .await
        .unwrap();
    exit(&mut invalid, false).await;
    assert!(
        !f.state.exists(),
        "unverified child must not open the profile"
    );
    let mut child = f.launch("--supervised-child");
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(b"native-connect-supervisor-v1\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let started = ready(&mut reader).await;
    drop(pipe);
    exit(&mut child, true).await;
    assert!(!PathBuf::from(started["invitationFile"].as_str().unwrap()).exists());
    let _lock = remodex_connect_native::profile_lock::acquire(&f.state).unwrap();
}

#[tokio::test]
async fn supervised_failed_startup_exits_without_retry_loop_or_invitation() {
    let f = Fixture::new();
    std::fs::write(&f.config, "{}").unwrap();
    let mut child = f.launch("--supervise");
    exit(&mut child, false).await;
    assert!(!std::fs::read_dir(&f.state)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().starts_with("pairing-")));
    let _lock = remodex_connect_native::profile_lock::acquire(&f.state).unwrap();
}

#[tokio::test]
async fn repeated_explicit_restarts_are_bounded_and_never_loop_forever() {
    let f = Fixture::new();
    let mut child = f.launch("--supervise");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    for _ in 0..6 {
        let v = ready(&mut reader).await;
        let session = f.dashboard(&v).await;
        assert_eq!(
            f.action(&session, "/api/system/restart", v["pid"].as_u64().unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            202
        );
    }
    exit(&mut child, false).await;
    let _lock = remodex_connect_native::profile_lock::acquire(&f.state).unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn unexpected_child_exit_is_terminal_and_does_not_restart_agents() {
    let f = Fixture::new();
    let mut child = f.launch("--supervise");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let started = ready(&mut reader).await;
    let pid = started["pid"].as_u64().unwrap() as u32;
    assert_ne!(pid, child.id().unwrap());
    #[cfg(unix)]
    unsafe {
        assert_eq!(libc::kill(pid as i32, libc::SIGKILL), 0);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
        };
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
            assert!(!process.is_null());
            assert_ne!(TerminateProcess(process, 1), 0);
            CloseHandle(process);
        }
    }
    exit(&mut child, false).await;
    let _lock = remodex_connect_native::profile_lock::acquire(&f.state).unwrap();
    let mut next = String::new();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut next))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn supervisor_termination_closes_only_its_child_and_preserves_state() {
    let f = Fixture::new();
    let mut child = f.launch("--supervise");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let started = ready(&mut reader).await;
    unsafe {
        assert_eq!(libc::kill(child.id().unwrap() as i32, libc::SIGTERM), 0);
    }
    exit(&mut child, true).await;
    assert!(!PathBuf::from(started["invitationFile"].as_str().unwrap()).exists());
    assert!(f.state.join("native-connect.sqlite").exists());
    let _lock = remodex_connect_native::profile_lock::acquire(&f.state).unwrap();
}
