//! Read-only metadata for the verified native connection. Call `event` only
//! for owner stdout or committed worker events, never client control echoes.
use super::*;

#[derive(Default)]
pub(super) struct Reads {
    pub context_request: Option<(String, Option<Instant>)>,
    pub context_checked: Option<Instant>,
    pub capability_seed: Value,
    pub capability_read: Option<CapabilityRead>,
    pub capability_cache: Option<(Instant, Value)>,
}
pub(super) struct CapabilityRead {
    pub id: String,
    pub context_catalog: bool,
    pub deadline: Option<Instant>,
    pub replies: Vec<oneshot::Sender<Result<Value, String>>>,
}
pub(super) enum Reply {
    Other,
    Capabilities,
    Context(Option<Value>),
}
impl Reads {
    pub fn event(&mut self, v: &Value) -> Reply {
        if !self.capability_seed.is_object() {
            self.capability_seed = json!({});
        }
        if (v["type"] == "control_response" && v["response"]["subtype"] == "success")
            || (v["type"] == "system" && v["subtype"] == "init")
        {
            crate::capabilities::seed(&mut self.capability_seed, v);
        }
        if v["type"] != "control_response" {
            return Reply::Other;
        }
        if self
            .capability_read
            .as_ref()
            .is_some_and(|r| v["response"]["request_id"] == r.id)
        {
            let read = self.capability_read.take().unwrap();
            let response = &v["response"]["response"];
            let result = if v["response"]["subtype"] == "success"
                && response[if read.context_catalog {
                    "mcpTools"
                } else {
                    "mcpServers"
                }]
                .is_array()
            {
                let data = if read.context_catalog {
                    crate::capabilities::project_context(&self.capability_seed, response)
                } else {
                    crate::capabilities::project(&self.capability_seed, &response["mcpServers"])
                };
                self.capability_cache = Some((Instant::now(), data.clone()));
                Ok(data)
            } else {
                Err("Claude could not report its tools; the chat is unchanged".into())
            };
            for reply in read.replies {
                let _ = reply.send(result.clone());
            }
            return Reply::Capabilities;
        }
        if self
            .context_request
            .as_ref()
            .is_some_and(|(id, _)| v["response"]["request_id"] == *id)
        {
            self.context_request = None;
            return Reply::Context(if v["response"]["subtype"] == "success" {
                crate::usage::context(&v["response"]["response"])
            } else {
                None
            });
        }
        Reply::Other
    }
    pub fn disconnect(&mut self) {
        if let Some(read) = self.capability_read.take() {
            for reply in read.replies {
                let _ = reply.send(Ok(crate::capabilities::unavailable()));
            }
        }
        self.context_request = None;
        self.capability_cache = None;
    }
    /// Expire only reads, never a turn or mutation. The caller releases these
    /// local transport identities; no Stop/cancel is sent to the native owner.
    pub fn expire(&mut self, now: Instant) -> (Vec<String>, bool) {
        let mut ids = vec![];
        if self
            .capability_read
            .as_ref()
            .is_some_and(|r| r.deadline.is_some_and(|d| now >= d))
        {
            let read = self.capability_read.take().unwrap();
            ids.push(read.id);
            for reply in read.replies {
                let _ = reply.send(Err(
                    "Claude tools did not respond in time; the chat is unchanged".into(),
                ));
            }
            // Briefly coalesce repeated screen refreshes after a timeout.
            self.capability_cache = Some((now, crate::capabilities::unavailable()));
        }
        let context = self
            .context_request
            .as_ref()
            .is_some_and(|(_, d)| d.is_some_and(|d| now >= d));
        if context {
            ids.push(self.context_request.take().unwrap().0);
        }
        (ids, context)
    }
}
impl Worker {
    pub fn metadata_ready(&self) -> bool {
        matches!(self.stage, "running" | "idle")
            && !self.stopping
            // A recovered owner stays marked recovering until the next real
            // turn. Its verified idle connection can already answer reads.
            // Handshake stages and the observer's catch-up barrier still gate
            // access; metadata must never require sending a model prompt.
            && self.observer.as_ref().is_none_or(|o| o.ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn observer_catalog_requires_matching_context_reply_not_placeholder_mcp_status() {
        let mut reads = Reads::default();
        let (send, receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "capabilities-context".into(),
            context_catalog: true,
            deadline: Some(Instant::now()),
            replies: vec![send],
        });
        let v = json!({"type":"control_response","response":{"request_id":"capabilities-context","subtype":"success","response":{"mcpServers":[]}}});
        reads.event(&v);
        assert!(
            receive.await.unwrap().is_err(),
            "missing context is unavailable, not an empty catalog"
        );
        let (send, receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "capabilities-context2".into(),
            context_catalog: true,
            deadline: Some(Instant::now()),
            replies: vec![send],
        });
        let mut v = json!({"type":"control_response","response":{"request_id":"old","subtype":"success","response":{"mcpTools":[{"name":"mcp__fixture__read","serverName":"fixture"}],"systemPrompt":"PRIVATE"}}});
        assert!(matches!(reads.event(&v), Reply::Other));
        v["response"]["request_id"] = json!("capabilities-context2");
        reads.event(&v);
        let value = receive.await.unwrap().unwrap();
        assert_eq!(value["mcpServers"][0]["tools"], json!(["read"]));
        assert_eq!(value["mcpServers"][0]["status"], "unknown");
        assert!(!value.to_string().contains("PRIVATE"));
    }
    #[tokio::test]
    async fn exact_read_responses_are_sanitized_and_disconnect_releases_waiters() {
        let mut reads = Reads::default();
        let (send, mut receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "ours".into(),
            context_catalog: false,
            deadline: Some(Instant::now()),
            replies: vec![send],
        });
        reads.event(&json!({"type":"system","subtype":"init","skills":["fixture:review"],"plugins":[{"name":"fixture","version":"1","path":"PRIVATE"}]}));
        let mut v = json!({"type":"control_response","response":{"request_id":"someone-else","subtype":"success","response":{"mcpServers":[{"name":"fixture","status":"connected","env":{"SECRET":"PRIVATE"},"tools":[{"name":"read_fixture"}]}]}}});
        assert!(matches!(reads.event(&v), Reply::Other));
        assert!(receive.try_recv().is_err());
        v["response"]["request_id"] = json!("ours");
        v["type"] = json!("user");
        assert!(matches!(reads.event(&v), Reply::Other));
        v["type"] = json!("control_response");
        assert!(matches!(reads.event(&v), Reply::Capabilities));
        let result = receive.await.unwrap().unwrap();
        assert_eq!(result["mcpServers"][0]["tools"][0], "read_fixture");
        assert_eq!(result["skills"][0]["name"], "fixture:review");
        assert!(!result.to_string().contains("PRIVATE"));
        let (send, receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "next".into(),
            context_catalog: false,
            deadline: None,
            replies: vec![send],
        });
        reads.disconnect();
        assert_eq!(receive.await.unwrap().unwrap()["status"], "unavailable");
        assert!(reads.capability_cache.is_none());
    }
    #[test]
    fn context_accepts_only_matching_native_reply_and_excludes_private_fields() {
        let mut reads = Reads {
            context_request: Some(("context".into(), None)),
            ..Default::default()
        };
        let mut v = json!({"type":"control_response","response":{"request_id":"older","subtype":"success","response":{"totalTokens":123,"rawMaxTokens":10000,"systemPrompt":"PRIVATE"}}});
        assert!(matches!(reads.event(&v), Reply::Other));
        assert!(reads.context_request.is_some());
        v["response"]["request_id"] = json!("context");
        let Reply::Context(Some(result)) = reads.event(&v) else {
            panic!("missing context");
        };
        assert_eq!(result["usedTokens"], 123.0);
        assert!(!result.to_string().contains("PRIVATE"));
        assert!(reads.context_request.is_none());
        assert!(matches!(reads.event(&v), Reply::Other));
    }
    #[tokio::test]
    async fn timed_out_reads_release_waiters_and_late_responses_cannot_complete_the_next_read() {
        let mut reads = Reads::default();
        let now = Instant::now();
        let (send, receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "capabilities-old".into(),
            context_catalog: false,
            deadline: Some(now),
            replies: vec![send],
        });
        reads.context_request = Some(("context-old".into(), Some(now)));
        assert_eq!(
            reads.expire(now),
            (
                vec!["capabilities-old".to_string(), "context-old".to_string()],
                true
            )
        );
        assert!(receive.await.unwrap().is_err());
        let (send, mut receive) = oneshot::channel();
        reads.capability_read = Some(CapabilityRead {
            id: "capabilities-new".into(),
            context_catalog: false,
            deadline: Some(now + Duration::from_secs(5)),
            replies: vec![send],
        });
        let mut reply = json!({"type":"control_response","response":{"request_id":"capabilities-old","subtype":"success","response":{"mcpServers":[]}}});
        assert!(matches!(reads.event(&reply), Reply::Other));
        assert!(receive.try_recv().is_err());
        reply["response"]["request_id"] = json!("capabilities-new");
        assert!(matches!(reads.event(&reply), Reply::Capabilities));
        assert_eq!(receive.await.unwrap().unwrap()["status"], "ready");
        assert!(reads.expire(now).0.is_empty());
    }
}
