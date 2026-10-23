//! Bounded row backups for archive reconciliation. The cleanup coordinator owns
//! the profile lease, file journal and commit ordering; these APIs do not run at startup.
use crate::storage_stage::{self, Identity, Lease};
use base64::Engine;
use rusqlite::{
    params_from_iter,
    types::{Value, ValueRef},
    Connection, OpenFlags, OptionalExtension,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ROWS: usize = 50_000;
type Result<T> = std::result::Result<T, &'static str>;
const FAILED: &str = "db_reconcile_failed";
fn failure(e: rusqlite::Error) -> &'static str {
    if matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        "codex_busy"
    } else {
        FAILED
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    State,
    Logs,
    Memories,
    Goals,
}
impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Self::State => "state_",
            Self::Logs => "logs_",
            Self::Memories => "memories_",
            Self::Goals => "goals_",
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    Threads,
    DynamicTools,
    SpawnEdges,
    Logs,
    Stage1,
    Jobs,
    Goals,
    Deferrals,
}
impl Table {
    fn name(self) -> &'static str {
        match self {
            Self::Threads => "threads",
            Self::DynamicTools => "thread_dynamic_tools",
            Self::SpawnEdges => "thread_spawn_edges",
            Self::Logs => "logs",
            Self::Stage1 => "stage1_outputs",
            Self::Jobs => "jobs",
            Self::Goals => "thread_goals",
            Self::Deferrals => "thread_goal_continuation_deferrals",
        }
    }
    fn kind(self) -> Kind {
        match self {
            Self::Threads | Self::DynamicTools | Self::SpawnEdges => Kind::State,
            Self::Logs => Kind::Logs,
            Self::Stage1 | Self::Jobs => Kind::Memories,
            Self::Goals | Self::Deferrals => Kind::Goals,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(String),
}
impl Cell {
    fn capture(v: ValueRef<'_>, budget: &mut Budget) -> Result<Self> {
        let bytes = match v {
            ValueRef::Text(b) | ValueRef::Blob(b) => b.len(),
            _ => 16,
        };
        budget.add(bytes + 16)?;
        Ok(match v {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(n) => Self::Integer(n),
            ValueRef::Real(n) if n.is_finite() => Self::Real(n),
            ValueRef::Real(_) => return Err(FAILED),
            ValueRef::Text(s) => Self::Text(std::str::from_utf8(s).map_err(|_| FAILED)?.to_owned()),
            ValueRef::Blob(b) => Self::Blob(base64::engine::general_purpose::STANDARD.encode(b)),
        })
    }
    fn value(&self) -> Result<Value> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Integer(n) => Value::Integer(*n),
            Self::Real(n) if n.is_finite() => Value::Real(*n),
            Self::Real(_) => return Err(FAILED),
            Self::Text(s) => Value::Text(s.clone()),
            Self::Blob(s) => Value::Blob(
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|_| FAILED)?,
            ),
        })
    }
}
struct Budget {
    bytes: usize,
    rows: usize,
}
impl Budget {
    fn add(&mut self, n: usize) -> Result<()> {
        self.bytes = self.bytes.checked_add(n).ok_or(FAILED)?;
        if self.bytes > MAX_BYTES {
            Err("storage_backup_too_large")
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rows {
    pub table: Table,
    columns: Vec<String>,
    keys: Vec<String>,
    rows: Vec<Vec<Cell>>,
}
impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
/// A live write transaction. Dropping it rolls back pending changes. File moves
/// must retain their journal until every database has committed or been restored.
pub struct Database<'a> {
    lease: &'a Lease,
    path: PathBuf,
    kind: Kind,
    identity: Identity,
    db: Connection,
    active: bool,
    deadline: Instant,
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
impl<'a> Database<'a> {
    pub fn open(lease: &'a Lease, kind: Kind) -> Result<Option<Self>> {
        lease.check().map_err(|_| FAILED)?;
        let mut selected = None;
        let mut count = 0;
        for e in fs::read_dir(lease.home()).map_err(|_| FAILED)? {
            count += 1;
            if count > 50_000 {
                return Err("storage_backup_too_large");
            }
            let e = e.map_err(|_| FAILED)?;
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(v) = name
                .strip_prefix(kind.prefix())
                .and_then(|s| s.strip_suffix(".sqlite"))
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u64>().ok())
            {
                if selected.as_ref().is_none_or(|(old, _)| v > *old) {
                    selected = Some((v, e.path()));
                }
            }
        }
        let Some((_, path)) = selected else {
            return Ok(None);
        };
        // SQLite must not create a new database or follow a redirected database,
        // WAL, shared-memory file or rollback journal in the selected profile.
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let p = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().unwrap().to_string_lossy()
            ));
            match fs::symlink_metadata(&p) {
                Ok(m) if m.is_file() && crate::profile_snapshot::regular(&m) => {
                    let handle = storage_stage::open(&p, false).map_err(|_| FAILED)?;
                    if storage_stage::links(&handle).map_err(|_| FAILED)? != 1 {
                        return Err(FAILED);
                    }
                }
                Err(e) if !suffix.is_empty() && e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(FAILED),
            }
        }
        let identity =
            storage_stage::identity(&storage_stage::open(&path, false).map_err(|_| FAILED)?)
                .map_err(|_| FAILED)?;
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(failure)?;
        if storage_stage::identity(&storage_stage::open(&path, false).map_err(|_| FAILED)?)
            .map_err(|_| FAILED)?
            != identity
        {
            return Err(FAILED);
        }
        db.busy_timeout(Duration::from_millis(100))
            .map_err(failure)?;
        // Limit SQLite's own allocation before reading cells, not just our Vec.
        db.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            MAX_BYTES as i32,
        )
        .map_err(failure)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        db.progress_handler(10_000, Some(move || Instant::now() > deadline))
            .map_err(failure)?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048; PRAGMA mmap_size=0; BEGIN IMMEDIATE;").map_err(failure)?;
        Ok(Some(Self {
            lease,
            path,
            kind,
            identity,
            db,
            active: true,
            deadline,
        }))
    }
    fn schema(&self, table: Table) -> Result<Option<(Vec<String>, Vec<String>)>> {
        if Instant::now() > self.deadline {
            return Err("storage_operation_timed_out");
        }
        if table.kind() != self.kind {
            return Err(FAILED);
        }
        let sql: Option<String> = self
            .db
            .query_row(
                "SELECT substr(sql,1,256) FROM sqlite_schema WHERE type='table' AND name=?1",
                [table.name()],
                |r| r.get(0),
            )
            .optional()
            .map_err(failure)?;
        let Some(sql) = sql else { return Ok(None) };
        if sql.to_ascii_uppercase().contains("VIRTUAL") {
            return Err(FAILED);
        }
        let triggers: i64 = self
            .db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name=?1",
                [table.name()],
                |r| r.get(0),
            )
            .map_err(failure)?;
        if triggers != 0 {
            return Err("storage_schema_not_supported");
        }
        let mut q = self
            .db
            .prepare(&format!("PRAGMA table_info({})", quote(table.name())))
            .map_err(failure)?;
        let rows = q
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
            .map_err(failure)?;
        let mut cols = Vec::new();
        let mut pk = Vec::new();
        for row in rows {
            let (name, key) = row.map_err(failure)?;
            if cols.len() >= 128 || name.len() > 256 {
                return Err(FAILED);
            }
            if key > 0 {
                pk.push((key, name.clone()));
            }
            cols.push(name);
        }
        pk.sort();
        if cols.is_empty() || pk.is_empty() {
            return Err("storage_schema_not_supported");
        }
        Ok(Some((cols, pk.into_iter().map(|(_, n)| n).collect())))
    }
    /// Captures only known per-thread rows. Consolidation-job changes require a
    /// separate compare-and-restore record and are not included in this primitive.
    pub fn capture(&self, table: Table, thread_ids: &[String]) -> Result<Option<Rows>> {
        let Some((columns, keys)) = self.schema(table)? else {
            return Ok(None);
        };
        if thread_ids.len() > MAX_ROWS
            || thread_ids.iter().any(|id| id.is_empty() || id.len() > 256)
        {
            return Err(FAILED);
        }
        let ids: BTreeSet<_> = thread_ids.iter().collect();
        let mut out = Rows {
            table,
            columns,
            keys,
            rows: Vec::new(),
        };
        let mut budget = Budget { bytes: 0, rows: 0 };
        for chunk in ids.into_iter().collect::<Vec<_>>().chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut values: Vec<Value> = chunk
                .iter()
                .map(|s| Value::Text((*s).to_string()))
                .collect();
            let predicate = match table {
                Table::Threads => format!("id IN ({placeholders})"),
                Table::SpawnEdges => {
                    values.extend(values.clone());
                    format!("parent_thread_id IN ({placeholders}) OR child_thread_id IN ({placeholders})")
                }
                Table::Jobs => format!("kind='memory_stage1' AND job_key IN ({placeholders})"),
                _ => format!("thread_id IN ({placeholders})"),
            };
            let sql = format!(
                "SELECT {} FROM {} WHERE {predicate} LIMIT {}",
                out.columns
                    .iter()
                    .map(|c| quote(c))
                    .collect::<Vec<_>>()
                    .join(","),
                quote(table.name()),
                MAX_ROWS + 1
            );
            let mut q = self.db.prepare(&sql).map_err(failure)?;
            let mut rows = q.query(params_from_iter(values)).map_err(failure)?;
            while let Some(row) = rows.next().map_err(failure)? {
                budget.rows += 1;
                if budget.rows > MAX_ROWS {
                    return Err("storage_backup_too_large");
                }
                let mut values = Vec::new();
                for i in 0..out.columns.len() {
                    values.push(Cell::capture(
                        row.get_ref(i).map_err(failure)?,
                        &mut budget,
                    )?);
                }
                // An edge can appear in two chunks; a backup must contain it once.
                out.rows.push(values);
            }
        }
        let mut seen = BTreeSet::new();
        out.rows.retain(|r| {
            seen.insert(<[u8; 32]>::from(Sha256::digest(
                serde_json::to_vec(r).expect("captured finite SQL cells serialize"),
            )))
        });
        Ok(Some(out))
    }
    fn validate(&self, backup: &Rows) -> Result<Vec<usize>> {
        let Some((columns, keys)) = self.schema(backup.table)? else {
            return Err(FAILED);
        };
        if columns != backup.columns || keys != backup.keys || backup.rows.len() > MAX_ROWS {
            return Err("storage_schema_changed");
        }
        if serde_json::to_vec(backup).map_err(|_| FAILED)?.len() > MAX_BYTES {
            return Err("storage_backup_too_large");
        }
        let positions: Vec<_> = keys
            .iter()
            .map(|key| columns.iter().position(|c| c == key).ok_or(FAILED))
            .collect::<Result<_>>()?;
        for row in &backup.rows {
            if row.len() != columns.len() || positions.iter().any(|i| matches!(row[*i], Cell::Null))
            {
                return Err(FAILED);
            }
        }
        Ok(positions)
    }
    fn current(&self, backup: &Rows, row: &[Cell], keys: &[usize]) -> Result<Option<Vec<Cell>>> {
        let sql = format!(
            "SELECT {} FROM {} WHERE {}",
            backup
                .columns
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<_>>()
                .join(","),
            quote(backup.table.name()),
            keys.iter()
                .map(|i| format!("{} IS ?", quote(&backup.columns[*i])))
                .collect::<Vec<_>>()
                .join(" AND ")
        );
        let values: Vec<_> = keys
            .iter()
            .map(|i| row[*i].value())
            .collect::<Result<_>>()?;
        let mut q = self.db.prepare(&sql).map_err(failure)?;
        let mut rows = q.query(params_from_iter(values)).map_err(failure)?;
        let Some(row) = rows.next().map_err(failure)? else {
            return Ok(None);
        };
        let mut budget = Budget { bytes: 0, rows: 0 };
        let mut cells = Vec::new();
        for i in 0..backup.columns.len() {
            cells.push(Cell::capture(
                row.get_ref(i).map_err(failure)?,
                &mut budget,
            )?);
        }
        Ok(Some(cells))
    }
    fn delete_inner(&self, backup: &Rows) -> Result<()> {
        let keys = self.validate(backup)?;
        self.reject_implicit_deletes(backup)?;
        let sql = format!(
            "DELETE FROM {} WHERE {}",
            quote(backup.table.name()),
            keys.iter()
                .map(|i| format!("{} IS ?", quote(&backup.columns[*i])))
                .collect::<Vec<_>>()
                .join(" AND ")
        );
        for row in &backup.rows {
            if self.current(backup, row, &keys)?.as_ref() != Some(row) {
                return Err("storage_rows_changed");
            }
            let values: Vec<_> = keys
                .iter()
                .map(|i| row[*i].value())
                .collect::<Result<_>>()?;
            if self
                .db
                .execute(&sql, params_from_iter(values))
                .map_err(failure)?
                != 1
            {
                return Err("storage_rows_changed");
            }
        }
        Ok(())
    }
    // SQLite CASCADE/SET NULL/SET DEFAULT must not change rows absent from the
    // backup. The coordinator removes recorded dependents first; if any remain,
    // refuse instead of letting a foreign-key action silently modify them.
    fn reject_implicit_deletes(&self, backup: &Rows) -> Result<()> {
        let mut query = self
            .db
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' LIMIT 129")
            .map_err(failure)?;
        let tables = query
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(failure)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(failure)?;
        if tables.len() > 128 {
            return Err("storage_schema_not_supported");
        }
        for table in tables {
            let mut query = self
                .db
                .prepare(&format!("PRAGMA foreign_key_list({})", quote(&table)))
                .map_err(failure)?;
            let refs = query
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(6)?,
                    ))
                })
                .map_err(failure)?;
            for item in refs {
                let (target, from, to, action) = item.map_err(failure)?;
                if target != backup.table.name()
                    || ["NO ACTION", "RESTRICT"].contains(&action.as_str())
                {
                    continue;
                }
                let target_column = to
                    .or_else(|| (backup.keys.len() == 1).then(|| backup.keys[0].clone()))
                    .ok_or("storage_schema_not_supported")?;
                let index = backup
                    .columns
                    .iter()
                    .position(|c| *c == target_column)
                    .ok_or(FAILED)?;
                let sql = format!(
                    "SELECT 1 FROM {} WHERE {} IS ?1 LIMIT 1",
                    quote(&table),
                    quote(&from)
                );
                for row in &backup.rows {
                    let present: Option<i64> = self
                        .db
                        .query_row(&sql, [row[index].value()?], |r| r.get(0))
                        .optional()
                        .map_err(failure)?;
                    if present.is_some() {
                        return Err("storage_dependents_remaining");
                    }
                }
            }
        }
        Ok(())
    }
    /// Idempotent restore inserts missing primary keys, accepts identical rows,
    /// and preserves concurrent edits by reporting a conflict without replacement.
    fn restore_inner(&self, backup: &Rows) -> Result<()> {
        let keys = self.validate(backup)?;
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote(backup.table.name()),
            backup
                .columns
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<_>>()
                .join(","),
            vec!["?"; backup.columns.len()].join(",")
        );
        for row in &backup.rows {
            if let Some(current) = self.current(backup, row, &keys)? {
                if current == *row {
                    continue;
                }
                return Err("storage_restore_conflict");
            }
            let values: Vec<_> = row.iter().map(Cell::value).collect::<Result<_>>()?;
            self.db
                .execute(&sql, params_from_iter(values))
                .map_err(failure)?;
        }
        Ok(())
    }
    pub fn delete(&mut self, backup: &Rows) -> Result<()> {
        self.modify(backup, false)
    }
    pub fn restore(&mut self, backup: &Rows) -> Result<()> {
        self.modify(backup, true)
    }
    fn modify(&mut self, backup: &Rows, restore: bool) -> Result<()> {
        if !self.active {
            return Err(FAILED);
        }
        let result = if restore {
            self.restore_inner(backup)
        } else {
            self.delete_inner(backup)
        };
        if result.is_err() {
            let _ = self.db.progress_handler(0, None::<fn() -> bool>);
            let _ = self.db.execute_batch("ROLLBACK");
            self.active = false;
        }
        result
    }
    pub fn commit(mut self) -> Result<()> {
        self.lease.check().map_err(|_| FAILED)?;
        if Instant::now() > self.deadline {
            return Err("storage_operation_timed_out");
        }
        if !self.active
            || storage_stage::identity(&storage_stage::open(&self.path, false).map_err(|_| FAILED)?)
                .map_err(|_| FAILED)?
                != self.identity
        {
            return Err(FAILED);
        }
        self.db.execute_batch("COMMIT").map_err(failure)?;
        self.active = false;
        Ok(())
    }
}
impl Drop for Database<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.db.progress_handler(0, None::<fn() -> bool>);
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
}
