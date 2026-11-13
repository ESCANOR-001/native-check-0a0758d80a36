use axum::serve::{Listener as _, ListenerExt as _};
use remodex_connect_native::invitation_file::InvitationFile;
use remodex_connect_native::{
    auth::{self, Auth},
    chat::{AgentKind, Chats},
    server::App,
    store::Store,
};
use std::{future::IntoFuture, path::PathBuf, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = remodex_connect_native::native_cli::normalize(std::env::args().collect())?;
    if args.get(1).is_some_and(|s| s == "--native-tray") {
        if args.len() != 5 && !(args.len() == 6 && matches!(args[2].as_str(), "host" | "host-show"))
        {
            return Err("Usage: rmx tray [status|quit]".into());
        }
        println!(
            "{}",
            remodex_connect_native::native_tray::run(
                &args[2],
                &PathBuf::from(&args[3]),
                &PathBuf::from(&args[4]),
                args.get(5).map(std::path::Path::new)
            )?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "--legacy-quick-launch") && args.len() == 6 {
        let config = remodex_connect_native::legacy_quick_launcher::launch(
            &PathBuf::from(&args[2]),
            &args[3],
            &args[4],
            &PathBuf::from(&args[5]),
        )?;
        println!("{}", serde_json::to_string(&config)?);
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "--legacy-quick-control") && args.len() == 4 {
        if !matches!(args[2].as_str(), "status" | "stop" | "wait") {
            return Err("Invalid tunnel control operation".into());
        }
        println!(
            "{}",
            remodex_connect_native::legacy_quick_launcher::control(
                &PathBuf::from(&args[3]),
                &args[2]
            )?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-package") {
        let publishable_beta = args.len() == 6 && args[5] == "--publishable-beta";
        if args.len() != 5 && !publishable_beta {
            return Err("Usage: rmx package NEW_OUTPUT_DIR --gui GUI_DIR --version VERSION [--publishable-beta]".into());
        }
        let build = if publishable_beta {
            remodex_connect_native::native_package::build_beta
        } else {
            remodex_connect_native::native_package::build
        };
        println!(
            "{}",
            build(
                &std::env::current_exe()?,
                &PathBuf::from(&args[3]),
                &PathBuf::from(&args[2]),
                &args[4]
            )?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-legacy-prepare") && args.len() == 4 {
        println!(
            "{}",
            remodex_connect_native::npm_upgrade::prepare_with_gui(
                &PathBuf::from(&args[2]),
                &PathBuf::from(&args[3])
            )
            .await?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-legacy-start") {
        if !(args.len() == 4 || args.len() == 6 && args[4] == "--port") {
            return Err("Invalid native upgrade start command".into());
        }
        let root = PathBuf::from(&args[2]);
        let gui = PathBuf::from(&args[3]);
        let port = args.get(5).map(|p| p.parse::<u16>()).transpose()?;
        let code =
            remodex_connect_native::npm_upgrade::start_with_gui(&root, &gui, port, shutdown()?)
                .await?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--features") && args.len() == 2 {
        println!("{}", remodex_connect_native::feature_policy::catalog());
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--retired-feature") {
        return Err(remodex_connect_native::feature_policy::retired(
            args.get(2).map(String::as_str).unwrap_or(""),
        )
        .unwrap_or("Unsupported native feature")
        .into());
    }
    if args.get(1).is_some_and(|a| a == "--save-tunnel-credential") {
        if args.len() != 3 {
            return Err("Invalid credential helper request".into());
        }
        remodex_connect_native::named_tunnel::save_helper(&args[2])?;
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-config") {
        if args.len() < 4 {
            return Err("Usage: config show | check FILE | apply FILE --revision N | rollback N --revision N".into());
        }
        let root = PathBuf::from(&args[3]);
        let op = args[2].as_str();
        let result = if op == "show" && args.len() == 4 {
            remodex_connect_native::installation::inspect(&root)?
        } else if op == "check" && args.len() == 5 {
            remodex_connect_native::installation::configure(&root, op, &args[4], None)?
        } else if matches!(op, "apply" | "rollback") && args.len() == 7 && args[5] == "--revision" {
            remodex_connect_native::installation::configure(
                &root,
                op,
                &args[4],
                Some(args[6].parse()?),
            )?
        } else {
            return Err("Invalid native configuration command".into());
        };
        println!("{result}");
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-provision") {
        if args.len() != 8 || args[6] != "--revision" {
            return Err(
                "Usage: remote provision ACCOUNT ZONE HOST --revision N (API token on stdin)"
                    .into(),
            );
        }
        println!(
            "{}",
            remodex_connect_native::installation::provision(
                &PathBuf::from(&args[2]),
                &args[3],
                &args[4],
                &args[5],
                remodex_connect_native::named_tunnel::read_secret_stdin()?,
                args[7].parse()?
            )
            .await?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-remote") {
        if args.len() < 6 {
            return Err("Usage: remote local|quick --revision N | named HOST --revision N (connector token on stdin)".into());
        }
        let mode = args[2].as_str();
        let root = PathBuf::from(&args[3]);
        let (host, revision, secret) =
            if matches!(mode, "local" | "quick") && args.len() == 6 && args[4] == "--revision" {
                (None, args[5].parse()?, None)
            } else if mode == "named" && args.len() == 7 && args[5] == "--revision" {
                (
                    Some(args[4].as_str()),
                    args[6].parse()?,
                    Some(remodex_connect_native::named_tunnel::read_secret_stdin()?),
                )
            } else {
                return Err("Invalid native remote command".into());
            };
        println!(
            "{}",
            remodex_connect_native::installation::remote(&root, mode, host, secret, revision)
                .await?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-service") {
        if args.len() < 4 {
            return Err("Usage: service prepare DIR | install|start|status|uninstall DIR | replace NEW_DIR OLD_DIR".into());
        }
        let op = args[2].as_str();
        let root = PathBuf::from(&args[3]);
        if op == "launch" && args.len() == 4 {
            let code = remodex_connect_native::native_service::launch(&root, shutdown()?).await?;
            if code != 0 {
                std::process::exit(code)
            }
            return Ok(());
        }
        let result = if op == "halt" && args.len() == 4 {
            remodex_connect_native::native_service::halt(&root).await?
        } else if op == "scm-activate" && args.len() == 4 {
            remodex_connect_native::native_service::activate_windows_service(&root)?
        } else if op == "task-retire" && args.len() == 4 {
            remodex_connect_native::native_service::retire_windows_task(&root)?
        } else if op == "prepare" && args.len() == 6 {
            remodex_connect_native::native_service::prepare(
                &root,
                &PathBuf::from(&args[4]),
                &PathBuf::from(&args[5]),
                &std::env::current_exe()?,
            )?
        } else if op == "replace" && args.len() == 5 {
            remodex_connect_native::native_service::replace(&root, &PathBuf::from(&args[4])).await?
        } else if args.len() == 4 {
            remodex_connect_native::native_service::run(op, &root).await?
        } else {
            return Err("Invalid native service command".into());
        };
        println!("{result}");
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--native-install") {
        let operation = args.get(2).map(String::as_str).unwrap_or("");
        if args.len() < 5 || !matches!(operation, "onboard" | "start") {
            return Err("Usage: --native-install onboard|start INSTALL_DIR GUI_DIR [--port PORT] [--config CONFIG_JSON]".into());
        }
        let root = PathBuf::from(&args[3]);
        let gui = PathBuf::from(&args[4]);
        let (mut port, mut config) = (None, None);
        let mut options = args[5..].chunks_exact(2);
        for pair in &mut options {
            match pair[0].as_str() {
                "--port" if port.is_none() => {
                    port = Some(
                        pair[1]
                            .parse::<u16>()
                            .map_err(|_| "Invalid dashboard port")?,
                    )
                }
                "--config" if config.is_none() && operation == "onboard" => {
                    config = Some(PathBuf::from(&pair[1]))
                }
                _ => return Err("Unknown or repeated native installation option".into()),
            }
        }
        if !options.remainder().is_empty() {
            return Err("Native installation option needs a value".into());
        }
        if operation == "onboard" {
            println!(
                "{}",
                remodex_connect_native::installation::prepare(&root, &gui, port, config.as_deref())
                    .await?
            );
        } else {
            let code =
                remodex_connect_native::installation::start(&root, &gui, port, shutdown()?).await?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--named-tunnel-helper") {
        match remodex_connect_native::named_tunnel::helper(&args).await {
            Ok(0) => return Ok(()),
            _ => std::process::exit(1),
        }
    }
    if args.get(1).is_some_and(|a| a == "--capture-codex-history") {
        if args.len() != 5 {
            return Err(
                "Usage: --capture-codex-history CONFIG_JSON NATIVE_ID NEW_OUTPUT_JSONL".into(),
            );
        }
        println!(
            "{}",
            remodex_connect_native::codex_history_reader::capture_from_config(
                &PathBuf::from(&args[2]),
                &args[3],
                &PathBuf::from(&args[4])
            )
            .await?
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--npm-upgrade") {
        let root = args
            .get(3)
            .map(PathBuf::from)
            .ok_or("An installation directory is required")?;
        match (args.get(2).map(String::as_str), args.len()) {
            (Some("prepare-source"), 4) => println!("{}", remodex_connect_native::npm_upgrade::prepare_registered(&root).await?),
            (Some("prepare-explicit-source"), 5) => {
                let plan = remodex_connect_native::upgrade_source::acquire(&root, &PathBuf::from(&args[4])).await?;
                println!("{}", remodex_connect_native::npm_upgrade::prepare(&root, &plan)?);
            }
            (Some("start"), 4) => {
                let code = remodex_connect_native::npm_upgrade::start(&root, shutdown()?).await?;
                if code != 0 { std::process::exit(code); }
            }
            (Some("start"), 5) => {
                let port: u16 = args[4].parse().map_err(|_| "Invalid dashboard port")?;
                let code = remodex_connect_native::npm_upgrade::start_on_port(&root, port, shutdown()?).await?;
                if code != 0 { std::process::exit(code); }
            }
            (Some("start-gui"), 5 | 6) => {
                let port = args.get(5).map(|p| p.parse::<u16>().map_err(|_| "Invalid dashboard port")).transpose()?;
                let code = remodex_connect_native::npm_upgrade::start_with_gui(&root, &PathBuf::from(&args[4]), port, shutdown()?).await?;
                if code != 0 { std::process::exit(code); }
            }
            (Some("automatic"), 5) => {
                let descriptor = PathBuf::from(&args[4]);
                let code = remodex_connect_native::npm_upgrade::launch(&root, &descriptor, shutdown()?).await?;
                if code != 0 { std::process::exit(code); }
            }
            (Some("resume"), 4) => {
                let code=remodex_connect_native::npm_upgrade::resume(&root,shutdown()?).await?;
                if code!=0 {std::process::exit(code);}
            }
            (Some(operation @ ("status"|"stop"|"restart")), 4) => println!("{}",remodex_connect_native::npm_upgrade::command(&root,operation).await?),
            (Some("prepare"), 5) => println!(
                "{}",
                remodex_connect_native::npm_upgrade::prepare(&root, &PathBuf::from(&args[4]))?
            ),
            (Some("run"), 5) => {
                let code = remodex_connect_native::npm_upgrade::run(
                    &root,
                    &PathBuf::from(&args[4]),
                    shutdown()?,
                )
                .await?;
                if code != 0 {
                    std::process::exit(code);
                }
            }
            (Some("cancel"), 4) => {
                println!("{}", remodex_connect_native::npm_upgrade::cancel(&root)?)
            }
            _ => {
                return Err(
                    "Usage: --npm-upgrade start|resume|status|cancel INSTALL_DIR | prepare|run INSTALL_DIR PLAN_JSON | automatic INSTALL_DIR SOURCE_JSON"
                        .into(),
                )
            }
        }
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--background-service") {
        let operation = args.get(2).map(String::as_str).unwrap_or("");
        let report = if operation == "prepare" && args.len() == 5 {
            remodex_connect_native::service_registration::prepare(
                &PathBuf::from(&args[3]),
                &PathBuf::from(&args[4]),
                &std::env::current_exe()?,
            )?
        } else if operation == "replace" && args.len() == 5 {
            remodex_connect_native::service_registration::replace(
                &PathBuf::from(&args[3]),
                &PathBuf::from(&args[4]),
            )
            .await?
        } else if matches!(operation, "install" | "start" | "status" | "uninstall")
            && args.len() == 4
        {
            remodex_connect_native::service_registration::run(operation, &PathBuf::from(&args[3]))
                .await?
        } else {
            return Err("Usage: --background-service prepare REGISTRATION_DIR CUTOVER_DIR | install|start|status|uninstall REGISTRATION_DIR | replace NEW_REGISTRATION_DIR PREVIOUS_REGISTRATION_DIR".into());
        };
        println!("{report}");
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "--service-cutover") {
        let operation = args.get(2).map(String::as_str).unwrap_or("");
        if !matches!(
            (operation, args.len()),
            ("select", 5) | ("run" | "cancel", 4)
        ) {
            return Err(
                "Usage: --service-cutover select SERVICE_DIR PLAN_JSON | run|cancel SERVICE_DIR"
                    .into(),
            );
        }
        let directory = PathBuf::from(&args[3]);
        match operation {
            "select" => println!(
                "{}",
                remodex_connect_native::service_cutover::select(
                    &directory,
                    &PathBuf::from(&args[4])
                )?
            ),
            "cancel" => println!(
                "{}",
                remodex_connect_native::service_cutover::cancel(&directory)?
            ),
            _ => {
                let code =
                    remodex_connect_native::service_cutover::run(&directory, shutdown()?).await?;
                if code != 0 {
                    std::process::exit(code);
                }
            }
        }
        return Ok(());
    }
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
    if args.get(1).is_some_and(|a| a == "--legacy-export") {
        if args.len() == 4 && args[2] == "recover-source" {
            println!(
                "{}",
                remodex_connect_native::export_recovery::run(&PathBuf::from(&args[3]))?
            );
            return Ok(0);
        }
        if args.len() != 5 {
            return Err(
                "Usage: --legacy-export ABSOLUTE_NEW_ROOT CLOSED_OWNER_JSON CONFIG_JSON | recover-source STOPPED_OWNER_JSON".into(),
            );
        }
        let report = remodex_connect_native::legacy_export::run(
            &PathBuf::from(&args[2]),
            &PathBuf::from(&args[3]),
            &PathBuf::from(&args[4]),
        )?;
        println!("{report}");
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--legacy-upgrade") {
        if args.len() != 5 {
            return Err(
                "Usage: --legacy-upgrade prepare|check|rollback ABSOLUTE_NEW_ROOT PLAN_JSON".into(),
            );
        }
        let report = remodex_connect_native::legacy_upgrade::run(
            &args[2],
            &PathBuf::from(&args[3]),
            &PathBuf::from(&args[4]),
        )?;
        println!("{report}");
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--legacy-activate") {
        if args.len() != 4 {
            return Err(
                "Usage: --legacy-activate ABSOLUTE_OFFLINE_STATE_DIR ISOLATED_CHAT_CONFIG_JSON"
                    .into(),
            );
        }
        let report = remodex_connect_native::legacy_activation::run(
            &PathBuf::from(&args[2]),
            &PathBuf::from(&args[3]),
        )?;
        println!("{report}");
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--legacy-stage") {
        if args.len() != 5 {
            return Err("Usage: --legacy-stage ABSOLUTE_EXPORT_SNAPSHOT ABSOLUTE_NEW_GENERATION ENVIRONMENT_ID".into());
        }
        let report = remodex_connect_native::legacy_stage::run(
            &PathBuf::from(&args[2]),
            &PathBuf::from(&args[3]),
            &args[4],
        )?;
        println!("{report}");
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--legacy-rollback") {
        if args.len() != 4 || !matches!(args[2].as_str(), "check" | "apply") {
            return Err(
                "Usage: --legacy-rollback check|apply ABSOLUTE_OFFLINE_MIGRATION_DIR".into(),
            );
        }
        let report = remodex_connect_native::legacy_rollback::run(
            &PathBuf::from(&args[3]),
            args[2] == "apply",
        )?;
        println!("{report}");
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--check-config") {
        if args.len() != 3 {
            return Err("Usage: --check-config ISOLATED_CHAT_CONFIG_JSON".into());
        }
        let config = remodex_connect_native::launch_config::load(&PathBuf::from(&args[2]))?;
        println!(
            "{}",
            serde_json::json!({"configurationValid":true,"configurationVersion":1,"nativeProfileHandoff":config.native_profile_handoff.is_some(),"runtimeStarted":false,"migrationComplete":false})
        );
        return Ok(0);
    }
    if args.get(1).is_some_and(|a| a == "--native-handoff") {
        if args.len() == 6 && args[2] == "capture" {
            let result = remodex_connect_native::profile_handoff::capture_export(
                &PathBuf::from(&args[3]),
                &PathBuf::from(&args[4]),
                &args[5],
            )?;
            println!("{result}");
            return Ok(0);
        }
        if args.len() != 5 {
            return Err("Usage: --native-handoff prepare ABSOLUTE_GENERATION_DIR PLAN_JSON | --native-handoff rollback ABSOLUTE_GENERATION_DIR ENVIRONMENT_ID".into());
        }
        let root = PathBuf::from(&args[3]);
        let result = match args[2].as_str() {
            "prepare" => remodex_connect_native::profile_handoff::prepare_file(
                &root,
                &PathBuf::from(&args[4]),
            )?,
            "rollback" => remodex_connect_native::profile_handoff::rollback(&root, &args[4])?,
            _ => return Err("Unknown native handoff operation".into()),
        };
        println!("{result}");
        return Ok(0);
    }
    if args.len() == 2 && args[1] == "--version" {
        let version =
            remodex_connect_native::native_package::verify_if_packaged(&std::env::current_exe()?)?
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
        println!(
            "{}",
            serde_json::json!({"name":"remodex-connect-native","version":version,"runtime":"rust","launcher":"rust","configurationVersion":1,"storageFormatVersion":1,"existingProfileHandoffVersion":1,"migrationComplete":false})
        );
        return Ok(0);
    }
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        println!("Remodex native backend candidate\nFresh setup: rmx onboard [--port PORT] [--config FILE], then rmx start\nSaved installation: rmx start uses its verified upgrade/recovery path.\nLifecycle: rmx status | stop | restart\nWindows tray: rmx tray [status|quit] (quit leaves the server running)\nMigration: rmx migrate [--source SOURCE_JSON | --plan PLAN_JSON]\nConfiguration: rmx config show|check|apply|rollback\nRemote: rmx remote local|quick|named|provision\nCapabilities: rmx features\nBackground startup: rmx service prepare|install|start|status|uninstall|replace\nNative package: rmx package NEW_OUTPUT_DIR --gui GUI_DIST_DIR --version VERSION [--publishable-beta]\nOPENCODEX_HOME selects saved state; REMODEX_GUI_DIR optionally selects dashboard assets.\nThe executable handles these commands directly; Node and Bun are not required.\nOld running bridges must provide their verified owner handoff; no compatibility runtime is launched.\nLow-level isolated usage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [CONFIG_JSON [GUI_DIST_DIR]]\nExperimental native server; OS-service support is qualified separately. migrationComplete=false");
        return Ok(0);
    }
    if !(4..=6).contains(&args.len()) {
        return Err("Usage: remodex-connect-native ABSOLUTE_ISOLATED_STATE_DIR BIND_ADDRESS PUBLIC_URL [ISOLATED_CHAT_CONFIG_JSON [GUI_DIST_DIRECTORY]]\nExperimental: native pairing and bounded chat orchestration; not a production replacement.".into());
    }
    // Reject an incompatible upgrade before opening SQLite, claiming ownership,
    // changing permissions, or clearing a previous pairing invitation.
    let config = args
        .get(4)
        .map(|path| remodex_connect_native::launch_config::load(&PathBuf::from(path)))
        .transpose()?;
    let connection = config.as_ref().and_then(|c| c.connection.clone());
    let directory = PathBuf::from(&args[1]);
    remodex_connect_native::installation::before_start(&directory, config.as_ref())?;
    let _profile_lock = remodex_connect_native::profile_lock::acquire(&directory)?;
    remodex_connect_native::legacy_activation::before_start(&directory, config.as_ref())?;
    InvitationFile::clear_previous(&directory)?;
    // Install signal handlers before advertising readiness. Otherwise an
    // immediate service stop can use the OS default instead of our cleanup.
    let stopping = shutdown()?;
    let listener = remodex_connect_native::listener::bind_async(&args[2]).await?;
    let dashboard_listener = match &connection {
        Some(c) => Some(remodex_connect_native::listener::bind_async(c.dashboard_bind).await?),
        None => None,
    };
    if let Some(origin) = connection.as_ref().and_then(|c| {
        c.named_tunnel
            .as_ref()
            .map(|t| t.origin)
            .or_else(|| c.inherited_quick_tunnel.as_ref().map(|t| t.origin))
    }) {
        if origin.port() != listener.local_addr()?.port() || dashboard_listener.is_none() {
            return Err("The saved tunnel must target the phone-only listener".into());
        }
    }
    let store = Store::open(&directory)?;
    remodex_connect_native::installation::bind_store(&store, config.as_ref())?;
    store.assert_migration_configuration(config.as_ref())?;
    let mut auth = Auth::new(store);
    let invite = if auth.control_enabled()?
        && auth.local_network_enabled()?
        && !auth.quick_tunnel_enabled()?
    {
        Some(
            auth.invite(
                connection
                    .as_ref()
                    .and_then(|c| c.label.as_deref())
                    .unwrap_or("Remodex"),
                &args[3],
                auth::now(),
            )?,
        )
    } else {
        None
    };
    let mut app = App::with_configuration(
        auth,
        connection
            .as_ref()
            .and_then(|c| c.label.as_deref())
            .unwrap_or("Remodex"),
        config.as_ref(),
    )?;
    app.local_url = Some(args[3].clone());
    let diagnostics = remodex_connect_native::diagnostics::Log::new(&directory)?;
    app.diagnostics = Some(diagnostics.clone());
    let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if parent.is_some() {
        app.restart = Some(restart.clone());
    }
    let stop_restart = restart.clone();
    if let Some(config) = config {
        app.chats = Some(Chats::start(app.auth.clone(), config).await?);
    }
    if let Some(path) = args.get(5) {
        app.dashboard = Some(remodex_connect_native::dashboard::Dashboard::new(
            &PathBuf::from(path),
            &args[3],
        )?);
    }
    // The managed helper is acquired lazily, only when quick mode is enabled.
    // Its cache and configuration belong exclusively to this native profile.
    let tunnel_origin = Some(
        remodex_connect_native::tunnel::Origin::start(
            &mut app,
            connection
                .as_ref()
                .and_then(|c| c.cloudflared.clone())
                .or_else(|| std::env::var_os("REMODEX_NATIVE_CLOUDFLARED").map(PathBuf::from)),
            &directory,
            connection.as_ref().and_then(|c| c.named_tunnel.clone()),
            connection
                .as_ref()
                .and_then(|c| c.inherited_quick_tunnel.clone()),
        )
        .await?,
    );
    let shutdown_tunnel = app.tunnel.clone();
    app.watch_local_network(listener.local_addr()?).await;
    let management = dashboard_listener.map(|listener| {
        remodex_connect_native::management_listener::Server::start(listener, app.clone())
    });
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
        (if connection.is_some() {
            app.phone_router()
        } else {
            app.router()
        })
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
    if let Some(management) = management {
        management.shutdown().await?;
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
