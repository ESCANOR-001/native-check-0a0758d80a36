use remodex_connect_native::invitation_file::InvitationFile;
use remodex_connect_native::{
    auth::{self, Auth},
    chat::{AgentKind, Chats, Config},
    server::App,
    store::Store,
};
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 && args.len() != 5 {
        return Err("Usage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [ISOLATED_CHAT_CONFIG_JSON]\nExperimental: native pairing and bounded chat orchestration; not a production replacement.".into());
    }
    let directory = PathBuf::from(&args[1]);
    let _profile_lock = remodex_connect_native::profile_lock::acquire(&directory)?;
    InvitationFile::clear_previous(&directory)?;
    // Install signal handlers before advertising readiness. Otherwise an
    // immediate service stop can use the OS default instead of our cleanup.
    let stopping = shutdown()?;
    let listener = tokio::net::TcpListener::bind(&args[2]).await?;
    let store = Store::open(&directory)?;
    let mut auth = Auth::new(store);
    let invite = auth.invite("Rust native foundation", &args[3], auth::now())?;
    let mut app = App::new(auth, "Rust native foundation")?;
    if let Some(path) = args.get(4) {
        let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        app.chats = Some(Chats::start(app.auth.clone(), config).await?);
    }
    let invitation = InvitationFile::create(&directory, &invite)?;
    let chats = app.chats.clone();
    let shutdown_chats = chats.clone();
    let shutdown_signal = app.stopping.clone();
    println!(
        "{}",
        serde_json::json!({"ready":true,"address":listener.local_addr()?.to_string(),
        "invitationFile":invitation.path(),"migrationComplete":false})
    );
    let served = axum::serve(listener, app.router())
        .with_graceful_shutdown(async move {
            stopping.await;
            shutdown_signal.send_replace(true);
            if let Some(chats) = shutdown_chats {
                let _ = chats
                    .call(
                        AgentKind::Codex,
                        "local",
                        "native.shutdown",
                        serde_json::json!({}),
                    )
                    .await;
            }
        })
        .await;
    drop(invitation);
    if let Some(chats) = chats {
        let _ = chats
            .call(
                AgentKind::Codex,
                "local",
                "native.shutdown",
                serde_json::json!({}),
            )
            .await;
    }
    served?;
    Ok(())
}

fn shutdown() -> std::io::Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
    }
    #[cfg(windows)]
    {
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        let mut close = tokio::signal::windows::ctrl_close()?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = close.recv() => {},
            }
        })
    }
}
