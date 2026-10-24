//! Complete, bounded original-owner history capture. An unfinished file is
//! retained for diagnosis but never has the completion receipt an importer
//! requires. No session is resumed, claimed, forked, interrupted or relaunched.
use crate::desktop_ipc::{Client, Owner};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

const ERROR: &str = "Desktop history is incomplete or changed during export; the original chat is intact and was not taken over";
const PAGE: usize = 32 * 1024 * 1024;
const FIELD: usize = 8 * 1024 * 1024;
const TOTAL: usize = 512 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

fn string(v: &Value, limit: usize) -> Result<&str, String> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .ok_or_else(|| ERROR.into())
}
fn state(v: &Value) -> Result<&Value, String> {
    v.get("conversationState")
        .or_else(|| v.get("conversation_state"))
        .or_else(|| v.get("state"))
        .filter(|v| v.is_object())
        .ok_or_else(|| ERROR.into())
}
fn settled(v: &Value) -> Result<(), String> {
    if !v["requests"].as_array().is_some_and(|a| a.is_empty())
        || v["isRunning"] == true
        || v["isWaitingForResponse"] == true
        || !v["turns"].as_array().is_some_and(|a| {
            a.len() <= 512
                && a.iter().all(|t| {
                    matches!(
                        t["status"].as_str(),
                        Some("completed" | "interrupted" | "failed")
                    )
                })
        })
    {
        return Err(ERROR.into());
    }
    Ok(())
}
fn page(v: &Value) -> Result<(&str, Option<&str>), String> {
    let p = &v["historyPage"];
    if p["kind"] != "desktop-history-page"
        || p["order"] != "oldest-to-newest"
        || p["itemsOmitted"] != 0
        || p["completeOrExplicitlyPartial"] != true
        || !p["hasOlder"].is_boolean()
        || !p["hasNewer"].is_boolean()
    {
        return Err(ERROR.into());
    }
    let revision = string(&p["sourceRevision"], 256)?;
    let older = if p["hasOlder"] == true {
        Some(string(&p["olderPageToken"], 8192)?)
    } else if p["olderPageToken"].is_null() {
        None
    } else {
        return Err(ERROR.into());
    };
    Ok((revision, older))
}

async fn content(
    client: &mut Client,
    owner: &Owner,
    reference: &Value,
    revision: &str,
) -> Result<Value, String> {
    let handle = string(&reference["handle"], 256)?;
    let total = reference["byteLength"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= FIELD as u64)
        .ok_or(ERROR)? as usize;
    let media = string(&reference["mediaType"], 128)?;
    if reference["truncatedInline"] != true || !matches!(media, "text/plain" | "application/json") {
        return Err(ERROR.into());
    }
    let mut bytes = Vec::with_capacity(total);
    while bytes.len() < total {
        let offset = bytes.len();
        let v = client
            .content(owner, handle, revision, offset as u64)
            .await?;
        let encoded = v["data"]
            .as_str()
            .filter(|s| s.len() <= CHUNK.div_ceil(3) * 4)
            .ok_or(ERROR)?;
        let part = STANDARD.decode(encoded).map_err(|_| ERROR)?;
        let next = offset
            .checked_add(part.len())
            .filter(|n| *n <= total)
            .ok_or(ERROR)?;
        if v["kind"] != "chunk"
            || v["handle"] != handle
            || v["offset"] != offset
            || v["encoding"] != "base64"
            || v["mediaType"] != media
            || v["totalByteLength"] != total
            || v["chunkByteLength"] != part.len()
            || part.is_empty()
            || part.len() > CHUNK
            || v["nextOffset"] != next
            || v["complete"] != (next == total)
        {
            return Err(ERROR.into());
        }
        bytes.extend_from_slice(&part);
    }
    if media == "application/json" {
        serde_json::from_slice(&bytes).map_err(|_| ERROR.into())
    } else {
        String::from_utf8(bytes)
            .map(Value::String)
            .map_err(|_| ERROR.into())
    }
}

fn expand<'a>(
    client: &'a mut Client,
    owner: &'a Owner,
    value: &'a mut Value,
    revision: &'a str,
    budget: &'a mut usize,
    depth: usize,
) -> futures_util::future::BoxFuture<'a, Result<(), String>> {
    Box::pin(async move {
        if depth > 48 {
            return Err(ERROR.into());
        }
        if value["kind"] == "missing" {
            return Err(ERROR.into());
        }
        if value["kind"] == "handle" {
            let size = value["byteLength"]
                .as_u64()
                .filter(|n| *n <= FIELD as u64)
                .ok_or(ERROR)? as usize;
            *budget = budget
                .checked_add(size)
                .filter(|n| *n <= PAGE)
                .ok_or(ERROR)?;
            *value = content(client, owner, value, revision).await?;
            // A JSON handle can contain further handles; enforce the same
            // recursion and byte budget rather than publishing its preview.
            return expand(client, owner, value, revision, budget, depth + 1).await;
        }
        match value {
            Value::Array(a) => {
                for child in a {
                    expand(client, owner, child, revision, budget, depth + 1).await?;
                }
            }
            Value::Object(o) => {
                for child in o.values_mut() {
                    expand(client, owner, child, revision, budget, depth + 1).await?;
                }
            }
            _ => {}
        }
        Ok(())
    })
}

/// Export pages in newest-page-first order; turns within each page retain
/// Desktop order. The completion row pins both the ordered byte digest and
/// source revision. Failed exports are never treated as complete or retried
/// into the same file, and no original profile is written.
pub async fn capture(
    client: &mut Client,
    native: &str,
    destination: &Path,
) -> Result<Value, String> {
    let owner = client.owner(native).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(destination).await.map_err(|_| ERROR)?;
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut hash = Sha256::new();
        let mut bytes = 0usize;
        let mut pages = 0usize;
        let mut turns = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        let mut token = None;
        let mut revision = String::new();
        loop {
            let value = client.history(&owner, if pages == 0 { "recent" } else { "older" }, token.as_deref()).await?;
            let mut state = state(&value)?.clone();
            settled(&state)?;
            let (source, older) = page(&state)?;
            if pages == 0 { revision = source.into(); }
            else if source != revision { return Err(ERROR.to_owned()); }
            token = older.map(str::to_owned);
            if token.as_ref().is_some_and(|t| !tokens.insert(t.clone())) { return Err(ERROR.into()); }
            let mut budget = state.to_string().len();
            if budget > PAGE { return Err(ERROR.into()); }
            expand(client, &owner, &mut state, &revision, &mut budget, 0).await?;
            for turn in state["turns"].as_array().ok_or(ERROR)? {
                let id = string(turn.get("turnId").unwrap_or(&turn["id"]), 256)?;
                if !turns.insert(id.to_owned()) || turns.len() > 100_000 { return Err(ERROR.into()); }
            }
            let mut row = serde_json::to_vec(&json!({"type":"page","page":pages,"state":state})).map_err(|_| ERROR)?;
            row.push(b'\n');
            bytes = bytes.checked_add(row.len()).filter(|n| *n <= TOTAL).ok_or(ERROR)?;
            if row.len() > PAGE || pages >= 10_000 { return Err(ERROR.into()); }
            file.write_all(&row).await.map_err(|_| ERROR)?;
            hash.update(&row);
            pages += 1;
            if token.is_none() { break; }
        }
        // A last fresh read detects a task starting, an edit or an owner
        // revision change while older pages/content were being downloaded.
        let fresh = client.history(&owner, "recent", None).await?;
        let fresh = state(&fresh)?;
        settled(fresh)?;
        if page(fresh)?.0 != revision { return Err(ERROR.to_owned()); }
        let receipt = json!({"type":"complete","version":1,"nativeId":native,"sourceRevision":revision,
            "pages":pages,"turns":turns.len(),"bytes":bytes,"sha256":format!("{:x}", hash.finalize())});
        file.write_all(format!("{receipt}\n").as_bytes()).await.map_err(|_| ERROR)?;
        file.sync_all().await.map_err(|_| ERROR)?;
        Ok(receipt)
    }).await.map_err(|_| ERROR)?
}
