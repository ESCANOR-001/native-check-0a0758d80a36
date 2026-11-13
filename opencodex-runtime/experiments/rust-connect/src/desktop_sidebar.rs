//! Notify Desktop when a separate owned CLI changes its saved task inventory.
//! The CLI persists the native chat; only the UI cache invalidation crosses IPC.
use crate::{agent::SharedServer, chat::AgentConfig, desktop_ipc::Endpoint};
use std::{path::PathBuf, time::Duration};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct Sidebar {
    changes: watch::Sender<u64>,
    task: JoinHandle<()>,
}
impl Sidebar {
    pub(crate) fn start(config: Option<&AgentConfig>) -> Option<Self> {
        let config = config?;
        let Some(SharedServer::Desktop {
            desktop_endpoint, ..
        }) = &config.shared_server
        else {
            return None;
        };
        Some(Self::spawn(
            desktop_endpoint.clone(),
            config.profile.clone(),
            Duration::from_millis(350),
            Duration::from_secs(30),
        ))
    }

    fn spawn(endpoint: Endpoint, profile: PathBuf, debounce: Duration, retry: Duration) -> Self {
        let (changes, mut receive) = watch::channel(0u64);
        let task = tokio::spawn(async move {
            while receive.changed().await.is_ok() {
                // One bounded revision, no queued chat IDs or transcript copies.
                tokio::time::sleep(debounce).await;
                loop {
                    let revision = *receive.borrow_and_update();
                    let sent = tokio::time::timeout(Duration::from_secs(5), async {
                        let mut client = endpoint.connect(&profile).await?;
                        client.invalidate_tasks().await
                    })
                    .await;
                    if matches!(sent, Ok(Ok(()))) {
                        if *receive.borrow() == revision {
                            break;
                        }
                        tokio::time::sleep(debounce).await;
                    } else {
                        // Desktop can be closed. Retain one pending refresh for
                        // its return; a missed refresh must never fail a prompt.
                        tokio::time::sleep(retry).await;
                    }
                }
            }
        });
        Self { changes, task }
    }

    pub(crate) fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
impl Drop for Sidebar {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn coalesces_and_recovers_after_desktop_returns_without_claiming_or_opening_a_chat() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint {
            path: dir.path().join("ipc.sock"),
        };
        let sidebar = Sidebar::spawn(
            endpoint.clone(),
            dir.path().into(),
            Duration::from_millis(10),
            Duration::from_millis(30),
        );
        for _ in 0..1000 {
            sidebar.changed();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listener = tokio::net::UnixListener::bind(&endpoint.path).unwrap();
        std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        async fn frame(stream: &mut tokio::net::UnixStream) -> Value {
            let size = stream.read_u32_le().await.unwrap();
            assert!(size < 4096);
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
        let init = frame(&mut stream).await;
        assert_eq!(init["method"], "initialize");
        let reply = serde_json::to_vec(&json!({"type":"response","requestId":init["requestId"],
            "method":"initialize","resultType":"success","result":{"clientId":"sidebar-test"}}))
        .unwrap();
        stream.write_u32_le(reply.len() as u32).await.unwrap();
        stream.write_all(&reply).await.unwrap();
        let refresh = frame(&mut stream).await;
        assert_eq!(
            refresh,
            json!({"type":"broadcast","sourceClientId":"sidebar-test",
            "method":"query-cache-invalidate","version":0,
            "params":{"queryKey":["tasks"],"reset":false}})
        );
        // No routing, resume, history, unarchive, or prompt operations follow.
        assert!(stream.read_u32_le().await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        drop(sidebar);
    }
}
