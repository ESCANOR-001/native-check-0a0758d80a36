//! Prepare exported project registries and Remodex-owned Claude switches offline.
//! No executable is launched, folder opened, or Desktop/CLI setting changed.
use crate::{auth, store::Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
const INVALID: &str =
    "Unsupported legacy project or agent settings; the original installation is unchanged";
const CONFLICT: &str = "The prepared settings changed; no saved settings were replaced";
const KEYS: [&str; 2] = ["native.projects", "native.claudeSettings"];
fn text(v: &Value, limit: usize) -> Result<&str, String> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .ok_or_else(|| INVALID.into())
}
fn boolean(v: &Value) -> Result<bool, String> {
    if v.is_null() {
        Ok(false)
    } else {
        v.as_bool().ok_or_else(|| INVALID.into())
    }
}
fn date(v: &Value) -> Result<(), String> {
    time::OffsetDateTime::parse(text(v, 64)?, &time::format_description::well_known::Rfc3339)
        .map(|_| ())
        .map_err(|_| INVALID.into())
}
fn source(raw: &[u8], limit: usize) -> Result<(String, Value), String> {
    if raw.len() > limit {
        return Err(INVALID.into());
    }
    let raw = std::str::from_utf8(raw).map_err(|_| INVALID)?;
    Ok((raw.into(), serde_json::from_str(raw).map_err(|_| INVALID)?))
}
fn connection(db: &rusqlite::Connection) -> Result<String, String> {
    let (digest, raw, environment, phase): (String, Vec<u8>, String, String) = db
        .query_row(
            "SELECT digest,raw,environment,phase FROM legacy_connection_import WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| INVALID)?;
    if phase != "prepared"
        || auth::digest(std::str::from_utf8(&raw).map_err(|_| INVALID)?) != digest
    {
        return Err(INVALID.into());
    }
    crate::legacy_connection::Prepared::parse(&raw, &environment)?;
    Ok(digest)
}
struct Prepared {
    raw: [String; 2],
    values: [String; 2],
    digest: String,
    count: usize,
    enabled: bool,
    sync: bool,
    tier_pending: usize,
}
impl Prepared {
    fn parse(claude: &[u8], codex: &[u8]) -> Result<Self, String> {
        let (c_raw, c) = source(claude, 1024 * 1024)?;
        let (x_raw, x) = source(codex, 2 * 1024 * 1024)?;
        if !c.is_object() {
            return Err(INVALID.into());
        }
        let empty = vec![];
        let cp = if c["projects"].is_null() {
            &empty
        } else {
            c["projects"].as_array().ok_or(INVALID)?
        };
        let xp = x.as_array().ok_or(INVALID)?;
        if cp.len() > 256 || xp.len() > 256 {
            return Err(INVALID.into());
        }
        let enabled = boolean(&c["enabled"])?;
        let sync = boolean(&c["desktopSync"])?;
        let binary = match c["binary"].as_str() {
            Some("") | None if c["binary"].is_null() || c["binary"] == "" => Value::Null,
            Some(s) => {
                text(&c["binary"], 2048)?;
                if !std::path::Path::new(s).is_absolute() {
                    return Err(INVALID.into());
                }
                json!(s)
            }
            _ => return Err(INVALID.into()),
        };
        let mut projects = BTreeMap::new();
        let mut tier_pending = 0;
        for (instance, rows) in [("claude-code", cp), ("codex", xp)] {
            let mut local = BTreeSet::new();
            for row in rows {
                let id = text(&row["id"], 128)?;
                if !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                    || !local.insert(id)
                {
                    return Err(INVALID.into());
                }
                if id == "codex-project-chats" {
                    // Virtual No Project is reconstructed, not a filesystem project.
                    if row["workspaceRoot"].is_null()
                        && row["defaultModelSelection"].is_null()
                        && (row["scripts"].is_null()
                            || row["scripts"].as_array().is_some_and(Vec::is_empty))
                    {
                        continue;
                    }
                    return Err(INVALID.into());
                }
                text(&row["title"], 256)?;
                let root = text(&row["workspaceRoot"], 4096)?;
                if !std::path::Path::new(root).is_absolute() {
                    return Err(INVALID.into());
                }
                date(&row["createdAt"])?;
                date(&row["updatedAt"])?;
                let scripts = if row["scripts"].is_null() {
                    json!([])
                } else {
                    crate::projects::validate_scripts(&row["scripts"])?
                };
                let default = if row["defaultModelSelection"].is_null() {
                    Value::Null
                } else {
                    let value = &row["defaultModelSelection"];
                    let provider = text(&value["instanceId"], 64)?;
                    if provider != instance && !(instance == "codex" && provider == "openai") {
                        return Err(INVALID.into());
                    }
                    let options = crate::legacy_codex::options(&value["options"])?;
                    if instance == "claude-code" && options.iter().any(|v| v["id"] == "serviceTier")
                    {
                        return Err(INVALID.into());
                    }
                    if options.iter().any(|v| v["id"] == "serviceTier") {
                        tier_pending += 1;
                    }
                    json!({"instanceId":instance,"model":text(&value["model"],256)?,"options":options})
                };
                let mut value = row.clone();
                value["defaultModelSelection"] = default;
                value["scripts"] = scripts;
                if let Some(old) = projects.insert(id.to_owned(), value.clone()) {
                    if old != value {
                        return Err("The exported agents disagree on a project ID. Keep both exports and resolve that identity before upgrading; no folder was renamed or removed.".into());
                    }
                }
            }
        }
        if projects.len() > 256 {
            return Err(INVALID.into());
        }
        let count = projects.len();
        let values = [
            serde_json::to_string(&projects.into_values().collect::<Vec<_>>())
                .map_err(|_| INVALID)?,
            json!({"enabled":enabled,"desktop_sync":sync,"binary":binary}).to_string(),
        ];
        let raw = [c_raw, x_raw];
        let digest = auth::digest(&serde_json::to_string(&raw).map_err(|_| INVALID)?);
        Ok(Self {
            raw,
            values,
            digest,
            count,
            enabled,
            sync,
            tier_pending,
        })
    }
    fn report(&self) -> Value {
        json!({"phase":"preferences-prepared","projects":self.count,"claudeEnabled":self.enabled,"claudeDesktopSync":self.sync,"serviceTierPending":self.tier_pending,"migrationComplete":false})
    }
    fn verify(&self, db: &rusqlite::Connection) -> Result<(), String> {
        for (key, value) in KEYS.iter().zip(&self.values) {
            let saved: Option<String> = db
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|_| CONFLICT)?;
            if saved.as_ref() != Some(value) {
                return Err(CONFLICT.into());
            }
        }
        Ok(())
    }
}
struct Receipt {
    digest: String,
    connection: String,
    claude: Vec<u8>,
    codex: Vec<u8>,
    phase: String,
}
fn receipt(db: &rusqlite::Connection) -> Result<Option<Receipt>, String> {
    db.query_row("SELECT digest,connection_digest,claude_raw,codex_raw,phase FROM legacy_preferences_import WHERE id=1",[],|r|Ok(Receipt {digest:r.get(0)?,connection:r.get(1)?,claude:r.get(2)?,codex:r.get(3)?,phase:r.get(4)?})).optional().map_err(|_|CONFLICT.into())
}
impl Store {
    pub fn prepare_legacy_preferences(
        &mut self,
        claude_settings: &[u8],
        codex_projects: &[u8],
    ) -> Result<Value, String> {
        let p = Prepared::parse(claude_settings, codex_projects)?;
        let binding = connection(&self.db)?;
        self.check_history(
            (claude_settings.len() + codex_projects.len()) as u64 * 4 + 1024 * 1024,
        )?;
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_preferences_import(id INTEGER PRIMARY KEY,digest TEXT NOT NULL,connection_digest TEXT NOT NULL,claude_raw BLOB NOT NULL,codex_raw BLOB NOT NULL,phase TEXT NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        if let Some(old) = receipt(&tx)? {
            if old.digest != p.digest
                || old.connection != binding
                || old.claude != claude_settings
                || old.codex != codex_projects
                || old.phase != "prepared"
            {
                return Err(CONFLICT.into());
            }
            p.verify(&tx)?;
            return Ok(p.report());
        }
        for (key, value) in KEYS.iter().zip(&p.values) {
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                params![key, value],
            )
            .map_err(|_| CONFLICT)?;
        }
        tx.execute(
            "INSERT INTO legacy_preferences_import VALUES(1,?1,?2,?3,?4,'prepared')",
            params![p.digest, binding, p.raw[0].as_bytes(), p.raw[1].as_bytes()],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(p.report())
    }
    pub fn rollback_legacy_preferences(&mut self) -> Result<Value, String> {
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let report = rollback(&tx)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(report)
    }
}

pub(crate) fn rollback(db: &rusqlite::Connection) -> Result<Value, String> {
    let binding = connection(db)?;
    let old = receipt(db)?.ok_or(CONFLICT)?;
    let p = Prepared::parse(&old.claude, &old.codex)?;
    if old.digest != p.digest || old.connection != binding {
        return Err(CONFLICT.into());
    }
    match old.phase.as_str() {
        "prepared" => {
            p.verify(db)?;
            for key in KEYS {
                db.execute("DELETE FROM metadata WHERE key=?1", [key])
                    .map_err(|_| INVALID)?;
            }
            db.execute(
                "UPDATE legacy_preferences_import SET phase='rolled-back' WHERE id=1",
                [],
            )
            .map_err(|_| INVALID)?;
        }
        "rolled-back" => {
            for key in KEYS {
                let present: bool = db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                        [key],
                        |r| r.get(0),
                    )
                    .map_err(|_| CONFLICT)?;
                if present {
                    return Err(CONFLICT.into());
                }
            }
        }
        _ => return Err(CONFLICT.into()),
    }
    Ok(json!({"phase":"rolled-back","sourceRetained":true,"migrationComplete":false}))
}
