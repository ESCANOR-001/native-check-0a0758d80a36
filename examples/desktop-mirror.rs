//! Populate a new, explicitly isolated projection for original-owner phone checks.
use remodex_connect_native::{chat::Config, desktop_ipc::Endpoint, store::Store};
use serde_json::json;
use std::path::PathBuf;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err("Expected NEW_STATE CONFIG NATIVE_ID".into());
    }
    let state = PathBuf::from(&args[0]);
    if !state.is_absolute() || state.exists() || !state.to_string_lossy().contains("/.tmp/") {
        return Err("A new isolated state path is required".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    config.validate()?;
    let endpoint = Endpoint {
        path: config
            .codex
            .as_ref()
            .ok_or("Configure Codex before using the Desktop mirror")?
            .profile
            .join("ipc/ipc.sock"),
    };
    let mut client = endpoint
        .connect(
            &config
                .codex
                .as_ref()
                .ok_or("Configure Codex before using the Desktop mirror")?
                .profile,
        )
        .await?;
    client.owner(&args[2]).await?;
    let mut store = Store::open(&state)?;
    store.init_chats()?;
    let time = remodex_connect_native::auth::timestamp(remodex_connect_native::auth::now());
    let id = "desktop-original-v128";
    let chat = json!({"agent":"codex","native_id":args[2],"resumed":true,"external_codex":true,"queue":[],"paused":true,"thread":{"id":id,"nativeSessionId":args[2],"projectId":"native-test-project","title":"Desktop original owner V128","worktreePath":config.workspace,"modelSelection":{"instanceId":"codex","model":config.codex.as_ref().ok_or("Configure Codex before using the Desktop mirror")?.model},"runtimeMode":"approval-required","interactionMode":"default","createdAt":time,"updatedAt":time,"archivedAt":null,"deletedAt":null,"messages":[],"activities":[],"proposedPlans":[],"latestTurn":null,"session":{"status":"disconnected","statusConfidence":"unknown","providerName":"codex","providerInstanceId":"codex","threadId":id}}});
    store.save_chat(&chat, None)?;
    println!(
        "{}",
        json!({"seeded":true,"originalOwnerVerified":true,"historyReadPending":true})
    );
    Ok(())
}
