use axum::serve::{Listener as _, ListenerExt as _};
use remodex_connect_native::invitation_file::InvitationFile;
use remodex_connect_native::{
    auth::{self, Auth},
    chat::{AgentKind, Chats, Config},
    server::App,
    store::Store,
};
use std::{future::IntoFuture, path::PathBuf, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--supervise") {
        let code = remodex_connect_native::supervisor::run(
            std::env::args_os().skip(2).collect(),
            shutdown()?,
        )
        .await?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let parent = if args.get(1).is_some_and(|a| a == "--supervised-child") {
        args.remove(1);
        Some(remodex_connect_native::supervisor::control().await?)
    } else {
        None
    };
    let code = serve(args, parent).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn serve(
    args: Vec<String>,
    parent: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<i32, Box<dyn std::error::Error>> {
    if args.len() == 2 && args[1] == "--version" {
        println!(
            "{}",
            serde_json::json!({"name":"remodex-connect-native","version":env!("CARGO_PKG_VERSION"),"runtime":"rust","migrationComplete":false})
        );
        return Ok(0);
    }
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        println!("Remodex native backend experiment\nUsage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [ISOLATED_CHAT_CONFIG_JSON [GUI_DIST_DIRECTORY]]\nOptional --supervise starts a native supervisor with idle-only dashboard restart.\nUse only an isolated state directory and test agent profiles.\nThis candidate does not install a service or migrate normal data. migrationComplete=false");
        return Ok(0);
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
    let invite = if auth.control_enabled()?
        && auth.local_network_enabled()?
        && !auth.quick_tunnel_enabled()?
    {
        Some(auth.invite("Rust native foundation", &args[3], auth::now())?)
    } else {
        None
    };
    let mut app = App::new(auth, "Rust native foundation")?;
    app.local_url = Some(args[3].clone());
    let diagnostics = remodex_connect_native::diagnostics::Log::new(&directory)?;
    app.diagnostics = Some(diagnostics.clone());
    let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if parent.is_some() {
        app.restart = Some(restart.clone());
    }
    let stop_restart = restart.clone();
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
    // Explicit candidate-only helper selection. Normal service configuration,
    // credentials, and existing cloudflared processes are never changed.
    let tunnel_origin = if let Some(executable) = std::env::var_os("REMODEX_NATIVE_CLOUDFLARED") {
        Some(
            remodex_connect_native::tunnel::Origin::start(
                &mut app,
                PathBuf::from(executable),
                &directory,
            )
            .await?,
        )
    } else {
        None
    };
    let shutdown_tunnel = app.tunnel.clone();
    let invitation = invite
        .as_ref()
        .map(|v| InvitationFile::create(&directory, v))
        .transpose()?;
    let listener = app.listener(listener).tap_io(|_| {});
    let chats = app.chats.clone();
    let shutdown_chats = chats.clone();
    app.start_storage_scheduler();
    let shutdown_storage = app.clone();
    let shutdown_signal = app.stopping.clone();
    let mut requested_stop = shutdown_signal.subscribe();
    let mut cleanup_stop = shutdown_signal.subscribe();
    let shutdown_log = diagnostics.clone();
    diagnostics.lifecycle(remodex_connect_native::diagnostics::Lifecycle::Ready);
    println!(
        "{}",
        serde_json::json!({"ready":true,"pid":std::process::id(),"address":listener.local_addr()?.to_string(),
        "invitationFile":invitation.as_ref().map(InvitationFile::path),"migrationComplete":false})
    );
    let parent_stop = async move {
        if let Some(parent) = parent {
            let _ = parent.await;
        } else {
            std::future::pending::<()>().await
        }
    };
    let serving = axum::serve(
        listener,
        app.router()
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            biased;
            _ = stopping => {stop_restart.store(false,std::sync::atomic::Ordering::Release);},
            _ = parent_stop => {stop_restart.store(false,std::sync::atomic::Ordering::Release);},
            _ = requested_stop.wait_for(|requested| *requested) => {},
        }
        shutdown_log.lifecycle(remodex_connect_native::diagnostics::Lifecycle::Stopping);
        shutdown_signal.send_replace(true);
        if let Some(tunnel) = shutdown_tunnel {
            tunnel.shutdown().await;
        }
        shutdown_storage.drain_storage_policy().await;
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
    .into_future();
    tokio::pin!(serving);
    // A slow response reader cannot hold the old listener indefinitely. Failure
    // to finish cleanup is terminal and never authorizes a replacement process.
    let served = tokio::select! {
        result = &mut serving => result,
        _ = cleanup_stop.wait_for(|requested| *requested) => {
            tokio::time::timeout(Duration::from_secs(15), &mut serving).await
                .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Bridge cleanup did not finish; restart was cancelled")))
        }
    };
    if let Some(origin) = tunnel_origin {
        origin.shutdown().await?;
    }
    drop(invitation);
    if let Some(chats) = chats {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            chats.call(
                AgentKind::Codex,
                "local",
                "native.shutdown",
                serde_json::json!({}),
            ),
        )
        .await
        .map_err(|_| "Chat cleanup timed out; restart was cancelled")?;
    }
    diagnostics.lifecycle(remodex_connect_native::diagnostics::Lifecycle::Stopped);
    diagnostics.flush().await;
    served?;
    Ok(if restart.load(std::sync::atomic::Ordering::Acquire) {
        remodex_connect_native::supervisor::RESTART_EXIT
    } else {
        0
    })
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
