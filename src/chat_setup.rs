//! Durable bridge-side Claude switches. These never edit Desktop preferences.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    pub enabled: bool,
    pub desktop_sync: bool,
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
        if object
            .iter()
            .any(|(k, v)| !matches!(k.as_str(), "enabled" | "desktopSync") || !v.is_boolean())
        {
            return Err("Provide only enabled and desktopSync booleans".into());
        }
        let next = Settings {
            enabled: p["enabled"].as_bool().unwrap_or(self.claude_enabled),
            desktop_sync: p["desktopSync"]
                .as_bool()
                .unwrap_or(self.config.claude.remote_control),
        };
        if next.enabled == self.claude_enabled
            && next.desktop_sync == self.config.claude.remote_control
        {
            return Ok(json!({}));
        }
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
        self.db.call(move |a| a.store.db.execute(
            "INSERT INTO metadata(key,value) VALUES('native.claudeSettings',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [data]
        )).await?.map_err(|_| "Cannot save Claude bridge settings; nothing changed")?;
        self.claude_enabled = next.enabled;
        self.config.claude.remote_control = next.desktop_sync;
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
