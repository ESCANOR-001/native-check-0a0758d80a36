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
/// Captured before/after images of the global memory job. Recovery restores it
/// only while the current image still belongs to this cleanup operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationChange {
    before: Rows,
    after: Rows,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub kind: Kind,
    filename: String,
    identity: Identity,
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
    /// Refuse an archive selection while any selected history is active, pinned,
    /// paginated or referenced across the deletion boundary. Runs inside this
    /// write transaction so another writer cannot change the checked rows.
    pub fn cleanup_ids(&self, logical_paths: &[String]) -> Result<Vec<String>> {
        if !self.active || self.kind != Kind::State || logical_paths.len() > 10_000 {
            return Err(FAILED);
        }
        let selected: BTreeSet<_> = logical_paths
            .iter()
            .map(|p| {
                crate::storage_preview::archived(p)
                    .filter(|n| n == p)
                    .ok_or(FAILED)
            })
            .collect::<Result<_>>()?;
        if selected.len() != logical_paths.len() {
            return Err(FAILED);
        }
        let Some((columns, _)) = self.schema(Table::Threads)? else {
            return Err(FAILED);
        };
        if !columns.iter().any(|c| c == "id") || !columns.iter().any(|c| c == "rollout_path") {
            return Err(FAILED);
        }
        let optional = |name: &str| {
            if columns.iter().any(|c| c == name) {
                quote(name)
            } else {
                "NULL".to_owned()
            }
        };
        let sql = format!(
            "SELECT id,rollout_path,{},{},{} FROM threads LIMIT {}",
            optional("archived"),
            optional("is_pinned"),
            optional("history_mode"),
            MAX_ROWS + 1
        );
        let mut query = self.db.prepare(&sql).map_err(failure)?;
        let mut rows = query.query([]).map_err(failure)?;
        let mut ids = BTreeSet::new();
        let mut count = 0;
        let mut budget = Budget { bytes: 0, rows: 0 };
        while let Some(row) = rows.next().map_err(failure)? {
            count += 1;
            if count > MAX_ROWS {
                return Err("storage_backup_too_large");
            }
            let path: String = row.get(1).map_err(failure)?;
            budget.add(path.len())?;
            if path.len() > 16_384 {
                return Err(FAILED);
            }
            let Some(path) = crate::storage_preview::logical_pin(self.lease.home(), &path) else {
                continue;
            };
            if !selected.contains(&path) {
                continue;
            }
            let id: String = row.get(0).map_err(failure)?;
            if id.is_empty() || id.len() > 256 {
                return Err(FAILED);
            }
            if columns.iter().any(|c| c == "archived")
                && row.get::<_, Option<i64>>(2).map_err(failure)? != Some(1)
            {
                return Err("referenced_history");
            }
            if row.get::<_, Option<i64>>(3).map_err(failure)? == Some(1) {
                return Err("pinned_thread");
            }
            if row
                .get::<_, Option<String>>(4)
                .map_err(failure)?
                .is_some_and(|s| s.eq_ignore_ascii_case("paginated"))
            {
                return Err("referenced_history");
            }
            ids.insert(id);
        }
        drop(rows);
        drop(query);
        let ids_vec: Vec<_> = ids.iter().cloned().collect();
        if ids.is_empty() {
            return Ok(ids_vec);
        }
        if let Some(edges) = self.capture(Table::SpawnEdges, &ids_vec)? {
            let parent = edges
                .columns
                .iter()
                .position(|s| s == "parent_thread_id")
                .ok_or(FAILED)?;
            let child = edges
                .columns
                .iter()
                .position(|s| s == "child_thread_id")
                .ok_or(FAILED)?;
            for edge in edges.rows {
                for index in [parent, child] {
                    match &edge[index] {
                        Cell::Text(s) if ids.contains(s) => {}
                        _ => return Err("referenced_history"),
                    }
                }
            }
        }
        for column in ["forked_from_id", "parent_thread_id", "source_thread_id"] {
            if !columns.iter().any(|s| s == column) {
                continue;
            }
            for chunk in ids_vec.chunks(200) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let sql = format!(
                    "SELECT id FROM threads WHERE {} IN ({placeholders}) LIMIT {}",
                    quote(column),
                    MAX_ROWS + 1
                );
                let mut q = self.db.prepare(&sql).map_err(failure)?;
                let mut rows = q.query(params_from_iter(chunk)).map_err(failure)?;
                let mut count = 0;
                while let Some(row) = rows.next().map_err(failure)? {
                    count += 1;
                    let id: String = row.get(0).map_err(failure)?;
                    budget.add(id.len())?;
                    if count > MAX_ROWS {
                        return Err("storage_backup_too_large");
                    }
                    if !ids.contains(&id) {
                        return Err("referenced_history");
                    }
                }
            }
        }
        Ok(ids_vec)
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
    fn global_job(&self) -> Result<Rows> {
        let (columns, keys) = self.schema(Table::Jobs)?.ok_or(FAILED)?;
        if keys != ["kind", "job_key"] {
            return Err("storage_schema_not_supported");
        }
        let mut out = Rows {
            table: Table::Jobs,
            columns,
            keys,
            rows: vec![],
        };
        let sql = format!(
            "SELECT {} FROM jobs WHERE kind='memory_consolidate_global' AND job_key='global'",
            out.columns
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut q = self.db.prepare(&sql).map_err(failure)?;
        let mut rows = q.query([]).map_err(failure)?;
        let mut budget = Budget { bytes: 0, rows: 0 };
        while let Some(row) = rows.next().map_err(failure)? {
            if !out.rows.is_empty() {
                return Err(FAILED);
            }
            let mut cells = Vec::new();
            for i in 0..out.columns.len() {
                cells.push(Cell::capture(
                    row.get_ref(i).map_err(failure)?,
                    &mut budget,
                )?);
            }
            out.rows.push(cells);
        }
        Ok(out)
    }
    /// Refresh the global memory job inside this uncommitted transaction, only
    /// when cleanup removes inputs that were selected for consolidation. The
    /// coordinator must persist both row images before committing any database.
    pub fn refresh_consolidation(&mut self, inputs: &Rows) -> Result<Option<ConsolidationChange>> {
        let result = self.refresh_consolidation_inner(inputs);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn refresh_consolidation_inner(&self, inputs: &Rows) -> Result<Option<ConsolidationChange>> {
        if !self.active || self.kind != Kind::Memories || inputs.table != Table::Stage1 {
            return Err(FAILED);
        }
        self.validate(inputs)?;
        let Some(index) = inputs
            .columns
            .iter()
            .position(|s| s == "selected_for_phase2")
        else {
            return Ok(None);
        };
        let mut selected = false;
        for row in &inputs.rows {
            match row[index] {
                Cell::Integer(n) => selected |= n != 0,
                Cell::Null => {}
                _ => return Err(FAILED),
            }
        }
        if !selected || self.schema(Table::Jobs)?.is_none() {
            return Ok(None);
        }
        let before = self.global_job()?;
        let now = crate::auth::now() / 1000;
        // Preserve the running worker and its lease. Only its input watermark
        // and retry budget change; a newer worker must not be erased on restore.
        self.db.execute("INSERT INTO jobs (
            kind,job_key,status,worker_id,ownership_token,started_at,finished_at,
            lease_until,retry_at,retry_remaining,last_error,input_watermark,last_success_watermark
        ) VALUES ('memory_consolidate_global','global','pending',NULL,NULL,NULL,NULL,NULL,NULL,3,NULL,?1,0)
        ON CONFLICT(kind,job_key) DO UPDATE SET
            status=CASE WHEN jobs.status='running' THEN 'running' ELSE 'pending' END,
            retry_at=CASE WHEN jobs.status='running' THEN jobs.retry_at ELSE NULL END,
            retry_remaining=max(jobs.retry_remaining,excluded.retry_remaining),
            input_watermark=CASE WHEN excluded.input_watermark>COALESCE(jobs.input_watermark,0)
                THEN excluded.input_watermark ELSE COALESCE(jobs.input_watermark,0)+1 END",[now]).map_err(failure)?;
        let after = self.global_job()?;
        // Refuse overflow/coercion rather than persisting an imprecise watermark.
        let watermark = after
            .columns
            .iter()
            .position(|s| s == "input_watermark")
            .ok_or(FAILED)?;
        if !matches!(
            after.rows.first().and_then(|r| r.get(watermark)),
            Some(Cell::Integer(_))
        ) {
            return Err(FAILED);
        }
        Ok(Some(ConsolidationChange { before, after }))
    }
    /// Returns false when a newer job is deliberately retained. Other per-thread
    /// rows can still be restored; this is not permission to overwrite that job.
    pub fn restore_consolidation(&mut self, change: &ConsolidationChange) -> Result<bool> {
        let result = self.restore_consolidation_inner(change);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn restore_consolidation_inner(&self, change: &ConsolidationChange) -> Result<bool> {
        if !self.active || self.kind != Kind::Memories {
            return Err(FAILED);
        }
        for rows in [&change.before, &change.after] {
            if rows.table != Table::Jobs || rows.len() > 1 {
                return Err(FAILED);
            }
            self.validate(rows)?;
            let kind = rows
                .columns
                .iter()
                .position(|s| s == "kind")
                .ok_or(FAILED)?;
            let key = rows
                .columns
                .iter()
                .position(|s| s == "job_key")
                .ok_or(FAILED)?;
            for row in &rows.rows {
                if row[kind] != Cell::Text("memory_consolidate_global".into())
                    || row[key] != Cell::Text("global".into())
                {
                    return Err(FAILED);
                }
            }
        }
        if change.after.len() != 1 {
            return Err(FAILED);
        }
        let current = self.global_job()?;
        if current.rows == change.before.rows {
            return Ok(true);
        }
        if current.rows != change.after.rows {
            return Ok(false);
        }
        self.delete_inner(&change.after)?;
        self.restore_inner(&change.before)?;
        Ok(true)
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
            self.abort();
        }
        result
    }
    pub(crate) fn binding(&self) -> Result<Binding> {
        Ok(Binding {
            kind: self.kind,
            filename: self
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or(FAILED)?
                .to_owned(),
            identity: self.identity.clone(),
        })
    }
    pub(crate) fn check_restore(&self, backup: &Rows) -> Result<()> {
        let keys = self.validate(backup)?;
        for row in &backup.rows {
            if self
                .current(backup, row, &keys)?
                .is_some_and(|current| current != *row)
            {
                return Err("storage_restore_conflict");
            }
        }
        Ok(())
    }
    fn abort(&mut self) {
        let _ = self.db.progress_handler(0, None::<fn() -> bool>);
        let _ = self.db.execute_batch("ROLLBACK");
        self.active = false;
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
