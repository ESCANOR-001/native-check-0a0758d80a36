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
    if args.len() == 2 && args[1] == "--version" {
        println!(
            "{}",
            serde_json::json!({"name":"remodex-connect-native","version":env!("CARGO_PKG_VERSION"),"runtime":"rust","migrationComplete":false})
        );
        return Ok(());
    }
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        println!("Remodex native backend experiment\nUsage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [ISOLATED_CHAT_CONFIG_JSON [GUI_DIST_DIRECTORY]]\nUse only an isolated state directory and test agent profiles.\nThis candidate does not install a service or migrate normal data. migrationComplete=false");
        return Ok(());
    }
    if !(4..=6).contains(&args.len()) {
        return Err("Usage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [ISOLATED_CHAT_CONFIG_JSON [GUI_DIST_DIRECTORY]]\nExperimental: native pairing and bounded chat orchestration; not a production replacement.".into());
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
    let invite = if auth.control_enabled()? {
        Some(auth.invite("Rust native foundation", &args[3], auth::now())?)
    } else {
        None
    };
    let mut app = App::new(auth, "Rust native foundation")?;
    if let Some(path) = args.get(4) {
        let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        app.chats = Some(Chats::start(app.auth.clone(), config).await?);
    }
    if let Some(path) = args.get(5) {
        app.dashboard = Some(remodex_connect_native::dashboard::Dashboard::new(
            &PathBuf::from(path),
            &args[3],
        )?);
    }
    let invitation = invite
        .as_ref()
        .map(|v| InvitationFile::create(&directory, v))
        .transpose()?;
    let chats = app.chats.clone();
    let shutdown_chats = chats.clone();
    let shutdown_signal = app.stopping.clone();
    let mut requested_stop = shutdown_signal.subscribe();
    println!(
        "{}",
        serde_json::json!({"ready":true,"address":listener.local_addr()?.to_string(),
        "invitationFile":invitation.as_ref().map(InvitationFile::path),"migrationComplete":false})
    );
    let served = axum::serve(
        listener,
        app.router()
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = stopping => {},
            _ = requested_stop.wait_for(|requested| *requested) => {},
        }
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
