//! Explicit, isolated native-profile handoff. The coordinator supplies quiesced
//! export snapshots; this module never stops an existing CLI or discovers a
//! normal profile. An exclusive lease covers preparation, runtime and rollback.
use crate::{migration_snapshot, private_directory, profile_lock, storage_stage};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const ERROR: &str = "Native profile handoff is incomplete or changed; original profiles and recovery copies were retained";
type Result<T> = std::result::Result<T, String>;

impl crate::store::Store {
    /// A stale launcher/configuration must not silently return a migrated
    /// bridge to the pre-upgrade profiles after new history has been written.
    pub(crate) fn check_native_profile_binding(&self, requested: Option<&Path>) -> Result<()> {
        use rusqlite::OptionalExtension;
        let saved: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='native.profileHandoff'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| ERROR)?;
        if let Some(saved) = saved {
            let requested = requested.ok_or("This connection uses handed-off native profiles. Restore its native_profile_handoff configuration; original profiles were not reopened")?;
            let canonical = requested.canonicalize().map_err(|_| ERROR)?;
            if saved.len() > 4096 || canonical.to_str() != Some(saved.as_str()) {
                return Err("This connection is bound to a different native profile generation; no profile was reopened".into());
            }
        }
        Ok(())
    }
    pub(crate) fn bind_native_profiles(&mut self, root: &Path) -> Result<()> {
        let path = root.canonicalize().map_err(|_| ERROR)?;
        let path = path.to_str().filter(|s| s.len() <= 4096).ok_or(ERROR)?;
        self.check_native_profile_binding(Some(root))?;
        self.db
            .execute(
                "INSERT OR IGNORE INTO metadata(key,value) VALUES('native.profileHandoff',?1)",
                [path],
            )
            .map_err(|_| ERROR)?;
        self.check_native_profile_binding(Some(root))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub original_profile: PathBuf,
    pub snapshot: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub environment: String,
    /// Existing installations retain their agent directories. Copy mode is
    /// retained for isolated rehearsals and already prepared generations.
    #[serde(default, skip_serializing_if = "ProfileMode::is_copy")]
    pub profile_mode: ProfileMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<Input>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<Input>,
}
impl Plan {
    pub(crate) fn inputs(&self) -> impl Iterator<Item = (&str, &Input)> {
        [
            ("codex", self.codex.as_ref()),
            ("claude", self.claude.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, input)| input.map(|i| (name, i)))
    }
    fn matches_profiles(&self, codex: Option<&Path>, claude: Option<&Path>) -> Result<bool> {
        for (input, path) in [(self.codex.as_ref(), codex), (self.claude.as_ref(), claude)] {
            match (input, path) {
                (None, None) => {}
                (Some(input), Some(path)) => {
                    let expected = if self.profile_mode == ProfileMode::Existing {
                        path.canonicalize().map_err(|_| ERROR)?
                    } else {
                        path.to_owned()
                    };
                    if input.original_profile != expected {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileMode {
    #[default]
    Copy,
    Existing,
}
impl ProfileMode {
    fn is_copy(&self) -> bool {
        *self == Self::Copy
    }
}
struct Journal {
    db: Connection,
    root: PathBuf,
    _lease: profile_lock::Lease,
    attempt: u32,
}
/// Held for the complete worker lifetime. Rollback cannot run while it exists.
pub struct Session {
    profiles: (Option<PathBuf>, Option<PathBuf>),
    existing: bool,
    _lease: profile_lock::Lease,
}
impl Session {
    pub fn retains_original_profiles(&self) -> bool {
        self.existing
    }
    pub fn profiles(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        self.profiles.clone()
    }
    fn from_journal(journal: Journal, plan: &Plan) -> Self {
        let existing = plan.profile_mode == ProfileMode::Existing;
        let path = |agent, input: &Option<Input>| {
            input.as_ref().map(|i| {
                if existing {
                    i.original_profile.clone()
                } else {
                    journal.data(agent)
                }
            })
        };
        Self {
            profiles: (path("codex", &plan.codex), path("claude", &plan.claude)),
            existing,
            _lease: journal._lease,
        }
    }
}
fn absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ERROR.into());
    }
    Ok(())
}
fn isolated(path: &Path) -> Result<()> {
    absolute(path)?;
    for key in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(key) {
            let home = PathBuf::from(home);
            let resolved = path.canonicalize().unwrap_or_else(|_| path.to_owned());
            if profile_lock::standard_agent_profile(path, &home)
                || [".codex", ".claude"].iter().any(|name| {
                    let normal = home.join(name);
                    resolved.starts_with(normal.canonicalize().unwrap_or(normal))
                })
            {
                return Err("Normal agent profiles cannot be handed off by this experiment".into());
            }
        }
    }
    Ok(())
}
fn ordinary_parents(path: &Path) -> Result<()> {
    absolute(path)?;
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        storage_stage::open(&current, true).map_err(|_| ERROR)?;
    }
    Ok(())
}
impl Journal {
    fn open(root: &Path, create: bool) -> Result<Self> {
        isolated(root)?;
        ordinary_parents(
            &root
                .parent()
                .ok_or(ERROR)?
                .canonicalize()
                .map_err(|_| ERROR)?,
        )?;
        if create && root.try_exists().map_err(|_| ERROR)? {
            // Reject a foreign directory before acquire() changes permissions
            // or creates a lock file. Read-only preflight is not ownership.
            storage_stage::open(root, true).map_err(|_| ERROR)?;
            if !root
                .join("handoff.sqlite")
                .try_exists()
                .map_err(|_| ERROR)?
            {
                for entry in fs::read_dir(root).map_err(|_| ERROR)? {
                    if entry.map_err(|_| ERROR)?.file_name() != "server.lock" {
                        return Err(ERROR.into());
                    }
                }
            }
        }
        if !create {
            storage_stage::open(root, true).map_err(|_| ERROR)?;
        }
        let lease = profile_lock::acquire(root)?;
        let root = root.canonicalize().map_err(|_| ERROR)?;
        let path = root.join("handoff.sqlite");
        let exists = path.try_exists().map_err(|_| ERROR)?;
        if !exists {
            if !create {
                return Err(ERROR.into());
            }
            for entry in fs::read_dir(&root).map_err(|_| ERROR)? {
                if entry.map_err(|_| ERROR)?.file_name() != "server.lock" {
                    return Err(ERROR.into());
                }
            }
        }
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let file = root.join(format!("handoff.sqlite{suffix}"));
            match fs::symlink_metadata(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(ERROR.into()),
                Ok(_) => {}
            }
            let f = storage_stage::open(&file, false).map_err(|_| ERROR)?;
            if storage_stage::links(&f).map_err(|_| ERROR)? != 1 {
                return Err(ERROR.into());
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | if exists {
                OpenFlags::empty()
            } else {
                OpenFlags::SQLITE_OPEN_CREATE
            };
        let db = Connection::open_with_flags(&path, flags).map_err(|_| ERROR)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|_| ERROR)?;
        }
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| ERROR)?;
        db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 65536)
            .map_err(|_| ERROR)?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA mmap_size=0;").map_err(|_| ERROR)?;
        if !exists {
            db.execute_batch("CREATE TABLE handoff(id INTEGER PRIMARY KEY CHECK(id=1),plan TEXT NOT NULL,codex_hash TEXT NOT NULL,claude_hash TEXT NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('copying','prepared','active','rolled-back')),attempt INTEGER NOT NULL CHECK(attempt BETWEEN 0 AND 3));").map_err(|_| ERROR)?;
        }
        let attempt = db
            .query_row("SELECT attempt FROM handoff WHERE id=1", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(Self {
            db,
            root,
            _lease: lease,
            attempt,
        })
    }
    fn data(&self, agent: &str) -> PathBuf {
        self.root
            .join(format!("attempt-{}", self.attempt))
            .join("working")
            .join(agent)
            .join("data")
    }
    fn baseline(&self, agent: &str) -> PathBuf {
        self.root
            .join(format!("attempt-{}", self.attempt))
            .join("baseline")
            .join(agent)
    }
    fn saved(&self) -> Result<(Plan, String)> {
        let (raw, phase): (String, String) = self
            .db
            .query_row("SELECT plan,phase FROM handoff WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|_| ERROR)?;
        let plan = serde_json::from_str(&raw).map_err(|_| ERROR)?;
        Ok((plan, phase))
    }
    fn transition(&self, before: &str, after: &str) -> Result<()> {
        if self
            .db
            .execute(
                "UPDATE handoff SET phase=?1 WHERE id=1 AND phase=?2",
                params![after, before],
            )
            .map_err(|_| ERROR)?
            != 1
        {
            return Err(ERROR.into());
        }
        Ok(())
    }
    fn verify_sources(&self, plan: &Plan) -> Result<()> {
        self.verify_sources_at(plan, false)
    }
    fn verify_sources_at(&self, plan: &Plan, running_existing: bool) -> Result<()> {
        validate(plan, &self.root)?;
        let hashes: (String, String) = self
            .db
            .query_row(
                "SELECT codex_hash,claude_hash FROM handoff WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| ERROR)?;
        for (input, expected) in [(&plan.codex, hashes.0), (&plan.claude, hashes.1)] {
            let Some(input) = input else {
                if !expected.is_empty() {
                    return Err(ERROR.into());
                }
                continue;
            };
            if snapshot_hash(&input.snapshot, &plan.environment)? != expected {
                return Err(ERROR.into());
            }
            // Once an existing installation has started, its original agent
            // folders are the live data. Never require or restore an old tree.
            // Snapshot hashes and canonical directory bindings still apply.
            if running_existing {
                continue;
            }
            (if plan.profile_mode == ProfileMode::Existing {
                migration_snapshot::verify_source_entries(&input.snapshot, &input.original_profile, &plan.environment)
            } else {
                migration_snapshot::verify_tree(&input.snapshot, &input.original_profile, &plan.environment)
            }).map_err(|_| "The original native profile changed after export. Handoff was stopped to avoid losing or splitting history; both copies are intact".to_owned())?;
        }
        Ok(())
    }
    fn unchanged(&self, plan: &Plan) -> Result<()> {
        if plan.profile_mode == ProfileMode::Existing {
            return self.verify_sources(plan);
        }
        for (agent, _) in plan.inputs() {
            migration_snapshot::verify_tree(
                &self.baseline(agent),
                &self.data(agent),
                &plan.environment,
            )
            .map_err(|_| ERROR)?;
        }
        Ok(())
    }
}
fn snapshot_hash(path: &Path, environment: &str) -> Result<String> {
    migration_snapshot::verify(path, environment).map_err(|_| ERROR)?;
    let raw = crate::profile_snapshot::bytes(&path.join("snapshot.json"), 8 * 1024 * 1024)
        .map_err(|_| ERROR)?;
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(raw)))
}
fn validate(plan: &Plan, root: &Path) -> Result<()> {
    if plan.version != 1 || !migration_snapshot::environment(&plan.environment) {
        return Err(ERROR.into());
    }
    for (_, input) in plan.inputs() {
        if plan.profile_mode == ProfileMode::Existing {
            // Existing mode binds the original directory and verifies selected
            // exports. It never copies authentication or replaces native files.
            absolute(&input.original_profile)?;
            ordinary_parents(&input.original_profile)?;
        } else {
            isolated(&input.original_profile)?;
        }
        if plan.profile_mode == ProfileMode::Copy
            && migration_snapshot::is_selected(&input.snapshot, &plan.environment)
                .map_err(|_| ERROR)?
        {
            return Err(ERROR.into());
        }
        if plan.profile_mode == ProfileMode::Existing
            && input.original_profile.canonicalize().map_err(|_| ERROR)? != input.original_profile
        {
            return Err(ERROR.into());
        }
        storage_stage::open(&input.snapshot, true).map_err(|_| ERROR)?;
        ordinary_parents(&input.snapshot.canonicalize().map_err(|_| ERROR)?)?;
        for path in [&input.original_profile, &input.snapshot] {
            let p = path.canonicalize().unwrap_or_else(|_| path.clone());
            if p.starts_with(root) || root.starts_with(p) {
                return Err(ERROR.into());
            }
        }
    }
    if plan
        .codex
        .as_ref()
        .zip(plan.claude.as_ref())
        .is_some_and(|(c, a)| c.original_profile == a.original_profile)
    {
        return Err(ERROR.into());
    }
    Ok(())
}

/// Bind original profiles, or create working copies for a copy-mode rehearsal.
/// Preparation never changes originals; failed partial copies are retained.
pub fn prepare(root: &Path, plan: &Plan) -> Result<Value> {
    let mut canonical = plan.clone();
    if canonical.profile_mode == ProfileMode::Existing {
        for input in [&mut canonical.codex, &mut canonical.claude]
            .into_iter()
            .flatten()
        {
            input.original_profile = input.original_profile.canonicalize().map_err(|_| ERROR)?;
        }
    }
    let plan = &canonical;
    absolute(root)?;
    validate(
        plan,
        &root.canonicalize().unwrap_or_else(|_| root.to_owned()),
    )?;
    let hash = |input: &Option<Input>| {
        input
            .as_ref()
            .map(|i| snapshot_hash(&i.snapshot, &plan.environment))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let hashes = [hash(&plan.codex)?, hash(&plan.claude)?];
    let mut journal = Journal::open(root, true)?;
    validate(plan, &journal.root)?;
    let raw = serde_json::to_string(plan).map_err(|_| ERROR)?;
    if raw.len() > 32768 {
        return Err(ERROR.into());
    }
    let count: i64 = journal
        .db
        .query_row("SELECT COUNT(*) FROM handoff", [], |r| r.get(0))
        .map_err(|_| ERROR)?;
    if count == 0 {
        journal
            .db
            .execute(
                "INSERT INTO handoff VALUES(1,?1,?2,?3,'copying',0)",
                params![raw, hashes[0], hashes[1]],
            )
            .map_err(|_| ERROR)?;
    } else {
        let saved: (String, String, String, String) = journal
            .db
            .query_row(
                "SELECT plan,codex_hash,claude_hash,phase FROM handoff WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|_| ERROR)?;
        if saved.0 != raw
            || saved.1 != hashes[0]
            || saved.2 != hashes[1]
            || !matches!(saved.3.as_str(), "copying" | "prepared")
        {
            return Err(ERROR.into());
        }
        if saved.3 == "prepared" {
            journal.unchanged(plan)?;
            return Ok(summary("prepared"));
        }
    }
    if plan.profile_mode == ProfileMode::Existing {
        journal.verify_sources(plan)?;
        journal.transition("copying", "prepared")?;
        return Ok(summary("prepared"));
    }
    // Interrupted attempts remain intact. A fresh numbered copy avoids trusting
    // a half-rebased database or an incomplete checkpoint after power loss.
    // Bound both retries and total retained disk, including failed attempts.
    let requested = plan.inputs().try_fold(0u64, |n, (_, input)| {
        migration_snapshot::verify(&input.snapshot, &plan.environment)
            .map(|r| {
                n.saturating_add(r.bytes.saturating_mul(2))
                    .saturating_add(32 * 1024 * 1024)
            })
            .map_err(|_| ERROR.to_owned())
    })?;
    if journal.attempt >= 3
        || retained_bytes(&journal.root)?.saturating_add(requested) > 8 * 1024 * 1024 * 1024
        || crate::storage_disk::available(&journal.root)?
            < requested.saturating_add(256 * 1024 * 1024)
    {
        return Err(
            "Native handoff storage or retry budget reached; retained copies were not removed"
                .into(),
        );
    }
    journal.attempt += 1;
    journal
        .db
        .execute(
            "UPDATE handoff SET attempt=?1 WHERE id=1 AND phase='copying'",
            [journal.attempt],
        )
        .map_err(|_| ERROR)?;
    let attempt = journal.root.join(format!("attempt-{}", journal.attempt));
    // Never adopt an unexpected pre-existing numbered attempt.
    if attempt.try_exists().map_err(|_| ERROR)? {
        return Err(ERROR.into());
    }
    private_directory::prepare(&attempt).map_err(|_| ERROR)?;
    private_directory::prepare(&attempt.join("working")).map_err(|_| ERROR)?;
    private_directory::prepare(&attempt.join("baseline")).map_err(|_| ERROR)?;
    for (agent, input) in plan.inputs() {
        let working = attempt.join("working").join(agent);
        migration_snapshot::restore(&input.snapshot, &working, &plan.environment)
            .map_err(|_| ERROR)?;
        if agent == "codex" {
            crate::handoff_codex::rebase(&journal.data(agent), &input.original_profile)?;
        }
        migration_snapshot::capture(
            &journal.data(agent),
            &journal.baseline(agent),
            &plan.environment,
        )
        .map_err(|_| ERROR)?;
    }
    journal.unchanged(plan)?;
    journal.transition("copying", "prepared")?;
    Ok(summary("prepared"))
}

/// Local maintenance entrypoint. Read a bounded, explicit plan; never print its
/// paths or profile contents in errors or status output.
pub fn prepare_file(root: &Path, file: &Path) -> Result<Value> {
    let raw = crate::profile_snapshot::bytes(file, 32768).map_err(|_| ERROR)?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    prepare(root, &plan)
}

/// Capture only an explicitly selected, stopped test profile. The exporting
/// coordinator is responsible for quiescing its CLI before invoking this.
pub fn capture_export(source: &Path, destination: &Path, environment: &str) -> Result<Value> {
    isolated(source)?;
    isolated(destination)?;
    let report =
        migration_snapshot::capture(source, destination, environment).map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"native-profile-exported","files":report.files,"bytes":report.bytes,"migrationComplete":false}),
    )
}
fn summary(phase: &str) -> Value {
    json!({"phase":phase,"originalProfilesChanged":false,"recoveryCopiesRetained":true,"migrationComplete":false})
}

/// Resolve a managed generation before any worker, authentication probe or
/// discovery starts. Shared/external owners need their own coordinated handoff.
pub fn activate(
    root: &Path,
    environment: &str,
    codex: Option<&Path>,
    claude: Option<&Path>,
) -> Result<Session> {
    let journal = Journal::open(root, false)?;
    let (plan, phase) = journal.saved()?;
    if plan.environment != environment || !plan.matches_profiles(codex, claude)? {
        return Err(
            "Native handoff does not belong to this connection or agent configuration".into(),
        );
    }
    journal.verify_sources_at(
        &plan,
        phase == "active" && plan.profile_mode == ProfileMode::Existing,
    )?;
    match phase.as_str() {
        "prepared" => {
            journal.unchanged(&plan)?;
            journal.transition("prepared", "active")?;
        }
        "active" => {
            // Mutable profiles are expected to gain messages and refreshed
            // credentials. Never replace them from an old snapshot on restart.
            if plan.profile_mode == ProfileMode::Copy {
                for (agent, _) in plan.inputs() {
                    ordinary_parents(&journal.data(agent))?;
                }
            }
        }
        _ => return Err("Native profile handoff is not ready to start".into()),
    }
    Ok(Session::from_journal(journal, &plan))
}

/// Check a stopped generation before selecting a replacement executable.
pub(crate) fn retained_mode(root: &Path) -> Result<ProfileMode> {
    let journal = Journal::open(root, false)?;
    let (plan, phase) = journal.saved()?;
    if !matches!(phase.as_str(), "prepared" | "active") {
        return Err(ERROR.into());
    }
    journal.verify_sources_at(
        &plan,
        phase == "active" && plan.profile_mode == ProfileMode::Existing,
    )?;
    Ok(plan.profile_mode)
}

/// Hold a prepared generation while the offline migration verifies its native
/// transcripts. This does not mark it active or start an agent.
pub(crate) fn inspect_prepared(
    root: &Path,
    environment: &str,
    codex: Option<&Path>,
    claude: Option<&Path>,
) -> Result<Session> {
    let journal = Journal::open(root, false)?;
    let (plan, phase) = journal.saved()?;
    if phase != "prepared"
        || plan.environment != environment
        || !plan.matches_profiles(codex, claude)?
    {
        return Err(ERROR.into());
    }
    journal.verify_sources(&plan)?;
    journal.unchanged(&plan)?;
    Ok(Session::from_journal(journal, &plan))
}

/// Withdraw an unused generation. After work changes, refusal preserves the
/// complete current profiles; rolling back the executable must keep those data.
/// This function never deletes a history or writes a byte to original profiles.
pub fn rollback(root: &Path, environment: &str) -> Result<Value> {
    let journal = Journal::open(root, false)?;
    let (plan, phase) = journal.saved()?;
    if plan.environment != environment {
        return Err(ERROR.into());
    }
    if phase == "rolled-back" {
        return Ok(summary(&phase));
    }
    if !matches!(phase.as_str(), "prepared" | "active") {
        return Err(ERROR.into());
    }
    if phase == "active" && plan.profile_mode == ProfileMode::Existing {
        return Err("This existing installation has started. Keep its current agent directories when rolling back the executable; no old data was restored.".into());
    }
    journal.unchanged(&plan).map_err(|_| "This generation contains new or changed data. Automatic data rollback was refused; keep its native profiles when rolling back the executable. All histories and recovery copies remain intact.".to_owned())?;
    journal.transition(&phase, "rolled-back")?;
    Ok(summary("rolled-back"))
}

fn retained_bytes(root: &Path) -> Result<u64> {
    let mut pending = vec![(root.to_owned(), 0usize)];
    let mut entries = 0;
    let mut bytes = 0u64;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while let Some((path, depth)) = pending.pop() {
        if depth > 64 || std::time::Instant::now() >= deadline {
            return Err(ERROR.into());
        }
        storage_stage::open(&path, true).map_err(|_| ERROR)?;
        for entry in fs::read_dir(&path).map_err(|_| ERROR)? {
            entries += 1;
            if entries > 120_000 {
                return Err(ERROR.into());
            }
            let entry = entry.map_err(|_| ERROR)?;
            let path = entry.path();
            if entry.file_type().map_err(|_| ERROR)?.is_dir() {
                pending.push((path, depth + 1));
            } else {
                let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
                bytes = bytes.saturating_add(file.metadata().map_err(|_| ERROR)?.len());
            }
        }
    }
    Ok(bytes)
}
