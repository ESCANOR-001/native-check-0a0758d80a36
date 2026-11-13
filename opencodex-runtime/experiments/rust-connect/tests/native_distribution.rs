//! Exercise the distributed native entry point with no JS/Bun launcher and no
//! real profiles, phones, registry entries or installed agent applications.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    gui: PathBuf,
    config: PathBuf,
    port: u16,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let state = base.join("saved state 测试");
        let gui = base.join("dashboard");
        std::fs::create_dir(&gui).unwrap();
        std::fs::write(
            gui.join("index.html"),
            b"<html><head></head><body>Native fixture</body></html>",
        )
        .unwrap();
        let port = (24000..31000)
            .step_by(6)
            .find(|port| {
                let first = std::net::TcpListener::bind(("127.0.0.1", *port));
                first.is_ok() && std::net::TcpListener::bind(("0.0.0.0", port + 5)).is_ok()
            })
            .unwrap();
        let config = base.join("setup.json");
        std::fs::write(
            &config,
            json!({"workspace":base,"connection":{"dashboard_bind":format!("127.0.0.1:{port}")}})
                .to_string(),
        )
        .unwrap();
        Self {
            _root: root,
            root: base,
            state,
            gui,
            config,
            port,
        }
    }
    fn command(&self, binary: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(binary);
        c.args(args)
            .current_dir(&self.root)
            .env("OPENCODEX_HOME", &self.state)
            .env("OPENCODEX_BUN_PATH", self.root.join("no-bun-here"))
            .env("PATH", self.root.join("no-js-runtime-here"))
            .env_remove("REMODEX_GUI_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        c.creation_flags(0x08000000);
        c
    }
    async fn call(&self, binary: &Path, args: &[&str]) -> Value {
        let output =
            tokio::time::timeout(Duration::from_secs(40), self.command(binary, args).output())
                .await
                .unwrap()
                .unwrap();
        assert!(
            output.status.success(),
            "Native command {:?} failed: {}",
            args.first(),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    async fn package(&self, version: &str) -> PathBuf {
        #[cfg(windows)]
        assert!(Path::new(env!("CARGO_BIN_EXE_remodex-startup")).is_file());
        let destination = self.root.join(format!("release {version}"));
        let result = self
            .call(
                Path::new(env!("CARGO_BIN_EXE_remodex-connect-native")),
                &[
                    "package",
                    destination.to_str().unwrap(),
                    "--gui",
                    self.gui.to_str().unwrap(),
                    "--version",
                    version,
                ],
            )
            .await;
        assert_eq!(result["nodeRequired"], false);
        assert_eq!(result["bunRequired"], false);
        let package = destination.join("package");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(package.join("package.json")).unwrap()).unwrap();
        assert!(manifest["dependencies"].is_null());
        assert!(manifest["scripts"].is_null());
        assert!(!package.join("src").exists());
        assert!(!package.join("bin/ocx.mjs").exists());
        #[cfg(windows)]
        {
            let helper = std::fs::read(package.join("remodex-startup.exe")).unwrap();
            // A GUI-subsystem executable creates no console even when started
            // directly by Task Scheduler in the interactive user's session.
            let pe = u32::from_le_bytes(helper[0x3c..0x40].try_into().unwrap()) as usize;
            assert_eq!(&helper[pe..pe + 4], b"PE\0\0");
            assert_eq!(
                u16::from_le_bytes(helper[pe + 24 + 68..pe + 24 + 70].try_into().unwrap()),
                2
            );
        }
        package.join(if cfg!(windows) { "rmx.exe" } else { "rmx" })
    }
    async fn start(&self, binary: &Path) -> (Child, Value) {
        let mut child = self.command(binary, &["start"]).spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["ready"], true);
        child.stdout = Some(lines.into_inner().into_inner());
        (child, ready)
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[tokio::test]
async fn public_beta_package_runs_without_js_and_keeps_stable_private() {
    let f = Fixture::new();
    let destination = f.root.join("public beta");
    f.call(
        Path::new(env!("CARGO_BIN_EXE_remodex-connect-native")),
        &[
            "package",
            destination.to_str().unwrap(),
            "--gui",
            f.gui.to_str().unwrap(),
            "--version",
            "2.0.0-beta.1",
            "--publishable-beta",
        ],
    )
    .await;
    let package = destination.join("package");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(package.join("package.json")).unwrap()).unwrap();
    assert_eq!(manifest["private"], false);
    assert_eq!(manifest["publishConfig"]["tag"], "beta");
    assert_eq!(
        f.call(&package.join("rmx.exe"), &["version"]).await["version"],
        "2.0.0-beta.1"
    );
    let stable = f.package("2.0.0").await;
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(stable.parent().unwrap().join("package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["private"], true);
}

#[tokio::test]
async fn native_package_fresh_update_rollback_preserves_phone_pairing_without_js() {
    let f = Fixture::new();
    let client = reqwest::Client::new();
    let mut identity = None;
    let mut phone_token = None;
    let mut binaries = Vec::new();
    for version in ["0.0.0", "0.0.1"] {
        let binary = f.package(version).await;
        assert_eq!(f.call(&binary, &["version"]).await["version"], version);
        let args = if identity.is_none() {
            vec!["onboard", "--config", f.config.to_str().unwrap()]
        } else {
            vec!["onboard"]
        };
        let setup = f.call(&binary, &args).await;
        assert_eq!(setup["runtime"], "rust");
        if let Some(previous) = identity.as_ref() {
            assert_eq!(&setup["environmentId"], previous);
        } else {
            identity = Some(setup["environmentId"].clone());
        }
        let (mut server, ready) = f.start(&binary).await;
        #[cfg(windows)]
        {
            // npm package folders change, but the actual phone listener must
            // keep the same firewall application path on update and rollback.
            let installed = f
                .state
                .join("native-installation/state/windows-runtime/remodex-server.exe");
            assert_eq!(
                std::fs::read(&installed).unwrap(),
                std::fs::read(&binary).unwrap()
            );
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&installed)
                    .is_err(),
                "The running executable must be protected from replacement"
            );
        }
        if phone_token.is_none() {
            let invitation: Value = serde_json::from_slice(
                &std::fs::read(ready["invitationFile"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            let response = client
                .post(format!("http://127.0.0.1:{}/oauth/token", f.port + 5))
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
                    (
                        "subject_token",
                        invitation["pairingToken"].as_str().unwrap(),
                    ),
                ])
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            phone_token = response.json::<Value>().await.unwrap()["access_token"]
                .as_str()
                .map(str::to_owned);
        }
        let status = client
            .get(format!("http://127.0.0.1:{}/api/auth/session", f.port + 5))
            .bearer_auth(phone_token.as_ref().unwrap())
            .send()
            .await
            .unwrap();
        assert!(status.status().is_success());
        assert_eq!(status.json::<Value>().await.unwrap()["authenticated"], true);
        assert_eq!(f.call(&binary, &["restart"]).await["restarted"], true);
        assert_eq!(f.call(&binary, &["stop"]).await["stopped"], true);
        assert!(tokio::time::timeout(Duration::from_secs(15), server.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert_eq!(f.call(&binary, &["status"]).await["running"], false);
        binaries.push(binary);
    }
    // Revert only the executable; the installation and its phone identity stay.
    let (mut server, _) = f.start(&binaries[0]).await;
    let response = client
        .get(format!("http://127.0.0.1:{}/api/auth/session", f.port + 5))
        .bearer_auth(phone_token.unwrap())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    f.call(&binaries[0], &["stop"]).await;
    assert!(tokio::time::timeout(Duration::from_secs(15), server.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_rust_startup_is_owned_hidden_and_supervises_until_stop() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let binary = f.package("0.0.2").await;
    f.call(
        &binary,
        &["onboard", "--config", f.config.to_str().unwrap()],
    )
    .await;
    let registration = f.root.join("startup 测试 ' & space");
    let dir = registration.to_str().unwrap();
    f.call(&binary, &["service", "prepare", dir]).await;
    assert!(!registration.join("startup.ps1").exists());
    let plan: Value =
        serde_json::from_slice(&std::fs::read(registration.join("service.json")).unwrap()).unwrap();
    assert_eq!(plan["version"], 2);
    assert_eq!(plan["startup_sha256"].as_str().unwrap().len(), 64);
    // Best-effort cleanup on assertion failure affects only this random temp
    // installation. Never names/stops a process or task in the user's profile.
    struct Cleanup<'a>(&'a Path, &'a Path, &'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let call = |args: &[&str]| {
                std::process::Command::new(self.0)
                    .args(args)
                    .env("OPENCODEX_HOME", self.1)
                    .creation_flags(0x08000000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
            };
            let _ = call(&["stop"]);
            for _ in 0..30 {
                if call(&["service", "uninstall", self.2.to_str().unwrap()])
                    .is_ok_and(|s| s.success())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _cleanup = Cleanup(&binary, &f.state, &registration);
    f.call(&binary, &["service", "install", dir]).await;
    f.call(&binary, &["service", "install", dir]).await;
    let receipt = registration.join("task-export.xml");
    let original = std::fs::read(&receipt).unwrap();
    let xml = String::from_utf8(original.clone()).unwrap();
    assert!(xml.contains("remodex-startup.exe"));
    assert!(!xml.contains("powershell"));
    assert!(!xml.contains("Bypass"));
    // Windows omits the default LeastPrivilege element when exporting XML.
    // Successful install/status also validates the actual COM RunLevel value.
    assert!(!xml.contains("HighestAvailable"));
    std::fs::write(
        &receipt,
        xml.replace(
            "Remodex per-user Rust server startup",
            "Changed registration",
        ),
    )
    .unwrap();
    let rejected = f
        .command(&binary, &["service", "uninstall", dir])
        .output()
        .await
        .unwrap();
    std::fs::write(&receipt, &original).unwrap();
    assert!(
        !rejected.status.success(),
        "A changed receipt must not authorize task removal"
    );
    assert_eq!(
        f.call(&binary, &["service", "status", dir]).await["registered"],
        true
    );
    f.call(&binary, &["service", "start", dir]).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if client
            .get(format!("http://127.0.0.1:{}/healthz", f.port + 5))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "Scheduled Rust startup did not become ready");
    let status = f.call(&binary, &["service", "status", dir]).await;
    assert_eq!(
        status["state"], "Running",
        "Scheduler must track the complete server lifetime"
    );
    let rejected = f
        .command(&binary, &["service", "uninstall", dir])
        .output()
        .await
        .unwrap();
    assert!(
        !rejected.status.success(),
        "A running service must not be unregistered"
    );
    f.call(&binary, &["stop"]).await;
    for _ in 0..50 {
        if f.call(&binary, &["service", "status", dir]).await["state"] == "Ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f.call(&binary, &["service", "uninstall", dir]).await;
    assert_eq!(
        f.call(&binary, &["service", "status", dir]).await["registered"],
        false
    );
    assert!(!receipt.exists());
}
