//! A thin projection of Desktop's live feed. Never retain prompt/tool content.
use serde_json::Value;
use std::collections::BTreeMap;
const INVALID: &str = "Desktop execution state changed or is incomplete";
#[derive(Default)]
struct Turn {
    id: Option<String>,
    running: bool,
    compact: BTreeMap<usize, String>,
    item_count: usize,
}
fn compact(item: &Value) -> Option<String> {
    (item["type"] == "contextCompaction" && item["completed"] == false)
        .then(|| {
            item["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .map(str::to_owned)
        })
        .flatten()
}
fn items(value: &Value) -> Result<BTreeMap<usize, String>, String> {
    let rows = value
        .as_array()
        .filter(|v| v.len() <= 32768)
        .ok_or(INVALID)?;
    Ok(rows
        .iter()
        .enumerate()
        .filter_map(|(i, v)| compact(v).map(|id| (i, id)))
        .collect())
}
fn turn(v: &Value) -> Result<Turn, String> {
    if !v.is_object() || (v["status"] == "inProgress" && !v["items"].is_array()) {
        return Err(INVALID.into());
    }
    Ok(Turn {
        id: v
            .get("turnId")
            .or_else(|| v.get("id"))
            .and_then(Value::as_str)
            .filter(|id| id.len() <= 256)
            .map(str::to_owned),
        running: v["status"] == "inProgress",
        compact: if v["items"].is_null() {
            BTreeMap::new()
        } else {
            items(&v["items"])?
        },
        item_count: v["items"].as_array().map_or(0, Vec::len),
    })
}
#[derive(Default)]
pub(super) struct Phase {
    revision: Option<u64>,
    turns: BTreeMap<String, Turn>,
}
impl Phase {
    pub(super) fn ready(&self) -> bool {
        self.revision.is_some()
    }
    pub(super) fn compacting(&self, current_turn: &str) -> Option<(String, String)> {
        self.turns.values().find_map(|t| {
            let id = t.id.as_ref()?;
            (id == current_turn && t.running && uuid::Uuid::parse_str(id).is_ok())
                .then(|| {
                    t.compact
                        .values()
                        .next()
                        .map(|item| (id.clone(), item.clone()))
                })
                .flatten()
        })
    }
    pub(super) fn apply(&mut self, change: &Value, native: &str) -> Result<(), String> {
        let revision = change["revision"].as_u64().ok_or(INVALID)?;
        if self.revision.is_some_and(|old| revision < old) {
            return Err(INVALID.into());
        }
        if change["type"] == "snapshot" {
            let state = &change["conversationState"];
            if state["id"] != native || state["hostId"] != "local" {
                return Err(INVALID.into());
            }
            let mut next = BTreeMap::new();
            if let Some(rows) = state["turnHistory"]["history"]["entitiesByKey"].as_object() {
                if rows.len() > 512 {
                    return Err(INVALID.into());
                }
                for (key, row) in rows {
                    if key.len() > 1024 {
                        return Err(INVALID.into());
                    }
                    next.insert(format!("entity:{key}"), turn(row)?);
                }
            } else if let Some(rows) = state["turns"].as_array() {
                if rows.len() > 512 {
                    return Err(INVALID.into());
                }
                for (i, row) in rows.iter().enumerate() {
                    next.insert(format!("index:{i}"), turn(row)?);
                }
            } else {
                return Err(INVALID.into());
            }
            self.turns = next;
        } else if change["type"] == "patches" {
            if self.revision != change["baseRevision"].as_u64()
                || self.revision.is_none()
                || self.revision.is_some_and(|r| revision <= r)
            {
                return Err(INVALID.into());
            }
            let patches = change["patches"]
                .as_array()
                .filter(|a| a.len() <= 32768)
                .ok_or(INVALID)?;
            for patch in patches {
                let path = patch["path"].as_array().ok_or(INVALID)?;
                let op = patch["op"]
                    .as_str()
                    .filter(|v| matches!(*v, "add" | "replace" | "remove"))
                    .ok_or(INVALID)?;
                let (key, fields) = if path.first().is_some_and(|v| v == "turnHistory") {
                    if path.len() < 3 {
                        return Err(INVALID.into());
                    }
                    if path[1] != "history" || path[2] != "entitiesByKey" {
                        continue;
                    }
                    if path.len() < 4 {
                        return Err(INVALID.into());
                    }
                    (
                        format!(
                            "entity:{}",
                            path[3]
                                .as_str()
                                .filter(|s| s.len() <= 1024)
                                .ok_or(INVALID)?
                        ),
                        &path[4..],
                    )
                } else if path.first().is_some_and(|v| v == "turns") {
                    if path.len() < 2 {
                        return Err(INVALID.into());
                    }
                    (
                        format!("index:{}", path[1].as_u64().ok_or(INVALID)?),
                        &path[2..],
                    )
                } else {
                    continue;
                };
                if fields.is_empty() {
                    // Legacy numeric turn positions shift on insertion/removal;
                    // require a fresh snapshot rather than associating stale IDs.
                    if key.starts_with("index:") && op != "replace" {
                        return Err(INVALID.into());
                    }
                    if patch["op"] == "remove" {
                        self.turns.remove(&key);
                    } else {
                        self.turns.insert(key, turn(&patch["value"])?);
                    }
                    if self.turns.len() > 512 {
                        return Err(INVALID.into());
                    }
                    continue;
                }
                let entry = self.turns.get_mut(&key).ok_or(INVALID)?;
                let value = &patch["value"];
                match fields[0].as_str() {
                    Some("turnId" | "id") => {
                        if fields.len() != 1 || value.as_str().is_some_and(|s| s.len() > 256) {
                            return Err(INVALID.into());
                        }
                        entry.id = value.as_str().map(str::to_owned);
                    }
                    Some("status") => {
                        if fields.len() != 1 {
                            return Err(INVALID.into());
                        }
                        entry.running = value == "inProgress";
                    }
                    Some("items") => {
                        if fields.len() == 1 {
                            entry.compact = items(value)?;
                            entry.item_count = value.as_array().ok_or(INVALID)?.len();
                        } else {
                            let index =
                                fields[1].as_u64().filter(|v| *v < 32768).ok_or(INVALID)? as usize;
                            if fields.len() == 2 {
                                if index > entry.item_count
                                    || (op != "add" && index == entry.item_count)
                                {
                                    return Err(INVALID.into());
                                }
                                if op == "add" {
                                    if entry.item_count >= 32768 {
                                        return Err(INVALID.into());
                                    }
                                    entry.compact = std::mem::take(&mut entry.compact)
                                        .into_iter()
                                        .map(|(i, id)| (if i >= index { i + 1 } else { i }, id))
                                        .collect();
                                    entry.item_count += 1;
                                }
                                entry.compact.remove(&index);
                                if op == "remove" {
                                    entry.compact = std::mem::take(&mut entry.compact)
                                        .into_iter()
                                        .map(|(i, id)| (if i > index { i - 1 } else { i }, id))
                                        .collect();
                                    entry.item_count -= 1;
                                } else if let Some(id) = compact(value) {
                                    entry.compact.insert(index, id);
                                }
                            } else if fields.len() == 3 && fields[2] == "completed" && value == true
                            {
                                entry.compact.remove(&index);
                            } else if entry.compact.contains_key(&index)
                                || (fields.len() == 3
                                    && matches!(fields[2].as_str(), Some("type" | "completed")))
                            {
                                return Err(INVALID.into());
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else {
            return Err(INVALID.into());
        }
        self.revision = Some(revision);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const N: &str = "11111111-1111-4111-8111-111111111111";
    const T: &str = "22222222-2222-4222-8222-222222222222";
    #[test]
    fn paginated_owner_compaction_tracks_native_identity_and_completion_without_content() {
        let mut p = Phase::default();
        p.apply(&json!({"type":"snapshot","revision":1,"conversationState":{"id":N,"hostId":"local","turns":[],"turnHistory":{"history":{"entitiesByKey":{}}}}}),N).unwrap();
        let patch = |base, fields: Value, value: Value| json!({"type":"patches","baseRevision":base,"revision":base+1,"patches":[{"op":"add","path":fields,"value":value}]});
        p.apply(&patch(1,json!(["turnHistory","history","entitiesByKey","tail"]),json!({"turnId":null,"status":"inProgress","params":{"input":"PRIVATE"},"items":[{"type":"contextCompaction","id":"compact","completed":false}]})),N).unwrap();
        assert_eq!(p.compacting(T), None);
        p.apply(
            &patch(
                2,
                json!(["turnHistory", "history", "entitiesByKey", "tail", "turnId"]),
                json!(T),
            ),
            N,
        )
        .unwrap();
        assert_eq!(p.compacting(T), Some((T.into(), "compact".into())));
        p.apply(
            &patch(
                3,
                json!([
                    "turnHistory",
                    "history",
                    "entitiesByKey",
                    "tail",
                    "items",
                    0,
                    "completed"
                ]),
                json!(true),
            ),
            N,
        )
        .unwrap();
        assert_eq!(p.compacting(T), None);
        assert!(p.apply(&json!({"type":"snapshot","revision":1,"conversationState":{"id":N,"hostId":"local","turns":[]}}),N).is_err());
        assert!(p
            .apply(
                &patch(1, json!(["threadRuntimeStatus"]), json!({"type":"active"})),
                N
            )
            .is_err());
    }
    #[test]
    fn completed_snapshot_and_foreign_identity_cannot_claim_active_compaction() {
        let mut p = Phase::default();
        let snapshot = json!({"type":"snapshot","revision":1,"conversationState":{"id":N,"hostId":"local","turns":[{"id":T,"status":"completed","items":[{"type":"contextCompaction","id":"compact","completed":false}]}]}});
        p.apply(&snapshot, N).unwrap();
        assert!(p.ready());
        assert_eq!(p.compacting(T), None);
        assert!(p.apply(&snapshot, T).is_err());
    }

    #[test]
    fn inserted_and_removed_tools_do_not_shift_compaction_onto_a_different_item() {
        let mut p = Phase::default();
        p.apply(&json!({"type":"snapshot","revision":1,"conversationState":{"id":N,"hostId":"local","turnHistory":{"history":{"entitiesByKey":{"tail":{"turnId":T,"status":"inProgress","items":[{"type":"contextCompaction","id":"compact","completed":false}]}}}}}}), N).unwrap();
        let patch = |base, op, path: Value, value: Value| json!({"type":"patches","baseRevision":base,"revision":base+1,"patches":[{"op":op,"path":path,"value":value}]});
        p.apply(
            &patch(
                1,
                "add",
                json!([
                    "turnHistory",
                    "history",
                    "entitiesByKey",
                    "tail",
                    "items",
                    0
                ]),
                json!({"type":"commandExecution","id":"tool"}),
            ),
            N,
        )
        .unwrap();
        assert_eq!(p.compacting(T), Some((T.into(), "compact".into())));
        p.apply(
            &patch(
                2,
                "remove",
                json!([
                    "turnHistory",
                    "history",
                    "entitiesByKey",
                    "tail",
                    "items",
                    0
                ]),
                Value::Null,
            ),
            N,
        )
        .unwrap();
        assert_eq!(p.compacting(T), Some((T.into(), "compact".into())));
        p.apply(
            &patch(
                3,
                "replace",
                json!([
                    "turnHistory",
                    "history",
                    "entitiesByKey",
                    "tail",
                    "items",
                    0,
                    "completed"
                ]),
                json!(true),
            ),
            N,
        )
        .unwrap();
        assert_eq!(p.compacting(T), None);
    }

    #[test]
    fn stale_compaction_cannot_mask_the_current_turn_and_missing_active_items_are_unknown() {
        let mut p = Phase::default();
        let old = "33333333-3333-4333-8333-333333333333";
        let snapshot = json!({"type":"snapshot","revision":1,"conversationState":{"id":N,"hostId":"local",
            "turns":[{"id":old,"status":"inProgress","items":[{"type":"contextCompaction","id":"old","completed":false}]},
                     {"id":T,"status":"inProgress","items":[{"type":"contextCompaction","id":"current","completed":false}]}]}});
        p.apply(&snapshot, N).unwrap();
        assert_eq!(p.compacting(T), Some((T.into(), "current".into())));
        assert_eq!(p.compacting(N), None);
        let mut incomplete = snapshot;
        incomplete["conversationState"]["turns"][1]["items"] = Value::Null;
        assert!(p.apply(&incomplete, N).is_err());
    }
}
