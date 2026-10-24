//! Relocate only native rollout indexes in a private restored copy. Never open
//! the source profile or regenerate timestamps through database triggers.
use crate::{profile_snapshot, storage_stage};
use rusqlite::{params, Connection, OpenFlags};
use std::{
    fs,
    path::{Component, Path},
    time::{Duration, Instant},
};

const ERROR: &str = "Codex profile handoff could not verify its native history paths; the original profile was not changed";
type Result<T> = std::result::Result<T, String>;

pub(crate) fn rebase(profile: &Path, original: &Path) -> Result<usize> {
    let mut changed = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut databases = Vec::new();
    for (n, entry) in fs::read_dir(profile).map_err(|_| ERROR)?.enumerate() {
        if n >= 20_000 {
            return Err(ERROR.into());
        }
        let entry = entry.map_err(|_| ERROR)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(ERROR.into());
        };
        if name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            databases.push(entry.path());
        }
    }
    if databases.len() > 16 {
        return Err(ERROR.into());
    }
    for path in databases {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let target = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().unwrap().to_str().ok_or(ERROR)?
            ));
            match fs::symlink_metadata(&target) {
                Err(e) if !suffix.is_empty() && e.kind() == std::io::ErrorKind::NotFound => {
                    continue
                }
                Err(_) => return Err(ERROR.into()),
                Ok(_) => {}
            }
            let file = storage_stage::open(&target, false).map_err(|_| ERROR)?;
            if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
                return Err(ERROR.into());
            }
        }
        let mut db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| ERROR)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| ERROR)?;
        db.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            8 * 1024 * 1024,
        )
        .map_err(|_| ERROR)?;
        db.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER,
            false,
        )
        .map_err(|_| ERROR)?;
        db.progress_handler(10_000, Some(move || Instant::now() >= deadline))
            .map_err(|_| ERROR)?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048; PRAGMA mmap_size=0;")
            .map_err(|_| ERROR)?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| ERROR)?;
        let sql: String = tx
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name='threads'",
                [],
                |r| r.get(0),
            )
            .map_err(|_| ERROR)?;
        if sql.to_ascii_uppercase().contains("VIRTUAL") {
            return Err(ERROR.into());
        }
        let mut stmt = tx
            .prepare("SELECT id,rollout_path FROM threads LIMIT 50001")
            .map_err(|_| ERROR)?;
        let mut rows = stmt.query([]).map_err(|_| ERROR)?;
        let mut updates = Vec::new();
        let mut retained_bytes = 0usize;
        while let Some(row) = rows.next().map_err(|_| ERROR)? {
            if updates.len() >= 50_000 || Instant::now() >= deadline {
                return Err(ERROR.into());
            }
            let id: String = row.get(0).map_err(|_| ERROR)?;
            let old: String = row.get(1).map_err(|_| ERROR)?;
            uuid::Uuid::parse_str(&id).map_err(|_| ERROR)?;
            if old.len() > 4096 {
                return Err(ERROR.into());
            }
            let old_path = Path::new(&old);
            let relative = if old_path.is_absolute() {
                old_path
                    .strip_prefix(original)
                    .or_else(|_| old_path.strip_prefix(profile))
                    .map_err(|_| ERROR)?
            } else {
                old_path
            };
            let mut parts = relative.components();
            if !matches!(parts.next(), Some(Component::Normal(s)) if s == "sessions" || s == "archived_sessions")
                || relative.components().count() < 2
                || relative
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
            {
                return Err(ERROR.into());
            }
            let target = profile.join(relative);
            // Every component must be ordinary, including parents. A native
            // session must not resume a path that escapes through a link.
            let mut current = profile.to_owned();
            let components: Vec<_> = relative.components().collect();
            for (i, component) in components.iter().enumerate() {
                current.push(component.as_os_str());
                storage_stage::open(&current, i + 1 != components.len()).map_err(|_| ERROR)?;
            }
            let (file, _) =
                profile_snapshot::open(&target, 512 * 1024 * 1024).map_err(|_| ERROR)?;
            use std::io::{BufRead, BufReader, Read};
            let mut first = Vec::new();
            BufReader::new(file)
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut first)
                .map_err(|_| ERROR)?;
            if first.len() > 1024 * 1024 || first.last() != Some(&b'\n') {
                return Err(ERROR.into());
            }
            let header: serde_json::Value = serde_json::from_slice(&first).map_err(|_| ERROR)?;
            if header["type"] != "session_meta" || header["payload"]["id"] != id {
                return Err(ERROR.into());
            }
            let target = target.to_str().ok_or(ERROR)?.to_owned();
            retained_bytes = retained_bytes.saturating_add(id.len() + old.len() + target.len());
            if retained_bytes > 32 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            updates.push((id, old, target));
        }
        drop(rows);
        drop(stmt);
        for (id, old, target) in updates {
            if old != target {
                let n = tx
                    .execute(
                        "UPDATE threads SET rollout_path=?1 WHERE id=?2 AND rollout_path=?3",
                        params![target, id, old],
                    )
                    .map_err(|_| ERROR)?;
                if n != 1 {
                    return Err(ERROR.into());
                }
                changed += 1;
            }
        }
        tx.commit().map_err(|_| ERROR)?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|_| ERROR)?;
    }
    Ok(changed)
}
