//! Original Desktop history remains in its native profile. A reference is not
//! an empty-history snapshot and never authorizes a second session writer.
use crate::{auth, legacy_codex_headers::table, storage_stage, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
};

const ERROR: &str = "The original Desktop chat reference changed; no chat was replaced";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Reference {
    version: u32,
    thread_id: String,
    native_id: String,
    profile: PathBuf,
    transcript: String,
    first_line_sha256: String,
    first_line_bytes: u64,
}
impl Reference {
    pub(crate) fn parse(raw: &[u8]) -> Result<Self, String> {
        if raw.len() > 16384 {
            return Err(ERROR.into());
        }
        let value: Self = serde_json::from_slice(raw).map_err(|_| ERROR)?;
        if value.version != 1
            || value.thread_id.is_empty()
            || value.thread_id.len() > 128
            || value.thread_id.chars().any(char::is_control)
            || uuid::Uuid::parse_str(&value.native_id).is_err()
            || value.native_id.len() != 36
            || !value.profile.is_absolute()
            || value
                .profile
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            || value.first_line_bytes == 0
            || value.first_line_bytes > 8 * 1024 * 1024
            || value.first_line_sha256.len() != 64
            || !value
                .first_line_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ERROR.into());
        }
        Ok(value)
    }
    pub(crate) fn check(&self, profile: &Path, id: &str, native: &str) -> Result<(), String> {
        if self.profile.canonicalize().map_err(|_| ERROR)?
            != profile.canonicalize().map_err(|_| ERROR)?
            || self.thread_id != id
            || self.native_id != native
        {
            return Err(ERROR.into());
        }
        let relative = Path::new(&self.transcript);
        if self.transcript.len() > 2048
            || self.transcript.contains(['\\', ':'])
            || relative.components().count() < 2
            || !matches!(relative.components().next(),Some(Component::Normal(s)) if s=="sessions" || s=="archived_sessions")
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(ERROR.into());
        }
        let mut path = profile.to_owned();
        storage_stage::open(&path, true).map_err(|_| ERROR)?;
        let parts: Vec<_> = relative.components().collect();
        for (index, part) in parts.iter().enumerate() {
            path.push(part);
            storage_stage::open(&path, index + 1 < parts.len()).map_err(|_| ERROR)?;
        }
        let file = storage_stage::open(&path, false).map_err(|_| ERROR)?;
        if storage_stage::links(&file).map_err(|_| ERROR)? != 1 {
            return Err(ERROR.into());
        }
        // Limit the read, not the size of the native transcript. Appends and
        // large histories are allowed; identity changes and truncation are not.
        let mut input = BufReader::new(file.take(self.first_line_bytes + 1));
        let mut line = Vec::new();
        input.read_until(b'\n', &mut line).map_err(|_| ERROR)?;
        if line.len() as u64 != self.first_line_bytes
            || line.last() != Some(&b'\n')
            || format!("{:x}", Sha256::digest(&line)) != self.first_line_sha256
        {
            return Err(ERROR.into());
        }
        let row: Value = serde_json::from_slice(&line).map_err(|_| ERROR)?;
        if row["type"] != "session_meta" || row["payload"]["id"] != native {
            return Err(ERROR.into());
        }
        Ok(())
    }
    pub(crate) fn matches_receipt(
        &self,
        profile: &Path,
        id: &str,
        transcript: &str,
        hash: &str,
    ) -> Result<(), String> {
        if self.transcript != transcript || self.first_line_sha256 != hash {
            return Err(ERROR.into());
        }
        self.check(profile, id, &self.native_id)
    }
}
pub(crate) fn verify(
    db: &rusqlite::Connection,
    base: &Value,
    profile: Option<&Path>,
) -> Result<bool, String> {
    if !table(db, "legacy_desktop_reference")? {
        return Ok(false);
    }
    let id = base["thread"]["id"].as_str().ok_or(ERROR)?;
    let row: Option<(String, Vec<u8>, String)> = db.query_row(
        "SELECT source_hash,raw,phase FROM legacy_desktop_reference WHERE chat=?1 AND length(raw)<=16384", [id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_|ERROR)?;
    let Some((hash, raw, phase)) = row else {
        return Ok(false);
    };
    if phase == "rolled-back" {
        return Ok(false);
    }
    if phase != "prepared"
        || base["external_codex"] != true
        || hash != auth::digest(&base.to_string())
    {
        return Err(ERROR.into());
    }
    let reference = Reference::parse(&raw)?;
    reference.check(
        profile.unwrap_or(&reference.profile),
        id,
        base["native_id"].as_str().ok_or(ERROR)?,
    )?;
    if table(db, "legacy_codex_history")?
        && db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_codex_history WHERE chat=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(|_| ERROR)?
    {
        return Err(ERROR.into());
    }
    Ok(true)
}
pub(crate) fn rollback(db: &rusqlite::Connection, base: &Value) -> Result<(), String> {
    if verify(db, base, None)? {
        db.execute(
            "UPDATE legacy_desktop_reference SET phase='rolled-back' WHERE chat=?1",
            [base["thread"]["id"].as_str().ok_or(ERROR)?],
        )
        .map_err(|_| ERROR)?;
    }
    Ok(())
}
impl Store {
    pub(crate) fn prepare_desktop_reference(&mut self, id: &str, raw: &[u8]) -> Result<(), String> {
        let base = self.legacy_codex_base(id)?;
        if base["external_codex"] != true {
            return Err(ERROR.into());
        }
        let reference = Reference::parse(raw)?;
        reference.check(
            &reference.profile,
            id,
            base["native_id"].as_str().ok_or(ERROR)?,
        )?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_desktop_reference(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,raw BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_|ERROR)?;
        let hash = auth::digest(&base.to_string());
        let tx = self.db.transaction().map_err(|_| ERROR)?;
        let previous: Option<(String, Vec<u8>, String)> = tx
            .query_row(
                "SELECT source_hash,raw,phase FROM legacy_desktop_reference WHERE chat=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| ERROR)?;
        if let Some(old) = previous {
            if old != (hash, raw.to_vec(), "prepared".into()) {
                return Err(ERROR.into());
            }
        } else {
            tx.execute(
                "INSERT INTO legacy_desktop_reference VALUES(?1,?2,?3,'prepared')",
                params![id, hash, raw],
            )
            .map_err(|_| ERROR)?;
        }
        verify(&tx, &base, None)?;
        tx.commit().map_err(|_| ERROR)?;
        Ok(())
    }
}
