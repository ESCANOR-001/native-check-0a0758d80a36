//! Recover older prompt aliases from native reply-parent identities. Text alone
//! is never an identity: repeated prompts, missing parents and multiple inputs
//! in one public turn cannot be guessed into a binding.
use crate::{auth, claude_turns, profile_snapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{BufRead, BufReader, Read},
    path::Path,
    time::{Duration, Instant},
};
const INVALID: &str = "An older Claude prompt has no unambiguous native reply-parent identity; the original history remains intact and offline";
/// SDK replies use the API message ID; imported/remote history can use the
/// transcript event UUID. Both are recorded native identities, never text keys.
pub(crate) fn native_reply_ids(row: &Value) -> impl Iterator<Item = &str> {
    [
        row["message"]["id"].as_str(),
        row["uuid"]
            .as_str()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok()),
    ]
    .into_iter()
    .flatten()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub native: String,
    pub message: String,
    pub turn: String,
}
pub(crate) struct Reconciled {
    pub inputs: Vec<Binding>,
    pub summaries: Vec<String>,
}
fn root(
    start: &str,
    parents: &HashMap<String, Option<String>>,
    users: &BTreeMap<String, Option<String>>,
    cache: &mut HashMap<String, String>,
) -> Result<String, String> {
    let mut node = start.to_owned();
    let mut walked = BTreeSet::new();
    let result = loop {
        if users.contains_key(&node) {
            break node;
        }
        if let Some(saved) = cache.get(&node) {
            break saved.clone();
        }
        if !walked.insert(node.clone()) || walked.len() > 200_000 {
            return Err(INVALID.into());
        }
        node = parents.get(&node).and_then(Clone::clone).ok_or(INVALID)?;
    };
    for node in walked {
        cache.insert(node, result.clone());
    }
    Ok(result)
}
pub(crate) fn reconcile(path: &Path, source: &Value) -> Result<Reconciled, String> {
    let messages = source["thread"]["messages"].as_array().ok_or(INVALID)?;
    let deliveries = source["deliveries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let known: BTreeSet<_> = deliveries
        .iter()
        .filter_map(|d| d["messageId"].as_str())
        .collect();
    let pending: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "user" && !known.contains(m["id"].as_str().unwrap_or("")))
        .collect();
    if pending.is_empty() {
        return Ok(Reconciled {
            inputs: vec![],
            summaries: vec![],
        });
    }
    let wanted: BTreeSet<_> = messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["id"].as_str())
        .collect();
    let mut user_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut reply_ids: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for message in messages {
        let Some(turn) = message["turnId"].as_str() else {
            continue;
        };
        if message["role"] == "user" {
            *user_counts.entry(turn).or_default() += 1;
        }
        if message["role"] == "assistant" {
            reply_ids
                .entry(turn)
                .or_default()
                .push(message["id"].as_str().ok_or(INVALID)?);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let (file, meta) = profile_snapshot::open(path, 512 * 1024 * 1024).map_err(|_| INVALID)?;
    let mut input = BufReader::with_capacity(64 * 1024, file);
    let mut row = Vec::new();
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut parents = HashMap::new();
    let mut users = BTreeMap::new();
    let mut summaries = BTreeMap::new();
    let mut anchors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
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
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = serde_json::from_slice(&row).map_err(|_| INVALID)?;
        if !v.is_object() || (v.get("sessionId").is_some() && v["sessionId"] != source["nativeId"])
        {
            return Err(INVALID.into());
        }
        if v["isSidechain"] == true {
            continue;
        }
        let Some(uuid) = v["uuid"].as_str() else {
            continue;
        };
        uuid::Uuid::parse_str(uuid).map_err(|_| INVALID)?;
        let parent = if v["parentUuid"].is_null() {
            None
        } else {
            let p = v["parentUuid"].as_str().ok_or(INVALID)?;
            uuid::Uuid::parse_str(p).map_err(|_| INVALID)?;
            Some(p.to_owned())
        };
        if parents
            .insert(uuid.to_owned(), parent.clone())
            .is_some_and(|old| old != parent)
        {
            return Err(INVALID.into());
        }
        // Older SDK history omitted this flag and exposed a native compaction
        // summary as a user bubble. Preserve that saved history, but never
        // invent an input/delivery binding for it. Require exact native UUID
        // and content; summary-like human text alone is not evidence.
        if v["type"] == "user"
            && v["message"]["role"] == "user"
            && v["isCompactSummary"] == true
            && v["parent_tool_use_id"].is_null()
        {
            let content = &v["message"]["content"];
            let text = content
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    content
                        .as_array()
                        .filter(|a| {
                            !a.is_empty()
                                && a.iter()
                                    .all(|b| b["type"] == "text" && b["text"].is_string())
                        })
                        .map(|a| {
                            a.iter()
                                .map(|b| b["text"].as_str().unwrap())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                })
                .ok_or(INVALID)?;
            let digest = auth::digest(&text);
            if summaries
                .insert(uuid.to_owned(), digest.clone())
                .is_some_and(|old| old != digest)
            {
                return Err(INVALID.into());
            }
        }
        if let Some((id, text)) = claude_turns::user_input(&v)? {
            let content = &v["message"]["content"];
            let plain = v["isMeta"] != true
                && v["isSynthetic"] != true
                && (content.is_string()
                    || content
                        .as_array()
                        .is_some_and(|a| a.iter().all(|b| b["type"] == "text")));
            let digest = plain.then(|| auth::digest(&text));
            if users
                .insert(id, digest.clone())
                .is_some_and(|old| old != digest)
            {
                return Err(INVALID.into());
            }
        }
        if v["type"] == "assistant" && v["message"]["role"] == "assistant" {
            for id in native_reply_ids(&v).filter(|id| wanted.contains(id)) {
                anchors
                    .entry(id.to_owned())
                    .or_default()
                    .insert(uuid.to_owned());
            }
        }
    }
    if bytes != meta.len() {
        return Err(INVALID.into());
    }
    let mut cache = HashMap::new();
    let mut used: BTreeSet<String> = deliveries
        .iter()
        .filter_map(|d| d["uuid"].as_str().map(str::to_owned))
        .collect();
    let mut result = Vec::new();
    let mut preserved_summaries = Vec::new();
    for message in pending {
        if Instant::now() >= deadline {
            return Err(INVALID.into());
        }
        let id = message["id"].as_str().ok_or(INVALID)?;
        if let Some(digest) = summaries.get(id) {
            if users.contains_key(id)
                || digest != &auth::digest(message["text"].as_str().ok_or(INVALID)?)
                || !used.insert(id.to_owned())
                || !(message["attachments"].is_null()
                    || message["attachments"].as_array().is_some_and(Vec::is_empty))
            {
                return Err(INVALID.into());
            }
            preserved_summaries.push(id.to_owned());
            continue;
        }
        let turn = message["turnId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or(INVALID)?;
        if users.contains_key(id) {
            if !used.insert(id.to_owned()) {
                return Err(INVALID.into());
            }
            result.push(Binding {
                native: id.to_owned(),
                message: id.to_owned(),
                turn: turn.to_owned(),
            });
            continue;
        }
        // Several inputs in one turn include Steer. Without the original
        // delivery ledger their individual native identities are not provable.
        if user_counts.get(turn) != Some(&1)
            || !(message["attachments"].is_null()
                || message["attachments"]
                    .as_array()
                    .is_some_and(|a| a.is_empty()))
        {
            return Err(INVALID.into());
        }
        let mut roots = BTreeSet::new();
        for reply in reply_ids.get(turn).ok_or(INVALID)? {
            let nodes = anchors.get(*reply).ok_or(INVALID)?;
            for node in nodes {
                roots.insert(root(node, &parents, &users, &mut cache)?);
            }
        }
        if roots.len() != 1 {
            return Err(INVALID.into());
        }
        let native = roots.into_iter().next().unwrap();
        let text = message["text"].as_str().ok_or(INVALID)?;
        if users.get(&native).and_then(Option::as_deref) != Some(auth::digest(text).as_str())
            || !used.insert(native.clone())
        {
            return Err(INVALID.into());
        }
        result.push(Binding {
            native,
            message: id.to_owned(),
            turn: turn.to_owned(),
        });
    }
    result.sort_by(|a, b| a.native.cmp(&b.native));
    preserved_summaries.sort();
    Ok(Reconciled {
        inputs: result,
        summaries: preserved_summaries,
    })
}
