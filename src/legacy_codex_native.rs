//! Verify an explicit offline Codex rollout against imported public history.
//! Identities come from native completed items, never text similarity. The
//! original profile is not opened and this proof does not enable activation.
use crate::{
    auth, legacy_codex_headers::table, migration_snapshot, profile_snapshot, storage_stage::open,
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
    path::{Component, Path},
    time::{Duration, Instant},
};

const ERROR: &str = "Codex native history does not match its exported chat; original history remains intact and offline";
const MAX: u64 = 512 * 1024 * 1024;
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Item {
    turn: String,
    native: String,
    public: Option<String>,
    role: Option<String>,
    row_hash: Option<String>,
    client: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Report {
    sha256: String,
    bytes: u64,
    items: Vec<Item>,
    turns: BTreeMap<String, String>,
}
fn text(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or(ERROR.into())
}
fn fingerprint(path: &Path) -> Result<String, String> {
    let bytes = profile_snapshot::bytes(path, 8 * 1024 * 1024).map_err(|_| ERROR)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
    Ok(auth::digest(&v.to_string()))
}
fn source(db: &rusqlite::Connection, id: &str) -> Result<(String, Vec<u8>), String> {
    let (hash, raw): (String, Vec<u8>) = db.query_row("SELECT digest,raw FROM legacy_codex_history WHERE chat=?1 AND phase='prepared' AND length(raw)<=16777216", [id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|_| ERROR)?;
    if auth::digest(std::str::from_utf8(&raw).map_err(|_| ERROR)?) != hash {
        return Err(ERROR.into());
    }
    Ok((hash, raw))
}
fn inspect(data: &Path, relative: &str, raw: &[u8]) -> Result<Report, String> {
    let path = Path::new(relative);
    if relative.len() > 2048
        || relative.contains(['\\', ':'])
        || path.components().count() < 2
        || !matches!(path.components().next(),Some(Component::Normal(s)) if s=="sessions" || s=="archived_sessions")
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err(ERROR.into());
    }
    let public: Value = serde_json::from_slice(raw).map_err(|_| ERROR)?;
    let native = text(&public["nativeId"])?;
    let turns: BTreeMap<String, String> = public["turns"]
        .as_array()
        .ok_or(ERROR)?
        .iter()
        .map(|t| {
            Ok((
                text(&t["nativeId"])?.to_owned(),
                text(&t["publicId"])?.to_owned(),
            ))
        })
        .collect::<Result<_, String>>()?;
    let messages: BTreeMap<&str, &Value> = public["messages"]
        .as_array()
        .ok_or(ERROR)?
        .iter()
        .map(|m| Ok((text(&m["id"])?, m)))
        .collect::<Result<_, String>>()?;
    let mut current = data.to_owned();
    open(&current, true).map_err(|_| ERROR)?;
    let parts: Vec<_> = path.components().collect();
    for (i, part) in parts.iter().enumerate() {
        current.push(part);
        open(&current, i + 1 < parts.len()).map_err(|_| ERROR)?;
    }
    let (file, meta) = profile_snapshot::open(&current, MAX).map_err(|_| ERROR)?;
    let identity = crate::storage_stage::identity(&file).map_err(|_| ERROR)?;
    let mut input = BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::new();
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut seen = BTreeSet::new();
    let mut matched = BTreeSet::new();
    let mut completed = BTreeSet::new();
    let mut items = vec![];
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        line.clear();
        let n = input
            .by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| ERROR)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        count += 1;
        if n > 8 * 1024 * 1024
            || count > 200_000
            || bytes > meta.len()
            || line.last() != Some(&b'\n')
            || Instant::now() >= deadline
        {
            return Err(ERROR.into());
        }
        hash.update(&line);
        let v: Value = serde_json::from_slice(&line).map_err(|_| ERROR)?;
        if count == 1 {
            if v["type"] != "session_meta" || v["payload"]["id"] != native {
                return Err(ERROR.into());
            }
            continue;
        }
        if v["type"] != "event_msg" {
            continue;
        }
        let p = &v["payload"];
        if p.get("thread_id").is_some_and(|id| id != native) {
            return Err(ERROR.into());
        }
        let Some(turn) = p["turn_id"].as_str().and_then(|t| turns.get(t)) else {
            continue;
        };
        if p["type"] == "task_started" {
            completed.remove(turn);
            continue;
        }
        if matches!(p["type"].as_str(), Some("task_complete" | "turn_aborted")) {
            completed.insert(turn.clone());
            continue;
        }
        if p["type"] != "item_completed" {
            continue;
        }
        let item = &p["item"];
        let item_id = text(&item["id"])?;
        if !seen.insert((turn.clone(), item_id.to_owned())) || items.len() >= 100_000 {
            return Err(ERROR.into());
        }
        let role = match item["type"].as_str() {
            Some("UserMessage") => Some("user"),
            Some("AgentMessage") => Some("assistant"),
            _ => None,
        };
        let client = item["client_id"].as_str().filter(|s| !s.is_empty());
        let candidate = if role == Some("user") {
            client
                .and_then(|id| messages.get(id))
                .or_else(|| messages.get(item_id))
        } else {
            messages.get(item_id)
        };
        if role == Some("user") && candidate.is_none() {
            return Err(ERROR.into());
        }
        let public_id = if let Some(row) = candidate {
            if Some(text(&row["role"])?) != role
                || row["turnId"] != *turn
                || !matched.insert(text(&row["id"])?.to_owned())
            {
                return Err(ERROR.into());
            }
            if let Some(other) = client.and_then(|id| messages.get(id)) {
                if other["id"] != row["id"] {
                    return Err(ERROR.into());
                }
            }
            Some(text(&row["id"])?.to_owned())
        } else {
            None
        };
        items.push(Item {
            turn: turn.clone(),
            native: item_id.to_owned(),
            public: public_id,
            role: role.map(str::to_owned),
            row_hash: candidate.map(|m| auth::digest(&m.to_string())),
            client: client.map(str::to_owned),
        });
    }
    if bytes != meta.len()
        || matched.len() != messages.len()
        || completed != turns.values().cloned().collect()
    {
        return Err(ERROR.into());
    }
    let after = open(&current, false).map_err(|_| ERROR)?;
    if crate::storage_stage::identity(&after).map_err(|_| ERROR)? != identity {
        return Err(ERROR.into());
    }
    items.sort_by(|a, b| (&a.turn, &a.native).cmp(&(&b.turn, &b.native)));
    Ok(Report {
        sha256: format!("{:x}", hash.finalize()),
        bytes,
        items,
        turns,
    })
}
pub(crate) fn verify(db: &rusqlite::Connection, directory: &Path, id: &str) -> Result<(), String> {
    if !table(db, "legacy_codex_native")? {
        return Ok(());
    }
    let saved:Option<(String,String,String,String,String)>=db.query_row("SELECT source_hash,bundle,transcript,report,phase FROM legacy_codex_native WHERE chat=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_|ERROR)?;
    let Some((hash, bundle, transcript, raw, phase)) = saved else {
        return Ok(());
    };
    if phase == "rolled-back" {
        return Ok(());
    }
    if phase != "prepared"
        || bundle.len() != 64
        || !bundle
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || raw.len() > 16 * 1024 * 1024
    {
        return Err(ERROR.into());
    }
    let (wanted, source) = source(db, id)?;
    if hash != wanted {
        return Err(ERROR.into());
    }
    let environment: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| ERROR)?;
    let restored = directory.join("legacy-native").join(&bundle);
    migration_snapshot::verify_restored(&restored, &environment).map_err(|_| ERROR)?;
    if fingerprint(&restored.join("restored.json"))? != bundle
        || serde_json::from_str::<Report>(&raw).map_err(|_| ERROR)?
            != inspect(&restored.join("data"), &transcript, &source)?
    {
        return Err(ERROR.into());
    }
    let report: Report = serde_json::from_str(&raw).map_err(|_| ERROR)?;
    let saved_turns: BTreeMap<String, String> = db
        .prepare(
            "SELECT native,public FROM legacy_codex_native_turns WHERE chat=?1 ORDER BY native",
        )
        .map_err(|_| ERROR)?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| ERROR)?
        .collect::<Result<_, _>>()
        .map_err(|_| ERROR)?;
    let saved_items:Vec<Item>=db.prepare("SELECT turn,native,public,role,row_hash,client FROM legacy_codex_native_items WHERE chat=?1 ORDER BY turn,native").map_err(|_|ERROR)?.query_map([id],|r|Ok(Item{turn:r.get(0)?,native:r.get(1)?,public:r.get(2)?,role:r.get(3)?,row_hash:r.get(4)?,client:r.get(5)?})).map_err(|_|ERROR)?.collect::<Result<_,_>>().map_err(|_|ERROR)?;
    let native_source: Value = serde_json::from_slice(&source).map_err(|_| ERROR)?;
    let wrong_session: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM legacy_codex_native_turns WHERE chat=?1 AND session<>?2)",
            params![id, text(&native_source["nativeId"])?],
            |r| r.get(0),
        )
        .map_err(|_| ERROR)?;
    if saved_turns != report.turns || saved_items != report.items || wrong_session {
        return Err(ERROR.into());
    }
    Ok(())
}
pub(crate) fn rollback(
    db: &rusqlite::Connection,
    directory: &Path,
    id: &str,
) -> Result<(), String> {
    verify(db, directory, id)?;
    if table(db, "legacy_codex_native")? {
        db.execute("DELETE FROM legacy_codex_native_items WHERE chat=?1", [id])
            .map_err(|_| ERROR)?;
        db.execute("DELETE FROM legacy_codex_native_turns WHERE chat=?1", [id])
            .map_err(|_| ERROR)?;
        db.execute(
            "UPDATE legacy_codex_native SET phase='rolled-back' WHERE chat=?1",
            [id],
        )
        .map_err(|_| ERROR)?;
    }
    Ok(())
}
impl Store {
    /// A verified imported item must not overwrite an older public row with a
    /// new renderer's projection. Unknown items in that historical turn stop
    /// reconciliation; newer turns and ordinary chats use the normal path.
    pub fn legacy_codex_replayed_item(
        &self,
        chat: &str,
        session: &str,
        public_turn: &str,
        native_turn: &str,
        item: &Value,
    ) -> Result<bool, String> {
        if !table(&self.db, "legacy_codex_native_turns")? {
            return Ok(false);
        }
        let known:Option<(String,String)>=self.db.query_row("SELECT t.public,t.session FROM legacy_codex_native_turns t JOIN legacy_codex_native i ON i.chat=t.chat JOIN legacy_codex_history h ON h.chat=t.chat WHERE t.chat=?1 AND t.native=?2 AND i.phase='prepared' AND h.phase='prepared' AND i.source_hash=h.digest",params![chat,native_turn],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|ERROR)?;
        let Some((turn, native_session)) = known else {
            return Ok(false);
        };
        if turn != public_turn || native_session != session {
            return Err(ERROR.into());
        }
        let saved:Item=self.db.query_row("SELECT turn,native,public,role,row_hash,client FROM legacy_codex_native_items WHERE chat=?1 AND turn=?2 AND native=?3",params![chat,turn,text(&item["id"])?],|r|Ok(Item{turn:r.get(0)?,native:r.get(1)?,public:r.get(2)?,role:r.get(3)?,row_hash:r.get(4)?,client:r.get(5)?})).map_err(|_|ERROR)?;
        let role = match item["type"].as_str() {
            Some("userMessage") => Some("user"),
            Some("agentMessage") => Some("assistant"),
            _ => None,
        };
        if saved.role.as_deref() != role
            || item["clientId"]
                .as_str()
                .is_some_and(|c| Some(c) != saved.client.as_deref())
        {
            return Err(ERROR.into());
        }
        if let Some(public) = saved.public {
            let raw:String=self.db.query_row("SELECT data FROM chat_rows WHERE chat=?1 AND kind='messages' AND id=?2 AND turn=?3",params![chat,public,turn],|r|r.get(0)).map_err(|_|ERROR)?;
            if Some(auth::digest(&raw)) != saved.row_hash {
                return Err(ERROR.into());
            }
        }
        Ok(true)
    }
    pub fn prepare_legacy_codex_native(
        &mut self,
        id: &str,
        snapshot: &Path,
        transcript: &str,
    ) -> Result<Value, String> {
        let base = self.legacy_codex_base(id)?;
        crate::legacy_codex_history::verify(&self.db, &base)?;
        let (hash, source) = source(&self.db, id)?;
        let native_source: Value = serde_json::from_slice(&source).map_err(|_| ERROR)?;
        let session = text(&native_source["nativeId"])?;
        let environment = self.environment_id().map_err(|_| ERROR)?;
        let summary = migration_snapshot::verify(snapshot, &environment).map_err(|_| ERROR)?;
        let bundle = fingerprint(&snapshot.join("snapshot.json"))?;
        let parent = self.directory.join("legacy-native");
        crate::private_directory::prepare(&parent).map_err(|_| ERROR)?;
        let restored = parent.join(&bundle);
        let retained = crate::legacy_claude_native::staged_bytes(&parent)?;
        let partial = if restored.exists() {
            crate::legacy_claude_native::staged_bytes(&restored)?
        } else {
            0
        };
        let requested = retained
            .checked_sub(partial)
            .ok_or(ERROR)?
            .saturating_add(summary.bytes)
            .saturating_add(32 * 1024 * 1024);
        if requested > 2 * 1024 * 1024 * 1024 {
            return Err(ERROR.into());
        }
        self.check_history(requested)?;
        if restored.exists() {
            migration_snapshot::resume_restore(snapshot, &restored, &environment)
        } else {
            migration_snapshot::restore(snapshot, &restored, &environment)
        }
        .map_err(|_| ERROR)?;
        let report = inspect(&restored.join("data"), transcript, &source)?;
        migration_snapshot::verify_restored(&restored, &environment).map_err(|_| ERROR)?;
        let encoded = serde_json::to_string(&report).map_err(|_| ERROR)?;
        if encoded.len() > 16 * 1024 * 1024 {
            return Err(ERROR.into());
        }
        self.check_history(encoded.len() as u64 * 2 + 65536)?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_codex_native(chat TEXT PRIMARY KEY,source_hash TEXT NOT NULL,bundle TEXT NOT NULL,transcript TEXT NOT NULL,report TEXT NOT NULL,phase TEXT NOT NULL);CREATE TABLE IF NOT EXISTS legacy_codex_native_turns(chat TEXT NOT NULL,native TEXT NOT NULL,public TEXT NOT NULL,session TEXT NOT NULL,PRIMARY KEY(chat,native));CREATE TABLE IF NOT EXISTS legacy_codex_native_items(chat TEXT NOT NULL,turn TEXT NOT NULL,native TEXT NOT NULL,public TEXT,role TEXT,row_hash TEXT,client TEXT,PRIMARY KEY(chat,turn,native));").map_err(|_|ERROR)?;
        let tx = self.db.transaction().map_err(|_| ERROR)?;
        let prior:Option<(String,String,String,String,String)>=tx.query_row("SELECT source_hash,bundle,transcript,report,phase FROM legacy_codex_native WHERE chat=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_|ERROR)?;
        if let Some(prior) = prior {
            if prior != (hash, bundle, transcript.into(), encoded, "prepared".into()) {
                return Err(ERROR.into());
            }
        } else {
            for (native, public) in &report.turns {
                tx.execute(
                    "INSERT INTO legacy_codex_native_turns VALUES(?1,?2,?3,?4)",
                    params![id, native, public, session],
                )
                .map_err(|_| ERROR)?;
            }
            for item in &report.items {
                tx.execute(
                    "INSERT INTO legacy_codex_native_items VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        id,
                        item.turn,
                        item.native,
                        item.public,
                        item.role,
                        item.row_hash,
                        item.client
                    ],
                )
                .map_err(|_| ERROR)?;
            }
            tx.execute(
                "INSERT INTO legacy_codex_native VALUES(?1,?2,?3,?4,?5,'prepared')",
                params![id, hash, bundle, transcript, encoded],
            )
            .map_err(|_| ERROR)?;
        }
        verify(&tx, &self.directory, id)?;
        tx.commit().map_err(|_| ERROR)?;
        Ok(
            json!({"phase":"codex-native-history-prepared","items":report.items.len(),"turns":report.turns.len(),"bytes":report.bytes,"migrationComplete":false}),
        )
    }
}
