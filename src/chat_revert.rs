use super::*;

impl Hub {
    pub(super) async fn prepare_revert(
        &self,
        r: &Request,
    ) -> Result<crate::revert::Preparation, String> {
        if r.agent != AgentKind::Codex {
            return Err("Conversation Revert is available for Codex; use Claude's checkpoint controls to restore files".into());
        }
        let c = &r.params;
        let id = s(&c["threadId"]);
        let command = s(&c["commandId"]);
        if id.is_empty()
            || id.len() > 128
            || command.is_empty()
            || command.len() > 128
            || c["turnCount"].as_u64().is_none()
        {
            return Err("Select a chat, command ID and non-negative conversation position".into());
        }
        let chat = self.checked(r.agent, id)?;
        if self.active.contains_key(id)
            || self.busy(id)
            || !chat.queue.is_empty()
            || (self.tool_controls.contains(id) && !self.pending_reverts.contains(id))
            || !chat.thread["archivedAt"].is_null()
        {
            return Err(
                "Stop the task and clear queued messages before reverting its conversation".into(),
            );
        }
        let key = format!("{}:{}:{command}", r.agent.instance(), r.client);
        let digest = format!("{:x}", Sha256::digest(c.to_string().as_bytes()));
        let chat = chat.clone();
        let command = c.clone();
        self.db
            .call(move |a| a.store.prepare_revert(&chat, &command, &key, &digest))
            .await?
    }
}
