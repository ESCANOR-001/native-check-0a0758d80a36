//! Keep phone subscriptions quiet when their visible data has not changed.
//! Each connection still starts with an authoritative, complete snapshot.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type Fingerprint = [u8; 32];
fn fingerprint(value: &Value) -> Fingerprint {
    Sha256::digest(serde_json::to_vec(value).expect("JSON value serializes")).into()
}

const GROUPS: [(&str, &str, &str); 4] = [
    ("projects", "project", "projectId"),
    ("threads", "thread", "threadId"),
    ("nativeProjects", "native-project", "projectId"),
    ("nativeThreads", "native-thread", "threadId"),
];

struct Shell {
    sequence: u64,
    metadata: Fingerprint,
    rows: Vec<BTreeMap<String, Fingerprint>>,
}

struct Thread {
    sequence: u64,
    metadata: Fingerprint,
    activities: BTreeMap<String, (Fingerprint, Fingerprint)>,
}
impl Thread {
    fn read(snapshot: &Value) -> Option<Self> {
        let mut metadata = snapshot.clone();
        metadata.as_object_mut()?.remove("snapshotSequence");
        let rows = metadata["thread"].as_object_mut()?.remove("activities")?;
        let mut activities = BTreeMap::new();
        for row in rows.as_array()? {
            let id = row["id"].as_str()?;
            // Android keeps an existing activity's original position and time.
            // A correction to those fields needs an authoritative snapshot.
            let position = fingerprint(&json!([row["createdAt"], row["sequence"]]));
            if id.is_empty()
                || activities
                    .insert(id.into(), (fingerprint(row), position))
                    .is_some()
            {
                return None;
            }
        }
        Some(Self {
            sequence: snapshot["snapshotSequence"].as_u64()?,
            metadata: fingerprint(&metadata),
            activities,
        })
    }
}
impl Shell {
    fn read(snapshot: &Value) -> Option<Self> {
        let mut metadata = snapshot.as_object()?.clone();
        metadata.remove("snapshotSequence");
        metadata.remove("updatedAt");
        let mut rows = Vec::new();
        for (name, _, _) in GROUPS {
            let values = metadata.remove(name).unwrap_or_else(|| json!([]));
            let mut group = BTreeMap::new();
            for value in values.as_array()? {
                let id = value["id"].as_str()?;
                if id.is_empty() || group.insert(id.to_owned(), fingerprint(value)).is_some() {
                    return None;
                }
            }
            rows.push(group);
        }
        Some(Self {
            sequence: snapshot["snapshotSequence"].as_u64()?,
            metadata: fingerprint(&Value::Object(metadata)),
            rows,
        })
    }
}

#[derive(Default)]
pub(crate) struct Delivery {
    shell: Option<Shell>,
    config: Option<Fingerprint>,
    thread: Option<Thread>,
}
impl Delivery {
    /// Called only for subscription responses. Explicit requests are always sent.
    /// Retain only row IDs/digests, not another complete chat list per socket.
    pub(crate) fn next(&mut self, method: &str, mut reply: Value, force: bool) -> Option<Value> {
        if !reply["error"].is_null() {
            // The next successful snapshot must clear a transient thread error,
            // even when the underlying conversation has not changed.
            if method == "orchestration.subscribeThread" {
                self.thread = None;
            }
            return Some(reply);
        }
        if method == "orchestration.subscribeThread" && reply["event"]["kind"] == "snapshot" {
            return self.thread_reply(reply, force);
        }
        if method == "subscribeServerConfig" {
            let mut stable = reply["event"].clone();
            if let Some(providers) = stable["config"]["providers"].as_array_mut() {
                for provider in providers {
                    if let Some(provider) = provider.as_object_mut() {
                        // This is the read time, not an account/model change.
                        provider.remove("checkedAt");
                    }
                }
            }
            let next = fingerprint(&stable);
            let previous = self.config.replace(next);
            return (force || previous != Some(next)).then_some(reply);
        }
        if method != "orchestration.subscribeShell" || reply["event"]["kind"] != "snapshot" {
            return Some(reply);
        }
        let snapshot = &reply["event"]["snapshot"];
        let Some(next) = Shell::read(snapshot) else {
            self.shell = None;
            return Some(reply);
        };
        let previous = self.shell.replace(next);
        let next = self.shell.as_ref().unwrap();
        let Some(previous) = previous.filter(|_| !force) else {
            return Some(reply);
        };
        if previous.metadata != next.metadata {
            return Some(reply);
        }
        let mut change = None;
        for (index, ((name, kind, id_key), rows)) in GROUPS.iter().zip(&next.rows).enumerate() {
            for (id, digest) in rows {
                if previous.rows[index].get(id) != Some(digest) {
                    if change.is_some() {
                        // Multiple changes share the hub's one sequence. Sending
                        // several deltas with it would make Android drop all but
                        // the first, so keep the atomic complete snapshot.
                        return Some(reply);
                    }
                    let row = snapshot[name].as_array()?.iter().find(|r| r["id"] == *id)?;
                    let field = if id_key == &"projectId" {
                        "project"
                    } else {
                        "thread"
                    };
                    change = Some(
                        json!({"kind":format!("{kind}-upserted"),"sequence":next.sequence,field:row}),
                    );
                }
            }
            for id in previous.rows[index]
                .keys()
                .filter(|id| !rows.contains_key(*id))
            {
                if change.is_some() {
                    return Some(reply);
                }
                change = Some(
                    json!({"kind":format!("{kind}-removed"),"sequence":next.sequence,*id_key:id}),
                );
            }
        }
        let change = change?;
        if next.sequence > previous.sequence {
            reply["event"] = change;
        }
        // Equal/lower sequence with changed data must remain an authoritative
        // snapshot (e.g. a change observed just before the next publish tick).
        Some(reply)
    }

    fn thread_reply(&mut self, mut reply: Value, force: bool) -> Option<Value> {
        let snapshot = &reply["event"]["snapshot"];
        let Some(next) = Thread::read(snapshot) else {
            self.thread = None;
            return Some(reply);
        };
        if let Some(previous) = self.thread.as_ref().filter(|_| !force) {
            if previous.metadata == next.metadata {
                if previous.activities == next.activities {
                    // Keep the *delivered* sequence. Advancing it here would
                    // allow a later event to jump over Android's sequence gate.
                    return None;
                }
                let changed: Vec<_> = next
                    .activities
                    .iter()
                    .filter(|(id, row)| previous.activities.get(*id) != Some(*row))
                    .collect();
                let removed = previous
                    .activities
                    .keys()
                    .any(|id| !next.activities.contains_key(id));
                if !removed
                    && changed.len() == 1
                    && previous.sequence.checked_add(1) == Some(next.sequence)
                {
                    let (id, (_, position)) = changed[0];
                    let stable_position = previous
                        .activities
                        .get(id)
                        .is_none_or(|(_, old)| old == position);
                    if stable_position {
                        let thread = &snapshot["thread"];
                        let activity = thread["activities"]
                            .as_array()?
                            .iter()
                            .find(|r| r["id"] == *id)?;
                        // Existing Android event contract; no new client feature,
                        // invented native RPC, or per-connection sequence scheme.
                        reply["event"] = json!({"kind":"event","event":{
                            "type":"thread.activity-appended","sequence":next.sequence,
                            "eventId":uuid::Uuid::new_v4().to_string(),"aggregateKind":"thread",
                            "aggregateId":thread["id"],"occurredAt":thread["updatedAt"],
                            "commandId":null,"causationEventId":null,"correlationId":null,"metadata":{},
                            "payload":{"threadId":thread["id"],"activity":activity}
                        }});
                    }
                }
            }
        }
        self.thread = Some(next);
        Some(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn thread(sequence: u64, activities: Value) -> Value {
        json!({"id":"thread-watch","event":{"kind":"snapshot","snapshot":{
            "snapshotSequence":sequence,"historyPage":{"hasOlder":true,"olderCursor":"saved-cursor"},
            "thread":{"id":"chat","updatedAt":"2026-11-09T11:30:00Z",
                "session":{"status":"running","statusConfidence":"confirmed","activeTurnId":"turn"},
                "messages":[{"id":"prompt","role":"user","text":"Keep my history"}],
                "activities":activities}}}})
    }
    fn activity(id: &str, detail: &str) -> Value {
        json!({"id":id,"turnId":"turn","kind":"tool.completed","tone":"info","summary":"Tool",
            "createdAt":"2026-11-09T11:30:00Z","sequence":1,"payload":{"detail":detail}})
    }
    #[test]
    fn unchanged_long_thread_is_not_retransmitted_by_unrelated_chat_updates() {
        let rows = json!([activity("tool", &"x".repeat(700_000))]);
        let mut d = Delivery::default();
        let initial = thread(10, rows.clone());
        assert_eq!(
            d.next("orchestration.subscribeThread", initial.clone(), false),
            Some(initial)
        );
        for sequence in 11..100 {
            assert!(d
                .next(
                    "orchestration.subscribeThread",
                    thread(sequence, rows.clone()),
                    false
                )
                .is_none());
        }
        let fresh = thread(100, rows);
        assert_eq!(
            d.next("orchestration.subscribeThread", fresh.clone(), true),
            Some(fresh.clone())
        );
        assert_eq!(
            Delivery::default().next("orchestration.subscribeThread", fresh.clone(), false),
            Some(fresh)
        );
    }
    #[test]
    fn thread_activity_delta_obeys_android_sequence_and_position_rules() {
        let method = "orchestration.subscribeThread";
        let mut d = Delivery::default();
        d.next(method, thread(1, json!([activity("a", "old")])), true);
        let changed = activity("a", "finished");
        let delta = d
            .next(method, thread(2, json!([changed.clone()])), false)
            .unwrap();
        assert_eq!(delta["event"]["kind"], "event");
        assert_eq!(delta["event"]["event"]["sequence"], 2);
        assert_eq!(delta["event"]["event"]["payload"]["activity"], changed);
        assert!(d.next(method, thread(3, json!([changed])), false).is_none());
        // Suppressed global changes cannot hide a sequence gap from Android.
        let gap = thread(4, json!([activity("a", "changed again")]));
        assert_eq!(d.next(method, gap.clone(), false), Some(gap));
        let mut moved = activity("a", "changed again");
        moved["sequence"] = json!(2);
        let moved = thread(5, json!([moved]));
        assert_eq!(d.next(method, moved.clone(), false), Some(moved));
    }
    #[test]
    fn thread_controls_history_removals_and_errors_are_never_hidden() {
        let method = "orchestration.subscribeThread";
        let mut d = Delivery::default();
        let original = thread(1, json!([activity("a", "saved")]));
        d.next(method, original.clone(), true);
        for key in [
            "session",
            "messages",
            "historyPage",
            "hasPendingUserInput",
            "hasPendingApprovals",
        ] {
            let mut updated = original.clone();
            updated["event"]["snapshot"]["thread"][key] = json!({"changed":true});
            assert_eq!(d.next(method, updated.clone(), false), Some(updated));
        }
        let removed = thread(20, json!([]));
        assert_eq!(
            d.next(method, removed.clone(), false),
            Some(removed.clone())
        );
        let error = json!({"id":"thread-watch","error":{"message":"Temporary connection error"}});
        assert_eq!(d.next(method, error.clone(), false), Some(error));
        assert_eq!(d.next(method, removed.clone(), false), Some(removed));
    }
    fn shell(sequence: u64, threads: Value) -> Value {
        json!({"id":"shell","event":{"kind":"snapshot","snapshot":{
            "snapshotSequence":sequence,"updatedAt":sequence.to_string(),
            "projects":[],"threads":threads,"nativeProjects":[],"nativeThreads":[]}}})
    }
    fn row(id: &str, status: &str) -> Value {
        json!({"id":id,"session":{"status":status},"updatedAt":"saved-time"})
    }
    #[test]
    fn initial_reconnect_and_explicit_refresh_are_complete_but_unchanged_polls_are_quiet() {
        let mut delivery = Delivery::default();
        let first = shell(1, json!([row("a", "running")]));
        assert_eq!(
            delivery.next("orchestration.subscribeShell", first.clone(), false),
            Some(first.clone())
        );
        assert!(delivery
            .next(
                "orchestration.subscribeShell",
                shell(2, json!([row("a", "running")])),
                false
            )
            .is_none());
        assert_eq!(
            delivery.next("orchestration.subscribeShell", first.clone(), true),
            Some(first.clone())
        );
        assert_eq!(
            Delivery::default().next("orchestration.subscribeShell", first.clone(), false),
            Some(first)
        );
    }
    #[test]
    fn one_changed_row_uses_the_existing_android_delta_contract() {
        let mut delivery = Delivery::default();
        let rows: Vec<_> = (0..540)
            .map(|i| row(&format!("chat-{i}"), "idle"))
            .collect();
        delivery.next("orchestration.subscribeShell", shell(5, json!(rows)), true);
        let mut changed = rows.clone();
        changed[3] = row("chat-3", "running");
        changed.reverse(); // Ordering alone must not replace the whole list.
        let update = delivery
            .next(
                "orchestration.subscribeShell",
                shell(6, json!(changed)),
                false,
            )
            .unwrap();
        assert_eq!(
            update["event"],
            json!({"kind":"thread-upserted","sequence":6,"thread":row("chat-3","running")})
        );
        assert!(update.to_string().len() < 256);
    }
    #[test]
    fn deletion_and_multiple_changes_are_not_lost_by_sequence_deduplication() {
        let mut delivery = Delivery::default();
        delivery.next(
            "orchestration.subscribeShell",
            shell(1, json!([row("a", "idle"), row("b", "idle")])),
            true,
        );
        let removed = delivery
            .next(
                "orchestration.subscribeShell",
                shell(2, json!([row("a", "idle")])),
                false,
            )
            .unwrap();
        assert_eq!(
            removed["event"],
            json!({"kind":"thread-removed","sequence":2,"threadId":"b"})
        );
        let multiple = shell(3, json!([row("a", "running"), row("c", "idle")]));
        assert_eq!(
            delivery.next("orchestration.subscribeShell", multiple.clone(), false),
            Some(multiple)
        );
        let equal_sequence = shell(3, json!([row("a", "idle"), row("c", "idle")]));
        assert_eq!(
            delivery.next(
                "orchestration.subscribeShell",
                equal_sequence.clone(),
                false
            ),
            Some(equal_sequence)
        );
    }
    #[test]
    fn config_read_time_is_ignored_but_real_changes_and_errors_are_delivered() {
        let mut delivery = Delivery::default();
        let first = json!({"id":1,"event":{"type":"snapshot","config":{"providers":[{"checkedAt":"a","status":"ready"}]}}});
        assert!(delivery
            .next("subscribeServerConfig", first.clone(), true)
            .is_some());
        let mut next = first;
        next["event"]["config"]["providers"][0]["checkedAt"] = json!("b");
        assert!(delivery
            .next("subscribeServerConfig", next.clone(), false)
            .is_none());
        next["event"]["config"]["providers"][0]["status"] = json!("error");
        assert_eq!(
            delivery.next("subscribeServerConfig", next.clone(), false),
            Some(next)
        );
        let error = json!({"id":1,"error":{"message":"temporarily unavailable"}});
        assert_eq!(
            delivery.next("subscribeServerConfig", error.clone(), false),
            Some(error)
        );
    }
}
