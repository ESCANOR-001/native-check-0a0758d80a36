//! Retain explicitly inventoried assets from the closed owner's export. Original
//! path strings are link metadata only; all bytes come from the verified copy.
use crate::{migration_snapshot, profile_snapshot, storage_stage};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

const ERROR: &str =
    "This export needs a complete asset inventory; original files and history were retained";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Uploads {
    original_root: PathBuf,
    export: String,
    manifest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Images {
    original_root: PathBuf,
    export: String,
}
fn child(root: &Path, name: &str, directory: bool) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 2048 || name.contains(['\\', ':']) {
        return Err(ERROR.into());
    }
    let mut path = root.to_owned();
    let parts: Vec<_> = name.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(ERROR.into());
        }
        path.push(part);
        storage_stage::open(&path, directory || i + 1 < parts.len()).map_err(|_| ERROR)?;
    }
    Ok(path)
}
fn original(root: &Path) -> Result<(), String> {
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(ERROR.into());
    }
    Ok(())
}
pub(crate) fn validate(
    history: &Value,
    agent: &str,
    public: &Path,
    uploads: Option<&Uploads>,
    images: Option<&Images>,
) -> Result<(), String> {
    let thread = if agent == "claude" {
        &history["thread"]
    } else {
        history
    };
    let mut covered = BTreeSet::new();
    if let Some(uploads) = uploads {
        original(&uploads.original_root)?;
        child(public, &uploads.export, true)?;
        let raw =
            profile_snapshot::bytes(&child(public, &uploads.manifest, false)?, 2 * 1024 * 1024)
                .map_err(|_| ERROR)?;
        let plan: Value = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
        let files = plan["files"]
            .as_array()
            .filter(|a| a.len() <= 2048)
            .ok_or(ERROR)?;
        if plan["version"] != 1 {
            return Err(ERROR.into());
        }
        for file in files {
            let name = file["original"].as_str().ok_or(ERROR)?;
            if !Path::new(name).starts_with(&uploads.original_root)
                || !covered.insert(name.to_owned())
            {
                return Err(ERROR.into());
            }
        }
    }
    for message in thread["messages"].as_array().ok_or(ERROR)? {
        if let Some(attachments) = message.get("attachments").filter(|v| !v.is_null()) {
            for attachment in attachments.as_array().ok_or(ERROR)? {
                if !covered.contains(attachment["path"].as_str().ok_or(ERROR)?) {
                    return Err(ERROR.into());
                }
            }
        }
        // Every old upload reference must have a complete-file inventory entry.
        // Do not treat a partial or unrecognized path as an ordinary external link.
        if let Some(text) = message["text"].as_str() {
            let mut remaining = text.to_owned();
            for path in &covered {
                remaining = crate::legacy_uploads::redact_reference(
                    &remaining,
                    path,
                    agent == "codex" && message["role"] == "user",
                );
            }
            if remaining.contains("android-remote-files/")
                || remaining.contains("android-remote-files\\")
            {
                return Err(ERROR.into());
            }
        }
    }
    for activity in thread["activities"].as_array().ok_or(ERROR)? {
        if let Some(paths) = activity["payload"]["data"]
            .get("toolImages")
            .filter(|v| !v.is_null())
        {
            let paths = paths.as_array().ok_or(ERROR)?;
            if !paths.is_empty() && (agent != "claude" || images.is_none()) {
                return Err(ERROR.into());
            }
        }
    }
    if let Some(images) = images {
        original(&images.original_root)?;
        child(public, &images.export, true)?;
    }
    Ok(())
}
pub(crate) fn prepare(
    public: &Path,
    source: &Path,
    index: usize,
    environment: &str,
    uploads: Option<&Uploads>,
    images: Option<&Images>,
    entry: &mut Value,
) -> Result<(), String> {
    if let Some(uploads) = uploads {
        let snapshot = format!("uploads-{index}");
        migration_snapshot::capture(
            &child(public, &uploads.export, true)?,
            &source.join(&snapshot),
            environment,
        )
        .map_err(|_| ERROR)?;
        let manifest = format!("uploads-{index}.json");
        let bytes =
            profile_snapshot::bytes(&child(public, &uploads.manifest, false)?, 2 * 1024 * 1024)
                .map_err(|_| ERROR)?;
        let mut file = fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file.mode(0o600);
        }
        use std::io::Write;
        let mut file = file.open(source.join(&manifest)).map_err(|_| ERROR)?;
        file.write_all(&bytes).map_err(|_| ERROR)?;
        file.sync_all().map_err(|_| ERROR)?;
        entry["uploads"] =
            json!({"original_root":uploads.original_root,"snapshot":snapshot,"manifest":manifest});
    }
    if let Some(images) = images {
        let snapshot = source.join(format!("images-{index}"));
        let restored = source.join(format!("image-copy-{index}"));
        migration_snapshot::capture(
            &child(public, &images.export, true)?,
            &snapshot,
            environment,
        )
        .map_err(|_| ERROR)?;
        migration_snapshot::restore(&snapshot, &restored, environment).map_err(|_| ERROR)?;
        entry["images"] = json!({"original_root":images.original_root,"export":format!("image-copy-{index}/data")});
    }
    Ok(())
}
