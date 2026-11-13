//! Inventory revisions wake idle Desktop followers without loading every transcript.
//! An owned chat changes route only after the original Desktop owner is verified.
use crate::{
    agent::SharedServer,
    chat::{AgentConfig, AgentKind, Chat},
    chat_projection::s,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// Keep one of the two history readers available for a phone's selected chat
/// or pending send. A slow dormant owner must not occupy both readers before
/// the phone connects and make its first status check wait up to 12 seconds.
pub(crate) struct ReadSlots {
    total: usize,
    background: usize,
}
impl ReadSlots {
    pub(crate) fn new(total: usize, background: usize) -> Self {
        Self { total, background }
    }
    pub(crate) fn claim(&mut self, foreground: bool) -> bool {
        if self.total >= 2 || (!foreground && self.background >= 1) {
            return false;
        }
        self.total += 1;
        self.background += usize::from(!foreground);
        true
    }
}

struct FailedRevision {
    revision: i64,
    attempts: u32,
    next: Instant,
}
enum Baseline {
    Native(i64),
    MessageTime(i64),
}
impl Baseline {
    fn unchanged(&self, revision: i64) -> bool {
        match *self {
            Self::Native(saved) => revision == saved,
            Self::MessageTime(saved) => revision <= saved,
        }
    }
}
#[derive(Default)]
pub(crate) struct Revisions(
    HashMap<String, i64>,
    HashMap<String, FailedRevision>,
    HashMap<String, Baseline>,
);
impl Revisions {
    /// Only headers present before discovery can supply a restart baseline.
    /// Newly imported chats still get their first task-state read. Unknown
    /// saved state stays unknown; this does not grant any control authority.
    pub(crate) fn from_saved(chats: &HashMap<String, Chat>) -> Self {
        Self(
            HashMap::new(),
            HashMap::new(),
            chats
                .values()
                .filter(|c| eligible(c) && c.external_codex && !needs_recovery(c))
                .filter_map(|c| saved_baseline(c).map(|at| (c.native_id.clone(), at)))
                .collect(),
        )
    }
    pub(crate) fn changed(&mut self, rows: &[Value], chats: &HashMap<String, Chat>) -> Vec<String> {
        self.changed_at(rows, chats, Instant::now())
    }
    fn changed_at(
        &mut self,
        rows: &[Value],
        chats: &HashMap<String, Chat>,
        now: Instant,
    ) -> Vec<String> {
        let known: HashMap<_, _> = chats
            .iter()
            .filter(|(_, c)| eligible(c))
            .map(|(id, c)| (c.native_id.as_str(), id))
            .collect();
        self.0
            .retain(|native, _| known.contains_key(native.as_str()));
        self.1
            .retain(|native, _| known.contains_key(native.as_str()));
        self.2
            .retain(|native, _| known.contains_key(native.as_str()));
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for row in rows {
            let native = s(&row["id"]);
            let Some(id) = known.get(native) else {
                continue;
            };
            let Some(revision) = row["updatedAt"].as_i64().filter(|v| *v > 0) else {
                continue;
            };
            if !seen.insert(native) {
                continue;
            }
            // A dormant, unchanged history that cannot be verified must not
            // restart a large decompression on every inventory scan. Native
            // changes and explicit requests still wake it immediately; active
            // and selected chats retain their independent live watches.
            let retry = if let Some(failed) = self.1.get_mut(native) {
                if failed.revision != revision {
                    self.1.remove(native);
                    false
                } else if now < failed.next {
                    continue;
                } else {
                    failed.next = now + Duration::from_secs(60);
                    true
                }
            } else {
                false
            };
            if retry || self.0.get(native) != Some(&revision) {
                // Do not reload every unchanged dormant transcript on restart.
                // This baseline is consumed once, so any subsequent native
                // revision change still wakes the follower immediately.
                let c = &chats[*id];
                let saved = self.2.remove(native).or_else(|| {
                    (c.external_codex
                        && c.thread["session"]["statusConfidence"] == "confirmed"
                        && !needs_recovery(c))
                    .then(|| saved_baseline(c))
                    .flatten()
                });
                let unchanged =
                    !self.0.contains_key(native) && saved.is_some_and(|at| at.unchanged(revision));
                self.0.insert(native.into(), revision);
                if !unchanged {
                    result.push((*id).clone());
                }
            }
        }
        result
    }
    pub(crate) fn retry(&mut self, native: &str) {
        self.1.remove(native);
        self.2.remove(native);
        self.0.insert(native.into(), -1);
    }
    pub(crate) fn revision(&self, native: &str) -> Option<i64> {
        self.0.get(native).copied().filter(|v| *v > 0)
    }
    pub(crate) fn failed(&mut self, native: &str, revision: Option<i64>) {
        // An old read must not postpone a newer update discovered while that
        // read was in flight.
        if revision.is_some() && self.revision(native) == revision {
            self.failed_at(native, Instant::now());
        }
    }
    fn failed_at(&mut self, native: &str, now: Instant) {
        let Some(&revision) = self.0.get(native).filter(|v| **v > 0) else {
            return;
        };
        let attempts = self
            .1
            .get(native)
            .filter(|f| f.revision == revision)
            .map_or(1, |f| f.attempts.saturating_add(1));
        let seconds = (60_u64 << attempts.saturating_sub(1).min(3)).min(300);
        self.1.insert(
            native.into(),
            FailedRevision {
                revision,
                attempts,
                next: now + Duration::from_secs(seconds),
            },
        );
    }
    pub(crate) fn succeeded(&mut self, native: &str, revision: Option<i64>) {
        if self
            .1
            .get(native)
            .is_some_and(|f| Some(f.revision) == revision)
        {
            self.1.remove(native);
        }
    }
}
fn saved_baseline(c: &Chat) -> Option<Baseline> {
    if let Some((_, revision)) = c
        .desktop_read_revision
        .as_ref()
        .filter(|(native, v)| native == &c.native_id && *v > 0)
    {
        return Some(Baseline::Native(*revision));
    }
    time::OffsetDateTime::parse(
        c.thread["updatedAt"].as_str()?,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()
    .map(|v| Baseline::MessageTime(v.unix_timestamp()))
}
pub(crate) fn needs_recovery(c: &Chat) -> bool {
    matches!(
        s(&c.thread["session"]["status"]),
        "starting" | "running" | "stopping"
    ) || (c.thread["latestTurn"].is_null()
        && matches!(
            s(&c.thread["session"]["statusConfidence"]),
            "unknown" | "unavailable"
        ))
        || c.thread["latestTurn"]["state"] == "running"
        || c.thread["session"]["activeTurnId"].is_string()
        || c.thread["hasPendingApprovals"] == true
        || c.thread["hasPendingUserInput"] == true
        || !c.queue.is_empty()
        || !c.pending_message.is_null()
        || c.native_control.is_some()
        || c.external_stop.is_some()
}
pub(crate) fn eligible(c: &Chat) -> bool {
    c.agent == AgentKind::Codex
        && uuid::Uuid::parse_str(&c.native_id).is_ok()
        && c.thread["deletedAt"].is_null()
        && c.thread["archivedAt"].is_null()
}
pub(crate) async fn has_owner(config: AgentConfig, native: &str) -> Result<bool, String> {
    let Some(SharedServer::Desktop {
        desktop_endpoint, ..
    }) = config.shared_server
    else {
        return Err("Desktop owner discovery is not configured".into());
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut client = desktop_endpoint.connect(&config.profile).await?;
        Ok(client.find_owner(native).await?.is_some())
    })
    .await
    .map_err(|_| {
        "The original Codex connection could not be verified; your prompt was not sent".to_owned()
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn chat(external: bool) -> Chat {
        serde_json::from_value(json!({"agent":"codex","native_id":"12345678-1234-4234-8234-123456789abc","resumed":true,"external_codex":external,"thread":{"id":"local-alias"}})).unwrap()
    }
    #[test]
    fn saved_dormant_headers_skip_restart_reads_but_not_changes_or_new_imports() {
        for confidence in ["confirmed", "unknown"] {
            let mut c = chat(true);
            c.thread["updatedAt"] = json!("2026-01-01T00:00:00Z");
            c.thread["session"] = json!({"status":"ready","statusConfidence":confidence});
            c.thread["latestTurn"] = json!({"state":"completed"});
            let native = c.native_id.clone();
            let mut chats = HashMap::from([("local-alias".into(), c)]);
            let mut r = Revisions::from_saved(&chats);
            let saved = vec![json!({"id":native,"updatedAt":1767225600_i64})];
            assert!(r.changed(&saved, &chats).is_empty());
            assert_eq!(
                chats["local-alias"].thread["session"]["statusConfidence"],
                confidence
            );
            let newer = vec![json!({"id":native,"updatedAt":1767225601_i64})];
            assert_eq!(r.changed(&newer, &chats), vec!["local-alias"]);
            let mut offline_change = Revisions::from_saved(&chats);
            assert_eq!(offline_change.changed(&newer, &chats), vec!["local-alias"]);
            r.retry(&native);
            assert_eq!(r.changed(&newer, &chats), vec!["local-alias"]);
            let mut imported = chat(true);
            imported.native_id = uuid::Uuid::new_v4().to_string();
            let row = json!({"id":imported.native_id,"updatedAt":1767225600_i64});
            chats.insert("new-import".into(), imported);
            assert_eq!(r.changed(&[row], &chats), vec!["new-import"]);
        }
    }
    #[test]
    fn a_saved_native_revision_is_not_confused_with_the_last_message_timestamp() {
        let mut c = chat(true);
        c.thread["updatedAt"] = json!("2026-01-01T00:00:00Z");
        c.thread["session"] = json!({"status":"ready","statusConfidence":"confirmed"});
        c.thread["latestTurn"] = json!({"state":"completed"});
        c.desktop_read_revision = Some((c.native_id.clone(), 1767225700));
        // Exercise the persisted chat format used across a server restart.
        let restored: Chat = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), restored)]);
        let row = json!({"id":native,"updatedAt":1767225700_i64});
        let mut r = Revisions::from_saved(&chats);
        assert!(r.changed(&[row], &chats).is_empty());
        for revision in [1767225699_i64, 1767225701] {
            assert_eq!(
                Revisions::from_saved(&chats)
                    .changed(&[json!({"id":native,"updatedAt":revision})], &chats),
                vec!["local-alias"]
            );
        }
        let mut foreign = c.clone();
        foreign.desktop_read_revision = Some((uuid::Uuid::new_v4().to_string(), 1767225700));
        let foreign = HashMap::from([("local-alias".into(), foreign)]);
        assert_eq!(
            Revisions::from_saved(&foreign)
                .changed(&[json!({"id":native,"updatedAt":1767225700_i64})], &foreign),
            vec!["local-alias"]
        );
        c.detach_native_session();
        assert!(c.desktop_read_revision.is_none());
    }
    #[test]
    fn restart_keeps_recovery_for_active_pending_and_missing_task_state() {
        let mut c = chat(true);
        c.thread["updatedAt"] = json!("2026-01-01T00:00:00Z");
        c.thread["session"] = json!({"status":"ready","statusConfidence":"unknown"});
        c.thread["latestTurn"] = json!({"state":"completed"});
        assert!(!needs_recovery(&c));
        for field in [
            "status",
            "activeTurnId",
            "latestTurn",
            "approval",
            "input",
            "queue",
            "message",
            "missing",
        ] {
            let mut active = c.clone();
            match field {
                "status" => active.thread["session"]["status"] = json!("running"),
                "activeTurnId" => active.thread["session"]["activeTurnId"] = json!("active"),
                "latestTurn" => active.thread["latestTurn"]["state"] = json!("running"),
                "approval" => active.thread["hasPendingApprovals"] = json!(true),
                "input" => active.thread["hasPendingUserInput"] = json!(true),
                "queue" => active.queue.push(json!({"message":"queued"})),
                "message" => active.pending_message = json!({"id":"uncertain"}),
                "missing" => active.thread["latestTurn"] = Value::Null,
                _ => unreachable!(),
            }
            assert!(needs_recovery(&active), "{field}");
            let native = active.native_id.clone();
            let chats = HashMap::from([("local-alias".into(), active)]);
            assert_eq!(
                Revisions::from_saved(&chats)
                    .changed(&[json!({"id":native,"updatedAt":10})], &chats),
                vec!["local-alias"]
            );
        }
    }
    #[test]
    fn unchanged_failed_histories_back_off_but_changes_and_explicit_retry_are_immediate() {
        let c = chat(true);
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), c)]);
        let rows = vec![json!({"id":native,"updatedAt":10})];
        let now = Instant::now();
        let mut revisions = Revisions::default();
        assert_eq!(
            revisions.changed_at(&rows, &chats, now),
            vec!["local-alias"]
        );
        revisions.failed_at(&native, now);
        for seconds in [0, 5, 30, 59] {
            assert!(revisions
                .changed_at(&rows, &chats, now + Duration::from_secs(seconds))
                .is_empty());
        }
        assert_eq!(
            revisions.changed_at(&rows, &chats, now + Duration::from_secs(60)),
            vec!["local-alias"]
        );
        assert!(revisions
            .changed_at(&rows, &chats, now + Duration::from_secs(61))
            .is_empty());
        revisions.failed_at(&native, now + Duration::from_secs(61));
        assert!(revisions
            .changed_at(&rows, &chats, now + Duration::from_secs(180))
            .is_empty());
        let newer = vec![json!({"id":native,"updatedAt":11})];
        assert_eq!(
            revisions.changed_at(&newer, &chats, now + Duration::from_secs(181)),
            vec!["local-alias"]
        );
        revisions.failed_at(&native, now + Duration::from_secs(181));
        revisions.retry(&native);
        assert_eq!(
            revisions.changed_at(&newer, &chats, now + Duration::from_secs(182)),
            vec!["local-alias"]
        );
        revisions.failed_at(&native, now + Duration::from_secs(182));
        revisions.succeeded(&native, Some(11));
        assert!(revisions
            .changed_at(&newer, &chats, now + Duration::from_secs(1000))
            .is_empty());
    }
    #[test]
    fn an_older_failed_read_cannot_delay_a_newly_discovered_revision() {
        let c = chat(true);
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), c)]);
        let mut revisions = Revisions::default();
        revisions.changed(&[json!({"id":native,"updatedAt":10})], &chats);
        let read_revision = revisions.revision(&native);
        assert_eq!(
            revisions.changed(&[json!({"id":native,"updatedAt":11})], &chats),
            vec!["local-alias"]
        );
        revisions.failed(&native, read_revision);
        assert!(!revisions.1.contains_key(&native));
        revisions.failed(&native, Some(11));
        assert!(revisions.1.contains_key(&native));
        revisions.succeeded(&native, read_revision);
        assert!(revisions.1.contains_key(&native));
    }
    #[test]
    fn dormant_histories_leave_a_reader_for_first_open_and_pending_send() {
        let mut idle = ReadSlots::new(0, 0);
        assert!(idle.claim(false));
        assert!(!idle.claim(false));
        // Opening a chat must start even while the dormant read is waiting.
        let mut opened = ReadSlots::new(1, 1);
        assert!(opened.claim(true));
        assert!(!opened.claim(true));
        // Both readers may serve phones, but total concurrency stays bounded.
        let mut phones = ReadSlots::new(0, 0);
        assert!(phones.claim(true));
        assert!(phones.claim(true));
        assert!(!phones.claim(false));
    }
    #[test]
    fn idle_owned_and_desktop_chats_refresh_only_on_native_changes_and_retry_failure() {
        for external in [false, true] {
            let c = chat(external);
            let native = c.native_id.clone();
            let mut chats = HashMap::from([("local-alias".into(), c)]);
            let mut r = Revisions::default();
            let rows = vec![json!({"id":native,"updatedAt":10,"status":{"type":"notLoaded"}})];
            assert_eq!(r.changed(&rows, &chats), vec!["local-alias"]);
            assert!(r.changed(&rows, &chats).is_empty());
            let newer = vec![json!({"id":native,"updatedAt":11})];
            assert_eq!(r.changed(&newer, &chats), vec!["local-alias"]);
            r.retry(&native);
            assert_eq!(r.changed(&newer, &chats), vec!["local-alias"]);
            chats.get_mut("local-alias").unwrap().thread["archivedAt"] = json!("archived");
            assert!(r.changed(&newer, &chats).is_empty());
            assert!(r.0.is_empty());
        }
    }
    #[test]
    fn fresh_external_baseline_is_quiet_but_explicit_retry_is_not() {
        let mut c = chat(true);
        c.thread["updatedAt"] = json!("2026-01-01T00:00:00Z");
        c.thread["session"] = json!({"statusConfidence":"confirmed"});
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), c)]);
        let rows = vec![json!({"id":native,"updatedAt":10})];
        let mut r = Revisions::default();
        assert!(r.changed(&rows, &chats).is_empty());
        r.retry(&native);
        assert_eq!(r.changed(&rows, &chats), vec!["local-alias"]);
        assert!(r.changed(&rows, &chats).is_empty());
    }
    #[test]
    fn newly_imported_or_unconfirmed_header_gets_status_on_first_inventory() {
        let mut c = chat(true);
        c.thread["updatedAt"] = json!("2026-01-01T00:00:00Z");
        c.thread["session"] = serde_json::json!({"statusConfidence":"unknown"});
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), c)]);
        let rows = vec![json!({"id":native,"updatedAt":10})];
        assert_eq!(
            Revisions::default().changed(&rows, &chats),
            vec!["local-alias"]
        );
    }
    #[test]
    fn malformed_unknown_and_duplicate_headers_do_not_create_work() {
        let c = chat(false);
        let native = c.native_id.clone();
        let chats = HashMap::from([("local-alias".into(), c)]);
        let mut r = Revisions::default();
        assert!(r
            .changed(
                &[json!({"id":native}), json!({"id":"unknown","updatedAt":10})],
                &chats
            )
            .is_empty());
        let row = json!({"id":native,"updatedAt":10});
        assert_eq!(r.changed(&[row.clone(), row], &chats), vec!["local-alias"]);
    }
}
