use crate::{
    chat_projection::{now, s},
    store::Store,
    workspace,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;
impl Store {
    pub fn projects(&self, workspace: &Path) -> rusqlite::Result<Value> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='native.projects'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match value {
            Some(v) => serde_json::from_str(&v).map_err(|_| rusqlite::Error::InvalidQuery),
            None => Ok(json!([project(
                "native-test-project",
                "Native Rust test",
                workspace
            )])),
        }
    }
    pub fn save_projects(
        &mut self,
        projects: &Value,
        receipt: (&str, &str),
    ) -> rusqlite::Result<()> {
        self.check_history(
            (projects.to_string().len() as u64)
                .saturating_mul(2)
                .saturating_add(65536),
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO metadata(key,value) VALUES('native.projects',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[projects.to_string()])?;
        tx.execute(
            "INSERT INTO chat_commands(id,digest) VALUES(?1,?2)",
            params![receipt.0, receipt.1],
        )?;
        tx.commit()
    }
}
fn project(id: &str, title: &str, root: &Path) -> Value {
    json!({"id":id,"title":title,"workspaceRoot":root,"repositoryIdentity":null,"defaultModelSelection":null,"scripts":[],"createdAt":now(),"updatedAt":now()})
}
pub fn change(base: &Path, projects: &Value, c: &Value) -> Result<Value, String> {
    let id = s(&c["projectId"]);
    if id.is_empty() || id.len() > 128 || id == "codex-project-chats" {
        return Err("Project ID required".into());
    }
    if c.get("createWorkspaceRootIfMissing")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("Folder creation must be explicitly true or false".into());
    }
    if c.get("title").is_some_and(|v| {
        v.as_str()
            .is_none_or(|s| s.trim().is_empty() || s.len() > 256)
    }) {
        return Err("A project title of up to 256 bytes is required".into());
    }
    let scripts = c.get("scripts").map(validate_scripts).transpose()?;
    let mut next = projects.as_array().ok_or("Invalid project store")?.clone();
    let existing = next.iter().position(|p| p["id"] == id);
    match s(&c["type"]) {
        "project.create" => {
            if existing.is_some() {
                return Err("Project already exists".into());
            }
            if next.len() >= 256 {
                return Err("Project capacity reached".into());
            }
            let root = workspace::directory(
                base,
                s(&c["workspaceRoot"]),
                c["createWorkspaceRootIfMissing"] == true,
            )?;
            if next
                .iter()
                .any(|p| p["workspaceRoot"].as_str() == root.to_str())
            {
                return Err("Folder already registered".into());
            }
            let name = if s(&c["title"]).is_empty() {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                s(&c["title"]).into()
            };
            if name.len() > 256 {
                return Err("Project title exceeds 256 bytes".into());
            }
            let mut value = project(id, &name, &root);
            if let Some(model) = c.get("defaultModelSelection") {
                value["defaultModelSelection"] = model.clone();
            }
            if let Some(scripts) = scripts {
                value["scripts"] = scripts;
            }
            next.push(value);
        }
        "project.meta.update" => {
            let i = existing.ok_or("Project not found")?;
            if c.get("workspaceRoot").is_some() {
                let root = workspace::directory(base, s(&c["workspaceRoot"]), false)?;
                if next
                    .iter()
                    .enumerate()
                    .any(|(j, p)| j != i && p["workspaceRoot"].as_str() == root.to_str())
                {
                    return Err("Folder already registered".into());
                }
                next[i]["workspaceRoot"] = json!(root);
            }
            if c.get("title").is_some() {
                let title = s(&c["title"]);
                if title.trim().is_empty() || title.len() > 256 {
                    return Err("Invalid project title".into());
                }
                next[i]["title"] = json!(title);
            }
            if let Some(model) = c.get("defaultModelSelection") {
                next[i]["defaultModelSelection"] = model.clone();
            }
            if let Some(scripts) = scripts {
                next[i]["scripts"] = scripts;
            }
            next[i]["updatedAt"] = json!(now());
        }
        "project.delete" => {
            next.remove(existing.ok_or("Project not found")?);
        }
        _ => return Err("Unsupported project operation".into()),
    }
    Ok(json!(next))
}

pub(crate) fn validate_scripts(value: &Value) -> Result<Value, String> {
    let rows = value.as_array().ok_or("Project scripts must be a list")?;
    if rows.len() > 16 {
        return Err("At most 16 project scripts are supported".into());
    }
    let mut saved = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for row in rows {
        let id = s(&row["id"]);
        let name = s(&row["name"]);
        let command = s(&row["command"]);
        if id.is_empty()
            || id.len() > 128
            || !ids.insert(id.to_owned())
            || name.trim().is_empty()
            || name.len() > 128
            || command.trim().is_empty()
            || command.len() > 8192
            || command.contains('\0')
        {
            return Err("Invalid or duplicate project script".into());
        }
        if !["play", "test", "lint", "configure", "build", "debug"].contains(&s(&row["icon"]))
            || !row["runOnWorktreeCreate"].is_boolean()
            || row.get("autoOpenPreview").is_some_and(|v| !v.is_boolean())
        {
            return Err("Invalid project script settings".into());
        }
        let mut clean = json!({"id":id,"name":name,"command":command,"icon":row["icon"],"runOnWorktreeCreate":row["runOnWorktreeCreate"]});
        if let Some(url) = row.get("previewUrl") {
            let text = url.as_str().ok_or("Invalid script preview URL")?;
            if text.len() > 2048
                || text.chars().any(char::is_control)
                || !(text.starts_with("http://") || text.starts_with("https://"))
            {
                return Err("Script preview must use an HTTP or HTTPS URL".into());
            }
            clean["previewUrl"] = url.clone();
        }
        if let Some(v) = row.get("autoOpenPreview") {
            clean["autoOpenPreview"] = v.clone();
        }
        saved.push(clean);
    }
    Ok(json!(saved))
}
