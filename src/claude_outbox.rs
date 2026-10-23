//! Durable prompt identities. Reserving a send precedes I/O; an existing
//! reservation is never an instruction to resend, even after process death.
use crate::{claude_turns::Input, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub input: Input,
    /// offered is deliberately uncertain after reopening the database.
    pub state: String,
}
impl Store {
    pub(crate) fn init_claude_outbox(&self) -> rusqlite::Result<()> {
        self.db.execute_batch("CREATE TABLE IF NOT EXISTS claude_outbox(chat TEXT NOT NULL,session TEXT NOT NULL,native TEXT NOT NULL,message TEXT NOT NULL,turn TEXT NOT NULL,steer INTEGER NOT NULL,digest TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(chat,session,native),UNIQUE(chat,session,message)); CREATE INDEX IF NOT EXISTS claude_outbox_pending ON claude_outbox(chat,session,state);")
    }
    pub fn offer_claude_input(
        &mut self,
        chat: &str,
        session: &str,
        input: &Input,
        payload: &Value,
    ) -> Result<bool, String> {
        crate::claude_turns::Router::default().register(Input {
            steer: false,
            ..input.clone()
        })?;
        if !input.local
            || payload["type"] != "user"
            || payload["uuid"] != input.native
            || payload["session_id"] != session
            || payload["message"]["role"] != "user"
            || payload["isMeta"] == true
            || payload["isSynthetic"] == true
            || !payload["parent_tool_use_id"].is_null()
        {
            return Err("Invalid Claude prompt reservation".into());
        }
        let bytes = serde_json::to_vec(payload).map_err(|_| "Invalid Claude prompt")?;
        if bytes.len() > crate::agent::MAX_INPUT_FRAME {
            return Err("Claude prompt exceeds the input limit".into());
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        self.check_history(4096)?;
        let tx = self
            .db
            .transaction()
            .map_err(|_| "Prompt store unavailable")?;
        let valid: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 AND agent='claude' AND json_extract(data,'$.native_id')=?2)",params![chat,session],|r|r.get(0)).map_err(|_|"Prompt store unavailable")?;
        if !valid {
            return Err("Claude prompt belongs to another chat".into());
        }
        let old: Option<(String,String,String,i64)>=tx.query_row("SELECT digest,message,turn,steer FROM claude_outbox WHERE chat=?1 AND session=?2 AND native=?3",params![chat,session,input.native],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_|"Prompt store unavailable")?;
        if let Some((old, message, turn, steer)) = old {
            if old != digest
                || message != input.message
                || turn != input.public_turn
                || (steer != 0) != input.steer
            {
                return Err("Claude prompt identity was reused for different input".into());
            }
            return Ok(false);
        }
        let pending:i64=tx.query_row("SELECT count(*) FROM claude_outbox WHERE chat=?1 AND session=?2 AND state IN ('offered','consumed')",params![chat,session],|r|r.get(0)).map_err(|_|"Prompt store unavailable")?;
        if pending >= 64 {
            return Err(
                "Claude has too many unconfirmed prompts; reconcile history before sending".into(),
            );
        }
        tx.execute("INSERT INTO claude_outbox(chat,session,native,message,turn,steer,digest,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'offered')",params![chat,session,input.native,input.message,input.public_turn,input.steer,digest]).map_err(|_|"Claude input was already reserved; it was not resent")?;
        tx.commit()
            .map_err(|_| "Could not save Claude delivery reservation")?;
        Ok(true)
    }
    pub fn claude_input(
        &self,
        chat: &str,
        session: &str,
        native: &str,
    ) -> rusqlite::Result<Option<Record>> {
        self.db.query_row("SELECT message,turn,steer,state FROM claude_outbox WHERE chat=?1 AND session=?2 AND native=?3",params![chat,session,native],|r|Ok(Record{input:Input{native:native.into(),message:r.get(0)?,public_turn:r.get(1)?,local:true,steer:r.get(2)?},state:r.get(3)?})).optional()
    }
    /// Only call on a verified worker frame, never another observer's claims.
    pub fn acknowledge_claude_input(
        &mut self,
        chat: &str,
        session: &str,
        frame: &Value,
    ) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        acknowledge(&tx, chat, session, frame)?;
        tx.commit()
    }
    pub fn mark_claude_delivery_uncertain(
        &self,
        chat: &str,
        session: &str,
    ) -> rusqlite::Result<()> {
        self.db.execute("UPDATE claude_outbox SET state=CASE state WHEN 'offered' THEN 'uncertain' ELSE 'consumed-unfinished' END WHERE chat=?1 AND session=?2 AND state IN ('offered','consumed')",params![chat,session])?;
        Ok(())
    }
    pub fn unresolved_claude_inputs(
        &self,
        chat: &str,
        session: &str,
    ) -> rusqlite::Result<Vec<String>> {
        self.db.prepare("SELECT native FROM claude_outbox WHERE chat=?1 AND session=?2 AND state NOT IN ('completed','cancelled','orphaned') ORDER BY rowid LIMIT 65")?
            .query_map(params![chat,session], |r| r.get(0))?.collect()
    }
}
pub(crate) fn acknowledge(
    tx: &rusqlite::Transaction<'_>,
    chat: &str,
    session: &str,
    frame: &Value,
) -> rusqlite::Result<()> {
    let receipt = crate::claude_host::Ledger::default()
        .observe(frame)
        .map_err(rusqlite::Error::InvalidParameterName)?;
    apply_receipt(tx, chat, session, &receipt)
}
pub(crate) fn apply_receipt(
    tx: &rusqlite::Transaction<'_>,
    chat: &str,
    session: &str,
    receipt: &crate::claude_host::Receipt,
) -> rusqlite::Result<()> {
    if receipt.owner_stopped {
        tx.execute("UPDATE claude_outbox SET state=CASE state WHEN 'offered' THEN 'uncertain' ELSE 'consumed-unfinished' END WHERE chat=?1 AND session=?2 AND state IN ('offered','consumed')",params![chat,session])?;
    }
    if receipt.owner_replaced {
        // Terminal: never pending again, never resent, never revived.
        tx.execute("UPDATE claude_outbox SET state='orphaned' WHERE chat=?1 AND session=?2 AND state IN ('offered','consumed','uncertain','consumed-unfinished')",params![chat,session])?;
    }
    for id in &receipt.consumed {
        tx.execute("UPDATE claude_outbox SET state='consumed' WHERE chat=?1 AND session=?2 AND native=?3 AND state IN ('offered','uncertain','consumed-unfinished')",params![chat,session,id])?;
    }
    for (state, ids) in [
        ("completed", &receipt.completed),
        ("cancelled", &receipt.cancelled),
    ] {
        for id in ids {
            tx.execute("UPDATE claude_outbox SET state=?4 WHERE chat=?1 AND session=?2 AND native=?3 AND state IN ('offered','consumed','uncertain','consumed-unfinished')",params![chat,session,id,state])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn lost_send_and_restart_never_reoffer_or_confuse_steering_receipts() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::open(d.path()).unwrap();
        store.init_chats().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        store.save_chat(&json!({"agent":"claude","native_id":session,"thread":{"id":"chat","messages":[],"activities":[],"proposedPlans":[]}}),None).unwrap();
        let first = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: "phone-one".into(),
            public_turn: "turn-one".into(),
            local: true,
            steer: false,
        };
        let second = Input {
            native: uuid::Uuid::new_v4().to_string(),
            message: "phone-steer".into(),
            public_turn: "turn-one".into(),
            local: true,
            steer: true,
        };
        let payload = |i: &Input| json!({"type":"user","uuid":i.native,"session_id":session,"message":{"role":"user","content":"Test"}});
        assert!(store
            .offer_claude_input("chat", &session, &first, &payload(&first))
            .unwrap());
        assert!(store
            .offer_claude_input("chat", &session, &second, &payload(&second))
            .unwrap());
        drop(store);
        let mut store = Store::open(d.path()).unwrap();
        assert!(!store
            .offer_claude_input("chat", &session, &first, &payload(&first))
            .unwrap());
        assert_eq!(
            store
                .claude_input("chat", &session, &first.native)
                .unwrap()
                .unwrap()
                .state,
            "offered"
        );
        let mut bad = payload(&first);
        bad["message"]["content"] = json!("Different");
        assert!(store
            .offer_claude_input("chat", &session, &first, &bad)
            .is_err());
        store
            .acknowledge_claude_input(
                "chat",
                &session,
                &json!({"type":"result","user_message_uuid":first.native}),
            )
            .unwrap();
        assert_eq!(
            store
                .claude_input("chat", &session, &first.native)
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        assert_eq!(
            store
                .claude_input("chat", &session, &second.native)
                .unwrap()
                .unwrap()
                .state,
            "offered"
        );
        store
            .acknowledge_claude_input(
                "chat",
                &session,
                &json!({"type":"assistant","user_message_uuids":[second.native]}),
            )
            .unwrap();
        assert_eq!(
            store
                .claude_input("chat", &session, &second.native)
                .unwrap()
                .unwrap()
                .state,
            "consumed"
        );
        store
            .acknowledge_claude_input(
                "chat",
                &session,
                &json!({"type":"system","subtype":"worker_shutting_down"}),
            )
            .unwrap();
        assert_eq!(
            store
                .claude_input("chat", &session, &first.native)
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        assert_eq!(
            store
                .claude_input("chat", &session, &second.native)
                .unwrap()
                .unwrap()
                .state,
            "consumed-unfinished"
        );
        assert!(!store
            .offer_claude_input("chat", &session, &second, &payload(&second))
            .unwrap());
        store
            .acknowledge_claude_input(
                "chat",
                &session,
                &json!({"type":"result","user_message_uuids":[first.native,second.native]}),
            )
            .unwrap();
        assert_eq!(
            store
                .claude_input("chat", &session, &second.native)
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        assert!(store
            .offer_claude_input("another-chat", &session, &first, &payload(&first))
            .is_err());
    }
}
