//! Offline connection-state migration. No source file is opened or modified here.
//! A prepared profile cannot serve until the remaining history/assets migration
//! is implemented and validated. Keep the original JSON for lossless rollback.
use crate::{
    auth,
    store::{Client, Store},
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_TIMESTAMP: i64 = 253_402_300_799_999;
const INVALID: &str =
    "Invalid or unsupported legacy connection state; source and destination are unchanged";
const CONFLICT: &str = "Destination is not an empty migration profile; no data was replaced";
const KEYS: [&str; 5] = [
    "environment_id",
    "phone_control_enabled",
    "keep_awake",
    "local_network_enabled",
    "quick_tunnel_enabled",
];

// Deliberately not Debug/Serialize: raw credential digests and legacy data must
// never escape through logs or an ordinary dashboard response.
pub struct Prepared {
    raw: String,
    hash: String,
    environment: String,
    clients: Vec<Client>,
    preferences: [bool; 4],
    aliases: usize,
    selections: usize,
}
fn text(v: &Value, max: usize) -> Result<&str, &'static str> {
    v.as_str()
        .filter(|s| {
            !s.is_empty() && s.len() <= max && s.trim() == *s && !s.chars().any(char::is_control)
        })
        .ok_or(INVALID)
}
fn hex(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn date(v: &Value) -> Result<i64, &'static str> {
    let dt =
        time::OffsetDateTime::parse(text(v, 64)?, &time::format_description::well_known::Rfc3339)
            .map_err(|_| INVALID)?;
    let millis = i64::try_from(dt.unix_timestamp_nanos() / 1_000_000).map_err(|_| INVALID)?;
    (0..=MAX_TIMESTAMP)
        .contains(&millis)
        .then_some(millis)
        .ok_or(INVALID)
}
fn boolean(v: &Value) -> Result<bool, &'static str> {
    if v.is_null() {
        Ok(false)
    } else {
        v.as_bool().ok_or(INVALID)
    }
}
impl Prepared {
    /// The original gateway's stable environment id comes from its snapshot
    /// manifest. It must not be replaced by a new UUID or inferred from this
    /// destination's (different) path.
    pub fn parse(raw: &[u8], environment: &str) -> Result<Self, &'static str> {
        if raw.len() > MAX_BYTES
            || environment.len() != 34
            || !environment.starts_with("opencodex-")
            || !environment[10..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(INVALID);
        }
        let raw = std::str::from_utf8(raw).map_err(|_| INVALID)?;
        let root: Value = serde_json::from_str(raw).map_err(|_| INVALID)?;
        if root["version"] != 1 {
            return Err(INVALID);
        }
        let settings = root["settings"].as_object().ok_or(INVALID)?;
        let mode = match settings.get("tunnelMode") {
            None => "quick",
            Some(value) => text(value, 16)?,
        };
        let choice = match settings.get("connectionChoice") {
            None => "local",
            Some(value) => text(value, 16)?,
        };
        // Named credentials live in the OS keyring. Never downgrade that route
        // or silently substitute an unprotected/plaintext credential copy.
        if mode != "quick" || !["local", "quick"].contains(&choice) {
            return Err("Named tunnel migration still requires secure credential migration; nothing imported");
        }
        let list = root["clients"]
            .as_array()
            .filter(|r| r.len() <= 256)
            .ok_or(INVALID)?;
        let mut ids = HashSet::new();
        let mut credentials = HashSet::new();
        let mut installations = HashSet::new();
        let required: HashSet<&str> = auth::SCOPES.split(' ').collect();
        let mut clients = Vec::new();
        for row in list {
            let id = text(&row["id"], 128)?;
            if !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                || !ids.insert(id.to_owned())
                || row["deviceType"] != "mobile"
            {
                return Err(INVALID);
            }
            let scopes = row["scopes"]
                .as_array()
                .filter(|s| s.len() <= 32)
                .ok_or(INVALID)?;
            let scopes: HashSet<&str> = scopes
                .iter()
                .map(|v| text(v, 96))
                .collect::<Result<_, _>>()?;
            if scopes != required {
                return Err(
                    "Legacy phone scopes differ from supported permissions; nothing imported",
                );
            }
            let digest = text(&row["credentialDigest"], 64)?;
            if !hex(digest) || !credentials.insert(digest.to_owned()) {
                return Err(INVALID);
            }
            let installation = if row["installationDigest"].is_null() {
                None
            } else {
                let value = text(&row["installationDigest"], 64)?;
                if !hex(value) || !installations.insert(value.to_owned()) {
                    return Err(INVALID);
                }
                Some(value.to_owned())
            };
            let created = date(&row["createdAt"])?;
            let expiry = if row["credentialExpiresAt"].is_null() {
                MAX_TIMESTAMP
            } else {
                date(&row["credentialExpiresAt"])?
            };
            if expiry < created {
                return Err(INVALID);
            }
            clients.push(Client {
                id: id.into(),
                label: text(&row["label"], 160)?.into(),
                os: text(&row["os"], 64)?.into(),
                created_at: created,
                expires_at: expiry,
                credential_digest: digest.into(),
                installation_digest: installation,
            });
        }
        let count = |key: &str, max: usize| -> Result<usize, &'static str> {
            if root[key].is_null() {
                Ok(0)
            } else {
                root[key]
                    .as_array()
                    .filter(|r| r.len() <= max)
                    .map(Vec::len)
                    .ok_or(INVALID)
            }
        };
        Ok(Self {
            raw: raw.into(),
            hash: auth::digest(raw),
            environment: environment.into(),
            clients,
            preferences: [
                boolean(&root["settings"]["controlEnabled"])?,
                boolean(&root["settings"]["keepAwake"])?,
                boolean(&root["settings"]["localNetworkEnabled"])?,
                true, // Original quick mode runs even when its local QR tab is selected.
            ],
            aliases: count("threadAliases", 1024)?,
            selections: count("taskSelections", 4096)?,
        })
    }
    pub fn summary(&self) -> Value {
        json!({"phase":"connection-prepared","pairedDevices":self.clients.len(),"threadAliasesPending":self.aliases,"taskSelectionsPending":self.selections,"migrationComplete":false})
    }
    fn values(&self) -> Vec<(&str, String)> {
        KEYS.iter()
            .enumerate()
            .map(|(i, k)| {
                (
                    *k,
                    if i == 0 {
                        self.environment.clone()
                    } else {
                        self.preferences[i - 1].to_string()
                    },
                )
            })
            .collect()
    }
    pub(crate) fn verify_identity(&self, db: &rusqlite::Connection) -> Result<(), String> {
        let matches: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import WHERE id=1 AND phase='prepared' AND digest=?1 AND environment=?2 AND raw=?3)", params![self.hash,self.environment,self.raw.as_bytes()],|r|r.get(0)).map_err(|_|INVALID)?;
        let clients: i64 = db
            .query_row("SELECT count(*) FROM clients", [], |r| r.get(0))
            .map_err(|_| INVALID)?;
        if !matches || clients != self.clients.len() as i64 {
            return Err(CONFLICT.into());
        }
        for client in &self.clients {
            let matches: bool=db.query_row("SELECT EXISTS(SELECT 1 FROM clients WHERE id=?1 AND label=?2 AND os=?3 AND created_at=?4 AND expires_at=?5 AND credential_digest=?6 AND installation_digest IS ?7)",params![client.id,client.label,client.os,client.created_at,client.expires_at,client.credential_digest,client.installation_digest],|r|r.get(0)).map_err(|_|INVALID)?;
            if !matches {
                return Err(CONFLICT.into());
            }
        }
        for (key, value) in self.values() {
            let matches: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1 AND value=?2)",
                    params![key, value],
                    |r| r.get(0),
                )
                .map_err(|_| INVALID)?;
            if !matches {
                return Err(CONFLICT.into());
            }
        }
        Ok(())
    }
}
impl Store {
    /// Call offline, with exclusive ownership of a fresh destination. Source
    /// transcript aliases/settings remain byte-for-byte in the private receipt.
    pub fn prepare_legacy_connection(&mut self, prepared: &Prepared) -> Result<Value, String> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS legacy_connection_import(id INTEGER PRIMARY KEY CHECK(id=1),phase TEXT NOT NULL,digest TEXT NOT NULL,environment TEXT NOT NULL,raw BLOB NOT NULL);").map_err(|_|INVALID)?;
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let prior: Option<(String, String, String)> = tx
            .query_row(
                "SELECT phase,digest,environment FROM legacy_connection_import WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| INVALID)?;
        if let Some((phase, hash, environment)) = prior {
            if phase != "prepared" || hash != prepared.hash || environment != prepared.environment {
                return Err(CONFLICT.into());
            }
            let intact: bool = tx
                .query_row(
                    "SELECT raw=?1 FROM legacy_connection_import WHERE id=1",
                    [prepared.raw.as_bytes()],
                    |r| r.get(0),
                )
                .map_err(|_| INVALID)?;
            if !intact {
                return Err(INVALID.into());
            }
            verify(&tx, prepared, &self.directory)?;
            return Ok(prepared.summary());
        }
        let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM clients) OR EXISTS(SELECT 1 FROM metadata) OR EXISTS(SELECT 1 FROM storage_assets)",[],|r|r.get(0)).map_err(|_|INVALID)?;
        if occupied || has_chats(&tx)? {
            return Err(CONFLICT.into());
        }
        for client in &prepared.clients {
            tx.execute(
                "INSERT INTO clients VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    client.id,
                    client.label,
                    client.os,
                    client.created_at,
                    client.expires_at,
                    client.credential_digest,
                    client.installation_digest
                ],
            )
            .map_err(|_| INVALID)?;
        }
        for (key, value) in prepared.values() {
            tx.execute("INSERT INTO metadata VALUES(?1,?2)", params![key, value])
                .map_err(|_| INVALID)?;
        }
        tx.execute(
            "INSERT INTO legacy_connection_import VALUES(1,'prepared',?1,?2,?3)",
            params![prepared.hash, prepared.environment, prepared.raw.as_bytes()],
        )
        .map_err(|_| INVALID)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(prepared.summary())
    }
    /// The pending receipt is a startup barrier, not an invitation to start with
    /// missing history. This will only be released by the complete upgrade path.
    pub fn assert_migration_ready(&self) -> Result<(), String> {
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_connection_import')",[],|r|r.get(0)).map_err(|_|INVALID)?;
        if exists
            && self
                .db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM legacy_connection_import)",
                    [],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(|_| INVALID)?
        {
            return Err("This is an offline migration profile. History and asset migration is not complete; the bridge was not started. The original installation is unchanged.".into());
        }
        Ok(())
    }
    pub fn rollback_legacy_connection(&mut self) -> Result<Value, String> {
        let tx = self.db.transaction().map_err(|_| INVALID)?;
        let report = rollback(&tx, &self.directory)?;
        tx.commit().map_err(|_| INVALID)?;
        Ok(report)
    }
}
fn has_chats(db: &rusqlite::Connection) -> Result<bool, String> {
    let mut stmt = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 129").map_err(|_| INVALID)?;
    let tables = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| INVALID)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| INVALID)?;
    if tables.len() > 128 {
        return Err(CONFLICT.into());
    }
    for name in tables {
        if matches!(
            name.as_str(),
            "legacy_claude_import"
                | "legacy_claude_assets"
                | "legacy_claude_resume"
                | "legacy_claude_native"
                | "legacy_claude_binding_import"
                | "legacy_codex_import"
                | "legacy_codex_headers"
                | "legacy_codex_history"
                | "legacy_preferences_import"
                | "legacy_uploads"
        ) {
            let active: bool = db.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {name} WHERE COALESCE(phase,'')<>'rolled-back')"), [], |r|r.get(0)
            ).map_err(|_|INVALID)?;
            if active {
                return Ok(true);
            }
            continue;
        }
        if crate::legacy_uploads::inactive_metadata(db, &name)? {
            continue;
        }
        if [
            "clients",
            "metadata",
            "storage_assets",
            "storage_totals",
            "legacy_connection_import",
        ]
        .contains(&name.as_str())
        {
            continue;
        }
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM \"{}\")",
            name.replace('"', "\"\"")
        );
        if db
            .query_row(&sql, [], |r| r.get::<_, bool>(0))
            .map_err(|_| INVALID)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn verify(
    db: &rusqlite::Connection,
    prepared: &Prepared,
    directory: &std::path::Path,
) -> Result<(), String> {
    let retained =
        crate::legacy_claude_assets::retained_assets(db, directory, &prepared.environment)?;
    let counts:(i64,i64,i64)=db.query_row("SELECT (SELECT count(*) FROM clients),(SELECT count(*) FROM metadata),(SELECT count(*) FROM storage_assets)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|_|INVALID)?;
    if counts != (prepared.clients.len() as i64, KEYS.len() as i64, retained) || has_chats(db)? {
        return Err(CONFLICT.into());
    }
    prepared.verify_identity(db)?;
    Ok(())
}

pub(crate) fn rollback(
    db: &rusqlite::Connection,
    directory: &std::path::Path,
) -> Result<Value, String> {
    let (phase, hash, environment, raw): (String, String, String, Vec<u8>) = db
        .query_row(
            "SELECT phase,digest,environment,raw FROM legacy_connection_import WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| INVALID)?;
    let prepared = Prepared::parse(&raw, &environment)?;
    if prepared.hash != hash {
        return Err(INVALID.into());
    }
    if phase == "rolled-back" {
        let occupied: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM clients) OR EXISTS(SELECT 1 FROM metadata)",
                [],
                |r| r.get(0),
            )
            .map_err(|_| INVALID)?;
        crate::legacy_claude_assets::retained_assets(db, directory, &environment)?;
        if occupied || has_chats(db)? {
            return Err(CONFLICT.into());
        }
    } else {
        if phase != "prepared" {
            return Err(CONFLICT.into());
        }
        verify(db, &prepared, directory)?;
        db.execute("DELETE FROM clients", []).map_err(|_| INVALID)?;
        db.execute("DELETE FROM metadata", [])
            .map_err(|_| INVALID)?;
        db.execute(
            "UPDATE legacy_connection_import SET phase='rolled-back' WHERE id=1",
            [],
        )
        .map_err(|_| INVALID)?;
    }
    Ok(
        json!({"phase":"rolled-back","sourceUnchanged":true,"backupRetained":true,"migrationComplete":false}),
    )
}
