//! Retain a bounded warm connection pool without making saved chats a quota.
use super::*;

pub(super) const BUSY: &str = "All Claude connections have active work or pending requests. Wait for one to finish and try again; this prompt was not sent.";

fn compare_last_used(a: &Value, b: &Value) -> std::cmp::Ordering {
    let parsed = |value: &Value| {
        time::OffsetDateTime::parse(
            s(&value["updatedAt"]),
            &time::format_description::well_known::Rfc3339,
        )
        .ok()
    };
    parsed(a).cmp(&parsed(b))
}

impl Hub {
    pub(super) fn claude_connection_count(&self) -> usize {
        self.active
            .values()
            .filter(|w| w.claude_router.is_some() || w.observer.is_some())
            .count()
    }

    fn idle_claude_connection(&self, requested: &str) -> Option<String> {
        self.active
            .iter()
            .filter(|(id, w)| {
                if id.as_str() == requested
                    || self.tool_controls.contains(*id)
                    || self.recovery_checks.contains_key(*id)
                {
                    return false;
                }
                let c = &self.chats[*id];
                c.agent == AgentKind::Claude
                    && claude_recoverable(c)
                    && w.stage == "idle"
                    && !w.stopping
                    && (w.claude_router.is_some() || w.observer.is_some())
                    && w.requests.is_empty()
                    && w.steers.is_empty()
                    && w.local_turn.is_none()
                    && w.control_reply.is_none()
                    && w.claude_stop.is_none()
                    && w.claude_deferred_result.is_none()
                    && self.background_tasks(id).is_ok_and(|tasks| !tasks.busy())
                    && !c.pending_work()
                    && w.pre_turn_events.is_empty()
                    && w.connection.events.is_empty()
                    && w.metadata.context_request.is_none()
                    && w.metadata.capability_read.is_none()
                    && w.claude_router
                        .as_ref()
                        .is_none_or(|r| r.active().is_none() && r.pending().next().is_none())
            })
            .min_by(|(aid, a), (bid, b)| {
                // Prefer dropping an observer socket (never its Desktop owner),
                // then the least recently used owned idle process.
                b.observer
                    .is_some()
                    .cmp(&a.observer.is_some())
                    .then_with(|| {
                        compare_last_used(&self.chats[*aid].thread, &self.chats[*bid].thread)
                    })
                    .then_with(|| aid.cmp(bid))
            })
            .map(|(id, _)| id.clone())
    }

    pub(super) fn claude_connection_available(&self, requested: &str) -> bool {
        self.active.contains_key(requested)
            || self.claude_connection_count() < crate::claude_reconnect::HOST_LIMIT
            || self.idle_claude_connection(requested).is_some()
    }

    pub(super) async fn make_claude_connection_room(
        &mut self,
        requested: &str,
    ) -> Result<(), String> {
        if self.active.contains_key(requested) {
            return Ok(());
        }
        while self.claude_connection_count() >= crate::claude_reconnect::HOST_LIMIT {
            let id = self.idle_claude_connection(requested).ok_or(BUSY)?;
            let before = self.chats[&id].clone();
            let chat = self.chats.get_mut(&id).unwrap();
            chat.claude_parked = true;
            if chat.external_claude {
                chat.thread["session"]["statusConfidence"] = json!("unknown");
            }
            projection::activity(
                chat,
                "claude-native-sync",
                "claude.desktop-sync.updated",
                "Claude connection",
                "info",
                json!({"status":"parked","detail":"Idle connection released for another chat. Saved history is available; sending a prompt reconnects this session."}),
            );
            // Persist the deliberate release before closing. A failed write
            // leaves the live worker untouched. Never finish/rewrite its turn.
            if let Err(error) = self.save(&id, None).await {
                self.chats.insert(id, before);
                return Err(error);
            }
            // A remote event may have arrived while the database was writing.
            // Let normal projection handle it before selecting this worker.
            if !self.active[&id].connection.events.is_empty() {
                self.chats.insert(id.clone(), before);
                // save_chat upserts activity rows. Explicitly restore the
                // connected row even if publish() paged it out of `before`.
                projection::activity(
                    self.chats.get_mut(&id).unwrap(),
                    "claude-native-sync",
                    "claude.desktop-sync.updated",
                    "Claude Desktop sync",
                    "info",
                    json!({"status":"connected"}),
                );
                self.save(&id, None).await?;
                // This worker's unread event now excludes it; try another
                // idle connection instead of failing an otherwise valid send.
                continue;
            }
            let mut worker = self.active.remove(&id).unwrap();
            worker.metadata.disconnect();
            self.claude_recovery.forget(&id);
            worker.connection.close().await;
            self.dirty = true;
        }
        if self.chats[requested].claude_parked {
            self.chats.get_mut(requested).unwrap().claude_parked = false;
            if let Err(error) = self.save(requested, None).await {
                self.chats.get_mut(requested).unwrap().claude_parked = true;
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Chat {
    fn pending_work(&self) -> bool {
        !self.queue.is_empty()
            || self.compaction_turn.is_some()
            || self.external_stop.is_some()
            || self
                .native_control
                .as_ref()
                .is_some_and(|c| c.result.is_none())
            || self.thread["hasPendingApprovals"] == true
            || self.thread["hasPendingUserInput"] == true
            || !self.thread["session"]["activeTurnId"].is_null()
            || matches!(
                s(&self.thread["session"]["status"]),
                "starting" | "running" | "stopping"
            )
            || self.thread["latestTurn"]["state"] == "running"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_connection_recency_uses_time_not_fractional_second_text() {
        // RFC 3339 omits trailing zeros: .1Z must precede .101Z, and a
        // whole second must precede that same second with a fraction.
        for (older, newer) in [
            ("2026-11-08T10:00:00Z", "2026-11-08T10:00:00.001Z"),
            ("2026-11-08T10:00:00.1Z", "2026-11-08T10:00:00.101Z"),
            ("2026-11-08T10:00:00.12Z", "2026-11-08T10:00:00.121Z"),
            ("2026-11-08T15:30:00+05:30", "2026-11-08T10:00:01Z"),
        ] {
            assert_eq!(
                compare_last_used(&json!({"updatedAt": older}), &json!({"updatedAt": newer})),
                std::cmp::Ordering::Less,
                "{older} must be released before {newer}"
            );
        }
    }

    #[test]
    fn pending_work_is_never_idle_capacity() {
        let base = json!({"agent":"claude","native_id":"session","resumed":true,"thread":{
            "session":{"status":"ready","activeTurnId":null},"latestTurn":{"state":"completed"}
        }});
        let chat: Chat = serde_json::from_value(base.clone()).unwrap();
        assert!(!chat.pending_work());
        for patch in [
            json!({"session":{"status":"running"}}),
            json!({"session":{"status":"starting"}}),
            json!({"session":{"status":"stopping"}}),
            json!({"session":{"activeTurnId":"native-turn"}}),
            json!({"latestTurn":{"state":"running"}}),
            json!({"hasPendingApprovals":true}),
            json!({"hasPendingUserInput":true}),
        ] {
            let mut value = base.clone();
            for (key, field) in patch.as_object().unwrap() {
                value["thread"][key] = field.clone();
            }
            let chat: Chat = serde_json::from_value(value).unwrap();
            assert!(chat.pending_work(), "{patch}");
        }
        let mut chat = chat.clone();
        chat.queue.push(json!({"message":"keep queued"}));
        assert!(chat.pending_work());
        chat.queue.clear();
        chat.compaction_turn = Some("compact".into());
        assert!(chat.pending_work());
    }
}
