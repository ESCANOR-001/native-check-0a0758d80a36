//! Isolated end-to-end qualification of the native observer and durable history.
use remodex_connect_native::{
    auth::Auth, auth_service::AuthService, claude_history, claude_link, claude_observer::Observer,
    claude_turns::Input, store::Store,
};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 4 || args.len() > 5 || args[..3].iter().any(|s| !s.contains("/.tmp/")) {
        return Err(
            "Usage: mirror ISOLATED_STATE PROFILE WORKSPACE NATIVE_UUID [send-marker]".into(),
        );
    }
    let state = PathBuf::from(&args[0]);
    let profile = PathBuf::from(&args[1]);
    let workspace = PathBuf::from(&args[2]);
    let native = args[3].clone();
    let link = claude_link::read(&profile, &workspace, &native)?;
    let mut store = Store::open(&state).map_err(|_| "Cannot open isolated mirror")?;
    store
        .init_chats()
        .map_err(|_| "Cannot initialize isolated mirror")?;
    let id = format!("observer-{native}");
    if store.load_chat(&id).is_err() {
        let time = remodex_connect_native::auth::timestamp(remodex_connect_native::auth::now());
        let chat = json!({"agent":"claude","native_id":native,"resumed":true,"remote_identity":link.identity,"queue":[],"paused":true,"thread":{"id":id,"projectId":"native-test-project","title":"Native observer qualification","worktreePath":link.cwd,"modelSelection":{"instanceId":"claude-code","model":"default"},"runtimeMode":"approval-required","interactionMode":"default","createdAt":time,"updatedAt":time,"archivedAt":null,"deletedAt":null,"messages":[],"activities":[],"proposedPlans":[],"latestTurn":null,"session":{"status":"idle","providerName":"claude","providerInstanceId":"claude-code","threadId":id}}});
        store
            .save_chat(&chat, None)
            .map_err(|_| "Cannot save isolated mirror chat")?;
    }
    let cursor = store
        .claude_remote_cursor(&id, link.identity.clone())
        .map_err(|_| "Saved mirror cursor unavailable")?;
    let start = cursor.sequence;
    let db =
        AuthService::new(Auth::new(store)).map_err(|_| "Cannot start isolated mirror store")?;
    let mut observer = Observer::connect(profile, workspace, native.clone(), cursor).await?;
    let mut live_projection = observer.cursor().projection.clone();
    let marker = format!("RUST_OBSERVER_RETURN_{}", uuid::Uuid::new_v4().simple());
    let sending = args.get(4).is_some_and(|s| s == "send-marker");
    let mut sent = false;
    let mut seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    for _ in 0..10000 {
        let catchup = !observer.ready();
        let event = tokio::time::timeout_at(deadline, observer.next())
            .await
            .map_err(|_| "Observer qualification timed out")??;
        let key = id.clone();
        let state = db
            .call(move |a| a.store.load_chat(&key))
            .await?
            .map_err(|_| "Cannot read mirror projection")?;
        let mut cursor = observer.cursor().clone();
        cursor.projection = live_projection;
        let (state, projection) =
            claude_history::project(&db, state, &cursor, &event, catchup).await?;
        live_projection = projection.clone();
        if event.sequence.is_some() {
            observer.commit(&db, state.clone(), projection).await?;
        }
        if state["thread"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["role"] == "assistant" && r["text"].as_str() == Some(&marker))
        {
            seen = true;
        }
        if observer.ready()
            && (!sending || seen && state["thread"]["latestTurn"]["state"] == "completed")
        {
            println!(
                "{}",
                json!({"ready":true,"startSequence":start,"committedSequence":observer.cursor().sequence,"markerSent":sent,"markerReceived":seen,"marker":if seen{Some(&marker)}else{None},"latestState":state["thread"]["latestTurn"]["state"],"actualDesktopUiTest":false})
            );
            return Ok(());
        }
        if observer.ready() && sending && !sent {
            let input = Input {
                native: uuid::Uuid::new_v4().to_string(),
                message: uuid::Uuid::new_v4().to_string(),
                public_turn: format!("observer-turn-{}", uuid::Uuid::new_v4()),
                local: true,
                steer: false,
            };
            let text = format!("Reply exactly {marker}. Do not use tools.");
            let mut updated = state;
            updated["thread"]["messages"].as_array_mut().unwrap().push(json!({"id":input.message,"text":text,"role":"user","turnId":input.public_turn,"streaming":false,"phase":"user","createdAt":remodex_connect_native::auth::timestamp(remodex_connect_native::auth::now())}));
            updated["thread"]["latestTurn"] = json!({"turnId":input.public_turn,"state":"running"});
            db.call(move |a| a.store.save_chat(&updated, None))
                .await?
                .map_err(|_| "Could not save mirror prompt")?;
            observer.send_prompt(&db,id.clone(),input.clone(),json!({"type":"user","session_id":native,"uuid":input.native,"parent_tool_use_id":null,"message":{"role":"user","content":text}})).await?;
            sent = true;
        }
    }
    Err("Observer qualification exceeded event budget".into())
}
