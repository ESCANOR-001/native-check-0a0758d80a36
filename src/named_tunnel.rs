//! Reuse the existing OS-keyring entry. Tokens never enter migration exports,
//! command arguments, dashboard responses, or diagnostic logs.
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::Path, process::Stdio, time::Duration};
use zeroize::Zeroizing;
const SERVICE: &str = "opencodex.android-remote.cloudflare.v1";
const ERROR: &str = "The saved tunnel credential is unavailable; unlock the operating-system credential store and retry";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub hostname: String,
    pub account: String,
    pub origin: SocketAddr,
}
pub fn account_valid(account: &str) -> bool {
    account.len() == 64
        && account
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let host = self.hostname.as_str();
        if !account_valid(&self.account)
            || !self.origin.ip().is_loopback()
            || self.origin.port() == 0
            || host.len() > 253
            || !host.contains('.')
            || host.parse::<std::net::IpAddr>().is_ok()
            || !host.split('.').all(|part| {
                !part.is_empty()
                    && part.len() <= 63
                    && !part.starts_with('-')
                    && !part.ends_with('-')
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
        {
            return Err("Invalid saved named-tunnel configuration".into());
        }
        Ok(())
    }
    pub fn public_url(&self) -> String {
        format!("https://{}", self.hostname)
    }
}
fn token(account: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    if !account_valid(account) {
        return Err(ERROR.into());
    }
    let bytes = Zeroizing::new(
        keyring::Entry::new(SERVICE, account)
            .map_err(|_| ERROR)?
            .get_secret()
            .map_err(|_| ERROR)?,
    );
    if bytes.is_empty() || bytes.len() > 16384 || !bytes.iter().all(|b| b.is_ascii_graphic()) {
        return Err(ERROR.into());
    }
    Ok(bytes)
}
/// Runs in a disposable helper process so a locked/hung keyring cannot block
/// the bridge executor or leave an unbounded blocking thread behind.
pub async fn helper(args: &[String]) -> Result<i32, String> {
    let account = args.get(2).ok_or(ERROR)?.clone();
    if !(args.len() == 3 || args.len() == 6) {
        return Err(ERROR.into());
    }
    let secret = tokio::task::spawn_blocking(move || token(&account))
        .await
        .map_err(|_| ERROR)??;
    if args.len() == 3 {
        return Ok(0);
    }
    let executable = Path::new(&args[3]);
    if !executable.is_absolute()
        || !executable.is_file()
        || !["quic", "http2"].contains(&args[4].as_str())
    {
        return Err(ERROR.into());
    }
    #[cfg(windows)]
    if !executable
        .extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
    {
        return Err(ERROR.into());
    }
    let empty = Path::new(&args[5]);
    if !empty.is_absolute()
        || !crate::profile_snapshot::bytes(empty, 16).is_ok_and(|b| b == b"{}\n")
    {
        return Err(ERROR.into());
    }
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("tunnel")
        .arg("--config")
        .arg(empty)
        .arg("--origincert")
        .arg(empty.with_extension("unused-cert"))
        .args([
            "--no-autoupdate",
            "--metrics",
            "127.0.0.1:0",
            "--protocol",
            &args[4],
            "run",
        ])
        .env(
            "TUNNEL_TOKEN",
            std::str::from_utf8(&secret).map_err(|_| ERROR)?,
        )
        .env("TUNNEL_NO_PRECHECKS", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| ERROR)?;
    drop(command);
    drop(secret);
    Ok(child.wait().await.map_err(|_| ERROR)?.code().unwrap_or(1))
}
pub async fn preflight(config: &Config) -> Result<(), String> {
    config.validate()?;
    let mut command = tokio::process::Command::new(std::env::current_exe().map_err(|_| ERROR)?);
    command
        .args(["--named-tunnel-helper", &config.account])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| ERROR)?;
    let result = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
    if !matches!(result, Ok(Ok(s)) if s.success()) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(ERROR.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_reference_without_exposing_or_copying_a_secret() {
        let mut c = Config {
            hostname: "phone.example.com".into(),
            account: "a".repeat(64),
            origin: "127.0.0.1:10105".parse().unwrap(),
        };
        assert!(c.validate().is_ok());
        for host in [
            "localhost",
            "127.0.0.1",
            "evil.example/path",
            "x@evil.example",
            "a..example",
            "-a.example",
        ] {
            c.hostname = host.into();
            assert!(c.validate().is_err());
        }
        c.hostname = "phone.example.com".into();
        c.origin = "0.0.0.0:10105".parse().unwrap();
        assert!(c.validate().is_err());
    }
}
