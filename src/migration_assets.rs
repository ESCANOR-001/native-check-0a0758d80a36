//! Count retained legacy upload bundles, including partial copies, in the same
//! private-asset quota as new uploads. Indexing never grants access or deletes
//! files; rollback intentionally keeps these recovery copies charged.
use crate::{storage_stage::open, store::Store};
use rusqlite::{params, Connection};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

const INVALID: &str = "Cannot verify retained migration copies; no additional files were copied";
struct Budget {
    entries: usize,
    deadline: Instant,
}
fn walk(db: &Connection, root: &Path, depth: usize, budget: &mut Budget) -> Result<(), String> {
    if depth > 64 || Instant::now() >= budget.deadline {
        return Err(INVALID.into());
    }
    open(root, true).map_err(|_| INVALID)?;
    for entry in fs::read_dir(root).map_err(|_| INVALID)? {
        budget.entries += 1;
        if budget.entries > 80_000 || Instant::now() >= budget.deadline {
            return Err(INVALID.into());
        }
        let entry = entry.map_err(|_| INVALID)?;
        let path = entry.path();
        if entry.file_type().map_err(|_| INVALID)?.is_dir() {
            walk(db, &path, depth + 1, budget)?;
        } else {
            let file = open(&path, false).map_err(|_| INVALID)?;
            let bytes =
                i64::try_from(file.metadata().map_err(|_| INVALID)?.len()).map_err(|_| INVALID)?;
            db.execute(
                "INSERT INTO storage_assets VALUES(?1,?2)",
                params![path.to_str().ok_or(INVALID)?, bytes],
            )
            .map_err(|_| INVALID)?;
        }
    }
    Ok(())
}
pub(crate) fn index(db: &Connection, directory: &Path) -> Result<(), String> {
    let root = directory.join("legacy-uploads");
    match fs::symlink_metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(INVALID.into()),
        Ok(_) => {}
    }
    walk(
        db,
        &root,
        0,
        &mut Budget {
            entries: 0,
            deadline: Instant::now() + Duration::from_secs(30),
        },
    )
}
fn prefix(path: &Path) -> Result<String, String> {
    Ok(format!(
        "{}{}",
        path.to_str().ok_or(INVALID)?,
        std::path::MAIN_SEPARATOR
    ))
}
impl Store {
    pub(crate) fn reindex_legacy_uploads(&mut self) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        tx.execute(
            "DELETE FROM storage_assets WHERE instr(path,?1)=1",
            [prefix(&self.directory.join("legacy-uploads"))?],
        )
        .map_err(|_| INVALID)?;
        index(&tx, &self.directory)?;
        tx.commit().map_err(|_| INVALID.into())
    }
    pub(crate) fn legacy_upload_bundle_usage(&self, bundle: &Path) -> Result<(u64, u64), String> {
        self.db
            .query_row(
                "SELECT COALESCE(SUM(bytes),0),COUNT(*) FROM storage_assets WHERE instr(path,?1)=1",
                [prefix(bundle)?],
                |r| {
                    let bytes = u64::try_from(r.get::<_, i64>(0)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let files = u64::try_from(r.get::<_, i64>(1)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((bytes, files))
                },
            )
            .map_err(|_| INVALID.into())
    }
}
