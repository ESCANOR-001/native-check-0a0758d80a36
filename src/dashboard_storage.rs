//! Existing dashboard storage contract, guarded by its local session and CSRF.
use super::*;
use crate::{storage_cleanup::Failure, storage_report::Mutation};
type Body = Result<Json<Value>, axum::extract::rejection::JsonRejection>;
pub(super) async fn policy_read(State(app): State<App>) -> Response {
    match app.storage_policy.read().await {
        Ok(v) => response(StatusCode::OK, v),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}
pub(super) async fn policy_save(State(app): State<App>, body: Body) -> Response {
    let Ok(Json(body)) = body else {
        return failed("invalid_json".into());
    };
    if *app.stopping.borrow() {
        return error(StatusCode::CONFLICT, "Bridge is stopping");
    }
    match app.storage_policy.save(body).await {
        Ok(v) => response(StatusCode::OK, v),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}
pub(super) async fn policy_run(State(app): State<App>) -> Response {
    let Some(chats) = app.chats else {
        return failed("storage_unavailable".into());
    };
    match app
        .storage_policy
        .launch(chats, app.stopping, crate::storage_policy::Reason::Manual)
        .await
    {
        Ok(Some(v)) => response(StatusCode::OK, v),
        Ok(None) => error(StatusCode::SERVICE_UNAVAILABLE, "Cleanup did not start"),
        Err(e) => error(
            if matches!(e.as_str(), "already_running" | "stopping") {
                StatusCode::CONFLICT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            &e,
        ),
    }
}

pub(super) async fn cleanup(State(app): State<App>, body: Body) -> Response {
    let Ok(Json(body)) = body else {
        return failed("invalid_json".into());
    };
    let Some(object) = body.as_object().filter(|v| {
        v.len() == 3
            && v.keys()
                .all(|k| matches!(k.as_str(), "percent" | "mode" | "digest"))
    }) else {
        return failed("invalid_request".into());
    };
    let Some(percent) = object["percent"]
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
    else {
        return failed("invalid_percent".into());
    };
    let Some(mode) = object["mode"]
        .as_str()
        .filter(|v| matches!(*v, "quarantine" | "permanent"))
    else {
        return failed("invalid_mode".into());
    };
    let Some(digest) = object["digest"]
        .as_str()
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return failed("invalid_digest".into());
    };
    mutate(
        app,
        Mutation::Cleanup {
            percent,
            mode: mode.into(),
            digest: digest.into(),
        },
    )
    .await
}
pub(super) async fn restore(State(app): State<App>, body: Body) -> Response {
    match id(body) {
        Ok(id) => mutate(app, Mutation::Restore(id)).await,
        Err(e) => failed(e),
    }
}
pub(super) async fn resume_purge(State(app): State<App>, body: Body) -> Response {
    match id(body) {
        Ok(id) => mutate(app, Mutation::ResumePurge(id)).await,
        Err(e) => failed(e),
    }
}
fn id(body: Body) -> Result<String, Failure> {
    let Json(body) = body.map_err(|_| Failure::from("invalid_json"))?;
    let object = body
        .as_object()
        .filter(|v| v.len() == 1)
        .ok_or("invalid_request")?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or("invalid_trash")?;
    let name = id.strip_prefix(".trash/").ok_or("invalid_trash")?;
    let parts: Vec<_> = name.split('-').collect();
    if !(1..=2).contains(&parts.len())
        || parts
            .iter()
            .any(|v| v.is_empty() || v.len() > 20 || !v.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid_trash".into());
    }
    Ok(id.into())
}
async fn mutate(app: App, action: Mutation) -> Response {
    let Some(chats) = app.chats else {
        return failed("storage_unavailable".into());
    };
    let (reservation, protected) = match chats.prepare_storage().await {
        Ok(v) => v,
        Err(e) => {
            return failed(
                if e == "codex_busy" {
                    "codex_busy"
                } else {
                    "storage_mutation_busy"
                }
                .into(),
            )
        }
    };
    if *app.stopping.borrow() {
        return failed("storage_mutation_busy".into());
    }
    match chats
        .storage_report
        .mutate(action, reservation, protected)
        .await
    {
        Ok(v) => response(StatusCode::OK, v),
        Err(e) => failed(e),
    }
}
fn failed(f: Failure) -> Response {
    let status = match f.error {
        "invalid_json" | "invalid_request" | "invalid_percent" | "invalid_mode"
        | "invalid_digest" | "invalid_trash" => StatusCode::BAD_REQUEST,
        "missing_trash" => StatusCode::NOT_FOUND,
        "storage_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "codex_busy"
        | "stale_preview"
        | "referenced_history"
        | "pinned_thread"
        | "storage_mutation_busy"
        | "restore_pending_overlap"
        | "dest_exists"
        | "storage_restore_conflict"
        | "storage_schema_changed"
        | "storage_database_changed"
        | "purge_started"
        | "purge_not_started" => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let message = match f.error {
        "codex_busy" => "A task or agent metadata operation is active. Wait for it to finish, then retry cleanup.",
        "stale_preview" => "Archived files changed. Run Preview again before cleanup.",
        "referenced_history" => "A chat still references the selected history. No archives were removed.",
        "pinned_thread" => "A selected chat is pinned. Unpin it before cleanup.",
        "storage_mutation_busy" => "Another storage operation or bridge restart is in progress. Retry shortly.",
        "purge_started" => "Permanent deletion has started for this entry and cannot be undone. Resume that deletion to finish it.",
        "purge_not_started" => "This entry is not an interrupted permanent deletion. It was kept unchanged.",
        "storage_restore_conflict" | "dest_exists" => "Newer history or files conflict with this restore. They were kept unchanged.",
        "storage_database_changed" | "storage_schema_changed" => "The agent database changed since cleanup. The recovery backup was kept.",
        "missing_trash" => "This trash entry is no longer available. Refresh the list.",
        "legacy_trash" => "This archive was created by the previous bridge. Its recovery files have been kept.",
        _ => "Storage operation could not finish. Recovery data was kept where available; refresh the list before retrying.",
    };
    let mut value = json!({"ok":false,"error":f.error,"message":message});
    if let Some(id) = f.trash_dir {
        value["trashDir"] = json!(id);
    }
    response(status, value)
}
