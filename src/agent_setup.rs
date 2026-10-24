//! Setup reads/writes are scoped to the explicit isolated Claude profile.
//! No Desktop settings discovery, normal-profile fallback or login automation.
use crate::chat::{AgentConfig, AgentKind};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_SETTINGS: u64 = 1024 * 1024;

pub(crate) fn verification_issue(detail: &str) -> Option<&'static str> {
    let text = detail.to_ascii_lowercase();
    if [
        "device verification required",
        "trusted_device_required",
        "trusted device is not enrolled",
        "session expired for trusted-device check",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        Some("device-verification")
    } else if [
        "sign in to claude code",
        "refresh claude code sign-in",
        "authentication_required",
        "authentication_error",
        "invalid authentication credentials",
        "sign-in has no session credential",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        Some("sign-in")
    } else {
        None
    }
}

#[derive(Clone)]
pub(crate) struct ClaudeSetup {
    config: AgentConfig,
    cwd: PathBuf,
    files: Arc<tokio::sync::Semaphore>,
    probes: Arc<tokio::sync::Semaphore>,
    account: Arc<Mutex<Option<(Instant, PathBuf, Value)>>>,
    handoff: Option<Arc<crate::profile_handoff::Session>>,
}
impl ClaudeSetup {
    pub fn new(
        mut config: AgentConfig,
        cwd: PathBuf,
        handoff: Option<Arc<crate::profile_handoff::Session>>,
    ) -> Result<Self, String> {
        config.profile = config
            .profile
            .canonicalize()
            .map_err(|_| "Claude test profile is unavailable")?;
        Ok(Self {
            config,
            cwd,
            files: Arc::new(tokio::sync::Semaphore::new(1)),
            probes: Arc::new(tokio::sync::Semaphore::new(1)),
            account: Arc::default(),
            handoff,
        })
    }
    pub async fn preference(&self, enabled: Option<bool>) -> Result<Value, String> {
        let permit = self
            .files
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Claude settings are busy; retry shortly")?;
        let profile = self.config.profile.clone();
        let handoff = self.handoff.clone();
        tokio::task::spawn_blocking(move || {
            let _handoff = handoff;
            let _permit = permit;
            if let Some(enabled) = enabled {
                save_preference(&profile, enabled)?;
            }
            Ok(preference_status(&profile))
        })
        .await
        .map_err(|_| "Claude settings worker stopped")?
    }
    pub async fn account(&self, refresh: bool, executable: &Path) -> Value {
        let cached = || {
            self.account.lock().ok().and_then(|v| {
                v.as_ref()
                    .filter(|(at, path, _)| {
                        path == executable
                            && at.elapsed() < Duration::from_secs(if refresh { 2 } else { 60 })
                    })
                    .map(|(_, _, v)| v.clone())
            })
        };
        if let Some(value) = cached() {
            return value;
        }
        let Ok(_permit) = self.probes.clone().try_acquire_owned() else {
            return json!({"ready":false,"error":"Claude sign-in status is being checked; refresh shortly","account":{}});
        };
        if let Some(value) = cached() {
            return value;
        }
        let mut command = tokio::process::Command::new(executable);
        command
            .args(["auth", "status", "--json"])
            .current_dir(&self.cwd)
            .env_clear()
            .envs(crate::discovery::environment(
                AgentKind::Claude,
                &self.config.profile,
            ));
        let value = match crate::subprocess::run(
            command,
            Duration::from_secs(10),
            64 * 1024,
            16 * 1024,
        )
        .await
        {
            Ok(output) => account_summary(output.code, &output.stdout),
            Err(_) => {
                json!({"ready":false,"error":"Could not check Claude sign-in on this PC. Check its executable and try again.","account":{}})
            }
        };
        if let Ok(mut cache) = self.account.lock() {
            *cache = Some((Instant::now(), executable.into(), value.clone()));
        }
        value
    }
}
fn account_summary(code: i32, bytes: &[u8]) -> Value {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return json!({"ready":false,"error":"The installed Claude CLI did not report a supported sign-in status.","account":{}});
    };
    let subscription = value["subscriptionType"].as_str().filter(|s| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let ready = code == 0
        && value["loggedIn"] == true
        && value["apiProvider"] == "firstParty"
        && value["authMethod"] == "claude.ai"
        && subscription.is_some();
    // Email, org identifiers, directories and arbitrary CLI errors are private.
    json!({"ready":ready,"error":if ready {Value::Null} else {json!("Sign in to the isolated Claude Code profile with a Claude subscription.")},"account":if ready {json!({"subscriptionType":subscription})} else {json!({})}})
}
struct Settings {
    source: Option<Vec<u8>>,
    value: Value,
    permissions: Option<fs::Permissions>,
}
fn read_settings(profile: &Path) -> Result<Settings, String> {
    let path = profile.join("settings.json");
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() && m.len() <= MAX_SETTINGS => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings {
                source: None,
                value: json!({}),
                permissions: None,
            })
        }
        _ => {
            return Err(
                "Claude user settings must be a regular JSON file smaller than 1 MB.".into(),
            )
        }
    };
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW);
    }
    let file = open
        .open(&path)
        .map_err(|_| "Could not read Claude user settings.")?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_SETTINGS)
    {
        return Err("Claude settings changed; refresh and retry.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read Claude user settings.")?;
    if bytes.len() as u64 > MAX_SETTINGS {
        return Err("Claude settings exceed the size limit.".into());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        "Claude user settings contain invalid JSON. Fix them on this PC before changing sync."
    })?;
    if !value.is_object() {
        return Err("Claude user settings must be a JSON object.".into());
    }
    Ok(Settings {
        source: Some(bytes),
        value,
        permissions: Some(meta.permissions()),
    })
}
fn preference_status(profile: &Path) -> Value {
    match read_settings(profile) {
        Ok(s) => {
            json!({"preference":s.value["remoteControlAtStartup"].as_bool(),"blockedByUserSettings":s.value["disableRemoteControl"] == true,"desktopPreference":"unknown","error":null})
        }
        Err(e) => {
            json!({"preference":null,"blockedByUserSettings":false,"desktopPreference":"unknown","error":e})
        }
    }
}
fn save_preference(profile: &Path, enabled: bool) -> Result<(), String> {
    let before = read_settings(profile)?;
    if enabled && before.value["disableRemoteControl"] == true {
        return Err(
            "Remote Control is disabled in Claude user settings. Enable it there first.".into(),
        );
    }
    if before.value["remoteControlAtStartup"] == enabled {
        return Ok(());
    }
    let mut next = before.value;
    next["remoteControlAtStartup"] = json!(enabled);
    let mut bytes =
        serde_json::to_vec_pretty(&next).map_err(|_| "Cannot encode Claude settings")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_SETTINGS {
        return Err("Claude settings exceed the size limit.".into());
    }
    let temporary = profile.join(format!(".remodex-settings-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "Could not save Claude user settings.")?;
        if let Some(p) = before.permissions {
            file.set_permissions(p)
                .map_err(|_| "Could not retain Claude settings permissions.")?;
        }
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not save Claude user settings.")?;
        drop(file);
        if read_settings(profile)?.source != before.source {
            return Err("Claude settings changed while saving. Refresh and retry.".into());
        }
        fs::rename(&temporary, profile.join("settings.json"))
            .map_err(|_| "Could not replace Claude settings.")?;
        #[cfg(unix)]
        fs::File::open(profile)
            .and_then(|f| f.sync_all())
            .map_err(|_| {
                "Claude preference was saved, but disk durability could not be confirmed."
            })?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_preserve_unrelated_settings_and_refuse_policy_or_invalid_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        assert_eq!(preference_status(root.path())["preference"], Value::Null);
        fs::write(
            &path,
            r#"{"nested":{"keep":[1,"yes"]},"remoteControlAtStartup":false}"#,
        )
        .unwrap();
        save_preference(root.path(), true).unwrap();
        assert_eq!(
            read_settings(root.path()).unwrap().value["nested"],
            json!({"keep":[1,"yes"]})
        );
        assert_eq!(preference_status(root.path())["preference"], true);
        let old = fs::read(&path).unwrap();
        save_preference(root.path(), true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), old);
        for text in [r#"{"disableRemoteControl":true}"#, "INVALID", "[]"] {
            fs::write(&path, text).unwrap();
            assert!(save_preference(root.path(), true).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::write(&path, vec![b' '; MAX_SETTINGS as usize + 1]).unwrap();
        assert!(save_preference(root.path(), false).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn preferences_reject_symlinks_and_preserve_file_mode() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, "{}").unwrap();
        let path = root.path().join("settings.json");
        symlink(&outside, &path).unwrap();
        assert!(save_preference(root.path(), true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "{}");
        fs::remove_file(&path).unwrap();
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        save_preference(root.path(), true).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    #[test]
    fn account_probe_never_exposes_identity_or_confuses_api_keys_with_subscription() {
        let mut v = json!({"loggedIn":true,"apiProvider":"firstParty","authMethod":"claude.ai","subscriptionType":"pro","email":"PRIVATE_EMAIL","orgId":"PRIVATE_ORG","configDirectory":"PRIVATE_PATH"});
        let good = account_summary(0, &serde_json::to_vec(&v).unwrap());
        assert_eq!(good["ready"], true);
        assert!(!good.to_string().contains("PRIVATE"));
        v["authMethod"] = json!("api_key");
        assert_eq!(
            account_summary(0, &serde_json::to_vec(&v).unwrap())["ready"],
            false
        );
        assert_eq!(
            account_summary(1, &serde_json::to_vec(&v).unwrap())["ready"],
            false
        );
        assert_eq!(account_summary(0, b"not-json")["ready"], false);
    }
}
