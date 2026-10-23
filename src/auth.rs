use crate::store::{Client, Store};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use subtle::ConstantTimeEq;

pub const SCOPES: &str =
    "orchestration:read orchestration:operate terminal:operate review:write relay:read";
const PAIRING_MS: i64 = 300_000;
pub const ACCESS_MS: i64 = 180 * 24 * 60 * 60 * 1000;
const TICKET_MS: i64 = 30_000;
const RECOVERY_MS: i64 = 120_000;
const MAX_PENDING: usize = 128;

fn secret(prefix: &str) -> String {
    let mut random = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut random);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(random))
}
pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn timestamp(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("valid wall clock")
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn clean(value: Option<&str>, max: usize, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .chars()
        .take(max)
        .collect()
}

struct Invitation {
    id: String,
    expires: i64,
}
struct Ticket {
    client: String,
    expires: i64,
}
struct Recovery {
    proof: String,
    expires: i64,
    result: Exchange,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    pub access_token: String,
    pub expires_at: i64,
    pub client_id: String,
    pub replaced_client_ids: Vec<String>,
}
#[derive(Default, Deserialize)]
pub struct Metadata {
    pub label: Option<String>,
    pub os: Option<String>,
    pub installation_id: Option<String>,
}

pub struct Auth {
    pub store: Store,
    invitations: HashMap<String, Invitation>,
    tickets: HashMap<String, Ticket>,
    recovery: HashMap<String, Recovery>,
}
impl Auth {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            invitations: HashMap::new(),
            tickets: HashMap::new(),
            recovery: HashMap::new(),
        }
    }
    fn prune(&mut self, now: i64) {
        self.invitations.retain(|_, v| v.expires > now);
        self.tickets.retain(|_, v| v.expires > now);
        self.recovery.retain(|_, v| v.expires > now);
    }
    pub fn invite(&mut self, label: &str, url: &str, now: i64) -> Result<Value, &'static str> {
        if !self.control_enabled().map_err(|_| "Settings unavailable")? {
            return Err("Phone control is disabled");
        }
        self.prune(now);
        if self.invitations.len() >= MAX_PENDING {
            return Err("too many pending invitations");
        }
        let raw = secret("ocx_pair_");
        let id = uuid::Uuid::new_v4().to_string();
        self.invitations.insert(
            digest(&raw),
            Invitation {
                id: id.clone(),
                expires: now + PAIRING_MS,
            },
        );
        Ok(json!({"type":"remodex-mobile-pairing", "version":1,
            "desktopName":label,"localUrls":[url],"cloudflareUrl":null,"directUrl":url,
            "pairingToken":raw,"pairingLinkId":id,"expiresAt":timestamp(now+PAIRING_MS)}))
    }
    pub fn has_invitation(&self, id: &str, now: i64) -> bool {
        self.invitations
            .values()
            .any(|v| v.id == id && v.expires > now)
    }
    pub fn exchange(
        &mut self,
        token: &str,
        proof: Option<&str>,
        metadata: Metadata,
        now: i64,
    ) -> Result<Option<Exchange>, rusqlite::Error> {
        if !self.control_enabled()? {
            return Ok(None);
        }
        self.prune(now);
        if proof.is_some_and(|p| {
            p.len() != 64
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Ok(None);
        }
        let key = digest(token);
        if let Some(previous) = self.recovery.get(&key) {
            if !proof
                .is_some_and(|p| bool::from(digest(p).as_bytes().ct_eq(previous.proof.as_bytes())))
            {
                return Ok(None);
            }
            if self.store.authorized(&previous.result.client_id, now)? {
                let mut result = previous.result.clone();
                result.replaced_client_ids.clear();
                return Ok(Some(result));
            }
            return Ok(None);
        }
        if !self.invitations.contains_key(&key) {
            return Ok(None);
        }
        // Reserve bounded recovery capacity before committing/consuming a token.
        if proof.is_some() && self.recovery.len() >= MAX_PENDING {
            return Ok(None);
        }
        let raw = secret("ocx_android_");
        let installation = metadata
            .installation_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 160)
            .map(|s| digest(&format!("remodex-installation-v1:{s}")));
        let client = Client {
            id: uuid::Uuid::new_v4().to_string(),
            label: clean(metadata.label.as_deref(), 160, "Android phone"),
            os: clean(metadata.os.as_deref(), 64, "android"),
            created_at: now,
            expires_at: now + ACCESS_MS,
            credential_digest: digest(&raw),
            installation_digest: installation,
        };
        let replaced = self.store.replace_client(&client)?;
        self.invitations.remove(&key);
        self.tickets.retain(|_, v| !replaced.contains(&v.client));
        let result = Exchange {
            access_token: raw,
            expires_at: client.expires_at,
            client_id: client.id,
            replaced_client_ids: replaced,
        };
        if let Some(proof) = proof {
            self.recovery.insert(
                key,
                Recovery {
                    proof: digest(proof),
                    expires: now + RECOVERY_MS,
                    result: result.clone(),
                },
            );
        }
        Ok(Some(result))
    }
    pub fn authenticate(&self, token: &str, now: i64) -> rusqlite::Result<Option<Client>> {
        if token.len() > 256 || !self.control_enabled()? {
            return Ok(None);
        }
        self.store.client(&digest(token), now)
    }
    pub fn ticket(&mut self, client: &str, now: i64) -> Result<Option<Value>, rusqlite::Error> {
        self.prune(now);
        if self.tickets.len() >= MAX_PENDING || !self.authorized(client, now)? {
            return Ok(None);
        }
        let raw = secret("ocx_ws_");
        self.tickets.insert(
            digest(&raw),
            Ticket {
                client: client.to_owned(),
                expires: now + TICKET_MS,
            },
        );
        Ok(Some(
            json!({"ticket":raw,"expiresAt":timestamp(now+TICKET_MS),"protocol":"opencodex-json-v1"}),
        ))
    }
    pub fn consume_ticket(&mut self, token: &str, now: i64) -> rusqlite::Result<Option<String>> {
        self.prune(now);
        let Some(ticket) = self.tickets.remove(&digest(token)) else {
            return Ok(None);
        };
        Ok(self
            .authorized(&ticket.client, now)?
            .then_some(ticket.client))
    }
    pub fn control_enabled(&self) -> rusqlite::Result<bool> {
        use rusqlite::OptionalExtension;
        let value: Option<String> = self
            .store
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='phone_control_enabled'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match value.as_deref() {
            None | Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
    pub fn set_control_enabled(&mut self, enabled: bool) -> rusqlite::Result<()> {
        self.store.db.execute(
            "INSERT INTO metadata(key,value) VALUES ('phone_control_enabled',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [if enabled { "true" } else { "false" }],
        )?;
        if !enabled {
            self.invitations.clear();
            self.tickets.clear();
            self.recovery.clear();
        }
        Ok(())
    }
    pub fn authorized(&self, client: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.control_enabled()? && self.store.authorized(client, now)?)
    }
    pub fn revoke(&mut self, client: &str) -> rusqlite::Result<bool> {
        let revoked = self.store.revoke(client)?;
        self.tickets.retain(|_, v| v.client != client);
        self.recovery.retain(|_, v| v.result.client_id != client);
        Ok(revoked)
    }
}
