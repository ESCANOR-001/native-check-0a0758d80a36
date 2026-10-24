//! Metadata-only presentation of uploads omitted by an older public transcript.
//! Build descriptors offline from verified copies. Display reads never open files
//! or native transcripts, and never rewrite the historical message rows.
use crate::{auth, legacy_uploads::File, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, io::Read, path::Path};

const INVALID: &str = "Historical attachment presentation changed; original messages were retained";
const MAX_DESCRIPTORS: usize = 8;
const MAX_ROW: usize = 48 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Descriptor {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    mime_type: String,
    size_bytes: u64,
    path: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Projection {
    message_hash: String,
    descriptors: Vec<Descriptor>,
}
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    crate::legacy_codex_headers::table(db, "legacy_upload_display")
}
fn prepared(db: &rusqlite::Connection, chat: &str) -> Result<bool, String> {
    if !crate::legacy_codex_headers::table(db, "legacy_upload_display_import")? {
        return Ok(false);
    }
    let version: Option<i64> = db
        .query_row(
            "SELECT version FROM legacy_upload_display_import WHERE chat=?1",
            [chat],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| INVALID)?;
    match version {
        None => Ok(false),
        Some(1) => Ok(true),
        _ => Err(INVALID.into()),
    }
}
fn stored_message(db: &rusqlite::Connection, chat: &str, id: &str) -> Result<Value, String> {
    let raw: String = db.query_row("SELECT data FROM chat_rows WHERE chat=?1 AND kind='messages' AND id=?2 AND length(data)<=?3", params![chat,id,crate::chat_store::ROW_BYTES as i64], |r| r.get(0)).map_err(|_|INVALID)?;
    serde_json::from_str(&raw).map_err(|_| INVALID.into())
}
fn rows(db: &rusqlite::Connection, chat: &str) -> Result<BTreeMap<String, Projection>, String> {
    if !table(db)? {
        return Ok(BTreeMap::new());
    }
    let mut query = db.prepare("SELECT message,message_hash,CASE WHEN length(descriptors)<=49152 THEN descriptors END FROM legacy_upload_display WHERE chat=?1 ORDER BY message LIMIT 2049").map_err(|_|INVALID)?;
    let values = query
        .query_map([chat], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| INVALID)?;
    let mut result = BTreeMap::new();
    for value in values {
        let (id, message_hash, raw) = value.map_err(|_| INVALID)?;
        let raw = raw.ok_or(INVALID)?;
        if raw.len() > MAX_ROW || result.len() >= 2048 {
            return Err(INVALID.into());
        }
        let descriptors = serde_json::from_str(&raw).map_err(|_| INVALID)?;
        result.insert(
            id,
            Projection {
                message_hash,
                descriptors,
            },
        );
    }
    Ok(result)
}
fn expected(
    db: &rusqlite::Connection,
    directory: &Path,
    chat: &str,
    bundle: &str,
    files: &[File],
) -> Result<BTreeMap<String, Projection>, String> {
    let mut result = BTreeMap::new();
    for f in files {
        let message = stored_message(db, chat, &f.message)?;
        if crate::legacy_uploads::referenced(&message, &f.original) {
            continue;
        }
        if message["role"] != "user" || !crate::legacy_upload_native::allows(db, chat, f, &message)?
        {
            return Err(INVALID.into());
        }
        let path = directory
            .join("legacy-uploads")
            .join(bundle)
            .join("data")
            .join(&f.relative);
        let (mut file, _) =
            crate::profile_snapshot::open(&path, crate::attachments::MAX_FILE as u64)
                .map_err(|_| INVALID)?;
        let mut prefix = Vec::with_capacity(12);
        (&mut file)
            .take(12)
            .read_to_end(&mut prefix)
            .map_err(|_| INVALID)?;
        let image_mime = crate::attachments::image_mime(&prefix);
        let descriptor = Descriptor {
            kind: if image_mime.is_some() {
                "image"
            } else {
                "file"
            }
            .into(),
            name: Path::new(&f.original)
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or(INVALID)?
                .into(),
            mime_type: image_mime.unwrap_or("application/octet-stream").into(),
            size_bytes: f.bytes,
            path: f.original.clone(),
        };
        let p = result
            .entry(f.message.clone())
            .or_insert_with(|| Projection {
                message_hash: auth::digest(&message.to_string()),
                descriptors: Vec::new(),
            });
        let existing = message["attachments"].as_array().map_or(0, Vec::len);
        if p.descriptors.len() + existing >= MAX_DESCRIPTORS {
            return Err(INVALID.into());
        }
        p.descriptors.push(descriptor);
        if serde_json::to_vec(&p.descriptors)
            .map_err(|_| INVALID)?
            .len()
            > MAX_ROW
        {
            return Err(INVALID.into());
        }
    }
    Ok(result)
}
pub(crate) fn prepare(
    db: &rusqlite::Connection,
    directory: &Path,
    chat: &str,
    bundle: &str,
    files: &[File],
) -> Result<(), String> {
    let expected = expected(db, directory, chat, bundle, files)?;
    let existing = rows(db, chat)?;
    if prepared(db, chat)? {
        return if existing == expected {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    }
    if !existing.is_empty() {
        return Err(INVALID.into());
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_upload_display_import(chat TEXT PRIMARY KEY,version INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS legacy_upload_display(chat TEXT NOT NULL,message TEXT NOT NULL,message_hash TEXT NOT NULL,descriptors TEXT NOT NULL,PRIMARY KEY(chat,message));").map_err(|_|INVALID)?;
    db.execute(
        "INSERT INTO legacy_upload_display_import VALUES(?1,1)",
        [chat],
    )
    .map_err(|_| INVALID)?;
    for (id, p) in expected {
        let raw = serde_json::to_string(&p.descriptors).map_err(|_| INVALID)?;
        db.execute(
            "INSERT INTO legacy_upload_display VALUES(?1,?2,?3,?4)",
            params![chat, id, p.message_hash, raw],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(())
}
pub(crate) fn additional_bytes(
    db: &rusqlite::Connection,
    directory: &Path,
    chat: &str,
    bundle: &str,
    files: &[File],
) -> Result<u64, String> {
    let expected = expected(db, directory, chat, bundle, files)?;
    let existing = rows(db, chat)?;
    if prepared(db, chat)? {
        return if existing == expected {
            Ok(0)
        } else {
            Err(INVALID.into())
        };
    }
    if !existing.is_empty() {
        return Err(INVALID.into());
    }
    expected.iter().try_fold(0u64, |total, (id, p)| {
        let bytes = serde_json::to_vec(&p.descriptors)
            .map_err(|_| INVALID)?
            .len();
        Ok(total.saturating_add((bytes + id.len() + p.message_hash.len() + 512) as u64))
    })
}
pub(crate) fn verify(
    db: &rusqlite::Connection,
    directory: &Path,
    chat: &str,
    bundle: &str,
    files: &[File],
) -> Result<(), String> {
    // Earlier staged exports have no display receipts yet. Preparing them again
    // creates verified metadata; absence does not grant resource access.
    if !prepared(db, chat)? {
        return if rows(db, chat)?.is_empty() {
            Ok(())
        } else {
            Err(INVALID.into())
        };
    }
    if rows(db, chat)? != expected(db, directory, chat, bundle, files)? {
        return Err(INVALID.into());
    }
    Ok(())
}
impl Store {
    /// Only for an outgoing page/snapshot, never the mutable worker's history.
    pub fn present_legacy_uploads(&self, chat: &str, message: &mut Value) -> Result<(), String> {
        if message["role"] != "user"
            || !table(&self.db)?
            || !prepared(&self.db, chat)?
            || !crate::legacy_codex_headers::table(&self.db, "legacy_claude_import")?
        {
            return Ok(());
        }
        let Some(id) = message["id"].as_str() else {
            return Ok(());
        };
        let row: Option<(String,Option<String>)> = self.db.query_row("SELECT d.message_hash,CASE WHEN length(d.descriptors)<=?3 THEN d.descriptors END FROM legacy_upload_display d JOIN legacy_uploads u ON u.chat=d.chat AND u.phase='prepared' JOIN legacy_claude_import c ON c.id=u.chat AND c.digest=u.source_hash AND c.phase='prepared' WHERE d.chat=?1 AND d.message=?2",params![chat,id,MAX_ROW as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|INVALID)?;
        let Some((hash, raw)) = row else {
            return Ok(());
        };
        let raw = raw.ok_or(INVALID)?;
        let canonical = stored_message(&self.db, chat, id)?;
        if hash != auth::digest(&canonical.to_string())
            || message["text"] != canonical["text"]
            || message["turnId"] != canonical["turnId"]
        {
            return Err(INVALID.into());
        }
        let descriptors: Vec<Descriptor> = serde_json::from_str(&raw).map_err(|_| INVALID)?;
        if descriptors.len() > MAX_DESCRIPTORS {
            return Err(INVALID.into());
        }
        let mut attachments = message["attachments"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for d in descriptors {
            if d.path.len() > 4096
                || !Path::new(&d.path).is_absolute()
                || d.name.is_empty()
                || d.name.len() > 256
                || d.mime_type.len() > 100
                || !matches!(d.kind.as_str(), "file" | "image")
                || (d.kind == "image"
                    && !matches!(
                        d.mime_type.as_str(),
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ))
                || (d.kind == "file" && d.mime_type != "application/octet-stream")
                || d.path.chars().any(char::is_control)
                || Path::new(&d.path).file_name().and_then(|n| n.to_str()) != Some(d.name.as_str())
                || d.size_bytes == 0
                || d.size_bytes > crate::attachments::MAX_FILE as u64
            {
                return Err(INVALID.into());
            }
            if crate::legacy_uploads::referenced(message, &d.path)
                || attachments.iter().any(|a| a["path"] == d.path)
            {
                continue;
            }
            attachments.push(serde_json::to_value(d).map_err(|_| INVALID)?);
        }
        if attachments.len() > MAX_DESCRIPTORS {
            return Err(INVALID.into());
        }
        if !attachments.is_empty() {
            message["attachments"] = Value::Array(attachments)
        }
        Ok(())
    }
}
