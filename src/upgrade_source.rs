//! Acquire the old bridge's fixed, closed-owner export. No process discovery,
//! PID killing, profile movement, credential migration or remote URL fetching.
use crate::{profile_snapshot, storage_stage};
use serde::Deserialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
const ERROR: &str = "The original bridge upgrade could not be verified. Its history and recovery files were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    version: u32,
    source_root: PathBuf,
    installation: PathBuf,
    instance: String,
    endpoint: String,
    token: String,
}
// Compare Node DOS spellings and Rust verbatim spellings without resolving
// aliases or accepting a different path. The installation may not exist yet.
fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(not(windows))]
    {
        left == right
    }
    #[cfg(windows)]
    {
        fn normalized(p: &Path) -> String {
            let s = p.to_string_lossy();
            if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc}")
            } else {
                s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
            }
        }
        normalized(left).eq_ignore_ascii_case(&normalized(right))
    }
}
fn canonical_spelling(path: &Path) -> Result<bool, String> {
    Ok(same_path(&path.canonicalize().map_err(|_| ERROR)?, path))
}
fn json_path(value: &Value, expected: &Path) -> bool {
    value
        .as_str()
        .is_some_and(|s| same_path(Path::new(s), expected))
}
fn private_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata().map_err(|_| ERROR)?;
        if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
            return Err(ERROR.into());
        }
    }
    profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn load(root: &Path, descriptor: &Path) -> Result<Source, String> {
    let source: Source =
        serde_json::from_slice(&private_bytes(descriptor, 32768)?).map_err(|_| ERROR)?;
    let directory = source
        .source_root
        .join(format!("native-upgrade-{}", source.instance));
    if source.version != 1
        || uuid::Uuid::parse_str(&source.instance).is_err()
        || !source.source_root.is_absolute()
        || !root.is_absolute()
        || !same_path(&source.installation, root)
        || !same_path(descriptor, &directory.join("source.json"))
        || source.token.len() != 64
        || !source.token.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ERROR.into());
    }
    for path in [&source.source_root, &directory] {
        let file = storage_stage::open(path, true).map_err(|_| ERROR)?;
        if !canonical_spelling(path)? {
            return Err(ERROR.into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = file.metadata().map_err(|_| ERROR)?;
            if m.uid() != unsafe { libc::geteuid() } {
                return Err(ERROR.into());
            }
            if path == &directory && m.mode() & 0o077 != 0 {
                return Err(ERROR.into());
            }
        }
        #[cfg(not(unix))]
        let _ = file;
    }
    let url = reqwest::Url::parse(&source.endpoint).map_err(|_| ERROR)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|p| p == 0)
        || url.path() != "/upgrade"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ERROR.into());
    }
    Ok(source)
}
fn completed(source: &Source, directory: &Path) -> Result<Option<PathBuf>, String> {
    let path = directory.join("npm-plan.json");
    if !path.try_exists().map_err(|_| ERROR)? {
        return Ok(None);
    }
    let plan: Value = serde_json::from_slice(&private_bytes(&path, 32768)?).map_err(|_| ERROR)?;
    let receipt = directory.join("export/closed-owner.json");
    if plan["version"] != 1 || !json_path(&plan["closed_owner"], &receipt) {
        return Err(ERROR.into());
    }
    let owner: Value =
        serde_json::from_slice(&private_bytes(&receipt, 8 * 1024 * 1024)?).map_err(|_| ERROR)?;
    if owner["profile_mode"] != "existing"
        || !json_path(
            &owner["source_hold"],
            &source.source_root.join("native-upgrade-source-hold.json"),
        )
    {
        return Err(ERROR.into());
    }
    // The npm coordinator separately validates the receipt, hold, all native
    // transcript hashes and the original profile bindings before selection.
    Ok(Some(path))
}
pub async fn acquire(root: &Path, descriptor: &Path) -> Result<PathBuf, String> {
    acquire_for(root, descriptor, Duration::from_secs(300)).await
}
async fn acquire_for(
    root: &Path,
    descriptor: &Path,
    deadline: Duration,
) -> Result<PathBuf, String> {
    let source = load(root, descriptor).map_err(|e| format!("{e} (source descriptor)"))?;
    let directory = descriptor.parent().ok_or(ERROR)?;
    // Once the owner published its plan, it may have exited. Read the immutable
    // receipt directly, without needing its old TCP port or issuing another POST.
    if let Some(plan) =
        completed(&source, directory).map_err(|e| format!("{e} (published receipt)"))?
    {
        return Ok(plan);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ERROR)?;
    let until = tokio::time::Instant::now() + deadline;
    let mut started = false;
    loop {
        if let Some(plan) =
            completed(&source, directory).map_err(|e| format!("{e} (published receipt)"))?
        {
            return Ok(plan);
        }
        let method = if started {
            reqwest::Method::GET
        } else {
            reqwest::Method::POST
        };
        started = true;
        let response = client
            .request(method, &source.endpoint)
            .bearer_auth(&source.token)
            .header("x-remodex-upgrade-instance", &source.instance)
            .send()
            .await;
        if let Ok(mut response) = response {
            if response.status() != reqwest::StatusCode::OK {
                return completed(&source, directory)
                    .map_err(|e| format!("{e} (published receipt)"))?
                    .ok_or_else(|| format!("{ERROR} (source HTTP status {})", response.status()));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| ERROR)? {
                if bytes.len() + chunk.len() > 4096 {
                    return Err(ERROR.into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
            if value["version"] != 1 || value["instance"] != source.instance {
                return Err(ERROR.into());
            }
            match value["phase"].as_str() {
                Some("ready") => {
                    return completed(&source, directory)
                        .map_err(|e| format!("{e} (published receipt)"))?
                        .ok_or_else(|| ERROR.into())
                }
                Some("exporting") => {}
                // Idle after a lost POST is uncertain delivery. Do not retry a
                // shutdown request or silently start a replacement runtime.
                _ => return Err(format!("{ERROR} (source response phase)")),
            }
        }
        if let Some(plan) =
            completed(&source, directory).map_err(|e| format!("{e} (published receipt)"))?
        {
            return Ok(plan);
        }
        if tokio::time::Instant::now() >= until {
            return Err(ERROR.into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    fn write(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        directory: PathBuf,
        descriptor: PathBuf,
        source: Value,
    }
    impl Fixture {
        fn new(port: u16) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let instance = uuid::Uuid::new_v4().to_string();
            let directory = base.join(format!("native-upgrade-{instance}"));
            fs::create_dir(&directory).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let root = base.join("installation");
            let descriptor = directory.join("source.json");
            let source = json!({"version":1,"source_root":base,"installation":root,"instance":instance,
                "endpoint":format!("http://127.0.0.1:{port}/upgrade"),"token":"a".repeat(64)});
            write(&descriptor, &source);
            Self {
                _temp: temp,
                root,
                directory,
                descriptor,
                source,
            }
        }
        fn completed(&self) {
            publish(&self.directory, &self.source);
        }
    }
    fn publish(directory: &Path, source: &Value) {
        let export = directory.join("export");
        fs::create_dir(&export).unwrap();
        write(
            &export.join("closed-owner.json"),
            &json!({"profile_mode":"existing",
            "source_hold":PathBuf::from(source["source_root"].as_str().unwrap()).join("native-upgrade-source-hold.json")}),
        );
        write(
            &directory.join("npm-plan.json"),
            &json!({"version":1,"closed_owner":export.join("closed-owner.json")}),
        );
    }
    #[tokio::test]
    async fn completed_source_is_recovered_without_the_old_listener() {
        let f = Fixture::new(1);
        f.completed();
        assert_eq!(
            acquire(&f.root, &f.descriptor).await.unwrap(),
            f.directory.join("npm-plan.json")
        );
        let mut bad = f.source.clone();
        bad["endpoint"] = json!("https://example.com/upgrade");
        write(&f.descriptor, &bad);
        assert!(acquire(&f.root, &f.descriptor).await.is_err());
        write(&f.descriptor, &f.source);
        assert!(acquire(&f.root.with_file_name("other"), &f.descriptor)
            .await
            .is_err());
        write(
            &f.directory.join("export/closed-owner.json"),
            &json!({"profile_mode":"copied","source_hold":"foreign"}),
        );
        assert!(acquire(&f.root, &f.descriptor).await.is_err());
    }
    #[tokio::test]
    async fn requests_one_owned_export_and_polls_until_the_durable_plan_exists() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = Fixture::new(listener.local_addr().unwrap().port());
        let posts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let p = posts.clone();
        let r = reads.clone();
        let source = f.source.clone();
        let directory = f.directory.clone();
        let app = axum::Router::new().route(
            "/upgrade",
            axum::routing::any(
                move |method: axum::http::Method, headers: axum::http::HeaderMap| {
                    let (p, r, source, directory) =
                        (p.clone(), r.clone(), source.clone(), directory.clone());
                    async move {
                        assert_eq!(
                            headers["authorization"],
                            format!("Bearer {}", "a".repeat(64))
                        );
                        assert_eq!(
                            headers["x-remodex-upgrade-instance"],
                            source["instance"].as_str().unwrap()
                        );
                        if method == axum::http::Method::POST {
                            p.fetch_add(1, Ordering::SeqCst);
                        } else if r.fetch_add(1, Ordering::SeqCst) == 0 {
                            publish(&directory, &source);
                        }
                        axum::Json(
                            json!({"version":1,"instance":source["instance"],"phase":"exporting"}),
                        )
                    }
                },
            ),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert!(acquire_for(&f.root, &f.descriptor, Duration::from_secs(3))
            .await
            .is_ok());
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        server.abort();
        assert!(acquire(&f.root, &f.descriptor).await.is_ok());
    }
    #[tokio::test]
    async fn wrong_instance_failed_or_redirected_owner_never_yields_an_upgrade() {
        for phase in ["failed", "idle", "wrong-instance", "redirect"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let f = Fixture::new(listener.local_addr().unwrap().port());
            let instance = f.source["instance"].clone();
            let posts = Arc::new(AtomicUsize::new(0));
            let count = posts.clone();
            let app=axum::Router::new().route("/upgrade",axum::routing::any(move || {
                let (count,instance)=(count.clone(),instance.clone());
                async move {
                    count.fetch_add(1,Ordering::SeqCst);
                    let status=if phase=="redirect" { axum::http::StatusCode::TEMPORARY_REDIRECT } else { axum::http::StatusCode::OK };
                    (status,axum::Json(json!({"version":1,"instance":if phase=="wrong-instance" {json!("foreign")} else {instance},"phase":phase})))
                }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            assert!(acquire(&f.root, &f.descriptor).await.is_err());
            assert_eq!(posts.load(Ordering::SeqCst), 1);
            assert!(!f.root.exists());
            server.abort();
        }
    }
    #[test]
    fn descriptor_links_and_shared_credentials_are_rejected() {
        let f = Fixture::new(1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f.descriptor, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load(&f.root, &f.descriptor).is_err());
            fs::set_permissions(&f.descriptor, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(load(&f.root, &f.descriptor).is_ok());
            let original = f.directory.join("original.json");
            fs::rename(&f.descriptor, &original).unwrap();
            std::os::unix::fs::symlink(&original, &f.descriptor).unwrap();
            assert!(load(&f.root, &f.descriptor).is_err());
        }
        #[cfg(windows)]
        {
            let other = f.directory.join("linked.json");
            fs::hard_link(&f.descriptor, other).unwrap();
            assert!(load(&f.root, &f.descriptor).is_err());
        }
    }
}
