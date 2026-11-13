//! Preserve a separate local dashboard without exposing it through the phone
//! port or a reverse tunnel. A failed listener stops the whole owned generation.
use crate::server::App;
use std::{io, net::SocketAddr, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
pub struct Server(Option<JoinHandle<io::Result<()>>>);
impl Server {
    pub fn start(listener: TcpListener, app: App) -> Self {
        let signal = app.stopping.clone();
        let mut stopping = signal.subscribe();
        Self(Some(tokio::spawn(async move {
            let result = axum::serve(
                listener,
                app.router()
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopping.wait_for(|v| *v).await;
            })
            .await;
            signal.send_replace(true);
            result
        })))
    }
    pub async fn shutdown(mut self) -> io::Result<()> {
        let Some(mut task) = self.0.take() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
            Ok(result) => result.map_err(io::Error::other)?,
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(io::Error::other(
                    "Dashboard cleanup did not finish; restart was cancelled",
                ))
            }
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
