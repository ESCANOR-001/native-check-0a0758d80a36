//! The resume cursor is committed in the same transaction as projected chat
//! rows. Receiving bytes is never sufficient evidence to advance this cursor.
use crate::{claude_host::Identity, claude_remote::Envelope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub identity: Identity,
    pub sequence: u64,
    pub event_id: String,
    pub digest: String,
    /// Bounded projection state (turn identities, not transcript content).
    pub projection: Value,
}
impl Cursor {
    pub fn new(identity: Identity) -> Result<Self, String> {
        identity.validate()?;
        Ok(Self {
            identity,
            sequence: 0,
            event_id: String::new(),
            digest: String::new(),
            projection: Value::Null,
        })
    }
    pub fn advance(&self, event: &Envelope, projection: Value) -> Result<Self, String> {
        self.validate()?;
        let sequence = event
            .sequence
            .ok_or("An ephemeral event cannot advance durable history")?;
        let event_id = event
            .event_id
            .as_deref()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or("Missing durable event identity")?;
        if sequence <= self.sequence || sequence > i64::MAX as u64 {
            return Err("Claude history cursor did not advance".into());
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&event.source, &event.payload, &event.created_at))
                    .map_err(|_| "Cannot validate Claude history event")?
            )
        );
        let next = Self {
            identity: self.identity.clone(),
            sequence,
            event_id: event_id.into(),
            digest,
            projection,
        };
        next.validate()?;
        Ok(next)
    }
    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        if self.sequence > i64::MAX as u64
            || (self.sequence == 0 && (!self.event_id.is_empty() || !self.digest.is_empty()))
            || (self.sequence > 0
                && (self.event_id.is_empty()
                    || self.event_id.len() > 256
                    || self.digest.len() != 64
                    || !self.digest.bytes().all(|b| b.is_ascii_hexdigit())))
            || self.projection.to_string().len() > 128 * 1024
        {
            return Err("Invalid saved Claude history cursor".into());
        }
        Ok(())
    }
}
pub(crate) struct Batch<'a> {
    pub before: &'a Cursor,
    pub after: &'a Cursor,
    pub receipt: Option<&'a crate::claude_host::Receipt>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;
    fn identity() -> Identity {
        Identity {
            native_session: "00000000-0000-4000-8000-000000000001".into(),
            bridge_session: "cse_cursor_test".into(),
        }
    }
    fn event() -> Envelope {
        Envelope {
            deliveries: vec![],
            receipt_checkpoint: false,
            recovery: None,
            sequence: Some(7),
            event_id: Some("event-7".into()),
            created_at: None,
            source: "worker".into(),
            payload: Some(json!({"type":"assistant"})),
        }
    }
    #[test]
    fn projection_and_cursor_are_atomic_and_reject_stale_or_wrong_session_commits() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::open(d.path()).unwrap();
        store.init_chats().unwrap();
        let mut chat = json!({"agent":"claude","native_id":identity().native_session,"remote_identity":identity(),"thread":{"id":"external-chat","messages":[{"id":"reply","text":"retained","turnId":"turn"}],"activities":[],"proposedPlans":[],"latestTurn":null}});
        store.save_chat(&chat, None).unwrap();
        let before = Cursor::new(identity()).unwrap();
        let after = before.advance(&event(), json!({"active":"turn"})).unwrap();
        let mut invalid = chat.clone();
        invalid["thread"]["messages"][0]["id"] = Value::Null;
        assert!(store.save_claude_remote(&invalid, &before, &after).is_err());
        assert_eq!(
            store
                .claude_remote_cursor("external-chat", identity())
                .unwrap(),
            before
        );
        assert_eq!(
            store.load_chat("external-chat").unwrap()["thread"]["messages"][0]["text"],
            "retained"
        );
        chat["thread"]["messages"][0]["text"] = json!("new reply");
        store.save_claude_remote(&chat, &before, &after).unwrap();
        assert_eq!(
            store
                .claude_remote_cursor("external-chat", identity())
                .unwrap(),
            after
        );
        assert!(store.save_claude_remote(&chat, &before, &after).is_err());
        let mut other = identity();
        other.bridge_session = "cse_other_link".into();
        assert!(store.claude_remote_cursor("external-chat", other).is_err());
        let mut wrong = after.clone();
        wrong.identity = Cursor::new(Identity {
            native_session: uuid::Uuid::new_v4().to_string(),
            bridge_session: "cse_other".into(),
        })
        .unwrap()
        .identity;
        wrong.sequence += 1;
        assert!(store.save_claude_remote(&chat, &after, &wrong).is_err());
        drop(store);
        let store = Store::open(d.path()).unwrap();
        assert_eq!(
            store
                .claude_remote_cursor("external-chat", identity())
                .unwrap(),
            after
        );
        assert_eq!(
            store.load_chat("external-chat").unwrap()["thread"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn ephemeral_and_oversized_state_never_advance_cursor() {
        let cursor = Cursor::new(identity()).unwrap();
        let mut e = event();
        e.sequence = None;
        assert!(cursor.advance(&e, Value::Null).is_err());
        assert!(cursor
            .advance(&event(), json!("x".repeat(128 * 1024)))
            .is_err());
        let after = cursor.advance(&event(), Value::Null).unwrap();
        assert!(after.advance(&event(), Value::Null).is_err());
    }
}
