//! Control only the selected installation through its local dashboard session.
//! Read-only journal inspection deliberately does not take the lifetime owner
//! leases. No PID discovery, process signalling, fallback or agent startup.
use super::*;
use std::net::SocketAddr;

const CONTROL_ERROR: &str =
    "The selected native bridge could not be verified. No other process was controlled";
struct Target {
    base: String,
    environment: String,
}
fn database(path: &Path) -> Result<Connection, String> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let p = PathBuf::from(name);
        if suffix.is_empty() || fs::symlink_metadata(&p).is_ok() {
            let file = storage_stage::open(&p, false).map_err(|_| CONTROL_ERROR)?;
            if storage_stage::links(&file).map_err(|_| CONTROL_ERROR)? != 1 {
                return Err(CONTROL_ERROR.into());
            }
        }
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| CONTROL_ERROR)?;
    db.busy_timeout(Duration::from_millis(500))
        .map_err(|_| CONTROL_ERROR)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 262144)
        .map_err(|_| CONTROL_ERROR)?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA cache_size=-256; PRAGMA mmap_size=0;")
        .map_err(|_| CONTROL_ERROR)?;
    Ok(db)
}
fn target(root: &Path) -> Result<Target, String> {
    let root = location(root)?;
    storage_stage::open(&root, true).map_err(|_| CONTROL_ERROR)?;
    let db = database(&root.join("npm-upgrade.sqlite"))?;
    let (raw, attempt, phase): (Vec<u8>, u32, String) = db
        .query_row(
            "SELECT plan,attempt,phase FROM upgrade WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| CONTROL_ERROR)?;
    if phase != "selected" || !(1..=3).contains(&attempt) {
        return Err(CONTROL_ERROR.into());
    }
    let plan: Plan = serde_json::from_slice(&raw).map_err(|_| CONTROL_ERROR)?;
    if plan.version != 1 {
        return Err(CONTROL_ERROR.into());
    }
    let generation_root = root.join(format!("attempt-{attempt}"));
    let expected = json!({"version":1,"export_root":generation_root,"bind":plan.bind,"public_url":plan.public_url,"gui":plan.gui});
    let intent: Value = serde_json::from_slice(&read(&root.join("cutover.json"), 32768)?)
        .map_err(|_| CONTROL_ERROR)?;
    if intent != expected {
        return Err(CONTROL_ERROR.into());
    }
    storage_stage::open(&root.join("service"), true).map_err(|_| CONTROL_ERROR)?;
    let service = database(&root.join("service/service.sqlite"))?;
    let (saved, digest, phase): (Vec<u8>, String, String) = service
        .query_row(
            "SELECT plan,config_hash,phase FROM selection WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| CONTROL_ERROR)?;
    if serde_json::from_slice::<Value>(&saved).map_err(|_| CONTROL_ERROR)? != expected
        || !matches!(phase.as_str(), "selected" | "started")
    {
        return Err(CONTROL_ERROR.into());
    }
    let generation = generation_root.join("generation");
    for p in [&generation_root, &generation] {
        storage_stage::open(p, true).map_err(|_| CONTROL_ERROR)?;
    }
    let bytes = read(&generation.join("configuration.json"), 262144)?;
    if hash(&bytes) != digest {
        return Err(CONTROL_ERROR.into());
    }
    let config: crate::chat::Config = serde_json::from_slice(&bytes).map_err(|_| CONTROL_ERROR)?;
    let address: SocketAddr = match config.connection {
        Some(c) => c.dashboard_bind,
        None => plan.bind.parse().map_err(|_| CONTROL_ERROR)?,
    };
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(CONTROL_ERROR.into());
    }
    let prepared: Value =
        serde_json::from_slice(&read(&generation_root.join("prepared.json"), 262144)?)
            .map_err(|_| CONTROL_ERROR)?;
    if prepared["phase"] != "prepared" || prepared["generation"] != json!(generation) {
        return Err(CONTROL_ERROR.into());
    }
    let hold = &prepared["source_hold"];
    crate::source_hold::check(hold)?;
    let owner: Value = serde_json::from_slice(&read(
        Path::new(hold["path"].as_str().ok_or(CONTROL_ERROR)?),
        4096,
    )?)
    .map_err(|_| CONTROL_ERROR)?;
    let environment = owner["environment"]
        .as_str()
        .filter(|s| {
            s.len() <= 128
                && !s.is_empty()
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .ok_or(CONTROL_ERROR)?
        .to_owned();
    Ok(Target {
        base: format!("http://{address}"),
        environment,
    })
}
async fn body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if !response.status().is_success() || response.content_length().is_some_and(|n| n > 65536) {
        return Err(CONTROL_ERROR.into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| CONTROL_ERROR)? {
        if bytes.len().saturating_add(chunk.len()) > 65536 {
            return Err(CONTROL_ERROR.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
struct Live {
    token: String,
    csrf: String,
    pid: u32,
}
async fn live(client: &reqwest::Client, target: &Target) -> Result<Live, String> {
    let response = client
        .get(format!("{}/opencodex-session", target.base))
        .send()
        .await
        .map_err(|_| CONTROL_ERROR)?;
    let html = String::from_utf8(body(response).await?).map_err(|_| CONTROL_ERROR)?;
    let meta = |name: &str| -> Result<String, String> {
        let prefix = format!("name=\"{name}\" content=\"");
        let mut parts = html.split(&prefix);
        parts.next();
        let value = parts
            .next()
            .and_then(|s| s.split('"').next())
            .ok_or(CONTROL_ERROR)?;
        if parts.next().is_some()
            || value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(CONTROL_ERROR.into());
        }
        Ok(value.to_owned())
    };
    let token = meta("opencodex-session-token")?;
    let csrf = meta("opencodex-session-csrf")?;
    let pid = identity(client, target, &token).await?;
    Ok(Live { token, csrf, pid })
}
async fn identity(client: &reqwest::Client, target: &Target, token: &str) -> Result<u32, String> {
    let response = client
        .get(format!("{}/api/connect/runtime", target.base))
        .header("x-opencodex-api-key", token)
        .header("x-opencodex-gui-origin", &target.base)
        .send()
        .await
        .map_err(|_| CONTROL_ERROR)?;
    let v: Value = serde_json::from_slice(&body(response).await?).map_err(|_| CONTROL_ERROR)?;
    if v["runtime"] != "rust" || v["environmentId"] != target.environment {
        return Err(CONTROL_ERROR.into());
    }
    let pid = v["pid"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or(CONTROL_ERROR)?;
    Ok(pid)
}
async fn offline(target: &Target) -> bool {
    let Some(address) = target.base.strip_prefix("http://") else {
        return false;
    };
    matches!(tokio::time::timeout(Duration::from_secs(1),tokio::net::TcpStream::connect(address)).await,
        Ok(Err(error)) if error.kind()==std::io::ErrorKind::ConnectionRefused)
}
pub async fn command(root: &Path, operation: &str) -> Result<Value, String> {
    if !matches!(operation, "status" | "stop" | "restart") {
        return Err(CONTROL_ERROR.into());
    }
    let target = target(root)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| CONTROL_ERROR)?;
    let owner = match live(&client, &target).await {
        Ok(owner) => owner,
        Err(error) => {
            // A refused TCP connection alone is not proof of shutdown. The
            // lifetime lease must also be available; never signal a PID.
            if operation == "status"
                && offline(&target).await
                && profile_lock::acquire(root).is_ok()
            {
                return Ok(
                    json!({"runtime":"rust","running":false,"dashboard":target.base,"profilesKeptInPlace":true}),
                );
            }
            return Err(error);
        }
    };
    if operation == "status" {
        return Ok(
            json!({"runtime":"rust","running":true,"pid":owner.pid,"dashboard":target.base,"profilesKeptInPlace":true}),
        );
    }
    let response = client
        .post(format!("{}/api/{operation}", target.base))
        .header("x-opencodex-api-key", &owner.token)
        .header("x-opencodex-csrf-token", &owner.csrf)
        .header("x-opencodex-gui-origin", &target.base)
        .header("origin", &target.base)
        .header("x-opencodex-expected-pid", owner.pid)
        .header("x-remodex-expected-environment", &target.environment)
        .json(&json!({}))
        .send()
        .await
        .map_err(|_| CONTROL_ERROR)?;
    let response: Value =
        serde_json::from_slice(&body(response).await?).map_err(|_| CONTROL_ERROR)?;
    if response["success"] != true {
        return Err(CONTROL_ERROR.into());
    }
    let until = tokio::time::Instant::now()
        + Duration::from_secs(if operation == "restart" { 80 } else { 45 });
    while tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Reuse the original session while this owner drains. Issuing one on
        // each poll would exhaust the bounded dashboard-session cache.
        let next = match identity(&client, &target, &owner.token).await {
            Ok(pid) => Ok(pid),
            Err(_) => live(&client, &target).await.map(|next| next.pid),
        };
        match next {
            Ok(pid) if operation == "restart" && pid != owner.pid => {
                return Ok(json!({"runtime":"rust","restarted":true,"pid":pid}))
            }
            Err(_)
                if operation == "stop"
                    && offline(&target).await
                    && profile_lock::acquire(root).is_ok() =>
            {
                return Ok(json!({"runtime":"rust","stopped":true}))
            }
            _ => {}
        }
    }
    Err("The native control request was acknowledged, but completion was not confirmed. No process was force-stopped".into())
}
