//! Live discovery of Desktop chats, independent of the one-time upgrade export.
//! Uses Codex's read-only thread/list contract, never resume/start or activation.
use crate::{
    agent::SharedServer,
    chat::{AgentConfig, AgentKind, Chat},
    chat_projection::s,
    codex_history_reader::Reader,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path};

const LIMIT: usize = 64;
const ERROR: &str = "Codex chat inventory could not be read";
pub(crate) fn ready<'a>(mut active: impl Iterator<Item = &'a Chat>) -> bool {
    !active.any(|c| c.agent == AgentKind::Codex && !c.external_codex && c.native_id.is_empty())
}
pub(crate) struct Page {
    pub rows: Vec<Value>,
    pub next: Option<String>,
}
fn params(cursor: Option<&str>) -> Value {
    // Omit section/cwd filters: null section means *unsectioned*, not all.
    // Empty providers means all providers. Interactive sources exclude subagents.
    json!({"cursor":cursor,"limit":LIMIT,"sortKey":"updated_at","sortDirection":"desc",
        "modelProviders":[],"sourceKinds":["cli","vscode","appServer"],"archived":false,"useStateDbOnly":true})
}
fn page(value: Value, previous: Option<&str>) -> Result<Page, String> {
    let rows = value["data"]
        .as_array()
        .filter(|v| v.len() <= LIMIT)
        .ok_or(ERROR)?;
    let next = match &value["nextCursor"] {
        Value::Null => None,
        Value::String(v)
            if !v.is_empty()
                && v.len() <= 8192
                && !v.chars().any(char::is_control)
                && Some(v.as_str()) != previous =>
        {
            Some(v.clone())
        }
        _ => return Err(ERROR.into()),
    };
    if rows.is_empty() && next.is_some() {
        return Err(ERROR.into());
    }
    Ok(Page {
        rows: rows.clone(),
        next,
    })
}
pub(crate) async fn scan(mut config: AgentConfig, cursor: Option<String>) -> Result<Page, String> {
    let Some(SharedServer::Desktop {
        reader_executable,
        reader_workspace,
        ..
    }) = config.shared_server.take()
    else {
        return Err(ERROR.into());
    };
    config.executable = reader_executable;
    let mut reader = Reader::open(&config, &reader_workspace).await?;
    let result = async {
        // Always revisit the newest page, including while backfilling older pages.
        // An insertion before a saved cursor must not wait for the entire scan.
        let mut head = page(reader.rpc("thread/list", params(None)).await?, None)?;
        if let Some(cursor) = cursor.filter(|c| Some(c) != head.next.as_ref()) {
            let older = page(
                reader.rpc("thread/list", params(Some(&cursor))).await?,
                Some(&cursor),
            )?;
            head.rows.extend(older.rows);
            head.next = older.next;
        } else if let Some(cursor) = head.next.clone() {
            let older = page(
                reader.rpc("thread/list", params(Some(&cursor))).await?,
                Some(&cursor),
            )?;
            head.rows.extend(older.rows);
            head.next = older.next;
        }
        Ok(head)
    }
    .await;
    reader.close().await;
    result
}

fn timestamp(value: &Value) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(value.as_i64()?)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}
fn title(row: &Value) -> String {
    let name = row["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            row["preview"]
                .as_str()
                .filter(|s| !s.trim().starts_with('#'))
        })
        .unwrap_or("New Codex chat");
    let mut result = String::new();
    for ch in name.trim().chars() {
        if result.len() + ch.len_utf8() > 256 {
            break;
        }
        result.push(if ch.is_control() { ' ' } else { ch });
    }
    result
}
fn eligible(row: &Value) -> bool {
    uuid::Uuid::parse_str(s(&row["id"])).is_ok()
        && matches!(s(&row["source"]), "cli" | "vscode" | "appServer")
        && row["ephemeral"] != true
        && row["archived"] != true
        && row["parentThreadId"].is_null()
        && (row["threadSource"].is_null() || row["threadSource"] == "user")
        && row["cwd"].as_str().is_some_and(|cwd| {
            !cwd.is_empty()
                && cwd.len() <= 4096
                && !cwd.chars().any(char::is_control)
                && Path::new(cwd).is_absolute()
        })
        && timestamp(&row["createdAt"]).is_some()
        && timestamp(&row["updatedAt"]).is_some()
}

/// Prepare metadata only. Existing aliases, hidden chats, settings and histories
/// are authoritative; discovery cannot resurrect them or create duplicate owners.
pub(crate) fn changes(
    rows: Vec<Value>,
    chats: &HashMap<String, Chat>,
    projects: &Value,
    workspace: &Path,
    default_model: &str,
) -> Result<(Value, Vec<Chat>), String> {
    let mut projects = projects.as_array().ok_or(ERROR)?.clone();
    let mut known: HashMap<_, _> = chats
        .values()
        .filter(|c| c.agent == AgentKind::Codex)
        .map(|c| (c.native_id.clone(), c))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut changed = Vec::new();
    let mut added = 0usize;
    for row in rows {
        if !eligible(&row) {
            continue;
        }
        let native = s(&row["id"]);
        if !seen.insert(native.to_owned()) {
            continue;
        }
        let name = title(&row);
        if let Some(existing) = known.remove(native) {
            // Only follow native title changes for discovered chats whose name
            // the user has not subsequently overridden in Android.
            if existing.external_codex
                && existing.thread["deletedAt"].is_null()
                && existing.thread["archivedAt"].is_null()
                && existing.discovered_codex_title.as_deref() == existing.thread["title"].as_str()
                && existing.discovered_codex_title.is_some()
                && existing.thread["title"] != name
            {
                let mut chat = existing.clone();
                chat.thread["title"] = json!(name);
                chat.discovered_codex_title = Some(name);
                changed.push(chat);
            }
            continue;
        }
        if chats.len() + added >= 10000 || chats.contains_key(native) {
            continue;
        }
        let cwd = s(&row["cwd"]);
        let project = projects.iter().find(|p| p["workspaceRoot"] == cwd);
        let project_id = if let Some(project) = project {
            project["id"].clone()
        } else if Path::new(cwd) == workspace {
            json!("codex-project-chats")
        } else {
            if projects.len() >= 256 {
                continue;
            }
            let id = format!(
                "codex-discovered-project-{:x}",
                Sha256::digest(cwd.as_bytes())
            );
            let label = Path::new(cwd)
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or(cwd);
            projects.push(json!({"id":id,"title":label,"workspaceRoot":cwd,"repositoryIdentity":null,
                "defaultModelSelection":null,"scripts":[],"createdAt":timestamp(&row["createdAt"]),"updatedAt":timestamp(&row["updatedAt"])}));
            json!(id)
        };
        let model = row["model"]
            .as_str()
            .filter(|m| !m.is_empty() && m.len() <= 256)
            .unwrap_or(default_model);
        let options =
            crate::legacy_codex::options(&json!({"reasoningEffort":row["reasoningEffort"]}))
                .unwrap_or_default();
        let created = timestamp(&row["createdAt"]).unwrap();
        let updated = timestamp(&row["updatedAt"]).unwrap();
        let mut thread = json!({"id":native,"nativeSessionId":native,"projectId":project_id,"title":name,
            "worktreePath":cwd,"modelSelection":{"instanceId":"codex","model":model,"options":options},
            "runtimeMode":"approval-required","interactionMode":"default","createdAt":created,"updatedAt":updated,
            "branch":row["gitInfo"]["branch"],"latestTurn":null,"archivedAt":null,"deletedAt":null,
            "latestUserMessageAt":updated,"messages":[],"activities":[],"checkpoints":[],"proposedPlans":[],
            "hasPendingApprovals":false,"hasPendingUserInput":false,"hasActionableProposedPlan":false,
            "session":{"threadId":native,"providerName":"codex","providerInstanceId":"codex","status":"disconnected",
            "statusConfidence":"unknown","runtimeMode":"approval-required","activeTurnId":null,"lastError":null,"updatedAt":updated}});
        // A private reader's notLoaded status says nothing about the Desktop
        // owner's running turn. The existing read-only follower resolves it.
        if project_id == "codex-project-chats" {
            thread["workspaceKind"] = json!("projectless");
        }
        let chat: Chat =
            serde_json::from_value(json!({"agent":"codex","thread":thread,"native_id":native,
            "resumed":true,"external_codex":true,"paused":true,"discovered_codex_title":name}))
            .map_err(|_| ERROR)?;
        changed.push(chat);
        added += 1;
    }
    Ok((json!(projects), changed))
}

impl crate::store::Store {
    /// Fork-based prompt edits retain their old native chat as a revision.
    /// Do not rediscover that unchanged revision as a second Android chat.
    /// A later native update still permits discovery if the user continues it.
    pub(crate) fn without_unchanged_edit_revisions(
        &self,
        rows: Vec<Value>,
    ) -> rusqlite::Result<Vec<Value>> {
        let mut query = self.db.prepare("SELECT MAX(json_extract(data,'$.thread.updatedAt')) FROM chat_revisions WHERE json_extract(data,'$.agent')='codex' AND json_extract(data,'$.native_id')=?1")?;
        let mut visible = Vec::with_capacity(rows.len());
        for row in rows {
            let saved: Option<String> = query.query_row([s(&row["id"])], |r| r.get(0))?;
            let saved = saved.and_then(|v| {
                time::OffsetDateTime::parse(&v, &time::format_description::well_known::Rfc3339).ok()
            });
            let unchanged = saved
                .zip(row["updatedAt"].as_i64())
                .is_some_and(|(saved, native)| native <= saved.unix_timestamp());
            if !unchanged {
                visible.push(row);
            }
        }
        Ok(visible)
    }

    /// Commit the new project/header inventory atomically. Existing transcripts,
    /// cursors, settings, queues and pairing records are never rewritten.
    pub(crate) fn save_codex_inventory(
        &mut self,
        projects: &Value,
        states: &[Value],
    ) -> rusqlite::Result<()> {
        self.check_history(1024 * 1024)
            .map_err(rusqlite::Error::InvalidParameterName)?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO metadata(key,value) VALUES('native.projects',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [projects.to_string()])?;
        for state in states {
            let id = s(&state["thread"]["id"]);
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if exists {
                let updated = tx.execute("UPDATE chats SET data=json_set(data,'$.thread.title',?2,'$.discovered_codex_title',?2) WHERE id=?1 AND agent='codex' AND json_extract(data,'$.external_codex')=1 AND json_extract(data,'$.discovered_codex_title')=json_extract(data,'$.thread.title')",
                    rusqlite::params![id, state["thread"]["title"].as_str()])?;
                if updated != 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            } else {
                let inserted = tx.execute("INSERT INTO chats(id,agent,data) SELECT ?1,'codex',?2 WHERE NOT EXISTS(SELECT 1 FROM chats WHERE agent='codex' AND json_extract(data,'$.native_id')=?3)",
                    rusqlite::params![id,state.to_string(),s(&state["native_id"])])?;
                if inserted != 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
        }
        tx.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "12345678-1234-4234-8234-123456789abc";
    #[test]
    fn edit_revisions_do_not_reappear_but_later_native_updates_do() {
        let root = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(root.path()).unwrap();
        store.init_chats().unwrap();
        let revision = json!({"agent":"codex","native_id":ID,"thread":{"updatedAt":"2023-11-14T22:13:30.900Z"}});
        store
            .db
            .execute(
                "INSERT INTO chat_revisions VALUES('revision','chat',?1)",
                [revision.to_string()],
            )
            .unwrap();
        let other = json!({"id":"different","updatedAt":1700000010});
        let filtered = store
            .without_unchanged_edit_revisions(vec![row(), other.clone()])
            .unwrap();
        assert_eq!(filtered, vec![other]);
        let mut continued = row();
        continued["updatedAt"] = json!(1700000011);
        assert_eq!(
            store
                .without_unchanged_edit_revisions(vec![continued.clone()])
                .unwrap(),
            vec![continued]
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM chat_revisions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    fn row() -> Value {
        json!({"id":ID,"name":"Extract and analyze ZIP","preview":"# Files mentioned by the user",
            "cwd":std::env::temp_dir().join("portable/Short-from Videos Creator"),"source":"vscode",
            "createdAt":1700000000,"updatedAt":1700000010,"status":{"type":"notLoaded"},"model":"gpt-test"})
    }
    fn changes_for(
        rows: Vec<Value>,
        chats: &HashMap<String, Chat>,
        projects: Value,
    ) -> (Value, Vec<Chat>) {
        changes(rows, chats, &projects, &std::env::temp_dir(), "fallback").unwrap()
    }
    #[test]
    fn discovers_external_disk_chat_without_claiming_owner_or_idle_status() {
        let r = row();
        let (projects, chats) = changes_for(
            vec![r.clone()],
            &HashMap::new(),
            json!([{"id":"desktop-project","workspaceRoot":r["cwd"]}]),
        );
        assert_eq!(projects.as_array().unwrap().len(), 1);
        let c = &chats[0];
        assert!(c.external_codex && c.resumed && c.paused);
        assert_eq!(c.thread["title"], "Extract and analyze ZIP");
        assert_eq!(c.thread["projectId"], "desktop-project");
        assert_eq!(c.thread["session"]["statusConfidence"], "unknown");
        assert_eq!(c.thread["messages"], json!([]));
    }
    #[test]
    fn aliases_hidden_owned_and_duplicate_rows_do_not_create_new_chats() {
        let (_, mut c) = changes_for(vec![row(), row()], &HashMap::new(), json!([]));
        assert_eq!(c.len(), 1);
        let mut chat = c.remove(0);
        chat.thread["id"] = json!("old-public-alias");
        chat.thread["deletedAt"] = json!("deleted");
        chat.thread["title"] = json!("User title");
        let mut known = HashMap::from([("old-public-alias".into(), chat.clone())]);
        assert!(changes_for(vec![row()], &known, json!([])).1.is_empty());
        chat.thread["deletedAt"] = Value::Null;
        chat.external_codex = false;
        known.insert("old-public-alias".into(), chat);
        assert!(changes_for(vec![row()], &known, json!([])).1.is_empty());
    }
    #[test]
    fn skips_subagents_ephemeral_invalid_and_archived_rows() {
        for (key, value) in [
            ("source", json!({"subAgent":{}})),
            ("parentThreadId", json!(ID)),
            ("ephemeral", json!(true)),
            ("archived", json!(true)),
            ("cwd", json!("relative")),
            ("id", json!("invalid")),
            ("threadSource", json!("subagent")),
        ] {
            let mut r = row();
            r[key] = value;
            assert!(
                changes_for(vec![r], &HashMap::new(), json!([]))
                    .1
                    .is_empty(),
                "{key}"
            );
        }
    }
    #[test]
    fn title_updates_preserve_local_overrides_and_saved_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let (p, mut c) = changes_for(vec![row()], &HashMap::new(), json!([]));
        let mut chat = c.remove(0);
        let mut state = serde_json::to_value(&chat).unwrap();
        state["thread"]["messages"] = json!([{"id":"saved","role":"user","text":"Keep me","createdAt":"2026-01-01T00:00:00Z"}]);
        store.save_chat(&state, None).unwrap();
        let before: String = store
            .db
            .query_row("SELECT data FROM chat_rows WHERE id='saved'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut r = row();
        r["name"] = json!("Renamed by Desktop");
        let known = HashMap::from([(ID.into(), chat.clone())]);
        let (_, changes) = changes_for(vec![r.clone()], &known, p.clone());
        assert_eq!(changes.len(), 1);
        store
            .save_codex_inventory(&p, &[serde_json::to_value(&changes[0]).unwrap()])
            .unwrap();
        assert_eq!(
            store.load_chat(ID).unwrap()["thread"]["title"],
            "Renamed by Desktop"
        );
        let after: String = store
            .db
            .query_row("SELECT data FROM chat_rows WHERE id='saved'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(before, after);
        chat.thread["title"] = json!("Local rename");
        assert!(changes_for(vec![r], &HashMap::from([(ID.into(), chat)]), p)
            .1
            .is_empty());
    }
    #[test]
    fn pagination_contract_covers_all_sections_and_bounds_retries() {
        let p = params(Some("cursor"));
        assert!(p.get("sectionId").is_none());
        assert!(p.get("cwd").is_none());
        assert_eq!(p["modelProviders"], json!([]));
        assert_eq!(p["useStateDbOnly"], true);
        assert!(page(json!({"data":[row()],"nextCursor":"same"}), Some("same")).is_err());
        assert!(page(json!({"data":[],"nextCursor":"next"}), None).is_err());
        assert!(page(json!({"data":vec![row();65]}), None).is_err());
        assert!(page(json!({"data":[row()],"nextCursor":"next"}), None).is_ok());
    }
    #[test]
    fn new_headers_commit_with_projects_and_duplicate_identity_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(dir.path()).unwrap();
        store.init_chats().unwrap();
        let (p, c) = changes_for(vec![row()], &HashMap::new(), json!([]));
        let state = serde_json::to_value(&c[0]).unwrap();
        store
            .save_codex_inventory(&p, std::slice::from_ref(&state))
            .unwrap();
        let mut alias = state;
        alias["thread"]["id"] = json!("duplicate");
        assert!(store.save_codex_inventory(&json!([]), &[alias]).is_err());
        assert_eq!(store.projects(dir.path()).unwrap(), p);
        assert_eq!(store.chat_headers().unwrap().len(), 1);
    }
    #[test]
    fn waits_for_android_worker_identity_before_importing_native_rows() {
        let (_, mut chats) = changes_for(vec![row()], &HashMap::new(), json!([]));
        let chat = &mut chats[0];
        chat.external_codex = false;
        chat.native_id.clear();
        assert!(!ready(chats.iter()));
        chats[0].native_id = ID.into();
        assert!(ready(chats.iter()));
        chats[0].native_id.clear();
        chats[0].agent = AgentKind::Claude;
        assert!(ready(chats.iter()));
    }
}
