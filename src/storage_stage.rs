//! Recoverable archive file moves. No HTTP mutation is enabled by this module.
//! Database reconciliation must own this lease and explicitly commit before purge.
//! Never overwrite a destination or remove an unrecognised file during recovery.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};
const JOURNAL: &str = "native-stage.json";
const MAX_JOURNAL: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
const MAX_FILE: u64 = 16 * 1024 * 1024 * 1024;
fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
fn ordinary(path: &Path, directory: bool) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !crate::profile_snapshot::regular(&m)
        || m.is_dir() != directory
        || (!directory && !m.is_file())
    {
        return Err(invalid());
    }
    Ok(())
}
fn options(directory: bool) -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(0x0020_0000 | if directory { 0x0200_0000 } else { 0 });
    }
    o
}
fn owned_temporary(name: &str) -> bool {
    name.strip_suffix(".tmp")
        .and_then(|s| {
            s.strip_prefix("journal-")
                .or_else(|| s.strip_prefix("record-"))
        })
        .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}
fn publish_new(directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = directory.join(format!("record-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&temp, directory.join(name))?;
        sync_directory(directory)?;
        fs::remove_file(&temp)?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    volume: u64,
    file: u64,
}
pub(crate) fn identity(file: &File) -> io::Result<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Identity {
            volume: m.dev(),
            file: m.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Identity {
            volume: info.dwVolumeSerialNumber as u64,
            file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        })
    }
}
pub(crate) fn links(file: &File) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks as u64)
    }
}
pub(crate) fn open(path: &Path, directory: bool) -> io::Result<File> {
    ordinary(path, directory)?;
    let f = options(directory).open(path)?;
    let meta = f.metadata()?;
    if !crate::profile_snapshot::regular(&meta)
        || meta.is_dir() != directory
        || (!directory && !meta.is_file())
    {
        return Err(invalid());
    }
    Ok(f)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        open(path, true)?.sync_all()
    }
    // Windows file contents are flushed before every atomic journal replacement.
    // FlushFileBuffers on a directory handle is not supported there.
    #[cfg(windows)]
    {
        ordinary(path, true)
    }
}
fn archive_name(relative: &str) -> io::Result<&str> {
    let n = relative
        .strip_prefix("archived_sessions/")
        .ok_or_else(invalid)?;
    if n.is_empty()
        || n.len() > 1024
        || n.contains(['/', '\\', ':'])
        || n.contains("..")
        || n.chars().any(char::is_control)
        || !(n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"))
    {
        return Err(invalid());
    }
    Ok(n)
}
fn stage_id(id: &str) -> bool {
    let Some(s) = id.strip_prefix(".trash/") else {
        return false;
    };
    let parts: Vec<_> = s.split('-').collect();
    (1..=2).contains(&parts.len())
        && parts
            .iter()
            .all(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
}
/// One native cleanup/restore per profile, released by the OS even after a crash.
/// The surrounding service must also stop admission of work in this profile.
pub struct Lease {
    home: PathBuf,
    home_id: Identity,
    _lock: File,
}
impl Lease {
    pub fn acquire(home: &Path) -> io::Result<Self> {
        if !home.is_absolute() {
            return Err(invalid());
        }
        ordinary(home, true)?;
        let home = home.canonicalize()?;
        let home_id = identity(&open(&home, true)?)?;
        let path = home.join(".remodex-storage.lock");
        if path.try_exists()? {
            ordinary(&path, false)?;
        }
        let mut o = options(false);
        o.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let lock = o.open(&path)?;
        let m = lock.metadata()?;
        if !m.is_file() || !crate::profile_snapshot::regular(&m) {
            return Err(invalid());
        }
        if links(&lock)? != 1 {
            return Err(invalid());
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
        let lease = Self {
            home,
            home_id,
            _lock: lock,
        };
        lease.check()?;
        Ok(lease)
    }
    pub(crate) fn check(&self) -> io::Result<()> {
        if identity(&open(&self.home, true)?)? != self.home_id {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    /// Caller must validate selection, pins, active threads and the preview digest
    /// under its database locks before invoking this operation.
    pub fn prepare<'a>(&'a self, relatives: &[String]) -> io::Result<Stage<'a>> {
        self.check()?;
        if relatives.is_empty() || relatives.len() > MAX_FILES {
            return Err(invalid());
        }
        ordinary(&self.home.join("archived_sessions"), true)?;
        let mut names = BTreeSet::new();
        let mut identities = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut files = Vec::new();
        for relative in relatives {
            let name = archive_name(relative)?;
            if !names.insert(name) {
                return Err(invalid());
            }
            let file = Fingerprint::read(&self.home.join(relative), relative.clone(), deadline)?;
            if !identities.insert(file.identity.clone()) {
                return Err(invalid());
            }
            files.push(file);
        }
        let trash = self.home.join(".trash");
        crate::private_directory::prepare(&trash)?;
        let id = format!(".trash/{}-{}", crate::auth::now(), rand::random::<u64>());
        let path = self.home.join(&id);
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&path)?;
        let mut s = Stage {
            lease: self,
            id,
            path: path.clone(),
            directory_id: identity(&open(&path, true)?)?,
            journal: Journal {
                version: 1,
                home: self.home_id.clone(),
                phase: Phase::Prepared,
                files,
                records: BTreeMap::new(),
            },
        };
        if let Err(e) = s.save() {
            let _ = fs::remove_dir(&path);
            return Err(e);
        }
        sync_directory(&trash)?;
        Ok(s)
    }
    /// Adopt a complete legacy quarantine only after database/schema preflight.
    /// No file is moved and no row is written here. Original recovery metadata
    /// stays intact until the new checksummed journal is durably published.
    pub(crate) fn adopt_legacy<'a>(
        &'a self,
        id: &str,
        relatives: &[String],
        records: &BTreeMap<String, Vec<u8>>,
    ) -> io::Result<Stage<'a>> {
        self.check()?;
        if !stage_id(id) || relatives.is_empty() || relatives.len() > MAX_FILES {
            return Err(invalid());
        }
        ordinary(&self.home.join(".trash"), true)?;
        let path = self.home.join(id);
        let directory_id = identity(&open(&path, true)?)?;
        if fs::symlink_metadata(path.join(JOURNAL)).is_ok() {
            return Err(invalid());
        }
        let mut names = BTreeSet::new();
        let mut identities = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut files = Vec::new();
        for relative in relatives {
            let name = archive_name(relative)?;
            if !names.insert(name.to_owned()) {
                return Err(invalid());
            }
            // A prior legacy partial restore cannot prove destination ownership.
            // Preserve both copies instead of guessing that a matching size is safe.
            match fs::symlink_metadata(self.home.join(relative)) {
                Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let file = Fingerprint::read(&path.join(name), relative.clone(), deadline)?;
            if !identities.insert(file.identity.clone()) {
                return Err(invalid());
            }
            files.push(file);
        }
        let mut metadata = BTreeMap::new();
        for (name, raw) in records {
            if raw.len() as u64 > Stage::auxiliary_limit(name)? {
                return Err(invalid());
            }
            if name != "native-rows.json"
                && crate::profile_snapshot::bytes(&path.join(name), raw.len() as u64 + 1)? != *raw
            {
                return Err(invalid());
            }
            metadata.insert(
                name.clone(),
                Record {
                    bytes: raw.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(raw)),
                },
            );
        }
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| invalid())?;
            if !names.contains(&name) && !records.contains_key(&name) && !owned_temporary(&name) {
                return Err(invalid());
            }
            ordinary(&entry.path(), false)?;
        }
        let stage = Stage {
            lease: self,
            id: id.into(),
            path: path.clone(),
            directory_id,
            journal: Journal {
                version: 1,
                home: self.home_id.clone(),
                phase: Phase::Reconciled,
                files,
                records: metadata,
            },
        };
        stage.check()?;
        let raw = records.get("native-rows.json").ok_or_else(invalid)?;
        match fs::symlink_metadata(path.join("native-rows.json")) {
            Ok(_) => {
                // A crash before publishing the journal may leave this immutable
                // record. It is reusable only if the validated conversion matches.
                if crate::profile_snapshot::bytes(&path.join("native-rows.json"), 16 * 1024 * 1024)?
                    != *raw
                {
                    return Err(invalid());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                publish_new(&path, "native-rows.json", raw)?
            }
            Err(e) => return Err(e),
        }
        stage.check()?;
        let raw = serde_json::to_vec(&stage.journal).map_err(|_| invalid())?;
        if raw.len() > MAX_JOURNAL {
            return Err(invalid());
        }
        publish_new(&path, JOURNAL, &raw)?;
        Ok(stage)
    }
    pub fn resume<'a>(&'a self, id: &str) -> io::Result<Stage<'a>> {
        self.check()?;
        if !stage_id(id) {
            return Err(invalid());
        }
        ordinary(&self.home.join(".trash"), true)?;
        let path = self.home.join(id);
        let directory_id = identity(&open(&path, true)?)?;
        let raw = crate::profile_snapshot::bytes(&path.join(JOURNAL), MAX_JOURNAL as u64)?;
        let journal: Journal = serde_json::from_slice(&raw).map_err(|_| invalid())?;
        if journal.version != 1
            || journal.home != self.home_id
            || journal.files.is_empty()
            || journal.files.len() > MAX_FILES
        {
            return Err(invalid());
        }
        let mut names = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for f in &journal.files {
            if !names.insert(archive_name(&f.relative)?)
                || !identities.insert(f.identity.clone())
                || f.bytes > MAX_FILE
                || f.sha256.len() != 64
                || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(invalid());
            }
        }
        for (name, record) in &journal.records {
            if record.bytes > Stage::auxiliary_limit(name)?
                || record.sha256.len() != 64
                || !record.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(invalid());
            }
        }
        Ok(Stage {
            lease: self,
            id: id.to_owned(),
            path,
            directory_id,
            journal,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fingerprint {
    relative: String,
    identity: Identity,
    bytes: u64,
    modified_ns: u64,
    sha256: String,
}
impl Fingerprint {
    fn read(path: &Path, relative: String, deadline: Instant) -> io::Result<Self> {
        let mut file = open(path, false)?;
        let before = file.metadata()?;
        if before.len() > MAX_FILE {
            return Err(invalid());
        }
        let id = identity(&file)?;
        let stamp = |m: &fs::Metadata| -> io::Result<u64> {
            u64::try_from(
                m.modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| invalid())?
                    .as_nanos(),
            )
            .map_err(|_| invalid())
        };
        let modified_ns = stamp(&before)?;
        let mut hash = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut bytes = 0;
        loop {
            if Instant::now() > deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            if bytes > before.len() {
                return Err(invalid());
            }
            hash.update(&buf[..n]);
        }
        let after = file.metadata()?;
        if bytes != before.len()
            || after.len() != bytes
            || stamp(&after)? != modified_ns
            || identity(&open(path, false)?)? != id
        {
            return Err(invalid());
        }
        Ok(Self {
            relative,
            identity: id,
            bytes,
            modified_ns,
            sha256: format!("{:x}", hash.finalize()),
        })
    }
    fn matches(&self, path: &Path, deadline: Instant) -> io::Result<()> {
        if Self::read(path, self.relative.clone(), deadline)? != *self {
            return Err(invalid());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    FilesStaged,
    Reconciled,
    Restoring,
    Restored,
    RowsRestored,
    Purging,
    Purged,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    home: Identity,
    phase: Phase,
    files: Vec<Fingerprint>,
    #[serde(default)]
    records: BTreeMap<String, Record>,
}
pub struct Stage<'a> {
    lease: &'a Lease,
    id: String,
    path: PathBuf,
    directory_id: Identity,
    journal: Journal,
}
impl Stage<'_> {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn directory(&self) -> &Path {
        &self.path
    }
    pub(crate) fn relatives(&self) -> Vec<String> {
        self.journal
            .files
            .iter()
            .map(|f| f.relative.clone())
            .collect()
    }
    pub(crate) fn rows_restored(&mut self) -> io::Result<()> {
        if !matches!(self.journal.phase, Phase::Restored | Phase::RowsRestored) {
            return Err(invalid());
        }
        self.journal.phase = Phase::RowsRestored;
        self.save()
    }
    pub(crate) fn restoration_complete(&self) -> bool {
        self.journal.phase == Phase::RowsRestored
    }
    fn auxiliary_limit(name: &str) -> io::Result<u64> {
        match name {
            "manifest.json" => Ok(1024 * 1024),
            "native-rows.json" | "satellite-backup.json" => Ok(16 * 1024 * 1024),
            _ => Err(invalid()),
        }
    }
    pub(crate) fn read_record(&self, name: &str) -> io::Result<Vec<u8>> {
        self.check()?;
        let record = self.journal.records.get(name).ok_or_else(invalid)?;
        let bytes =
            crate::profile_snapshot::bytes(&self.path.join(name), Self::auxiliary_limit(name)?)?;
        if bytes.len() as u64 != record.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != record.sha256
        {
            return Err(invalid());
        }
        Ok(bytes)
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.journal.files.iter().map(|f| f.bytes).sum()
    }
    pub(crate) fn uncommitted(&self) -> bool {
        self.journal.phase == Phase::Prepared
    }

    pub(crate) fn write_record(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.check()?;
        if self.journal.phase != Phase::Prepared
            || self.journal.records.contains_key(name)
            || bytes.len() as u64 > Self::auxiliary_limit(name)?
        {
            return Err(invalid());
        }
        let temporary = self
            .path
            .join(format!("record-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Records are immutable. A resumed operation must read and validate
            // the original rather than replace it with a newer snapshot.
            fs::hard_link(&temporary, self.path.join(name))?;
            sync_directory(&self.path)?;
            fs::remove_file(&temporary)?;
            sync_directory(&self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.journal.records.insert(
            name.to_owned(),
            Record {
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            },
        );
        self.save()
    }
    fn check(&self) -> io::Result<()> {
        self.lease.check()?;
        ordinary(&self.lease.home.join(".trash"), true)?;
        if identity(&open(&self.path, true)?)? != self.directory_id {
            return Err(invalid());
        }
        ordinary(&self.lease.home.join("archived_sessions"), true)
    }
    fn save(&mut self) -> io::Result<()> {
        self.check()?;
        let bytes = serde_json::to_vec(&self.journal).map_err(|_| invalid())?;
        if bytes.len() > MAX_JOURNAL {
            return Err(invalid());
        }
        let temp = self
            .path
            .join(format!("journal-{}.tmp", uuid::Uuid::new_v4()));
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let result = (|| {
            let mut f = o.open(&temp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&temp, self.path.join(JOURNAL))?;
            sync_directory(&self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    /// Hard-link then unlink is atomic with respect to destination creation.
    /// If interrupted between them, recovery recognises the same inode/file ID.
    fn move_file(
        &self,
        f: &Fingerprint,
        from: &Path,
        to: &Path,
        deadline: Instant,
    ) -> io::Result<()> {
        self.check()?;
        f.matches(from, deadline)?;
        match fs::hard_link(from, to) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                f.matches(to, deadline)?;
            }
            Err(e) => return Err(e),
        }
        f.matches(to, deadline)?;
        sync_directory(to.parent().ok_or_else(invalid)?)?;
        f.matches(from, deadline)?;
        fs::remove_file(from)?;
        sync_directory(from.parent().ok_or_else(invalid)?)
    }
    pub fn stage_files(&mut self) -> io::Result<()> {
        if !matches!(self.journal.phase, Phase::Prepared | Phase::FilesStaged) {
            return Err(invalid());
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        for f in &self.journal.files {
            let from = self.lease.home.join(&f.relative);
            let to = self.path.join(archive_name(&f.relative)?);
            match fs::symlink_metadata(&from) {
                Ok(_) => self.move_file(f, &from, &to, deadline)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => f.matches(&to, deadline)?,
                Err(e) => return Err(e),
            }
        }
        self.journal.phase = Phase::FilesStaged;
        self.save()
    }
    /// Only the database coordinator may mark this, after all row commits are durable.
    pub fn reconciled(&mut self) -> io::Result<()> {
        if self.journal.phase != Phase::FilesStaged {
            return Err(invalid());
        }
        self.check()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        for f in &self.journal.files {
            f.matches(&self.path.join(archive_name(&f.relative)?), deadline)?;
        }
        self.journal.phase = Phase::Reconciled;
        self.save()
    }
    pub(crate) fn purge_started(&self) -> bool {
        matches!(self.journal.phase, Phase::Purging | Phase::Purged)
    }
    fn begin_purge(&mut self) -> io::Result<()> {
        self.check()?;
        if self.purge_started() {
            return Ok(());
        }
        if self.journal.phase != Phase::Reconciled {
            return Err(invalid());
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        // Validate every file before crossing the irreversible boundary.
        for f in &self.journal.files {
            f.matches(&self.path.join(archive_name(&f.relative)?), deadline)?;
        }
        self.journal.phase = Phase::Purging;
        self.save()
    }
    pub(crate) fn purge_files(&mut self) -> io::Result<()> {
        self.purge_files_inner(&|_| Ok(()))
    }
    fn purge_files_inner(&mut self, hook: &dyn Fn(usize) -> io::Result<()>) -> io::Result<()> {
        self.begin_purge()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        for (index, f) in self.journal.files.iter().enumerate() {
            self.check()?;
            let path = self.path.join(archive_name(&f.relative)?);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    f.matches(&path, deadline)?;
                    fs::remove_file(path)?;
                    sync_directory(&self.path)?;
                    hook(index)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        self.journal.phase = Phase::Purged;
        self.save()
    }
    pub(crate) fn finish_purge(self) -> io::Result<()> {
        if self.journal.phase != Phase::Purged {
            return Err(invalid());
        }
        self.check()?;
        for f in &self.journal.files {
            match fs::symlink_metadata(self.path.join(archive_name(&f.relative)?)) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(invalid()),
            }
        }
        self.remove_metadata(true)
    }
    /// Can roll back a partial stage or resume an interrupted restore. It never
    /// replaces another file, even when its content happens to be identical.
    pub fn restore_files(&mut self) -> io::Result<()> {
        if self.purge_started() {
            return Err(invalid());
        }
        self.check()?;
        self.journal.phase = Phase::Restoring;
        self.save()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        for f in &self.journal.files {
            let from = self.path.join(archive_name(&f.relative)?);
            let to = self.lease.home.join(&f.relative);
            match fs::symlink_metadata(&from) {
                Ok(_) => self.move_file(f, &from, &to, deadline)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => f.matches(&to, deadline)?,
                Err(e) => return Err(e),
            }
        }
        self.journal.phase = Phase::Restored;
        self.save()
    }
    /// Called only after both files and rows have been restored. Unknown entries
    /// (including database backups) must first be reconciled by their owner.
    pub fn finish_restore(self) -> io::Result<()> {
        if !matches!(self.journal.phase, Phase::Restored | Phase::RowsRestored) {
            return Err(invalid());
        }
        self.check()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        for f in &self.journal.files {
            f.matches(&self.lease.home.join(&f.relative), deadline)?;
        }
        let complete = self.restoration_complete();
        self.remove_metadata(complete)
    }
    fn remove_metadata(self, complete: bool) -> io::Result<()> {
        let entries = fs::read_dir(&self.path)?.collect::<io::Result<Vec<_>>>()?;
        for e in &entries {
            let name = e.file_name();
            if name == JOURNAL {
                continue;
            }
            let owned_temp = name.to_str().is_some_and(owned_temporary);
            if !complete
                || (!owned_temp
                    && !matches!(name.to_str(), Some("manifest.json" | "native-rows.json"))
                    && !name
                        .to_str()
                        .is_some_and(|name| self.journal.records.contains_key(name)))
            {
                return Err(io::ErrorKind::DirectoryNotEmpty.into());
            }
            ordinary(&e.path(), false)?;
            if let Some(name) = name
                .to_str()
                .filter(|name| self.journal.records.contains_key(*name))
            {
                self.read_record(name)?;
            }
        }
        // RowsRestored is durable before any backup is removed. A retry can
        // finish this phase even after a crash between the metadata removals.
        for entry in &entries {
            if complete && entry.file_name() != JOURNAL {
                match fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        fs::remove_file(self.path.join(JOURNAL))?;
        fs::remove_dir(&self.path)?;
        sync_directory(&self.lease.home.join(".trash"))
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;
    fn root() -> tempfile::TempDir {
        let h = tempfile::tempdir().unwrap();
        fs::create_dir(h.path().join("archived_sessions")).unwrap();
        for name in ["a", "b"] {
            fs::write(
                h.path().join(format!("archived_sessions/{name}.jsonl")),
                name,
            )
            .unwrap();
        }
        h
    }
    fn paths() -> Vec<String> {
        vec![
            "archived_sessions/a.jsonl".into(),
            "archived_sessions/b.jsonl".into(),
        ]
    }
    #[test]
    fn purge_requires_committed_rows_and_never_deletes_before_reconciliation() {
        let h = root();
        let lease = Lease::acquire(h.path()).unwrap();
        let mut stage = lease.prepare(&paths()).unwrap();
        assert!(stage.purge_files().is_err());
        stage.stage_files().unwrap();
        assert!(stage.purge_files().is_err());
        assert!(stage.directory().join("a.jsonl").exists());
        stage.restore_files().unwrap();
        stage.finish_restore().unwrap();
        assert_eq!(
            fs::read(h.path().join("archived_sessions/a.jsonl")).unwrap(),
            b"a"
        );
    }
    #[test]
    fn interrupted_purge_resumes_without_restoring_metadata_or_touching_new_files() {
        let h = root();
        let lease = Lease::acquire(h.path()).unwrap();
        let mut stage = lease.prepare(&paths()).unwrap();
        let id = stage.id().to_owned();
        stage.stage_files().unwrap();
        stage.reconciled().unwrap();
        assert!(stage
            .purge_files_inner(&|i| if i == 0 {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(())
            })
            .is_err());
        assert!(stage.purge_started());
        assert!(!stage.directory().join("a.jsonl").exists());
        assert!(stage.directory().join("b.jsonl").exists());
        assert!(stage.restore_files().is_err());
        drop(stage);
        drop(lease);
        fs::write(h.path().join("archived_sessions/a.jsonl"), "USER_NEW_FILE").unwrap();
        let lease = Lease::acquire(h.path()).unwrap();
        let mut stage = lease.resume(&id).unwrap();
        stage.purge_files().unwrap();
        stage.finish_purge().unwrap();
        assert!(!h.path().join(id).exists());
        assert_eq!(
            fs::read(h.path().join("archived_sessions/a.jsonl")).unwrap(),
            b"USER_NEW_FILE"
        );
        assert!(!h.path().join("archived_sessions/b.jsonl").exists());
    }
    #[test]
    fn purge_refuses_changed_archive_and_preserves_unknown_stage_entries() {
        let h = root();
        let lease = Lease::acquire(h.path()).unwrap();
        let mut stage = lease.prepare(&paths()).unwrap();
        let id = stage.id().to_owned();
        stage.stage_files().unwrap();
        stage.reconciled().unwrap();
        let path = stage.directory().join("b.jsonl");
        let old = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "CHANGED").unwrap();
        assert!(stage.purge_files().is_err());
        assert!(!stage.purge_started());
        assert!(stage.directory().join("a.jsonl").exists());
        fs::write(&path, "b").unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        fs::write(stage.directory().join("unknown"), "KEEP").unwrap();
        stage.purge_files().unwrap();
        assert!(stage.finish_purge().is_err());
        assert_eq!(
            fs::read(h.path().join(&id).join("unknown")).unwrap(),
            b"KEEP"
        );
        let mut stage = lease.resume(&id).unwrap();
        assert!(stage.restore_files().is_err());
        fs::remove_file(stage.directory().join("unknown")).unwrap();
        stage.finish_purge().unwrap();
    }
}
