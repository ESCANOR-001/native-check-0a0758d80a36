//! Read the published, stopped 1.2.27 layout without executing a JS exporter.
//! Native transcripts stay in their original profiles. No PID is guessed or
//! terminated, and a temporary remote address is never silently replaced.
use crate::{profile_snapshot, storage_stage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

const ERROR: &str =
    "The npm 1.2.27 saved data could not be verified; original chats and pairing were retained";
#[path = "npm_legacy_claude.rs"]
mod claude;
const RUNNING: &str = "Finish active tasks and run the old rmx stop before upgrading npm 1.2.27. Its running server and saved pairing were left unchanged";
fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
        return Err(ERROR.into());
    }
    profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn write(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| ERROR)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| ERROR)?;
    file.write_all(&bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR.into())
}
pub(crate) fn stopped(source: &Path) -> Result<(), String> {
    for name in ["runtime-port.json", "ocx.pid"] {
        if source.join(name).try_exists().map_err(|_| ERROR)? {
            return Err(RUNNING.into());
        }
    }
    Ok(())
}
pub(crate) fn check_hold(source: &Path, proof: &Value) -> Result<(), String> {
    stopped(source)?;
    if proof["version"] != "1.2.27"
        || proof["connectionSha256"]
            != hash(&read(
                &source.join("android-remote.json"),
                16 * 1024 * 1024,
            )?)
    {
        return Err(ERROR.into());
    }
    Ok(())
}
fn source_identity(source: &Path, hostname: &str) -> Result<(String, String), String> {
    let owner_path = source.join(".opencodex-owner.json");
    let root = match fs::symlink_metadata(&owner_path) {
        Ok(_) => {
            // An invalid, linked, or unreadable record is not a missing record.
            let owner: Value =
                serde_json::from_slice(&read(&owner_path, 8192)?).map_err(|_| ERROR)?;
            if owner["version"] != 1
                || uuid::Uuid::parse_str(owner["ownerId"].as_str().ok_or(ERROR)?).is_err()
            {
                return Err(ERROR.into());
            }
            owner["root"].as_str().ok_or(ERROR)?.to_owned()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 1.2.27 creates its uninstall ownership record only in an empty
            // directory. Connect still supports pre-existing state without it.
            // Its gateway/keyring hash the lexically resolved configuration
            // directory, not realpath. Keep the CLI-selected path spelling:
            // Windows canonicalize() adds a \\?\ prefix and would break pairing.
            if !source.is_absolute()
                || source.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(ERROR.into());
            }
            storage_stage::open(source, true).map_err(|_| ERROR)?;
            source.to_str().ok_or(ERROR)?.to_owned()
        }
        Err(_) => return Err(ERROR.into()),
    };
    if Path::new(&root).canonicalize().map_err(|_| ERROR)?
        != source.canonicalize().map_err(|_| ERROR)?
        || hostname.is_empty()
        || hostname.chars().any(char::is_control)
    {
        return Err(ERROR.into());
    }
    // Match the original gateway and OS-keyring account exactly, including its
    // original Windows path spelling. Never hash the new destination's path.
    Ok((
        format!(
            "opencodex-{}",
            &hash(format!("{hostname}\0{root}").as_bytes())[..24]
        ),
        hash(format!("opencodex-android-remote-cloudflare-v1\0{root}").as_bytes()),
    ))
}
fn connection_mode(connection: &Value) -> Result<(), String> {
    if connection["settings"]["connectionChoice"] == "quick" {
        return Err("npm 1.2.27 cannot retain a temporary internet address when it stops. Choose a permanent address in the old dashboard before upgrading; chats and pairing have not been changed".into());
    }
    Ok(())
}
fn cloudflared(source: &Path) -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("OPENCODEX_CLOUDFLARED_PATH") {
        let path = PathBuf::from(path);
        storage_stage::open(&path, false).map_err(|_| ERROR)?;
        return path.canonicalize().map_err(|_| ERROR.into());
    }
    let root = source.join("tools/cloudflared");
    let mut versions = fs::read_dir(&root)
        .map_err(|_| ERROR)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ERROR)?;
    versions.sort_by_key(|e| e.file_name());
    for version in versions.into_iter().rev() {
        let path = version.path().join("win32-x64/cloudflared.exe");
        if path.try_exists().map_err(|_| ERROR)? {
            storage_stage::open(&path, false).map_err(|_| ERROR)?;
            return path.canonicalize().map_err(|_| ERROR.into());
        }
    }
    Err("The saved Cloudflare connection helper is missing; restore it before upgrading".into())
}
fn reference(
    profile: &Path,
    path: &Path,
    id: &str,
    native: &str,
) -> Result<(Value, String, String), String> {
    let absolute = path.canonicalize().map_err(|_| ERROR)?;
    let relative = absolute
        .strip_prefix(profile)
        .map_err(|_| ERROR)?
        .to_str()
        .ok_or(ERROR)?
        .replace('\\', "/");
    let mut rollout =
        crate::codex_rollout::open(&absolute, 2 * 1024 * 1024 * 1024).map_err(|_| ERROR)?;
    let mut line = Vec::new();
    BufReader::new((&mut rollout.reader).take(8 * 1024 * 1024 + 1))
        .read_until(b'\n', &mut line)
        .map_err(|_| ERROR)?;
    let digest = hash(&line);
    let value = json!({"version":1,"threadId":id,"nativeId":native,"profile":profile,"transcript":relative,"firstLineSha256":digest,"firstLineBytes":line.len()});
    crate::legacy_desktop_reference::Reference::parse(
        &serde_json::to_vec(&value).map_err(|_| ERROR)?,
    )?
    .matches_receipt(profile, id, &relative, &digest)?;
    Ok((value, relative, digest))
}
async fn selection_aliases(
    connection: &mut Value,
    reader: &mut crate::codex_history_reader::Reader,
) -> Result<(), String> {
    let selections = connection["taskSelections"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut retained = Vec::new();
    for selected in selections {
        let native = selected["nativeThreadId"].as_str().ok_or(ERROR)?;
        let aliases = connection["threadAliases"].as_array_mut().ok_or(ERROR)?;
        if aliases.iter().any(|a| a["nativeThreadId"] == native) {
            continue;
        }
        if uuid::Uuid::parse_str(native).is_err() {
            if selected["remoteThreadId"] != native || !native.starts_with("thread-") {
                return Err(ERROR.into());
            }
            retained.push(selected);
            continue;
        }
        let header = reader.metadata(native).await?;
        let cwd = header["cwd"].as_str().ok_or(ERROR)?;
        let timestamp = |key: &str| -> Result<String, String> {
            time::OffsetDateTime::from_unix_timestamp(header[key].as_i64().ok_or(ERROR)?)
                .map_err(|_| ERROR)?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| ERROR.into())
        };
        let title = header["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or(header["preview"].as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Codex chat");
        let provider = header["modelProvider"].as_str().ok_or(ERROR)?;
        let model = selected["model"].as_str().ok_or(ERROR)?;
        let bare = model.strip_prefix(&format!("{provider}/")).unwrap_or(model);
        aliases.push(json!({"remoteThreadId":selected["remoteThreadId"],"nativeThreadId":native,
            "projectId":format!("codex-project-{}",&hash(cwd.to_lowercase().as_bytes())[..20]),
            "cwd":cwd,"title":title,"model":bare,"instanceId":provider,"nativeModelProvider":provider,
            "runtimeMode":"approval-required","interactionMode":"default","createdAt":timestamp("createdAt")?,"updatedAt":timestamp("updatedAt")?,"externalCodex":true}));
    }
    if !retained.is_empty() {
        connection["taskSelections"]
            .as_array_mut()
            .ok_or(ERROR)?
            .retain(|s| !retained.contains(s));
        connection["retainedCodexMetadata"] =
            json!({"reason":"native-chat-unavailable","aliases":[],"selections":retained});
    }
    Ok(())
}
/// Select this reader only when no modern source registration exists. An
/// interrupted modern upgrade must never be reinterpreted as a legacy layout.
pub(crate) fn applicable(root: &Path) -> Result<bool, String> {
    let hold = root
        .parent()
        .ok_or(ERROR)?
        .join("native-upgrade-source-hold.json");
    let historical = if hold.try_exists().map_err(|_| ERROR)? {
        let value: Value = serde_json::from_slice(&read(&hold, 4096)?).map_err(|_| ERROR)?;
        value.get("historicalSource").is_some()
    } else {
        false
    };
    Ok(!root
        .join("npm-source.json")
        .try_exists()
        .map_err(|_| ERROR)?
        && (historical
            || !root
                .join("npm-upgrade.sqlite")
                .try_exists()
                .map_err(|_| ERROR)?)
        && root
            .parent()
            .ok_or(ERROR)?
            .join("android-remote.json")
            .try_exists()
            .map_err(|_| ERROR)?)
}
pub(crate) async fn prepare(root: &Path, gui: &Path, port: Option<u16>) -> Result<Value, String> {
    if !cfg!(windows) {
        return Err("The npm 1.2.27 reader currently supports Windows only".into());
    }
    let selected_source = root.parent().ok_or(ERROR)?;
    let source = selected_source.canonicalize().map_err(|_| ERROR)?;
    stopped(&source)?;
    let retained_hold = source.join("native-upgrade-source-hold.json");
    if retained_hold.try_exists().map_err(|_| ERROR)? {
        let value: Value =
            serde_json::from_slice(&read(&retained_hold, 4096)?).map_err(|_| ERROR)?;
        check_hold(&source, &value["historicalSource"])?;
        let export = Path::new(value["export"].as_str().ok_or(ERROR)?)
            .canonicalize()
            .map_err(|_| ERROR)?;
        if export.parent() != Some(source.as_path())
            || export
                .file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !n.starts_with("native-legacy-"))
        {
            return Err(ERROR.into());
        }
        let binding = crate::source_hold::bind(
            &retained_hold,
            &export,
            value["environment"].as_str().ok_or(ERROR)?,
        )?;
        crate::source_hold::check(&binding)?;
        return crate::npm_upgrade::prepare(root, &export.join("plan.json"));
    }
    let original = read(&source.join("android-remote.json"), 16 * 1024 * 1024)?;
    let mut connection: Value = serde_json::from_slice(&original).map_err(|_| ERROR)?;
    connection_mode(&connection)?;
    let hostname = std::env::var("COMPUTERNAME").map_err(|_| ERROR)?;
    let (environment, account) = source_identity(selected_source, &hostname)?;
    crate::legacy_connection::Prepared::parse(&original, &environment)?;
    let port = port.unwrap_or(10100);
    // Published 1.2.27 always used 10105 for phones, even when its dashboard
    // port was changed. Preserve that endpoint rather than deriving a new one.
    if port != 10100 {
        return Err(
            "npm 1.2.27 upgrades currently retain dashboard port 10100 and phone port 10105".into(),
        );
    }
    let retained_profile = crate::native_service::legacy_codex_profile(root)?;
    let mut config = crate::installation::defaults::configuration_with_codex_profile(
        port,
        retained_profile.as_deref(),
    )
    .await?;
    // The historical gateway used the user's home for projectless chats, not
    // whichever directory happened to contain the npm upgrade command.
    config.workspace = PathBuf::from(std::env::var_os("USERPROFILE").ok_or(ERROR)?)
        .canonicalize()
        .map_err(|_| ERROR)?;
    let codex = config
        .codex
        .as_mut()
        .ok_or("The original Codex CLI/profile is required for this upgrade")?;
    codex.shared_server = Some(serde_json::from_value(json!({"desktop_endpoint":{"path":r"\\.\pipe\codex-ipc"},"reader_executable":codex.executable,"reader_workspace":config.workspace,"activation":{"kind":"system"}})).map_err(|_| ERROR)?);
    let c = config.connection.as_mut().ok_or(ERROR)?;
    c.label = Some(hostname);
    let public_url = if connection["settings"]["tunnelMode"] == "named" {
        let named = crate::named_tunnel::Config {
            hostname: connection["settings"]["namedTunnelHostname"]
                .as_str()
                .ok_or(ERROR)?
                .into(),
            account,
            origin: "127.0.0.1:10105".parse().unwrap(),
        };
        named.validate()?;
        crate::named_tunnel::preflight(&named).await?;
        c.cloudflared = Some(cloudflared(&source)?);
        let url = named.public_url();
        c.named_tunnel = Some(named);
        url
    } else {
        format!("http://{}:10105", crate::installation::defaults::local_ip())
    };
    let claude_root = source.join("managed-claude-code");
    let settings = if claude_root
        .join("settings.json")
        .try_exists()
        .map_err(|_| ERROR)?
    {
        serde_json::from_slice(&read(&claude_root.join("settings.json"), 1024 * 1024)?)
            .map_err(|_| ERROR)?
    } else {
        json!({"enabled":false,"projects":[]})
    };
    if settings["enabled"] == true {
        let claude = config.claude.as_mut().ok_or(
            "Restore the installed Claude executable before upgrading its enabled connection",
        )?;
        claude.remote_control = settings["desktopSync"] == true;
        if let Some(binary) = settings["binary"].as_str().filter(|s| !s.is_empty()) {
            claude.executable = PathBuf::from(binary);
        }
    }
    config.validate_export()?;
    let export = source.join(format!("native-legacy-{}", uuid::Uuid::new_v4()));
    crate::private_directory::prepare(&export).map_err(|_| ERROR)?;
    let hold_path = source.join("native-upgrade-source-hold.json");
    if hold_path.try_exists().map_err(|_| ERROR)? {
        return Err("A previous upgrade has retained recovery data; resume or cancel it before starting another".into());
    }
    let mut reader =
        crate::codex_history_reader::Reader::open(config.codex()?, &config.workspace).await?;
    let result = async {
        selection_aliases(&mut connection,&mut reader).await?;
        let aliases = connection["threadAliases"].as_array_mut().ok_or(ERROR)?;
        let mut chats=Vec::new(); let mut projects=BTreeMap::new();
        for (index,alias) in aliases.iter_mut().enumerate() {
            let id=alias["remoteThreadId"].as_str().ok_or(ERROR)?.to_owned();
            let native=alias["nativeThreadId"].as_str().ok_or(ERROR)?.to_owned();
            let header=reader.metadata(&native).await?;
            let header=header.get("thread").unwrap_or(&header);
            let profile=&config.codex()?.profile;
            let (value,transcript,digest)=reference(profile,Path::new(header["path"].as_str().ok_or(ERROR)?),&id,&native)?;
            alias["externalCodex"]=json!(true);
            alias["cwd"]=header["cwd"].clone();
            let history=format!("codex-{index}.json"); write(&export.join(&history),&value)?;
            chats.push(json!({"agent":"codex","id":id,"history":history,"transcript":transcript,"sha256":digest,"desktopReference":true}));
            if alias["projectId"] != "codex-project-chats" {
                let cwd=alias["cwd"].as_str().ok_or(ERROR)?;
                let project=alias["projectId"].as_str().ok_or(ERROR)?;
                let title=Path::new(cwd).file_name().and_then(|s|s.to_str()).unwrap_or(cwd);
                let row=json!({"id":project,"title":title,"workspaceRoot":cwd,"createdAt":alias["createdAt"],"updatedAt":alias["updatedAt"]});
                if let Some(previous)=projects.insert(project.to_owned(),row.clone()) {
                    if previous["workspaceRoot"] != row["workspaceRoot"] { return Err(ERROR.into()); }
                }
            }
        }
        chats.extend(claude::export(&source,config.claude.as_ref().map(|c|c.profile.as_path()),&export,&mut connection)?);
        write(&export.join("connection.json"),&connection)?;
        write(&export.join("codex-projects.json"),&json!(projects.values().collect::<Vec<_>>()))?;
        write(&export.join("claude-settings.json"),&settings)?;
        write(&export.join("configuration.json"),&serde_json::to_value(&config).map_err(|_| ERROR)?)?;
        let receipt=json!({"version":1,"profile_mode":"existing","environment":environment,"profiles":{"codex":config.codex.as_ref().map(|c|&c.profile),"claude":config.claude.as_ref().map(|c|&c.profile)},"source_hold":hold_path,"chats":chats,"connection":"connection.json","codex_projects":"codex-projects.json","claude_settings":"claude-settings.json"});
        write(&export.join("closed-owner.json"),&receipt)?;
        let proof=json!({"version":"1.2.27","connectionSha256":hash(&original)});
        check_hold(&source,&proof)?;
        let plan=export.join("plan.json");
        write(&plan,&json!({"version":1,"closed_owner":export.join("closed-owner.json"),"configuration":export.join("configuration.json"),"bind":"0.0.0.0:10105","public_url":public_url,"gui":gui}))?;
        write(&hold_path,&json!({"version":1,"environment":environment,"export":export,"historicalSource":proof}))?;
        crate::npm_upgrade::prepare(root,&plan)
    }.await;
    reader.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_uninstall_record_keeps_the_selected_legacy_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Existing Connect state");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("android-remote.json"), b"saved connection").unwrap();
        let root = source.to_str().unwrap();
        let (environment, account) = source_identity(&source, "TEST-PC").unwrap();
        assert_eq!(
            environment,
            format!(
                "opencodex-{}",
                &hash(format!("TEST-PC\0{root}").as_bytes())[..24]
            )
        );
        assert_eq!(
            account,
            hash(format!("opencodex-android-remote-cloudflare-v1\0{root}").as_bytes())
        );
        assert!(!source.join(".opencodex-owner.json").exists());
        assert_eq!(
            fs::read(source.join("android-remote.json")).unwrap(),
            b"saved connection"
        );
        assert!(source_identity(&source, "\n").is_err());
        assert!(source_identity(Path::new("relative-source"), "TEST-PC").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn ownerless_windows_identity_uses_cli_path_not_canonical_device_path() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("Connect State");
        fs::create_dir(&original).unwrap();
        let original = original.to_str().unwrap();
        let original = original.strip_prefix(r"\\?\").unwrap_or(original);
        for supplied in [original.to_owned(), original.replace('\\', "/")] {
            let installation =
                crate::native_cli::installation_directory(Some(&supplied), dir.path(), dir.path())
                    .unwrap();
            let selected = installation.parent().unwrap();
            let actual = source_identity(selected, "TEST-PC").unwrap();
            assert_eq!(
                actual.0,
                format!(
                    "opencodex-{}",
                    &hash(format!("TEST-PC\0{original}").as_bytes())[..24]
                )
            );
            assert_eq!(
                actual.1,
                hash(format!("opencodex-android-remote-cloudflare-v1\0{original}").as_bytes())
            );
            assert_ne!(
                actual,
                source_identity(&selected.canonicalize().unwrap(), "TEST-PC").unwrap()
            );
        }
    }
    #[test]
    fn present_invalid_owner_records_never_fall_back_to_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let source = dir.path();
        let record = source.join(".opencodex-owner.json");
        for invalid in [
            b"not JSON".to_vec(),
            serde_json::to_vec(&json!({"version":2,"ownerId":uuid::Uuid::new_v4().to_string(),"root":source})).unwrap(),
            serde_json::to_vec(&json!({"version":1,"ownerId":"bad-id","root":source})).unwrap(),
            serde_json::to_vec(&json!({"version":1,"ownerId":uuid::Uuid::new_v4().to_string(),"root":other.path()})).unwrap(),
        ] {
            fs::write(&record, &invalid).unwrap();
            assert!(source_identity(source, "TEST-PC").is_err());
            assert_eq!(fs::read(&record).unwrap(), invalid);
        }
        fs::remove_file(&record).unwrap();
        fs::create_dir(&record).unwrap();
        assert!(source_identity(source, "TEST-PC").is_err());
        fs::remove_dir(&record).unwrap();
        let target = source.join("shared-owner.json");
        fs::write(
            &target,
            serde_json::to_vec(
                &json!({"version":1,"ownerId":uuid::Uuid::new_v4().to_string(),"root":source}),
            )
            .unwrap(),
        )
        .unwrap();
        fs::hard_link(&target, &record).unwrap();
        assert!(source_identity(source, "TEST-PC").is_err());
    }
    #[test]
    fn retains_original_identity_and_keyring_path_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().canonicalize().unwrap();
        let root = source.to_str().unwrap();
        write(
            &source.join(".opencodex-owner.json"),
            &json!({"version":1,"ownerId":uuid::Uuid::new_v4().to_string(),"root":root}),
        )
        .unwrap();
        let (environment, account) = source_identity(&source, "TEST-PC").unwrap();
        assert_eq!(
            environment,
            format!(
                "opencodex-{}",
                &hash(format!("TEST-PC\0{root}").as_bytes())[..24]
            )
        );
        assert_eq!(
            account,
            hash(format!("opencodex-android-remote-cloudflare-v1\0{root}").as_bytes())
        );
        assert_ne!(account, hash(root.as_bytes()));
        assert!(source_identity(&source, "\n").is_err());
    }
    #[test]
    fn native_reference_keeps_append_only_history_without_copying_messages() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().canonicalize().unwrap();
        fs::create_dir(profile.join("sessions")).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let path = profile.join("sessions/chat.jsonl");
        let line = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":native}})
        );
        fs::write(&path, &line).unwrap();
        let (saved, name, digest) = reference(&profile, &path, "phone-alias", &native).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"event_msg\"}\n")
            .unwrap();
        crate::legacy_desktop_reference::Reference::parse(&serde_json::to_vec(&saved).unwrap())
            .unwrap()
            .matches_receipt(&profile, "phone-alias", &name, &digest)
            .unwrap();
        fs::write(&path, b"different identity\n").unwrap();
        assert!(crate::legacy_desktop_reference::Reference::parse(
            &serde_json::to_vec(&saved).unwrap()
        )
        .unwrap()
        .matches_receipt(&profile, "phone-alias", &name, &digest)
        .is_err());
    }
    #[test]
    fn refuses_live_or_changed_sources_and_temporary_remote_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path();
        fs::write(source.join("android-remote.json"), b"original").unwrap();
        let proof = json!({"version":"1.2.27","connectionSha256":hash(b"original")});
        check_hold(source, &proof).unwrap();
        for name in ["runtime-port.json", "ocx.pid"] {
            fs::write(source.join(name), b"live").unwrap();
            assert!(check_hold(source, &proof).is_err());
            fs::remove_file(source.join(name)).unwrap();
        }
        fs::write(source.join("android-remote.json"), b"changed").unwrap();
        assert!(check_hold(source, &proof).is_err());
        assert!(connection_mode(&json!({"settings":{"connectionChoice":"quick"}})).is_err());
        connection_mode(&json!({"settings":{"connectionChoice":"named"}})).unwrap();
    }
    #[test]
    fn identifies_only_unregistered_historical_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native-installation");
        fs::write(dir.path().join("android-remote.json"), b"{}").unwrap();
        assert!(applicable(&root).unwrap());
        fs::create_dir(&root).unwrap();
        fs::write(root.join("npm-upgrade.sqlite"), b"journal").unwrap();
        assert!(!applicable(&root).unwrap());
    }
}
