//! Build the native import from a closed TypeScript owner's immutable receipt.
//! This command never discovers/stops a source process. A missing receipt,
//! changed rollout or unsupported asset blocks activation, retaining all inputs.
use crate::{
    legacy_upgrade, migration_snapshot, private_directory, profile_handoff, profile_snapshot,
    storage_stage,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const ERROR: &str =
    "Automatic export is incomplete or changed; source history and recovery copies were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    codex: PathBuf,
    claude: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    agent: String,
    id: String,
    history: String,
    transcript: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    environment: String,
    profiles: Profiles,
    chats: Vec<Chat>,
    connection: String,
    codex_projects: String,
    claude_settings: String,
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    profile_snapshot::bytes(path, limit).map_err(|_| ERROR.into())
}
fn child(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.is_empty()
        || path.len() > 2048
        || path.contains(['\\', ':'])
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ERROR.into());
    }
    let mut current = root.to_owned();
    let parts: Vec<_> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(ERROR.into());
        }
        current.push(part);
        storage_stage::open(&current, i + 1 < parts.len()).map_err(|_| ERROR)?;
    }
    Ok(current)
}
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| ERROR)?;
    file.write_all(bytes).map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    Ok(())
}
fn json_file(path: &Path, value: &Value) -> Result<(), String> {
    write(path, &serde_json::to_vec(value).map_err(|_| ERROR)?)
}
fn fingerprint(path: &Path) -> Result<String, String> {
    let (mut input, before) = profile_snapshot::open(path, 512 * 1024 * 1024).map_err(|_| ERROR)?;
    let mut digest = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let n = input.read(&mut buffer).map_err(|_| ERROR)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > before.len() {
            return Err(ERROR.into());
        }
        digest.update(&buffer[..n]);
    }
    if bytes != before.len() {
        return Err(ERROR.into());
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn assets_present(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(k, v)| {
            matches!(k.as_str(), "attachments" | "toolImages")
                && v.as_array().is_some_and(|a| !a.is_empty())
                || assets_present(v)
        }),
        Value::Array(items) => items.iter().any(assets_present),
        Value::String(s) => {
            s.contains("android-remote-files/") || s.contains("android-remote-files\\")
        }
        _ => false,
    }
}
/// Fresh export roots only. Failed/partial roots are retained, never overwritten.
/// Once prepared, the existing coordinator handles same-input retries/rollback.
pub fn run(root: &Path, receipt_path: &Path, config_path: &Path) -> Result<Value, String> {
    let raw = read(receipt_path, 8 * 1024 * 1024)?;
    let receipt: Receipt = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    let original = receipt_path
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    let config_raw = read(config_path, 262144)?;
    let config: crate::chat::Config = serde_json::from_slice(&config_raw).map_err(|_| ERROR)?;
    config.validate()?;
    if receipt.version != 1
        || receipt.chats.len() > 10_000
        || config.codex.shared_server.is_some()
        || config.claude.shared_server.is_some()
        || config.codex.profile != receipt.profiles.codex
        || config.claude.profile != receipt.profiles.claude
        || !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || fs::symlink_metadata(root).is_ok()
    {
        return Err(ERROR.into());
    }
    let root = root
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?
        .join(root.file_name().ok_or(ERROR)?);
    for source in [&original, &receipt.profiles.codex, &receipt.profiles.claude] {
        let source = source.canonicalize().map_err(|_| ERROR)?;
        if root.starts_with(&source) || source.starts_with(&root) {
            return Err(ERROR.into());
        }
    }
    let mut ids = BTreeSet::new();
    let mut files = BTreeSet::new();
    for chat in &receipt.chats {
        if !matches!(chat.agent.as_str(), "codex" | "claude")
            || chat.id.is_empty()
            || chat.id.len() > 256
            || !ids.insert(&chat.id)
            || !files.insert(&chat.history)
            || chat.sha256.len() != 64
        {
            return Err(ERROR.into());
        }
        let v: Value =
            serde_json::from_slice(&read(&child(&original, &chat.history)?, 16 * 1024 * 1024)?)
                .map_err(|_| ERROR)?;
        if (if chat.agent == "codex" {
            &v["threadId"]
        } else {
            &v["thread"]["id"]
        }) != &chat.id
        {
            return Err(ERROR.into());
        }
        if assets_present(&v) {
            return Err("This export contains attachments or tool images. Keep the source installation; asset inventory is required before automatic cutover".into());
        }
    }
    private_directory::prepare(&root).map_err(|_| ERROR)?;
    migration_snapshot::capture(&original, &root.join("owner-export"), &receipt.environment)
        .map_err(|_| ERROR)?;
    migration_snapshot::restore(
        &root.join("owner-export"),
        &root.join("owner-copy"),
        &receipt.environment,
    )
    .map_err(|_| ERROR)?;
    let public = root.join("owner-copy/data");
    if read(
        &child(
            &public,
            receipt_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or(ERROR)?,
        )?,
        8 * 1024 * 1024,
    )? != raw
    {
        return Err(ERROR.into());
    }
    let source = root.join("source");
    private_directory::prepare(&source).map_err(|_| ERROR)?;
    for (agent, profile) in [
        ("codex", &receipt.profiles.codex),
        ("claude", &receipt.profiles.claude),
    ] {
        profile_handoff::capture_export(
            profile,
            &source.join(format!("{agent}-native")),
            &receipt.environment,
        )?;
    }
    for (name, input) in [
        ("connection.json", &receipt.connection),
        ("codex-projects.json", &receipt.codex_projects),
        ("claude-settings.json", &receipt.claude_settings),
    ] {
        write(
            &source.join(name),
            &read(&child(&public, input)?, 16 * 1024 * 1024)?,
        )?;
    }
    let mut stage = json!({"version":1,"connection":"connection.json","codex_projects":"codex-projects.json","claude_settings":"claude-settings.json","codex":[],"claude":[]});
    for (index, chat) in receipt.chats.iter().enumerate() {
        let snapshot = source.join(format!("{}-native", chat.agent));
        let manifest: Value =
            serde_json::from_slice(&read(&snapshot.join("snapshot.json"), 8 * 1024 * 1024)?)
                .map_err(|_| ERROR)?;
        let entry = manifest["entries"]
            .as_array()
            .ok_or(ERROR)?
            .iter()
            .find(|e| e["name"] == chat.transcript)
            .ok_or(ERROR)?;
        let object = entry["sha256"].as_str().ok_or(ERROR)?;
        if fingerprint(&child(&snapshot, &format!("objects/{object}"))?)? != chat.sha256 {
            return Err(ERROR.into());
        }
        let name = format!("{}-{index}.json", chat.agent);
        let bytes = read(&child(&public, &chat.history)?, 16 * 1024 * 1024)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
        write(&source.join(&name), &bytes)?;
        let mut entry = json!({"id":chat.id,"history":name,"native":{"snapshot":format!("{}-native",chat.agent),"transcript":chat.transcript}});
        if chat.agent == "claude" {
            entry["resume"] = json!(v["bridgeId"].as_str().is_some_and(|s| !s.is_empty()));
        }
        stage[&chat.agent].as_array_mut().ok_or(ERROR)?.push(entry);
    }
    json_file(&source.join("stage.json"), &stage)?;
    migration_snapshot::capture(&source, &root.join("snapshot"), &receipt.environment)
        .map_err(|_| ERROR)?;
    let generation = root.join("generation");
    let mut next: Value = serde_json::from_slice(&config_raw).map_err(|_| ERROR)?;
    next["native_profile_handoff"] = json!(generation.join("native"));
    json_file(&root.join("configuration.json"), &next)?;
    json_file(
        &root.join("native-plan.json"),
        &json!({"version":1,"environment":receipt.environment,
        "codex":{"original_profile":receipt.profiles.codex,"snapshot":source.join("codex-native")},
        "claude":{"original_profile":receipt.profiles.claude,"snapshot":source.join("claude-native")}}),
    )?;
    let plan = root.join("upgrade-plan.json");
    json_file(
        &plan,
        &json!({"version":1,"environment":receipt.environment,"export_snapshot":root.join("snapshot"),"native_plan":root.join("native-plan.json"),"configuration":root.join("configuration.json")}),
    )?;
    legacy_upgrade::run("prepare", &generation, &plan)?;
    let report = json!({"phase":"prepared","chats":receipt.chats.len(),"generation":generation,"plan":plan,"sourceStoppedByExporter":false,"migrationComplete":false});
    json_file(&root.join("prepared.json"), &report)?;
    Ok(report)
}
