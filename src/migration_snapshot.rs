//! Read-only capture of an explicitly supplied, quiesced export directory.
//! This is a staging primitive, not an upgrade/activation command. The caller
//! must stop admission in the exporting runtime; this code never stops it.
//! Only a complete, verified manifest is consumable after an interrupted copy.
use crate::storage_stage::{identity, open, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    time::{Duration, Instant, SystemTime},
};

const MANIFEST: &str = "snapshot.json";
const RESTORED: &str = "restored.json";
const RESTORE_PLAN: &str = "restore-plan.json";
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Upgrade snapshot is incomplete, changed or unsupported; no original data was modified",
    )
}
fn budget(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 2048
        && name.split('/').count() <= 64
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(['\\', ':'])
                && !part.chars().any(char::is_control)
                && !part.ends_with(['.', ' '])
                && !matches!(
                    part.split('.')
                        .next()
                        .unwrap()
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
}
fn digest_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(unix))]
    builder.recursive(false);
    builder.create(path)
}
fn writer(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    open(path, true)?.sync_all()?;
    #[cfg(windows)]
    open(path, true)?;
    Ok(())
}
fn commit_manifest(directory: &Path, name: &str, manifest: &Manifest) -> io::Result<()> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid());
    }
    let pending = directory.join("manifest.pending");
    let mut out = writer(&pending)?;
    out.write_all(&bytes)?;
    out.sync_all()?;
    drop(out);
    fs::hard_link(&pending, directory.join(name))?;
    fs::remove_file(pending)?;
    sync_directory(directory)?;
    sync_directory(directory.parent().ok_or_else(invalid)?)
}

#[derive(Clone, PartialEq, Eq)]
struct Observed {
    identity: Identity,
    directory: bool,
    bytes: u64,
    modified: SystemTime,
}
impl Observed {
    fn read(path: &Path, is_dir: bool) -> io::Result<Self> {
        let file = open(path, is_dir)?;
        let meta = file.metadata()?;
        if !is_dir && meta.len() > MAX_FILE {
            return Err(invalid());
        }
        Ok(Self {
            identity: identity(&file)?,
            directory: is_dir,
            bytes: if is_dir { 0 } else { meta.len() },
            modified: meta.modified()?,
        })
    }
}
fn inventory(root: &Path, deadline: Instant) -> io::Result<BTreeMap<String, Observed>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![(root.to_owned(), String::new())];
    let mut bytes = 0u64;
    while let Some((folder, prefix)) = pending.pop() {
        budget(deadline)?;
        open(&folder, true)?;
        for entry in fs::read_dir(&folder)? {
            budget(deadline)?;
            let entry = entry?;
            let leaf = entry.file_name().into_string().map_err(|_| invalid())?;
            let name = if prefix.is_empty() {
                leaf
            } else {
                format!("{prefix}/{leaf}")
            };
            if !valid_name(&name) || result.len() >= MAX_ENTRIES {
                return Err(invalid());
            }
            let is_dir = entry.file_type()?.is_dir();
            let observed = Observed::read(&entry.path(), is_dir)?;
            bytes = bytes.checked_add(observed.bytes).ok_or_else(invalid)?;
            if bytes > MAX_BYTES || result.insert(name.clone(), observed).is_some() {
                return Err(invalid());
            }
            if is_dir {
                pending.push((entry.path(), name));
            }
        }
    }
    Ok(result)
}

// The manifest may contain private names. Deliberately no Debug implementation.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    directory: bool,
    bytes: u64,
    sha256: Option<String>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    environment: String,
    entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    selected_files: bool,
}
/// Safe for diagnostics: counts only, no paths, prompts or credential contents.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
}
fn environment(value: &str) -> bool {
    // Existing TypeScript installations use opencodex-<hex>; isolated native
    // profiles retain their UUID identity through a profile-generation upgrade.
    (value.len() == 36 && uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value))
        || (value.len() == 34
            && value.starts_with("opencodex-")
            && value[10..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}
fn stream(
    path: &Path,
    expected: u64,
    mut out: Option<&mut File>,
    deadline: Instant,
) -> io::Result<String> {
    let mut file = open(path, false)?;
    if file.metadata()?.len() != expected || expected > MAX_FILE {
        return Err(invalid());
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        budget(deadline)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > expected {
            return Err(invalid());
        }
        hash.update(&buffer[..n]);
        if let Some(out) = out.as_deref_mut() {
            out.write_all(&buffer[..n])?;
        }
    }
    if bytes != expected {
        return Err(invalid());
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Copy a stable offline export into a fresh private destination. Unknown files
/// are retained too. Neither original files nor existing destinations change.
/// Failed/crashed destinations remain uncommitted and are never accepted by
/// `verify`; no retry overwrites them or silently drops their contents.
pub fn capture(
    source: &Path,
    destination: &Path,
    original_environment: &str,
) -> io::Result<Report> {
    capture_inner(source, destination, original_environment, || Ok(()))
}
/// Snapshot only explicitly inventoried native transcripts. Live profiles stay
/// in place; sockets, logs, credentials and unrelated sessions are not copied.
pub fn capture_selected(
    source: &Path,
    destination: &Path,
    environment: &str,
    files: &[String],
) -> io::Result<Report> {
    capture_scoped(source, destination, environment, Some(files), || Ok(()))
}
pub fn is_selected(snapshot: &Path, environment: &str) -> io::Result<bool> {
    verify_inner(snapshot, environment).map(|(m, _)| m.selected_files)
}
fn selected_inventory(
    root: &Path,
    files: &[String],
    deadline: Instant,
) -> io::Result<BTreeMap<String, Observed>> {
    open(root, true)?;
    if files.len() > MAX_ENTRIES {
        return Err(invalid());
    }
    let mut result = BTreeMap::new();
    let mut chosen = BTreeSet::new();
    let mut bytes = 0u64;
    for name in files {
        budget(deadline)?;
        if !valid_name(name) || !chosen.insert(name) {
            return Err(invalid());
        }
        let parts: Vec<_> = name.split('/').collect();
        let mut relative = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                relative.push('/');
            }
            relative.push_str(part);
            let is_dir = i + 1 < parts.len();
            if let Some(old) = result.get(&relative) {
                let old: &Observed = old;
                if old.directory != is_dir {
                    return Err(invalid());
                }
                continue;
            }
            let mut observed = Observed::read(&root.join(&relative), is_dir)?;
            if is_dir {
                observed.modified = SystemTime::UNIX_EPOCH;
            }
            bytes = bytes.checked_add(observed.bytes).ok_or_else(invalid)?;
            if bytes > MAX_BYTES || result.len() >= MAX_ENTRIES {
                return Err(invalid());
            }
            result.insert(relative.clone(), observed);
        }
    }
    Ok(result)
}
fn capture_inner(
    source: &Path,
    destination: &Path,
    original_environment: &str,
    copied: impl FnOnce() -> io::Result<()>,
) -> io::Result<Report> {
    capture_scoped(source, destination, original_environment, None, copied)
}
fn capture_scoped(
    source: &Path,
    destination: &Path,
    original_environment: &str,
    files: Option<&[String]>,
    copied: impl FnOnce() -> io::Result<()>,
) -> io::Result<Report> {
    if !source.is_absolute() || !destination.is_absolute() || !environment(original_environment) {
        return Err(invalid());
    }
    let deadline = Instant::now() + TIMEOUT;
    let root_before = Observed::read(source, true)?;
    let source = source.canonicalize()?;
    let parent = destination.parent().ok_or_else(invalid)?;
    open(parent, true)?;
    let parent = parent.canonicalize()?;
    let destination = parent.join(destination.file_name().ok_or_else(invalid)?);
    if parent.starts_with(&source) || source.starts_with(&destination) {
        return Err(invalid());
    }
    let inventory_source = || match files {
        Some(files) => selected_inventory(&source, files, deadline),
        None => inventory(&source, deadline),
    };
    let before = inventory_source()?;
    directory(&destination)?; // Atomic refusal of an existing file/directory/link.
    directory(&destination.join("objects"))?;
    let mut entries = Vec::new();
    for (index, (name, observed)) in before.iter().enumerate() {
        budget(deadline)?;
        let mut entry = Entry {
            name: name.clone(),
            directory: observed.directory,
            bytes: observed.bytes,
            sha256: None,
        };
        if !entry.directory {
            let temp = destination.join("objects").join(format!("pending-{index}"));
            let mut out = writer(&temp)?;
            let digest = stream(&source.join(name), entry.bytes, Some(&mut out), deadline)?;
            out.sync_all()?;
            drop(out);
            let object = destination.join("objects").join(&digest);
            match fs::hard_link(&temp, &object) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if stream(&object, entry.bytes, None, deadline)? != digest {
                        return Err(invalid());
                    }
                }
                Err(e) => return Err(e),
            }
            fs::remove_file(temp)?;
            entry.sha256 = Some(digest);
        }
        entries.push(entry);
    }
    copied()?;
    // Re-read bytes, not just timestamps: equal-length or restored-mtime writes
    // and inter-file changes cannot produce an accepted mixed snapshot.
    for entry in &entries {
        if let Some(expected) = &entry.sha256 {
            if &stream(&source.join(&entry.name), entry.bytes, None, deadline)? != expected {
                return Err(invalid());
            }
        }
    }
    let root_after = Observed::read(&source, true)?;
    if before != inventory_source()?
        || root_before.identity != root_after.identity
        || (files.is_none() && root_before != root_after)
    {
        return Err(invalid());
    }
    sync_directory(&destination.join("objects"))?;
    let manifest = Manifest {
        version: 1,
        environment: original_environment.into(),
        entries,
        selected_files: files.is_some(),
    };
    commit_manifest(&destination, MANIFEST, &manifest)?;
    verify(&destination, original_environment)
}

/// Check every object before a later migration may consume it. This does not
/// mark a profile ready, execute stored code, or connect to either agent.
pub fn verify(snapshot: &Path, original_environment: &str) -> io::Result<Report> {
    verify_inner(snapshot, original_environment).map(|(_, report)| report)
}

/// Compare a quiesced working copy with its immutable checkpoint. This never
/// restores over new work. Upgrade rollback uses it before changing ownership.
pub fn verify_tree(snapshot: &Path, data: &Path, original_environment: &str) -> io::Result<Report> {
    let (manifest, report) = verify_inner(snapshot, original_environment)?;
    if !data.is_absolute() {
        return Err(invalid());
    }
    verify_data(data, &manifest, Instant::now() + TIMEOUT)?;
    Ok(report)
}
/// Verify selected source files without classifying a running profile's
/// unrelated files as migrated. Restore and rollback of copied data remain exact.
pub fn verify_source_entries(
    snapshot: &Path,
    data: &Path,
    environment: &str,
) -> io::Result<Report> {
    let (manifest, report) = verify_inner(snapshot, environment)?;
    if !manifest.selected_files {
        return verify_tree(snapshot, data, environment);
    }
    if !data.is_absolute() {
        return Err(invalid());
    }
    let deadline = Instant::now() + TIMEOUT;
    let files: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| !e.directory)
        .map(|e| e.name.clone())
        .collect();
    let before = selected_inventory(data, &files, deadline)?;
    if before.len() != manifest.entries.len() {
        return Err(invalid());
    }
    for entry in &manifest.entries {
        let observed = before.get(&entry.name).ok_or_else(invalid)?;
        if observed.directory != entry.directory || observed.bytes != entry.bytes {
            return Err(invalid());
        }
        if let Some(hash) = &entry.sha256 {
            if stream(&data.join(&entry.name), entry.bytes, None, deadline)? != *hash {
                return Err(invalid());
            }
        }
    }
    if before != selected_inventory(data, &files, deadline)? {
        return Err(invalid());
    }
    Ok(report)
}
fn read_manifest(directory: &Path, name: &str) -> io::Result<Manifest> {
    open(directory, true)?;
    serde_json::from_slice(&crate::profile_snapshot::bytes(
        &directory.join(name),
        MAX_MANIFEST,
    )?)
    .map_err(|_| invalid())
}
fn verify_inner(snapshot: &Path, original_environment: &str) -> io::Result<(Manifest, Report)> {
    if !snapshot.is_absolute() || !environment(original_environment) {
        return Err(invalid());
    }
    open(snapshot, true)?;
    open(&snapshot.join("objects"), true)?;
    let manifest = read_manifest(snapshot, MANIFEST)?;
    let report = manifest_summary(&manifest, original_environment)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut objects = BTreeSet::new();
    for entry in &manifest.entries {
        if let Some(digest) = &entry.sha256 {
            if stream(
                &snapshot.join("objects").join(digest),
                entry.bytes,
                None,
                deadline,
            )? != *digest
            {
                return Err(invalid());
            }
            objects.insert(digest.to_owned());
        }
    }
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(snapshot.join("objects"))? {
        budget(deadline)?;
        let name = entry?.file_name().into_string().map_err(|_| invalid())?;
        if !objects.contains(&name) || !found.insert(name) {
            return Err(invalid());
        }
    }
    if objects != found {
        return Err(invalid());
    }
    Ok((manifest, report))
}
fn manifest_summary(manifest: &Manifest, original_environment: &str) -> io::Result<Report> {
    if manifest.version != 1
        || manifest.environment != original_environment
        || manifest.entries.len() > MAX_ENTRIES
    {
        return Err(invalid());
    }
    let mut names = BTreeMap::new();
    let mut report = Report {
        files: 0,
        directories: 0,
        bytes: 0,
    };
    for entry in &manifest.entries {
        if !valid_name(&entry.name) || names.insert(entry.name.clone(), entry.directory).is_some() {
            return Err(invalid());
        }
        if entry.directory {
            if entry.bytes != 0 || entry.sha256.is_some() {
                return Err(invalid());
            }
            report.directories += 1;
        } else {
            entry
                .sha256
                .as_deref()
                .filter(|s| digest_name(s))
                .ok_or_else(invalid)?;
            report.bytes = report.bytes.checked_add(entry.bytes).ok_or_else(invalid)?;
            if report.bytes > MAX_BYTES || entry.bytes > MAX_FILE {
                return Err(invalid());
            }
            report.files += 1;
        }
    }
    for entry in &manifest.entries {
        if let Some((parent, _)) = entry.name.rsplit_once('/') {
            if names.get(parent) != Some(&true) {
                return Err(invalid());
            }
        }
    }
    Ok(report)
}

/// Restore the exported bytes below a new `data` directory. Existing folders
/// are never merged or replaced. A completion receipt is written last; a crash
/// before that leaves an unusable staged copy, never a runnable installation.
/// Runtime activation must separately validate conversions and own its lease.
pub fn restore(
    snapshot: &Path,
    destination: &Path,
    original_environment: &str,
) -> io::Result<Report> {
    restore_inner(snapshot, destination, original_environment, || Ok(()))
}
fn restore_inner(
    snapshot: &Path,
    destination: &Path,
    original_environment: &str,
    copied: impl FnOnce() -> io::Result<()>,
) -> io::Result<Report> {
    if !destination.is_absolute() {
        return Err(invalid());
    }
    let (manifest, _) = verify_inner(snapshot, original_environment)?;
    let snapshot = snapshot.canonicalize()?;
    let parent = destination.parent().ok_or_else(invalid)?;
    open(parent, true)?;
    let parent = parent.canonicalize()?;
    let destination = parent.join(destination.file_name().ok_or_else(invalid)?);
    if parent.starts_with(&snapshot) || snapshot.starts_with(&destination) {
        return Err(invalid());
    }
    directory(&destination)?;
    // This immutable receipt identifies a resumable copy. A pre-existing
    // unmarked directory is never adopted or cleaned up.
    commit_manifest(&destination, RESTORE_PLAN, &manifest)?;
    let data = destination.join("data");
    directory(&data)?;
    directory(&destination.join("copying"))?;
    let deadline = Instant::now() + TIMEOUT;
    // Sorting ensures parent directories exist even for a reordered manifest.
    let mut entries = manifest.entries.clone();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &entries {
        budget(deadline)?;
        let path = data.join(&entry.name);
        if entry.directory {
            directory(&path)?;
        } else {
            copy_restored_file(&snapshot, &destination, entry, deadline)?;
        }
    }
    copied()?;
    verify_data(&data, &manifest, deadline)?;
    for entry in entries.iter().rev().filter(|e| e.directory) {
        sync_directory(&data.join(&entry.name))?;
    }
    sync_directory(&data)?;
    commit_manifest(&destination, RESTORED, &manifest)?;
    verify_restored(&destination, original_environment)
}

fn copy_restored_file(
    snapshot: &Path,
    destination: &Path,
    entry: &Entry,
    deadline: Instant,
) -> io::Result<()> {
    let digest = entry.sha256.as_ref().ok_or_else(invalid)?;
    let pending = destination.join("copying").join(format!("{digest}.copy"));
    let mut out = writer(&pending)?;
    if stream(
        &snapshot.join("objects").join(digest),
        entry.bytes,
        Some(&mut out),
        deadline,
    )? != *digest
    {
        return Err(invalid());
    }
    out.sync_all()?;
    drop(out);
    let target = destination.join("data").join(&entry.name);
    fs::hard_link(&pending, &target)?;
    sync_directory(target.parent().ok_or_else(invalid)?)?;
    fs::remove_file(pending)?;
    sync_directory(&destination.join("copying"))
}

/// Retry only a private, explicitly journaled restore from the exact same
/// verified snapshot. Existing data files must already match completely; only
/// missing files are copied. Recognized temporary prefixes are safe to discard.
/// Unknown or changed bytes remain in place and block completion.
pub fn resume_restore(
    snapshot: &Path,
    destination: &Path,
    original_environment: &str,
) -> io::Result<Report> {
    if !destination.is_absolute() {
        return Err(invalid());
    }
    let (manifest, report) = verify_inner(snapshot, original_environment)?;
    let snapshot = snapshot.canonicalize()?;
    open(destination, true)?;
    let destination = destination.canonicalize()?;
    if destination.starts_with(&snapshot) || snapshot.starts_with(&destination) {
        return Err(invalid());
    }
    if destination.join(RESTORED).try_exists()? {
        if read_manifest(&destination, RESTORED)? != manifest {
            return Err(invalid());
        }
        return verify_restored(&destination, original_environment);
    }
    if read_manifest(&destination, RESTORE_PLAN)? != manifest {
        return Err(invalid());
    }
    let deadline = Instant::now() + TIMEOUT;
    for e in fs::read_dir(&destination)? {
        budget(deadline)?;
        let e = e?;
        let name = e.file_name();
        let name = name.to_str().ok_or_else(invalid)?;
        if ![RESTORE_PLAN, "data", "copying", "manifest.pending"].contains(&name) {
            return Err(invalid());
        }
        open(&e.path(), matches!(name, "data" | "copying"))?;
    }
    let data = destination.join("data");
    let found = if data.try_exists()? {
        inventory(&data, deadline)?
    } else {
        BTreeMap::new()
    };
    let expected: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    for (name, observed) in &found {
        let entry = expected.get(name.as_str()).ok_or_else(invalid)?;
        if observed.directory != entry.directory || observed.bytes != entry.bytes {
            return Err(invalid());
        }
        if let Some(hash) = &entry.sha256 {
            if stream(&data.join(name), entry.bytes, None, deadline)? != *hash {
                return Err(invalid());
            }
        }
    }
    let copying = destination.join("copying");
    let objects: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .filter_map(|e| e.sha256.as_deref().map(|s| (s, e.bytes)))
        .collect();
    let mut temporary = vec![];
    if copying.try_exists()? {
        open(&copying, true)?;
        for e in fs::read_dir(&copying)? {
            budget(deadline)?;
            let e = e?;
            let name = e.file_name();
            let name = name
                .to_str()
                .and_then(|s| s.strip_suffix(".copy"))
                .ok_or_else(invalid)?;
            let length = objects.get(name).ok_or_else(invalid)?;
            matching_prefix(
                &e.path(),
                &snapshot.join("objects").join(name),
                *length,
                deadline,
            )?;
            temporary.push(e.path());
            if temporary.len() > MAX_ENTRIES {
                return Err(invalid());
            }
        }
    }
    let pending = destination.join("manifest.pending");
    if pending.try_exists()? {
        let bytes = crate::profile_snapshot::bytes(&pending, MAX_MANIFEST)?;
        let expected = serde_json::to_vec(&manifest).map_err(|_| invalid())?;
        if !expected.starts_with(&bytes) {
            return Err(invalid());
        }
        temporary.push(pending);
    }
    // All existing entries were checked before any temporary file is removed.
    for path in temporary {
        fs::remove_file(path)?;
    }
    if !data.try_exists()? {
        directory(&data)?;
    }
    if !copying.try_exists()? {
        directory(&copying)?;
    }
    let mut entries = manifest.entries.clone();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &entries {
        budget(deadline)?;
        if found.contains_key(&entry.name) {
            continue;
        }
        if entry.directory {
            directory(&data.join(&entry.name))?;
        } else {
            copy_restored_file(&snapshot, &destination, entry, deadline)?;
        }
    }
    verify_data(&data, &manifest, deadline)?;
    for entry in entries.iter().rev().filter(|e| e.directory) {
        sync_directory(&data.join(&entry.name))?;
    }
    sync_directory(&data)?;
    sync_directory(&copying)?;
    commit_manifest(&destination, RESTORED, &manifest)?;
    if verify_restored(&destination, original_environment)? != report {
        return Err(invalid());
    }
    Ok(report)
}
fn matching_prefix(
    partial: &Path,
    source: &Path,
    maximum: u64,
    deadline: Instant,
) -> io::Result<()> {
    let mut part = open(partial, false)?;
    let before = part.metadata()?;
    let ident = identity(&part)?;
    if before.len() > maximum {
        return Err(invalid());
    }
    let mut src = open(source, false)?;
    if src.metadata()?.len() != maximum {
        return Err(invalid());
    }
    let mut a = [0u8; 64 * 1024];
    let mut b = [0u8; 64 * 1024];
    let mut left = before.len();
    while left > 0 {
        budget(deadline)?;
        let n = left.min(a.len() as u64) as usize;
        part.read_exact(&mut a[..n])?;
        src.read_exact(&mut b[..n])?;
        if a[..n] != b[..n] {
            return Err(invalid());
        }
        left -= n as u64;
    }
    let after = open(partial, false)?;
    if identity(&after)? != ident || after.metadata()?.len() != before.len() {
        return Err(invalid());
    }
    Ok(())
}
fn verify_data(data: &Path, manifest: &Manifest, deadline: Instant) -> io::Result<()> {
    let before = inventory(data, deadline)?;
    if before.len() != manifest.entries.len() {
        return Err(invalid());
    }
    for entry in &manifest.entries {
        let observed = before.get(&entry.name).ok_or_else(invalid)?;
        if observed.directory != entry.directory || observed.bytes != entry.bytes {
            return Err(invalid());
        }
        if let Some(digest) = &entry.sha256 {
            if stream(&data.join(&entry.name), entry.bytes, None, deadline)? != *digest {
                return Err(invalid());
            }
        }
    }
    if before != inventory(data, deadline)? {
        return Err(invalid());
    }
    Ok(())
}
pub fn verify_restored(destination: &Path, original_environment: &str) -> io::Result<Report> {
    if !destination.is_absolute() || !environment(original_environment) {
        return Err(invalid());
    }
    let manifest = read_manifest(destination, RESTORED)?;
    let report = manifest_summary(&manifest, original_environment)?;
    verify_data(
        &destination.join("data"),
        &manifest,
        Instant::now() + TIMEOUT,
    )?;
    Ok(report)
}

/// Exact files retained by a completed restore. Used only by offline rollback;
/// unknown/partial files are preserved and prevent clearing the connection.
pub(crate) fn retained_asset_rows(
    destination: &Path,
    original_environment: &str,
) -> io::Result<BTreeMap<String, i64>> {
    verify_restored(destination, original_environment)?;
    if read_manifest(destination, RESTORED)? != read_manifest(destination, RESTORE_PLAN)? {
        return Err(invalid());
    }
    let found = inventory(destination, Instant::now() + TIMEOUT)?;
    let mut rows = BTreeMap::new();
    for (name, observed) in found {
        if observed.directory {
            if name != "data" && name != "copying" && !name.starts_with("data/") {
                return Err(invalid());
            }
        } else {
            if name != RESTORED && name != RESTORE_PLAN && !name.starts_with("data/") {
                return Err(invalid());
            }
            // Manifest names use '/', but the storage index records paths from
            // native directory iteration. Joining the entire name on Windows
            // leaves mixed separators and falsely classifies valid copies as
            // unknown assets. Build the same native path component by component.
            let path = name
                .split('/')
                .fold(destination.to_path_buf(), |root, part| root.join(part));
            rows.insert(
                path.to_str().ok_or_else(invalid)?.into(),
                i64::try_from(observed.bytes).map_err(|_| invalid())?,
            );
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    const ENV: &str = "opencodex-0123456789abcdef01234567";
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("export");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("sessions")).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(
            source.join("connection.json"),
            "{\"version\":1,\"keepUnknown\":true}\n",
        )
        .unwrap();
        fs::write(source.join("sessions/chat.json"), "EXACT saved history\n").unwrap();
        fs::write(source.join("attachment.bin"), [0, 1, 2, 0, 255]).unwrap();
        let destination = root.path().join("snapshot");
        (root, source, destination)
    }
    #[test]
    fn captures_exact_bytes_empty_directories_unknown_fields_and_private_files() {
        let (_root, source, destination) = fixture();
        let before = inventory(&source, Instant::now() + TIMEOUT).unwrap();
        let report = capture(&source, &destination, ENV).unwrap();
        assert_eq!(report.files, 3);
        assert_eq!(report.directories, 2);
        assert_eq!(verify(&destination, ENV).unwrap(), report);
        assert!(before == inventory(&source, Instant::now() + TIMEOUT).unwrap());
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(destination.join(MANIFEST)).unwrap()).unwrap();
        for entry in manifest.entries.iter().filter(|e| !e.directory) {
            assert_eq!(
                fs::read(source.join(&entry.name)).unwrap(),
                fs::read(
                    destination
                        .join("objects")
                        .join(entry.sha256.as_ref().unwrap())
                )
                .unwrap()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(destination.join(MANIFEST))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn refuses_changed_or_added_source_files_without_committing_a_partial_snapshot() {
        for added in [false, true] {
            let (_root, source, destination) = fixture();
            assert!(capture_inner(&source, &destination, ENV, || {
                fs::write(
                    source.join(if added {
                        "new.json"
                    } else {
                        "sessions/chat.json"
                    }),
                    "changed",
                )
            })
            .is_err());
            assert!(!destination.join(MANIFEST).exists());
            assert!(verify(&destination, ENV).is_err());
            assert_eq!(
                fs::read(source.join("attachment.bin")).unwrap(),
                [0, 1, 2, 0, 255]
            );
        }
    }
    #[test]
    fn equal_length_edits_with_restored_timestamp_still_fail_verification() {
        let (_root, source, destination) = fixture();
        let path = source.join("sessions/chat.json");
        let before = fs::metadata(&path).unwrap();
        assert!(capture_inner(&source, &destination, ENV, || {
            fs::write(&path, vec![b'x'; before.len() as usize])?;
            OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_times(fs::FileTimes::new().set_modified(before.modified()?))
        })
        .is_err());
        assert!(!destination.join(MANIFEST).exists());
    }
    #[test]
    fn interruption_and_existing_destinations_never_overwrite_original_data() {
        let (_root, source, destination) = fixture();
        assert!(capture_inner(&source, &destination, ENV, || Err(
            io::ErrorKind::Interrupted.into()
        ))
        .is_err());
        assert!(verify(&destination, ENV).is_err());
        let before = inventory(&destination, Instant::now() + TIMEOUT).unwrap();
        assert!(capture(&source, &destination, ENV).is_err());
        assert!(before == inventory(&destination, Instant::now() + TIMEOUT).unwrap());
        assert!(capture(&source, &source.join("nested"), ENV).is_err());
        assert!(!source.join("nested").exists());
    }
    #[test]
    fn corrupt_missing_extra_or_wrong_environment_objects_fail_closed() {
        let (_root, source, destination) = fixture();
        capture(&source, &destination, ENV).unwrap();
        assert!(verify(&destination, "opencodex-aaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        let file = fs::read_dir(destination.join("objects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let before = fs::read(&file).unwrap();
        fs::write(&file, vec![b'x'; before.len()]).unwrap();
        assert!(verify(&destination, ENV).is_err());
        fs::write(&file, &before).unwrap();
        fs::write(destination.join("objects/extra"), "extra").unwrap();
        assert!(verify(&destination, ENV).is_err());
        fs::remove_file(destination.join("objects/extra")).unwrap();
        fs::remove_file(file).unwrap();
        assert!(verify(&destination, ENV).is_err());
    }
    #[test]
    fn duplicate_content_is_retained_without_duplicate_object_copies() {
        let (_root, source, destination) = fixture();
        fs::copy(source.join("attachment.bin"), source.join("duplicate.bin")).unwrap();
        let report = capture(&source, &destination, ENV).unwrap();
        assert_eq!(report.files, 4);
        assert_eq!(
            fs::read_dir(destination.join("objects")).unwrap().count(),
            3
        );
    }
    #[test]
    fn retained_asset_keys_use_native_separators_like_the_storage_index() {
        let (root, source, snapshot) = fixture();
        capture(&source, &snapshot, ENV).unwrap();
        let destination = root.path().join("restored");
        restore(&snapshot, &destination, ENV).unwrap();
        let rows = retained_asset_rows(&destination, ENV).unwrap();
        let nested = destination.join("data").join("sessions").join("chat.json");
        assert_eq!(
            rows.get(nested.to_str().unwrap()),
            Some(&(fs::metadata(&nested).unwrap().len() as i64))
        );
        assert!(rows.contains_key(destination.join(RESTORED).to_str().unwrap()));
        assert!(rows.contains_key(destination.join(RESTORE_PLAN).to_str().unwrap()));
    }
    #[test]
    fn restore_retains_all_original_bytes_and_empty_folders_in_a_fresh_destination() {
        let (root, source, snapshot) = fixture();
        let report = capture(&source, &snapshot, ENV).unwrap();
        let destination = root.path().join("restored");
        assert_eq!(restore(&snapshot, &destination, ENV).unwrap(), report);
        assert_eq!(verify_restored(&destination, ENV).unwrap(), report);
        let files = inventory(&source, Instant::now() + TIMEOUT).unwrap();
        for (name, observed) in files {
            let path = destination.join("data").join(&name);
            if observed.directory {
                assert!(path.is_dir());
            } else {
                assert_eq!(
                    fs::read(source.join(name)).unwrap(),
                    fs::read(path).unwrap()
                );
            }
        }
        assert_eq!(verify(&snapshot, ENV).unwrap(), report);
        assert!(restore(&snapshot, &source, ENV).is_err());
        assert_eq!(
            fs::read_to_string(source.join("sessions/chat.json")).unwrap(),
            "EXACT saved history\n"
        );
    }
    #[test]
    fn restore_interruption_tampering_and_retry_cannot_create_an_accepted_partial_export() {
        let (root, source, snapshot) = fixture();
        capture(&source, &snapshot, ENV).unwrap();
        let destination = root.path().join("restore-interrupted");
        assert!(restore_inner(&snapshot, &destination, ENV, || Err(
            io::ErrorKind::Interrupted.into()
        ))
        .is_err());
        assert!(!destination.join(RESTORED).exists());
        assert!(verify_restored(&destination, ENV).is_err());
        assert!(restore(&snapshot, &destination, ENV).is_err());
        let destination = root.path().join("restore-tampered");
        assert!(restore_inner(&snapshot, &destination, ENV, || fs::write(
            destination.join("data/attachment.bin"),
            "modified"
        ))
        .is_err());
        assert!(!destination.join(RESTORED).exists());
        let destination = root.path().join("restore-valid");
        restore(&snapshot, &destination, ENV).unwrap();
        fs::write(destination.join("data/extra"), "cannot ignore extra data").unwrap();
        assert!(verify_restored(&destination, ENV).is_err());
    }
    #[test]
    fn unsafe_or_missing_parent_manifest_entries_cannot_write_outside_restore() {
        for bad in [
            "../escaped",
            "/escaped",
            "x\\escaped",
            "C:escaped",
            "CON",
            "dir/NUL.json",
            "missing/file",
        ] {
            let (root, source, snapshot) = fixture();
            capture(&source, &snapshot, ENV).unwrap();
            let mut manifest = read_manifest(&snapshot, MANIFEST).unwrap();
            manifest
                .entries
                .iter_mut()
                .find(|e| !e.directory)
                .unwrap()
                .name = bad.into();
            fs::write(
                snapshot.join(MANIFEST),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let destination = root.path().join("restore-invalid");
            assert!(restore(&snapshot, &destination, ENV).is_err(), "{bad}");
            assert!(!destination.exists());
        }
    }
    #[test]
    fn streams_large_attachments_and_rejects_oversized_sources_before_destination_creation() {
        let (root, source, snapshot) = fixture();
        let large = source.join("large-attachment.bin");
        let mut file = File::create(&large).unwrap();
        for _ in 0..256 {
            file.write_all(&[123; 64 * 1024]).unwrap();
        }
        drop(file);
        let report = capture(&source, &snapshot, ENV).unwrap();
        assert!(report.bytes > 16 * 1024 * 1024);
        let restored = root.path().join("restored-large");
        assert_eq!(restore(&snapshot, &restored, ENV).unwrap(), report);
        assert_eq!(
            stream(&large, 16 * 1024 * 1024, None, Instant::now() + TIMEOUT).unwrap(),
            stream(
                &restored.join("data/large-attachment.bin"),
                16 * 1024 * 1024,
                None,
                Instant::now() + TIMEOUT
            )
            .unwrap()
        );
        OpenOptions::new()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        let oversized = root.path().join("oversized");
        assert!(capture(&source, &oversized, ENV).is_err());
        assert!(!oversized.exists());
    }
    #[test]
    fn marked_restore_resumes_missing_files_and_verified_partial_temporaries() {
        let (root, source, snapshot) = fixture();
        let report = capture(&source, &snapshot, ENV).unwrap();
        let target = root.path().join("resume");
        assert!(restore_inner(&snapshot, &target, ENV, || Err(
            io::ErrorKind::Interrupted.into()
        ))
        .is_err());
        let manifest = read_manifest(&snapshot, MANIFEST).unwrap();
        let file = manifest
            .entries
            .iter()
            .find(|e| e.name == "sessions/chat.json")
            .unwrap();
        let hash = file.sha256.as_ref().unwrap();
        fs::remove_file(target.join("data").join(&file.name)).unwrap();
        let object = fs::read(snapshot.join("objects").join(hash)).unwrap();
        fs::write(
            target.join("copying").join(format!("{hash}.copy")),
            &object[..4],
        )
        .unwrap();
        let encoded = serde_json::to_vec(&manifest).unwrap();
        fs::write(target.join("manifest.pending"), &encoded[..17]).unwrap();
        assert_eq!(resume_restore(&snapshot, &target, ENV).unwrap(), report);
        assert_eq!(resume_restore(&snapshot, &target, ENV).unwrap(), report);
        assert_eq!(
            fs::read(target.join("data").join(&file.name)).unwrap(),
            object
        );
        assert_eq!(fs::read_dir(target.join("copying")).unwrap().count(), 0);
        assert!(!target.join("manifest.pending").exists());
        assert_eq!(fs::read(source.join("sessions/chat.json")).unwrap(), object);
    }
    #[test]
    fn restore_plan_before_data_creation_and_interrupted_hard_link_publish_recover() {
        let (root, source, snapshot) = fixture();
        let report = capture(&source, &snapshot, ENV).unwrap();
        let manifest = read_manifest(&snapshot, MANIFEST).unwrap();
        let target = root.path().join("resume");
        directory(&target).unwrap();
        commit_manifest(&target, RESTORE_PLAN, &manifest).unwrap();
        assert_eq!(resume_restore(&snapshot, &target, ENV).unwrap(), report);
        let target = root.path().join("publish-interrupted");
        assert!(restore_inner(&snapshot, &target, ENV, || Err(
            io::ErrorKind::Interrupted.into()
        ))
        .is_err());
        let entry = manifest.entries.iter().find(|e| !e.directory).unwrap();
        let file = target.join("data").join(&entry.name);
        let id = identity(&open(&file, false).unwrap()).unwrap();
        fs::hard_link(
            &file,
            target
                .join("copying")
                .join(format!("{}.copy", entry.sha256.as_ref().unwrap())),
        )
        .unwrap();
        resume_restore(&snapshot, &target, ENV).unwrap();
        assert!(identity(&open(&file, false).unwrap()).unwrap() == id);
        assert_eq!(fs::read_dir(target.join("copying")).unwrap().count(), 0);
    }
    #[test]
    fn resume_refuses_unknown_or_changed_staged_bytes_before_removing_anything() {
        for mutation in [
            "unmarked",
            "unknown-root",
            "unknown-data",
            "unknown-temp",
            "changed-temp",
            "changed-data",
            "changed-pending",
            "changed-plan",
        ] {
            let (root, source, snapshot) = fixture();
            capture(&source, &snapshot, ENV).unwrap();
            let target = root.path().join("resume");
            assert!(restore_inner(&snapshot, &target, ENV, || Err(
                io::ErrorKind::Interrupted.into()
            ))
            .is_err());
            let manifest = read_manifest(&snapshot, MANIFEST).unwrap();
            let entry = manifest
                .entries
                .iter()
                .find(|e| e.name == "sessions/chat.json")
                .unwrap();
            let hash = entry.sha256.as_ref().unwrap();
            let temp = target.join("copying").join(format!("{hash}.copy"));
            fs::write(&temp, b"EX").unwrap();
            match mutation {
                "unmarked" => {
                    fs::remove_file(target.join(RESTORE_PLAN)).unwrap();
                }
                "unknown-root" => fs::write(target.join("keep-user-file"), b"KEEP").unwrap(),
                "unknown-data" => fs::write(target.join("data/keep-user-file"), b"KEEP").unwrap(),
                "unknown-temp" => fs::write(target.join("copying/unknown.copy"), b"KEEP").unwrap(),
                "changed-temp" => fs::write(&temp, b"wrong").unwrap(),
                "changed-data" => {
                    fs::write(target.join("data/sessions/chat.json"), b"new user data").unwrap()
                }
                "changed-pending" => {
                    fs::write(target.join("manifest.pending"), b"unknown").unwrap()
                }
                "changed-plan" => fs::write(target.join(RESTORE_PLAN), b"{}").unwrap(),
                _ => unreachable!(),
            }
            let before = inventory(&target, Instant::now() + TIMEOUT).unwrap();
            let saved = fs::read(&temp).unwrap();
            assert!(
                resume_restore(&snapshot, &target, ENV).is_err(),
                "{mutation}"
            );
            assert!(
                before == inventory(&target, Instant::now() + TIMEOUT).unwrap(),
                "{mutation}"
            );
            assert_eq!(fs::read(temp).unwrap(), saved);
            assert!(!target.join(RESTORED).exists());
        }
    }
    #[test]
    fn a_different_snapshot_or_modified_completed_restore_is_never_repaired_over_data() {
        let (root, source, snapshot) = fixture();
        capture(&source, &snapshot, ENV).unwrap();
        let target = root.path().join("restore");
        restore(&snapshot, &target, ENV).unwrap();
        fs::write(source.join("attachment.bin"), b"updated export").unwrap();
        let other = root.path().join("other-snapshot");
        capture(&source, &other, ENV).unwrap();
        assert!(resume_restore(&other, &target, ENV).is_err());
        assert_eq!(
            fs::read(target.join("data/attachment.bin")).unwrap(),
            [0, 1, 2, 0, 255]
        );
        fs::write(target.join("data/attachment.bin"), b"user change").unwrap();
        assert!(resume_restore(&snapshot, &target, ENV).is_err());
        assert_eq!(
            fs::read(target.join("data/attachment.bin")).unwrap(),
            b"user change"
        );
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn linked_sources_and_objects_cannot_escape_the_explicit_export() {
        let (root, source, destination) = fixture();
        let outside = root.path().join("outside");
        fs::write(&outside, "UNTOUCHED").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, source.join("linked")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, source.join("linked")).unwrap();
        assert!(capture(&source, &destination, ENV).is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "UNTOUCHED");
        fs::remove_file(source.join("linked")).unwrap();
        capture(&source, &destination, ENV).unwrap();
        let moved = root.path().join("moved-objects");
        fs::rename(destination.join("objects"), &moved).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&moved, destination.join("objects")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&moved, destination.join("objects")).unwrap();
        assert!(verify(&destination, ENV).is_err());
    }
}
