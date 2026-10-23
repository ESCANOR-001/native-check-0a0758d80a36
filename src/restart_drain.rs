//! Bounded restart drain. Admission stays reserved until durable shutdown. The
//! existing shutdown path closes owned workers and detaches external observers;
//! it never kills a shared daemon or replays an uncertain prompt.
use crate::{
    chat::{AgentKind, Chats, RestartReservation},
    server::App,
};
use serde_json::{json, Value};
use std::{sync::atomic::Ordering, time::Duration};

pub const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn allowed(method: &str, params: &Value) -> bool {
    matches!(
        method,
        "server.getConfig"
            | "subscribeServerConfig"
            | "orchestration.subscribeShell"
            | "orchestration.subscribeThread"
            | "orchestration.getArchivedShellSnapshot"
            | "orchestration.getThreadPage"
            | "orchestration.getTurnWork"
            | "native.watchShared"
            | "native.claudeSyncStatus"
            | "server.getStorageStatus"
            | "assets.createUrl"
            | "filesystem.readTextFile"
            | "projects.listEntries"
            | "projects.searchEntries"
            | "projects.readFile"
    ) || (method == "orchestration.dispatchCommand"
        && matches!(
            params["type"].as_str(),
            Some(
                "thread.turn.interrupt"
                    | "thread.session.stop"
                    | "thread.approval.respond"
                    | "thread.user-input.respond"
                    | "thread.user-input.draft.update"
                    | "thread.turn.queue.cancel"
            )
        ))
}

pub async fn status(chats: &Chats) -> Result<Value, String> {
    chats
        .call(AgentKind::Codex, "local", "native.restartStatus", json!({}))
        .await
}

/// `limit` is supplied by the native caller, never a network or environment
/// setting. Tests use a short deadline against the same orchestration path.
pub async fn finish(app: App, reservation: Option<RestartReservation>, limit: Duration) {
    let Some(restart) = &app.restart else { return };
    let mut stopping = app.stopping.subscribe();
    let wait = async {
        while let Some(chats) = &app.chats {
            if status(chats).await.is_ok_and(|v| v["drainReady"] == true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::select! {
        biased;
        _ = stopping.wait_for(|stopped| *stopped) => { return; }
        _ = tokio::time::timeout(limit, wait) => {}
    }
    // Stop overrides restart even if it arrives at the drain deadline. No await
    // between this check and shutdown admission/guard commitment.
    if *app.stopping.borrow() || !restart.load(Ordering::Acquire) {
        return;
    }
    app.stopping.send_replace(true);
    if let Some(reservation) = reservation {
        reservation.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_keeps_controls_but_refuses_new_work() {
        for kind in [
            "thread.turn.start",
            "thread.create",
            "project.create",
            "thread.turn.queue.steer",
            "thread.turn.queue.update",
        ] {
            assert!(!allowed(
                "orchestration.dispatchCommand",
                &json!({"type":kind})
            ));
        }
        for method in [
            "claude.sessionAction",
            "native.configureClaude",
            "sourceControl.cloneRepository",
            "native.retryClaudeSync",
        ] {
            assert!(!allowed(method, &json!({})));
        }
        for kind in [
            "thread.turn.interrupt",
            "thread.approval.respond",
            "thread.user-input.respond",
            "thread.turn.queue.cancel",
        ] {
            assert!(allowed(
                "orchestration.dispatchCommand",
                &json!({"type":kind})
            ));
        }
        assert!(allowed("orchestration.subscribeThread", &json!({})));
        assert!(!allowed("orchestration.dispatchCommand", &json!({})));
    }
}
