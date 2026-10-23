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
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    directory: bool,
    bytes: u64,
    sha256: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    environment: String,
    entries: Vec<Entry>,
}
/// Safe for diagnostics: counts only, no paths, prompts or credential contents.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
}
fn environment(value: &str) -> bool {
    value.len() == 34
        && value.starts_with("opencodex-")
        && value[10..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
fn capture_inner(
    source: &Path,
    destination: &Path,
    original_environment: &str,
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
    let before = inventory(&source, deadline)?;
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
    if before != inventory(&source, deadline)? || root_before != Observed::read(&source, true)? {
        return Err(invalid());
    }
    sync_directory(&destination.join("objects"))?;
    let manifest = Manifest {
        version: 1,
        environment: original_environment.into(),
        entries,
    };
    commit_manifest(&destination, MANIFEST, &manifest)?;
    verify(&destination, original_environment)
}

/// Check every object before a later migration may consume it. This does not
/// mark a profile ready, execute stored code, or connect to either agent.
pub fn verify(snapshot: &Path, original_environment: &str) -> io::Result<Report> {
    verify_inner(snapshot, original_environment).map(|(_, report)| report)
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
    let data = destination.join("data");
    directory(&data)?;
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
            let digest = entry.sha256.as_ref().ok_or_else(invalid)?;
            let mut out = writer(&path)?;
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
