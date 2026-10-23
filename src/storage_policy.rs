//! Opt-in cleanup settings. Run metadata is merged into the latest saved policy;
//! malformed saved settings fail closed and never authorize a cleanup.
use crate::auth_service::AuthService;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
const KEY: &str = "native.storageCleanupPolicy";
const DAY: u64 = 86_400_000;
pub(crate) const DEFER: u64 = 900_000;
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Policy {
    pub enabled: bool,
    pub trigger: Trigger,
    pub target: Target,
    pub schedule: Schedule,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Trigger {
    pub archived_bytes_over: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum Target {
    Percent {
        #[serde(rename = "removeOldestPercent")]
        percent: f64,
    },
    Bytes {
        #[serde(rename = "reduceToBytes")]
        bytes: u64,
    },
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Schedule {
    Manual,
    Startup,
    Daily,
    Weekly,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Quarantine,
    Permanent,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LastRun {
    pub at: u64,
    pub freed_bytes: u64,
    pub removed: u64,
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Reason {
    Manual,
    Startup,
    Schedule,
}
impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Startup => "startup",
            Self::Schedule => "schedule",
        }
    }
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger: Trigger {
                archived_bytes_over: 5 * 1024 * 1024 * 1024,
            },
            target: Target::Percent { percent: 25.0 },
            schedule: Schedule::Manual,
            mode: Mode::Quarantine,
            last_run: None,
            next_run: None,
        }
    }
}
impl Policy {
    fn validate(&self) -> Result<()> {
        let limit = 9_007_199_254_740_991;
        if self.trigger.archived_bytes_over > limit
            || self.next_run.is_some_and(|n| n == 0 || n > limit)
            || self.last_run.as_ref().is_some_and(|r| {
                r.at == 0 || r.at > limit || r.freed_bytes > limit || r.removed > limit
            })
            || match self.target {
                Target::Percent { percent } => {
                    !percent.is_finite() || !(1.0..=100.0).contains(&percent)
                }
                Target::Bytes { bytes } => bytes > limit,
            }
        {
            return Err("Invalid cleanup policy; no cleanup was started".into());
        }
        Ok(())
    }
    pub fn due(&self, now: u64, reason: Reason) -> bool {
        self.enabled
            && match reason {
                Reason::Manual => true,
                _ => match self.schedule {
                    Schedule::Manual => false,
                    Schedule::Startup => {
                        reason == Reason::Startup || self.next_run.is_some_and(|n| now >= n)
                    }
                    _ => self.next_run.is_none_or(|n| now >= n),
                },
            }
    }
    fn advance(&mut self, now: u64) {
        self.next_run = match self.schedule {
            Schedule::Daily => Some(now.saturating_add(DAY)),
            Schedule::Weekly => Some(now.saturating_add(7 * DAY)),
            _ => None,
        };
    }
    pub fn selection(&self) -> crate::storage_preview::Target {
        match self.target {
            Target::Percent { percent } => crate::storage_preview::Target::Percent(percent.floor()),
            Target::Bytes { bytes } => crate::storage_preview::Target::ReduceTo(bytes),
        }
    }
    pub fn mode(&self) -> &'static str {
        match self.mode {
            Mode::Permanent => "permanent",
            Mode::Quarantine => "quarantine",
        }
    }
    fn patch(&self, body: Value, now: u64) -> Result<Self> {
        let object = body
            .as_object()
            .filter(|v| {
                !v.is_empty()
                    && v.keys().all(|k| {
                        matches!(
                            k.as_str(),
                            "enabled" | "trigger" | "target" | "schedule" | "mode"
                        )
                    })
            })
            .ok_or("Provide cleanup enabled, trigger, target, schedule or mode settings")?;
        let mut value = serde_json::to_value(self).map_err(|_| "Cannot encode cleanup policy")?;
        for (k, v) in object {
            value[k] = v.clone();
        }
        let mut next: Self = serde_json::from_value(value).map_err(|_| "Invalid cleanup policy")?;
        next.validate()?;
        if let Target::Percent { percent } = &mut next.target {
            *percent = percent.floor();
        }
        if next.schedule != self.schedule || next.next_run.is_none() {
            next.advance(now);
        }
        Ok(next)
    }
}
fn read(db: &rusqlite::Connection) -> Result<Policy> {
    let saved: Option<String> = db
        .query_row(
            "SELECT substr(value,1,4097) FROM metadata WHERE key=?1",
            [KEY],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| "Cleanup settings are unavailable")?;
    match saved {
        None => Ok(Policy::default()),
        Some(s) if s.len() <= 4096 => {
            let p: Policy = serde_json::from_str(&s)
                .map_err(|_| "Saved cleanup settings are invalid; automatic cleanup is disabled")?;
            p.validate()?;
            Ok(p)
        }
        _ => Err("Saved cleanup settings exceed their limit".into()),
    }
}
fn write(db: &rusqlite::Connection, policy: &Policy) -> Result<()> {
    policy.validate()?;
    let data = serde_json::to_string(policy).map_err(|_| "Cannot encode cleanup settings")?;
    db.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[KEY,&data])
        .map_err(|_|"Cannot save cleanup settings")?;
    Ok(())
}
pub(crate) async fn load(db: &AuthService) -> Result<Policy> {
    db.call(|a| read(&a.store.db)).await?
}
pub(crate) async fn save(db: &AuthService, body: Value, now: u64) -> Result<Policy> {
    db.call(move |a| {
        let tx = a
            .store
            .db
            .transaction()
            .map_err(|_| "Cleanup settings are busy")?;
        let next = read(&tx)?.patch(body, now)?;
        write(&tx, &next)?;
        tx.commit().map_err(|_| "Cannot commit cleanup settings")?;
        Ok(next)
    })
    .await?
}
pub(crate) async fn complete(db: &AuthService, now: u64, outcome: Value) -> Result<Policy> {
    db.call(move |a| {
        let tx = a
            .store
            .db
            .transaction()
            .map_err(|_| "Cleanup settings are busy")?;
        let mut latest = read(&tx)?;
        if outcome["deferred"] == "codex_busy" {
            latest.next_run = Some(now.saturating_add(DEFER));
        } else {
            latest.advance(now);
        }
        if outcome["ok"] == true && outcome.get("skipped").is_none() {
            latest.last_run = Some(LastRun {
                at: now,
                freed_bytes: outcome["freedBytes"].as_u64().unwrap_or(0),
                removed: outcome["removed"].as_u64().unwrap_or(0),
            });
        }
        write(&tx, &latest)?;
        tx.commit().map_err(|_| "Cannot commit cleanup result")?;
        Ok(latest)
    })
    .await?
}
pub(crate) fn value(policy: &Policy, job: Value) -> Value {
    let mut v = json!(policy);
    v["job"] = job;
    v
}
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn completion_preserves_concurrent_settings_and_malformed_persistence_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let db = AuthService::new(crate::auth::Auth::new(
            crate::store::Store::open(root.path()).unwrap(),
        ))
        .unwrap();
        save(
            &db,
            json!({"enabled":true,"schedule":"daily","target":{"removeOldestPercent":50}}),
            100,
        )
        .await
        .unwrap();
        let starting = load(&db).await.unwrap();
        save(
            &db,
            json!({"enabled":false,"schedule":"weekly","target":{"reduceToBytes":1234}}),
            200,
        )
        .await
        .unwrap();
        let result = complete(&db, 300, json!({"ok":true,"freedBytes":99,"removed":2}))
            .await
            .unwrap();
        assert!(!result.enabled);
        assert_ne!(starting.target, result.target);
        assert_eq!(result.target, Target::Bytes { bytes: 1234 });
        assert_eq!(result.next_run, Some(300 + 7 * DAY));
        assert_eq!(result.last_run.unwrap().removed, 2);
        let deferred = complete(&db, 400, json!({"ok":false,"deferred":"codex_busy"}))
            .await
            .unwrap();
        assert_eq!(deferred.next_run, Some(400 + DEFER));
        assert!(!deferred.enabled);
        db.call(|a| {
            a.store.db.execute(
                "UPDATE metadata SET value=?1 WHERE key=?2",
                ["{\"enabled\":true,\"target\":{}}", KEY],
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(load(&db).await.is_err());
        assert!(save(&db, json!({"mode":"permanent"}), 500).await.is_err());
    }
    #[test]
    fn default_and_invalid_targets_cannot_authorize_deletion() {
        let p = Policy::default();
        assert!(!p.due(100, Reason::Manual));
        for body in [
            json!({"enabled":"yes"}),
            json!({"target":{}}),
            json!({"target":{"reduceToBytes":0,"removeOldestPercent":50}}),
            json!({"target":{"removeOldestPercent":0}}),
            json!({"target":{"reduceToBytes":-1}}),
            json!({"lastRun":{}}),
        ] {
            assert!(p.patch(body, 100).is_err());
        }
        assert!(!p.patch(json!({"mode":"permanent"}), 100).unwrap().enabled);
    }
    #[test]
    fn schedules_preserve_due_times_and_support_deferred_startup() {
        let p = Policy::default()
            .patch(json!({"enabled":true,"schedule":"daily"}), 100)
            .unwrap();
        assert_eq!(p.next_run, Some(100 + DAY));
        assert!(!p.due(101, Reason::Startup));
        assert!(p.due(101, Reason::Manual));
        assert_eq!(
            p.patch(json!({"schedule":"daily","mode":"permanent"}), 200)
                .unwrap()
                .next_run,
            p.next_run
        );
        let mut p = p.patch(json!({"schedule":"startup"}), 200).unwrap();
        assert_eq!(p.next_run, None);
        assert!(p.due(200, Reason::Startup));
        assert!(!p.due(200, Reason::Schedule));
        p.next_run = Some(300);
        assert!(!p.due(299, Reason::Schedule));
        assert!(p.due(300, Reason::Schedule));
    }
}
