//! One bounded policy job shared by the dashboard and scheduler.
use crate::{
    auth_service::AuthService,
    chat::Chats,
    storage_policy::{self as policy, Policy, Reason},
    storage_report::Mutation,
};
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct Runner {
    db: AuthService,
    state: Arc<Mutex<Value>>,
    running: watch::Sender<bool>,
}
impl Runner {
    pub fn new(db: AuthService) -> Self {
        Self {
            db,
            state: Arc::new(Mutex::new(json!({"status":"idle"}))),
            running: watch::channel(false).0,
        }
    }
    fn state(&self) -> Result<Value, String> {
        self.state
            .lock()
            .map(|v| v.clone())
            .map_err(|_| "Cleanup status is unavailable".into())
    }
    pub async fn read(&self) -> Result<Value, String> {
        Ok(policy::value(&policy::load(&self.db).await?, self.state()?))
    }
    pub async fn save(&self, body: Value) -> Result<Value, String> {
        let p = policy::save(&self.db, body, policy::now()).await?;
        Ok(json!({"ok":true,"policy":p,"job":self.state()?}))
    }
    pub async fn launch(
        &self,
        chats: Chats,
        stopping: watch::Sender<bool>,
        reason: Reason,
    ) -> Result<Option<Value>, String> {
        let p = policy::load(&self.db).await?;
        let now = policy::now();
        if reason != Reason::Manual && !p.due(now, reason) {
            return Ok(None);
        }
        let job = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Cleanup status is unavailable")?;
            if *stopping.borrow() {
                return Err("stopping".into());
            }
            if state["status"] == "running" {
                return Err("already_running".into());
            }
            let started = now.max(state["startedAt"].as_u64().unwrap_or(0).saturating_add(1));
            *state = json!({"status":"running","reason":reason.name(),"startedAt":started});
            self.running.send_replace(true);
            state.clone()
        };
        let response = json!({"ok":true,"started":true,"job":job,"policy":p});
        let this = self.clone();
        tokio::spawn(async move {
            let result =
                std::panic::AssertUnwindSafe(execute(&this.db, &chats, &stopping, &p, reason))
                    .catch_unwind()
                    .await;
            let mut outcome =
                result.unwrap_or_else(|_| json!({"ok":false,"error":"worker_failed"}));
            let finished = policy::now();
            if outcome["skipped"] != "disabled"
                && outcome["skipped"] != "not_due"
                && policy::complete(&this.db, finished, outcome.clone())
                    .await
                    .is_err()
            {
                // Cleanup may already be committed. Do not claim rollback or
                // replay it; preserve the recovery location in the outcome.
                outcome["ok"] = json!(false);
                outcome["error"] = json!("policy_metadata_failed");
            }
            if let Ok(mut state) = this.state.lock() {
                state["status"] = json!("idle");
                state["finishedAt"] = json!(finished);
                state["lastOutcome"] = outcome.clone();
                if let Some(error) = outcome.get("error") {
                    state["lastError"] = error.clone();
                }
            }
            this.running.send_replace(false);
        });
        Ok(Some(response))
    }
    pub fn schedule(&self, chats: Chats, stopping: watch::Sender<bool>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut stop = stopping.subscribe();
            if *stop.borrow() {
                return;
            }
            let _ = this
                .launch(chats.clone(), stopping.clone(), Reason::Startup)
                .await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await;
            loop {
                tokio::select! {biased;
                    _=async {let _=stop.wait_for(|v|*v).await;}=>break,
                    _=tick.tick()=>{let _=this.launch(chats.clone(),stopping.clone(),Reason::Schedule).await;}
                }
            }
        });
    }
    pub async fn drain(&self) {
        let mut active = self.running.subscribe();
        let _ = active.wait_for(|v| !*v).await;
    }
}
async fn execute(
    db: &AuthService,
    chats: &Chats,
    stopping: &watch::Sender<bool>,
    p: &Policy,
    reason: Reason,
) -> Value {
    if !p.enabled {
        return json!({"ok":true,"skipped":"disabled"});
    }
    if !p.due(policy::now(), reason) {
        return json!({"ok":true,"skipped":"not_due"});
    }
    if *stopping.borrow() {
        return json!({"ok":false,"error":"stopping"});
    }
    // Reserve the next due window durably before irreversible work. A crash or
    // failed final metadata write must not retry a daily/weekly deletion every
    // scheduler tick. Explicit Run now still deliberately bypasses this window.
    if policy::complete(db, policy::now(), json!({"ok":false}))
        .await
        .is_err()
    {
        return json!({"ok":false,"error":"policy_metadata_failed"});
    }
    let (reservation, protected) = match chats.prepare_storage().await {
        Ok(v) => v,
        Err(e) => {
            return if e == "codex_busy" {
                json!({"ok":false,"error":"codex_busy","deferred":"codex_busy"})
            } else {
                json!({"ok":false,"error":"storage_mutation_busy","deferred":"codex_busy"})
            }
        }
    };
    if *stopping.borrow() {
        return json!({"ok":false,"error":"stopping"});
    }
    match chats
        .storage_report
        .mutate(Mutation::Policy(p.clone()), reservation, protected)
        .await
    {
        Ok(v) => {
            let mut result =
                json!({"ok":true,"mode":p.mode(),"freedBytes":v["bytes"],"removed":v["count"]});
            if let Some(id) = v.get("trashDir") {
                result["trashDir"] = id.clone();
            }
            result
        }
        Err(e) => match e.error {
            "under_threshold" | "nothing_selected" => json!({"ok":true,"skipped":e.error}),
            "codex_busy" | "storage_mutation_busy" => {
                json!({"ok":false,"error":e.error,"deferred":"codex_busy"})
            }
            _ => {
                let mut result = json!({"ok":false,"error":e.error});
                if let Some(id) = e.trash_dir {
                    result["trashDir"] = json!(id);
                }
                result
            }
        },
    }
}
