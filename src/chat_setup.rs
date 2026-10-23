//! Durable bridge-side Claude switches. These never edit Desktop preferences.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    pub enabled: bool,
    pub desktop_sync: bool,
    #[serde(default)]
    pub binary: Option<PathBuf>,
}
impl Settings {
    pub async fn load(db: &AuthService, remote_control: bool) -> Result<Self, String> {
        let saved = db
            .call(|a| -> rusqlite::Result<Option<String>> {
                use rusqlite::OptionalExtension;
                a.store
                    .db
                    .query_row(
                        "SELECT value FROM metadata WHERE key='native.claudeSettings'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
            })
            .await?
            .map_err(|_| "Claude bridge settings are unavailable")?;
        match saved {
            Some(v) if v.len() <= 4096 => serde_json::from_str(&v).map_err(|_| {
                "Saved Claude bridge settings are invalid; no settings were overwritten".into()
            }),
            Some(_) => Err("Saved Claude bridge settings exceed their limit".into()),
            None => Ok(Self {
                enabled: true,
                desktop_sync: remote_control,
                binary: None,
            }),
        }
    }
}

impl Hub {
    pub(super) async fn configure_claude(&mut self, p: &Value) -> Result<Value, String> {
        let object = p
            .as_object()
            .filter(|o| !o.is_empty())
            .ok_or("Provide Claude bridge settings")?;
        if object.iter().any(|(k, v)| !match k.as_str() {
            "enabled" | "desktopSync" => v.is_boolean(),
            "binary" => v.is_string(),
            _ => false,
        }) {
            return Err("Provide enabled/desktopSync booleans and an executable path".into());
        }
        let binary = match p.get("binary") {
            Some(v) => selected_binary(v.as_str().unwrap())?,
            None => self.claude_binary_override.clone(),
        };
        let executable = binary
            .clone()
            .unwrap_or_else(|| self.claude_default_binary.clone());
        let binary_changed = executable != self.config.claude.executable;
        let next = Settings {
            binary,
            enabled: p["enabled"].as_bool().unwrap_or(self.claude_enabled),
            desktop_sync: p["desktopSync"]
                .as_bool()
                .unwrap_or(self.config.claude.remote_control),
        };
        if next.enabled == self.claude_enabled
            && next.desktop_sync == self.config.claude.remote_control
            && next.binary == self.claude_binary_override
        {
            return Ok(json!({}));
        }
        let reconnect = binary_changed
            || next.enabled != self.claude_enabled
            || next.desktop_sync != self.config.claude.remote_control;
        // The native owner can be running while its observer is temporarily
        // absent. Check saved state as well as live workers and pending controls.
        if self.chats.iter().any(|(id, c)| {
            c.agent == AgentKind::Claude
                && (self.busy(id)
                    || self.tool_controls.contains(id)
                    || !c.queue.is_empty()
                    || matches!(
                        s(&c.thread["session"]["status"]),
                        "running" | "starting" | "stopping"
                    )
                    || c.native_control
                        .as_ref()
                        .is_some_and(|i| i.result.is_none())
                    || c.external_stop.is_some())
        }) {
            return Err("Finish Claude tasks, queued messages and pending controls before changing bridge setup".into());
        }
        let data = serde_json::to_string(&next).map_err(|_| "Cannot encode Claude settings")?;
        // Save before disconnecting anything; a failed database write leaves
        // workers and configuration untouched. Restart reads the same switches.
        let mut models = self.models.clone();
        if binary_changed {
            models.remove(AgentKind::Claude.instance());
        }
        let catalog = serde_json::to_string(&models).map_err(|_| "Cannot encode model catalog")?;
        self.db.call(move |a| -> rusqlite::Result<()> {
            let tx=a.store.db.transaction()?;
            tx.execute("INSERT INTO metadata(key,value) VALUES('native.claudeSettings',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[data])?;
            if binary_changed {tx.execute("INSERT INTO metadata(key,value) VALUES('native.models',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[catalog])?;}
            tx.commit()
        }).await?.map_err(|_| "Cannot save Claude bridge settings; nothing changed")?;
        self.claude_enabled = next.enabled;
        self.config.claude.remote_control = next.desktop_sync;
        self.claude_binary_override = next.binary;
        self.config.claude.executable = executable;
        // Saving the already-effective path (or clearing an equivalent override)
        // changes only persistence, not the worker or its outstanding history.
        if !reconnect {
            return Ok(json!({}));
        }
        if binary_changed {
            self.models = models;
            self.discovery_errors.remove(AgentKind::Claude.instance());
            self.usage_cache = None;
        }
        let ids: Vec<_> = self
            .active
            .keys()
            .filter(|id| self.chats[*id].agent == AgentKind::Claude)
            .cloned()
            .collect();
        for id in ids {
            // Only idle connections reach here. External owners are detached,
            // never interrupted. Codex workers are excluded.
            self.end(
                &id,
                "interrupted",
                Some("Claude bridge setup changed. History is retained."),
            )
            .await;
        }
        self.claude_recovery = crate::claude_reconnect::Recovery::default();
        if next.enabled && next.desktop_sync {
            let mut ids: Vec<_> = self
                .chats
                .iter()
                .filter(|(_, c)| claude_recoverable(c))
                .map(|(id, c)| (s(&c.thread["updatedAt"]).to_owned(), id.clone()))
                .collect();
            ids.sort_by(|a, b| b.cmp(a));
            for (_, id) in ids.into_iter().take(crate::claude_reconnect::HOST_LIMIT) {
                self.claude_recovery.remember(&id, Instant::now());
            }
        }
        self.dirty = true;
        Ok(json!({}))
    }
}

fn selected_binary(value: &str) -> Result<Option<PathBuf>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return Err("Choose an installed Claude executable using its full path".into());
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() || !path.is_file() {
        return Err("Choose an installed Claude executable using its full path".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&path)
            .map_err(|_| "Cannot inspect Claude executable")?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err("The selected Claude file is not executable".into());
        }
    }
    #[cfg(windows)]
    {
        // No implicit shell invocation or unquoted batch command construction.
        if !path
            .extension()
            .is_some_and(|v| v.eq_ignore_ascii_case("exe"))
        {
            return Err("Select the installed native Claude .exe; batch and PowerShell launchers are not supported yet".into());
        }
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_selection_requires_a_native_executable_and_reads_older_settings() {
        let old: Settings =
            serde_json::from_str(r#"{"enabled":true,"desktop_sync":false}"#).unwrap();
        assert!(old.binary.is_none());
        assert_eq!(selected_binary("  ").unwrap(), None);
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-executable.cmd");
        std::fs::write(&file, "echo unsafe").unwrap();
        assert!(selected_binary(file.to_str().unwrap()).is_err());
        let exe = std::env::current_exe().unwrap();
        assert_eq!(selected_binary(exe.to_str().unwrap()).unwrap(), Some(exe));
    }
}
