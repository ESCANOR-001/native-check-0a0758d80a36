//! Prove omitted attachment metadata from an immutable, verified Claude export.
//! Runtime taps use bounded receipts; they never scan a native transcript.
use crate::{auth, legacy_uploads::File, profile_snapshot};
use base64::Engine;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
    path::Path,
    time::{Duration, Instant},
};
const INVALID: &str = "An old attachment has no verified native prompt evidence; history remains intact and migration stays offline";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Evidence {
    message: String,
    original: String,
    message_hash: String,
    native_hash: String,
}
pub(crate) fn storage_bytes(evidence: &[Evidence]) -> u64 {
    evidence
        .iter()
        .map(|e| {
            (e.message.len() + e.original.len() + e.message_hash.len() + e.native_hash.len() + 512)
                as u64
        })
        .sum()
}
fn table(db: &rusqlite::Connection) -> Result<bool, String> {
    crate::legacy_codex_headers::table(db, "legacy_upload_evidence")
}
fn rows(db: &rusqlite::Connection, id: &str) -> Result<Vec<Evidence>, String> {
    if !table(db)? {
        return Ok(vec![]);
    }
    let mut query=db.prepare("SELECT message,original,message_hash,native_hash FROM legacy_upload_evidence WHERE chat=?1 ORDER BY message,original").map_err(|_|INVALID)?;
    let rows = query
        .query_map([id], |r| {
            Ok(Evidence {
                message: r.get(0)?,
                original: r.get(1)?,
                message_hash: r.get(2)?,
                native_hash: r.get(3)?,
            })
        })
        .map_err(|_| INVALID)?;
    let mut result = vec![];
    for row in rows {
        if result.len() >= 2048 {
            return Err(INVALID.into());
        }
        result.push(row.map_err(|_| INVALID)?);
    }
    Ok(result)
}
pub(crate) fn save(
    db: &rusqlite::Connection,
    id: &str,
    evidence: &[Evidence],
) -> Result<(), String> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_upload_evidence(chat TEXT NOT NULL,message TEXT NOT NULL,original TEXT NOT NULL,message_hash TEXT NOT NULL,native_hash TEXT NOT NULL,PRIMARY KEY(chat,message,original));").map_err(|_|INVALID)?;
    if !rows(db, id)?.is_empty() {
        return Err(INVALID.into());
    }
    for e in evidence {
        db.execute(
            "INSERT INTO legacy_upload_evidence VALUES(?1,?2,?3,?4,?5)",
            params![id, e.message, e.original, e.message_hash, e.native_hash],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(())
}
pub(crate) fn verify(
    db: &rusqlite::Connection,
    directory: &Path,
    id: &str,
    thread: &Value,
    files: &[File],
) -> Result<(), String> {
    if rows(db, id)? != collect(db, directory, id, thread, files)? {
        return Err(INVALID.into());
    }
    Ok(())
}
pub(crate) fn allows(
    db: &rusqlite::Connection,
    id: &str,
    f: &File,
    message: &Value,
) -> Result<bool, String> {
    if !table(db)? {
        return Ok(false);
    }
    let proof:Option<(String,String,String)>=db.query_row("SELECT e.message_hash,e.native_hash,n.report FROM legacy_upload_evidence e JOIN legacy_claude_native n ON n.chat=e.chat JOIN legacy_claude_import i ON i.id=n.chat AND i.digest=n.source_hash WHERE e.chat=?1 AND e.message=?2 AND e.original=?3 AND n.phase='prepared' AND i.phase='prepared' AND length(n.report)<=2097152",params![id,f.message,f.original],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_|INVALID)?;
    Ok(proof.is_some_and(|(m, n, report)| {
        m == auth::digest(&message.to_string()) && n == auth::digest(&report)
    }))
}
pub(crate) fn collect(
    db: &rusqlite::Connection,
    directory: &Path,
    id: &str,
    thread: &Value,
    files: &[File],
) -> Result<Vec<Evidence>, String> {
    let messages: BTreeMap<_, _> = thread["messages"]
        .as_array()
        .ok_or(INVALID)?
        .iter()
        .filter_map(|m| m["id"].as_str().map(|id| (id, m)))
        .collect();
    let mut missing = vec![];
    for f in files {
        let message = messages.get(f.message.as_str()).ok_or(INVALID)?;
        if !crate::legacy_uploads::referenced(message, &f.original) {
            missing.push(f);
        }
    }
    if missing.is_empty() {
        return Ok(vec![]);
    }
    // This verifier never touches the original profile and never calls upload
    // verification, so there is no recursive validation cycle.
    crate::legacy_claude_native::verify(db, directory, id)?;
    let (bundle,transcript,report,raw):(String,String,String,Vec<u8>)=db.query_row("SELECT n.bundle,n.transcript,n.report,i.raw FROM legacy_claude_native n JOIN legacy_claude_import i ON i.id=n.chat AND i.digest=n.source_hash WHERE n.chat=?1 AND n.phase='prepared' AND i.phase='prepared' AND length(n.report)<=2097152 AND length(i.raw)<=16777216",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|INVALID)?;
    let source: Value = serde_json::from_slice(&raw).map_err(|_| INVALID)?;
    let report_value: Value = serde_json::from_str(&report).map_err(|_| INVALID)?;
    let report_hash = auth::digest(&report);
    let mut wanted: BTreeMap<String, Vec<&File>> = BTreeMap::new();
    for f in missing {
        let message = messages[f.message.as_str()];
        if message["role"] != "user" {
            return Err(INVALID.into());
        }
        let delivery = source["deliveries"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["messageId"] == f.message));
        if delivery.is_some_and(|d| d["state"] == "cancelled") {
            return Err(INVALID.into());
        }
        let reconciled = report_value["reconciled"]
            .as_array()
            .and_then(|r| r.iter().find(|b| b["message"] == f.message));
        let native = delivery
            .and_then(|d| d["uuid"].as_str())
            .or_else(|| reconciled.and_then(|b| b["native"].as_str()))
            .unwrap_or(&f.message);
        uuid::Uuid::parse_str(native).map_err(|_| INVALID)?;
        wanted.entry(native.to_owned()).or_default().push(f);
    }
    let path = directory
        .join("legacy-native")
        .join(bundle)
        .join("data")
        .join(transcript);
    let (file, meta) = profile_snapshot::open(&path, 512 * 1024 * 1024).map_err(|_| INVALID)?;
    let identity = crate::storage_stage::identity(&file).map_err(|_| INVALID)?;
    let mut input = BufReader::with_capacity(64 * 1024, file);
    let mut row = Vec::new();
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut hash = Sha256::new();
    let mut seen = BTreeSet::new();
    let mut result = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        row.clear();
        let n = input
            .by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut row)
            .map_err(|_| INVALID)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        count += 1;
        if n > 8 * 1024 * 1024
            || bytes > meta.len()
            || count > 200_000
            || row.last() != Some(&b'\n')
            || Instant::now() >= deadline
        {
            return Err(INVALID.into());
        }
        hash.update(&row);
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = serde_json::from_slice(&row).map_err(|_| INVALID)?;
        let Some(files) = v["uuid"].as_str().and_then(|u| wanted.get(u)) else {
            continue;
        };
        if v["type"] != "user"
            || v["isMeta"] == true
            || v["isSynthetic"] == true
            || v["isSidechain"] == true
            || v["message"]["role"] != "user"
            || !v["parent_tool_use_id"].is_null()
            || v["sessionId"] != source["nativeId"]
        {
            return Err(INVALID.into());
        }
        let native = v["uuid"].as_str().ok_or(INVALID)?;
        // A repeated identity must not accumulate conflicting evidence.
        if !seen.insert(native.to_owned()) {
            return Err(INVALID.into());
        }
        let blocks = v["message"]["content"].as_array().ok_or(INVALID)?;
        let first = blocks.first().ok_or(INVALID)?;
        if first["type"] != "text" || blocks[1..].iter().any(|b| b["type"] != "image") {
            return Err(INVALID.into());
        }
        let mut images = BTreeSet::new();
        for image in &blocks[1..] {
            if image["source"]["type"] != "base64" {
                return Err(INVALID.into());
            }
            let data = image["source"]["data"].as_str().ok_or(INVALID)?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| INVALID)?;
            images.insert((
                format!("{:x}", Sha256::digest(&decoded)),
                decoded.len() as u64,
            ));
        }
        for f in files {
            let message = messages[f.message.as_str()];
            let public = message["text"].as_str().ok_or(INVALID)?;
            let public = if public.is_empty() {
                "Inspect the attached files."
            } else {
                public
            };
            let native_text = first["text"].as_str().ok_or(INVALID)?;
            let attached = if native_text == public {
                None
            } else {
                Some(
                    native_text
                        .strip_prefix(&format!("{public}\n\nAttached files on this PC:\n"))
                        .ok_or(INVALID)?,
                )
            };
            let path_match = attached.is_some_and(|s| s.lines().any(|line| line == f.original));
            if !path_match && !images.contains(&(f.sha256.clone(), f.bytes)) {
                return Err(INVALID.into());
            }
            result.insert(Evidence {
                message: f.message.clone(),
                original: f.original.clone(),
                message_hash: auth::digest(&message.to_string()),
                native_hash: report_hash.clone(),
            });
        }
    }
    let after = crate::storage_stage::open(&path, false).map_err(|_| INVALID)?;
    if bytes != meta.len()
        || report_value["bytes"].as_u64() != Some(bytes)
        || report_value["sha256"] != format!("{:x}", hash.finalize())
        || crate::storage_stage::identity(&after).map_err(|_| INVALID)? != identity
        || seen.len() != wanted.len()
    {
        return Err(INVALID.into());
    }
    Ok(result.into_iter().collect())
}
