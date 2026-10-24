//! Resumable offline import of one verified export. The source and retained
//! export are never edited; every importer keeps its original receipt. This
//! stages data only and does not authorize native-profile activation.
use crate::{
    legacy_connection::Prepared, migration_snapshot, profile_lock, storage_stage, store::Store,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const ERROR: &str =
    "Cannot stage this offline export; existing chats and source files were not replaced";
const MAX_JSON: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Uploads {
    original_root: PathBuf,
    snapshot: String,
    manifest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Native {
    snapshot: String,
    transcript: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Images {
    original_root: PathBuf,
    export: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Codex {
    id: String,
    history: String,
    native: Option<Native>,
    uploads: Option<Uploads>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claude {
    id: String,
    history: String,
    native: Option<Native>,
    images: Option<Images>,
    uploads: Option<Uploads>,
    #[serde(default)]
    resume: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    connection: String,
    claude_settings: String,
    codex_projects: String,
    codex_headers: Option<String>,
    codex: Vec<Codex>,
    claude: Vec<Claude>,
}

fn absolute(path: &Path) -> Result<(), String> {
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
fn checked(root: &Path, relative: &str, directory: bool) -> Result<PathBuf, String> {
    if relative.is_empty() || relative.len() > 2048 || relative.contains(['\\', ':']) {
        return Err(ERROR.into());
    }
    let mut path = root.to_owned();
    let parts: Vec<_> = relative.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(ERROR.into());
        }
        path.push(part);
        storage_stage::open(&path, i + 1 < parts.len() || directory).map_err(|_| ERROR)?;
    }
    Ok(path)
}
fn read(path: &Path) -> Result<Vec<u8>, String> {
    let file = storage_stage::open(path, false).map_err(|_| ERROR)?;
    if file.metadata().map_err(|_| ERROR)?.len() > MAX_JSON {
        return Err(ERROR.into());
    }
    let mut data = Vec::new();
    file.take(MAX_JSON + 1)
        .read_to_end(&mut data)
        .map_err(|_| ERROR)?;
    if data.len() as u64 > MAX_JSON {
        return Err(ERROR.into());
    }
    Ok(data)
}
fn input(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    read(&checked(root, relative, false)?)
}
fn database(path: &Path) -> Result<(), String> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = PathBuf::from(format!("{}{suffix}", path.to_str().ok_or(ERROR)?));
        match fs::symlink_metadata(&file) {
            Ok(_) => {
                storage_stage::open(&file, false).map_err(|_| ERROR)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ERROR.into()),
        }
    }
    Ok(())
}
pub(crate) fn existing_store(directory: &Path) -> Result<Store, String> {
    let path = directory.join("native-connect.sqlite");
    database(&path)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|_| ERROR)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(|_| ERROR)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA cache_size=-2048;")
        .map_err(|_| ERROR)?;
    Ok(Store {
        db,
        directory: directory.to_owned(),
        storage: Default::default(),
    })
}
fn verify_index(store: &Store) -> Result<(), String> {
    if !crate::legacy_codex_headers::table(&store.db, "storage_assets")? {
        return Ok(());
    }
    let mut stmt = store
        .db
        .prepare("SELECT path,bytes FROM storage_assets LIMIT 80001")
        .map_err(|_| ERROR)?;
    let mut rows = stmt.query([]).map_err(|_| ERROR)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|_| ERROR)? {
        count += 1;
        if count > 80000 || Instant::now() >= deadline {
            return Err(ERROR.into());
        }
        let path = PathBuf::from(row.get::<_, String>(0).map_err(|_| ERROR)?);
        let bytes: i64 = row.get(1).map_err(|_| ERROR)?;
        let relative = path.strip_prefix(&store.directory).map_err(|_| ERROR)?;
        let mut current = store.directory.clone();
        let parts: Vec<_> = relative.components().collect();
        for (i, part) in parts.iter().enumerate() {
            if !matches!(part, Component::Normal(_)) {
                return Err(ERROR.into());
            }
            current.push(part.as_os_str());
            let file = storage_stage::open(&current, i + 1 < parts.len()).map_err(|_| ERROR)?;
            if i + 1 == parts.len()
                && i64::try_from(file.metadata().map_err(|_| ERROR)?.len()).map_err(|_| ERROR)?
                    != bytes
            {
                return Err(ERROR.into());
            }
        }
        if parts.is_empty() {
            return Err(ERROR.into());
        }
    }
    Ok(())
}
fn initialize_empty(store: &mut Store) -> Result<(), String> {
    // A crash inside Store::open can leave an empty, partly initialized schema.
    // Repair only that case, never a database containing imported/user data.
    let names:Vec<String>=store.db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 10").map_err(|_|ERROR)?.query_map([],|r|r.get(0)).map_err(|_|ERROR)?.collect::<rusqlite::Result<_>>().map_err(|_|ERROR)?;
    for name in names {
        if ![
            "clients",
            "metadata",
            "storage_assets",
            "storage_totals",
            "legacy_connection_import",
        ]
        .contains(&name.as_str())
        {
            return Err(ERROR.into());
        }
        let occupied:bool=store.db.query_row(&if name=="storage_totals" {"SELECT EXISTS(SELECT 1 FROM storage_totals WHERE id<>1 OR bytes<>0 OR files<>0)".to_owned()} else {format!("SELECT EXISTS(SELECT 1 FROM {name})")},[],|r|r.get(0)).map_err(|_|ERROR)?;
        if occupied {
            return Err(ERROR.into());
        }
    }
    for entry in fs::read_dir(&store.directory).map_err(|_| ERROR)? {
        let name = entry.map_err(|_| ERROR)?.file_name();
        if ![
            "native-connect.sqlite",
            "native-connect.sqlite-wal",
            "native-connect.sqlite-shm",
            "native-connect.sqlite-journal",
            "server.lock",
        ]
        .iter()
        .any(|s| name == *s)
        {
            return Err(ERROR.into());
        }
    }
    *store = Store::open(&store.directory).map_err(|_| ERROR)?;
    Ok(())
}
fn bind_connection(
    store: &mut Store,
    raw: &[u8],
    environment: &str,
    staged: bool,
) -> Result<(), String> {
    let prepared = Prepared::parse(raw, environment)?;
    let prior = if crate::legacy_codex_headers::table(&store.db, "legacy_connection_import")? {
        store
            .db
            .query_row(
                "SELECT raw,environment,phase FROM legacy_connection_import WHERE id=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ERROR)?
    } else {
        None
    };
    if let Some((saved, env, phase)) = prior {
        if saved != raw || env != environment || phase != "prepared" {
            return Err(ERROR.into());
        }
        prepared.verify_identity(&store.db)?;
        verify_index(store)?;
        if staged {
            // A completed generation gets the complete rollback preview. For
            // an interrupted import, unbound partial copies must first finish
            // through the original importers; all receipts are verified there
            // and the complete preview is required before marking staged.
            store.rollback_legacy_imports(false)?;
        }
    } else {
        if staged {
            return Err(ERROR.into());
        }
        initialize_empty(store)?;
        store.prepare_legacy_connection(&prepared)?;
    }
    Ok(())
}
fn uploads(store: &mut Store, root: &Path, id: &str, item: &Option<Uploads>) -> Result<(), String> {
    if let Some(item) = item {
        store.prepare_legacy_uploads(
            id,
            &item.original_root,
            &checked(root, &item.snapshot, true)?,
            &input(root, &item.manifest)?,
        )?;
    }
    Ok(())
}
fn import(store: &mut Store, root: &Path, plan: &Plan, deadline: Instant) -> Result<(), String> {
    store.prepare_legacy_preferences(
        &input(root, &plan.claude_settings)?,
        &input(root, &plan.codex_projects)?,
    )?;
    if let Some(headers) = &plan.codex_headers {
        store.prepare_legacy_codex_headers(&input(root, headers)?)?;
    }
    store.prepare_legacy_codex()?;
    for chat in &plan.codex {
        if Instant::now() >= deadline {
            return Err(ERROR.into());
        }
        store.prepare_legacy_codex_history(&chat.id, &input(root, &chat.history)?)?;
        if let Some(native) = &chat.native {
            store.prepare_legacy_codex_native(
                &chat.id,
                &checked(root, &native.snapshot, true)?,
                &native.transcript,
            )?;
        }
        uploads(store, root, &chat.id, &chat.uploads)?;
    }
    for chat in &plan.claude {
        if Instant::now() >= deadline {
            return Err(ERROR.into());
        }
        let raw = input(root, &chat.history)?;
        let value: Value = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
        if value["thread"]["id"].as_str() != Some(&chat.id) {
            return Err(ERROR.into());
        }
        drop(value);
        store.prepare_legacy_claude(&raw)?;
        store.prepare_legacy_claude_bindings(&chat.id)?;
        if let Some(native) = &chat.native {
            store.prepare_legacy_claude_native(
                &chat.id,
                &checked(root, &native.snapshot, true)?,
                &native.transcript,
            )?;
        }
        if let Some(images) = &chat.images {
            store.prepare_legacy_claude_tool_images(
                &chat.id,
                &images.original_root,
                &checked(root, &images.export, true)?,
            )?;
        }
        uploads(store, root, &chat.id, &chat.uploads)?;
        if chat.resume {
            store.prepare_legacy_claude_resume(&chat.id)?;
        }
    }
    let ids: BTreeSet<String> = store
        .db
        .prepare("SELECT id FROM chats ORDER BY id LIMIT 4097")
        .map_err(|_| ERROR)?
        .query_map([], |r| r.get(0))
        .map_err(|_| ERROR)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| ERROR)?;
    let expected: BTreeSet<_> = plan
        .codex
        .iter()
        .map(|c| c.id.clone())
        .chain(plan.claude.iter().map(|c| c.id.clone()))
        .collect();
    // Every imported Codex header/alias must have a history entry. An export
    // cannot silently omit a chat just because it was absent from its plan.
    if ids != expected {
        return Err(ERROR.into());
    }
    store.rollback_legacy_imports(false)?;
    Ok(())
}

/// Read only the supplied snapshot. A fresh generation is required. Retrying
/// the same generation accepts only the same verified export and unchanged
/// imported receipts. Both generation and state leases are held throughout.
pub fn run(snapshot: &Path, generation: &Path, environment: &str) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    absolute(snapshot)?;
    absolute(generation)?;
    // Resolve the existing parent to handle platform temp-directory aliases;
    // the final generation itself may never be a symlink/reparse point.
    let parent = generation
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    storage_stage::open(&parent, true).map_err(|_| ERROR)?;
    let generation = parent.join(generation.file_name().ok_or(ERROR)?);
    let snapshot = snapshot.canonicalize().map_err(|_| ERROR)?;
    if snapshot.starts_with(&generation) || generation.starts_with(&snapshot) {
        return Err(ERROR.into());
    }
    let snapshot_report = migration_snapshot::verify(&snapshot, environment).map_err(|_| ERROR)?;
    let manifest = read(&snapshot.join("snapshot.json"))?;
    let journal_path = generation.join("staging.sqlite");
    if generation.try_exists().map_err(|_| ERROR)? {
        storage_stage::open(&generation, true).map_err(|_| ERROR)?;
        for entry in fs::read_dir(&generation).map_err(|_| ERROR)? {
            let name = entry.map_err(|_| ERROR)?.file_name();
            if ![
                "staging.sqlite",
                "staging.sqlite-wal",
                "staging.sqlite-shm",
                "staging.sqlite-journal",
                "server.lock",
                "export",
                "state",
            ]
            .iter()
            .any(|s| name == *s)
            {
                return Err(ERROR.into());
            }
            if !journal_path.try_exists().map_err(|_| ERROR)? && name != "server.lock" {
                return Err(ERROR.into());
            }
        }
    }
    let _generation_lease = profile_lock::acquire(&generation)?;
    database(&journal_path)?;
    let journal = Connection::open(&journal_path).map_err(|_| ERROR)?;
    let tables: Vec<String> = journal
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 2")
        .map_err(|_| ERROR)?
        .query_map([], |r| r.get(0))
        .map_err(|_| ERROR)?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| ERROR)?;
    if tables.iter().any(|name| name != "staging") {
        return Err(ERROR.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).map_err(|_| ERROR)?;
    }
    journal.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-2048; CREATE TABLE IF NOT EXISTS staging(id INTEGER PRIMARY KEY CHECK(id=1),environment TEXT NOT NULL,manifest BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_| ERROR)?;
    let prior: Option<(String, Vec<u8>, String)> = journal
        .query_row(
            "SELECT environment,manifest,phase FROM staging WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|_| ERROR)?;
    let resumed = prior.is_some();
    let staged = prior
        .as_ref()
        .is_some_and(|(_, _, phase)| phase == "staged");
    if let Some((env, saved, phase)) = prior {
        if env != environment
            || saved != manifest
            || !["preparing", "staged"].contains(&phase.as_str())
        {
            return Err(ERROR.into());
        }
    } else {
        if generation.join("state").exists() || generation.join("export").exists() {
            return Err(ERROR.into());
        }
        journal
            .execute(
                "INSERT INTO staging VALUES(1,?1,?2,'preparing')",
                params![environment, manifest],
            )
            .map_err(|_| ERROR)?;
    }
    let export = generation.join("export");
    if !export
        .join("restored.json")
        .try_exists()
        .map_err(|_| ERROR)?
        && crate::storage_disk::available(&generation)?
            < snapshot_report.bytes.saturating_add(256 * 1024 * 1024)
    {
        return Err(
            "Not enough free disk space to retain this export; no source data was changed".into(),
        );
    }
    if export.exists() {
        migration_snapshot::resume_restore(&snapshot, &export, environment)
    } else {
        migration_snapshot::restore(&snapshot, &export, environment)
    }
    .map_err(|_| ERROR)?;
    let root = export.join("data");
    let plan: Plan = serde_json::from_slice(&input(&root, "stage.json")?).map_err(|_| ERROR)?;
    let ids: Vec<_> = plan
        .codex
        .iter()
        .map(|c| &c.id)
        .chain(plan.claude.iter().map(|c| &c.id))
        .collect();
    if plan.version != 1
        || ids.len() > 4096
        || ids
            .iter()
            .any(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err(ERROR.into());
    }
    let state = generation.join("state");
    let _state_lease = profile_lock::acquire(&state)?;
    let mut store = if state
        .join("native-connect.sqlite")
        .try_exists()
        .map_err(|_| ERROR)?
    {
        existing_store(&state)?
    } else {
        Store::open(&state).map_err(|_| ERROR)?
    };
    bind_connection(
        &mut store,
        &input(&root, &plan.connection)?,
        environment,
        staged,
    )?;
    import(&mut store, &root, &plan, deadline)?;
    migration_snapshot::verify_restored(&export, environment).map_err(|_| ERROR)?;
    if Instant::now() >= deadline {
        return Err(ERROR.into());
    }
    journal
        .execute("UPDATE staging SET phase='staged' WHERE id=1", [])
        .map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"legacy-export-staged","resumed":resumed,"codexChats":plan.codex.len(),"claudeChats":plan.claude.len(),"sourceUnchanged":true,"backupRetained":true,"runtimeStarted":false,"migrationComplete":false}),
    )
}
