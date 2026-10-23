use remodex_connect_native::store::Store;
use serde_json::{json, Value};
use std::collections::HashSet;
fn state(rows: Vec<Value>) -> Value {
    json!({"agent":"codex","thread":{"id":"large","messages":rows,"activities":[]},"queue":[],"native_id":"preserved-session","resumed":true})
}
#[test]
fn replaying_a_history_page_does_not_write_unchanged_rows_or_advance_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let chat = state(vec![json!({"id":"one","turnId":"turn","text":"retained"})]);
    db.save_chat(&chat, None).unwrap();
    let reader = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    let sequence = || {
        reader
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='chat_rows'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    let before = sequence();
    for _ in 0..20 {
        db.save_chat(&chat, None).unwrap();
    }
    assert_eq!(
        sequence(),
        before,
        "unchanged rows must not allocate sequence numbers"
    );
    let mut changed = chat.clone();
    changed["thread"]["messages"][0]["text"] = json!("updated");
    db.save_chat(&changed, None).unwrap();
    let page = db.history_page("large", None, None).unwrap();
    assert_eq!(page["messages"][0]["sequence"], before);
    assert_eq!(page["messages"][0]["text"], "updated");
    drop(db);
    let mut reopened = Store::open(dir.path()).unwrap();
    reopened.init_chats().unwrap();
    assert_eq!(reopened.history_page("large", None, None).unwrap(), page);
}
#[test]
fn claude_connection_confirmation_survives_paging_without_loading_old_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut chat = state(vec![]);
    chat["agent"] = json!("claude");
    chat["thread"]["activities"] = json!([{"id":"claude-native-sync","kind":"claude.desktop-sync.updated","payload":{"status":"connected","existingSession":true,"readOnly":false}}]);
    db.save_chat(&chat, None).unwrap();
    chat["thread"]["activities"] = json!([]);
    for i in 0..70 {
        chat["thread"]["messages"] =
            json!([{"id":format!("message-{i}"),"text":"A saved message"}]);
        db.save_chat(&chat, None).unwrap();
    }
    assert!(db.load_chat("large").unwrap()["thread"]["activities"]
        .as_array()
        .unwrap()
        .is_empty());
    let display = db.load_chat_display("large", &[]).unwrap();
    assert_eq!(
        display["thread"]["activities"][0]["payload"]["readOnly"],
        false
    );
    assert_eq!(display["thread"]["messages"].as_array().unwrap().len(), 24);
    chat["thread"]["activities"] = json!([{"id":"claude-native-sync","kind":"claude.desktop-sync.updated","payload":{"status":"disconnected","existingSession":true,"readOnly":true}}]);
    db.save_chat(&chat, None).unwrap();
    assert_eq!(
        db.load_chat_display("large", &[]).unwrap()["thread"]["activities"][0]["payload"]
            ["readOnly"],
        true
    );
}

#[test]
fn metadata_loads_and_turn_cleanup_do_not_depend_on_the_recent_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut first = state(vec![
        json!({"id":"old-message","turnId":"turn","streaming":true,"text":"old response"}),
    ]);
    first["thread"]["activities"] = json!([
        {"id":"pending-question","turnId":"turn","kind":"user-input.requested","payload":{"status":"pending"}},
        {"id":"pending-steer","turnId":"turn","kind":"steer.updated","payload":{"status":"pending"}},
        {"id":"running-tool","turnId":"turn","kind":"tool.started","payload":{"status":"running"}}
    ]);
    db.save_chat(&first, None).unwrap();
    for i in 0..50 {
        db.save_chat(
            &state(vec![
                json!({"id":format!("new-{i}"),"turnId":"turn","text":"x".repeat(4096)}),
            ]),
            None,
        )
        .unwrap();
    }
    let headers = db.chat_headers().unwrap();
    assert!(serde_json::to_vec(&headers).unwrap().len() < 1024);
    assert!(headers[0]["thread"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let mut stopped = headers[0].clone();
    stopped["thread"]["latestTurn"] = json!({"turnId":"turn","state":"interrupted"});
    db.save_chat(&stopped, None).unwrap();
    assert_eq!(
        db.history_row("large", "messages", "old-message")
            .unwrap()
            .unwrap()["streaming"],
        false
    );
    assert_eq!(
        db.history_row("large", "activities", "pending-question")
            .unwrap()
            .unwrap()["kind"],
        "user-input.resolved"
    );
    assert_eq!(
        db.history_row("large", "activities", "pending-steer")
            .unwrap()
            .unwrap()["payload"]["status"],
        "failed"
    );
    assert_eq!(
        db.history_row("large", "activities", "running-tool")
            .unwrap()
            .unwrap()["payload"]["status"],
        "stopped"
    );
    assert_eq!(
        db.load_chat("large").unwrap()["thread"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        24
    );
}
#[test]
fn actionable_plan_flag_includes_plans_outside_the_live_history_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut s = state(vec![]);
    s["thread"]["proposedPlans"] = json!([
        {"id":"first","completed":true,"implementedAt":null},
        {"id":"second","completed":true,"implementedAt":null}
    ]);
    db.save_chat(&s, None).unwrap();
    for i in 0..50 {
        db.save_chat(
            &state(vec![json!({"id":format!("later-{i}"),"text":"later"})]),
            None,
        )
        .unwrap();
    }
    let mut recent = db.load_chat("large").unwrap();
    assert!(recent["thread"]["proposedPlans"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(recent["thread"]["hasActionableProposedPlan"], true);
    recent["thread"]["proposedPlans"] =
        json!([{ "id":"second","completed":true,"implementedAt":"now"}]);
    recent["thread"]["hasActionableProposedPlan"] = json!(false);
    db.save_chat(&recent, None).unwrap();
    assert_eq!(
        db.load_chat("large").unwrap()["thread"]["hasActionableProposedPlan"],
        true
    );
    recent["thread"]["proposedPlans"] =
        json!([{ "id":"first","completed":true,"implementedAt":"now"}]);
    db.save_chat(&recent, None).unwrap();
    assert_eq!(
        db.load_chat("large").unwrap()["thread"]["hasActionableProposedPlan"],
        false
    );
}
#[test]
fn large_history_pages_are_bounded_stable_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    for batch in 0..50 {
        let rows=(0..100).map(|n| {let i=batch*100+n;json!({"id":format!("m{i}"),"role":"user","text":"x".repeat(4096),"turnId":format!("t{}",i/2)})}).collect();
        db.save_chat(&state(rows), None).unwrap();
    }
    let recent = db.load_chat("large").unwrap();
    assert!(recent.to_string().len() < 180 * 1024);
    assert_eq!(recent["native_id"], "preserved-session");
    let cursor = recent["thread"]["historyPage"]["olderCursor"]
        .as_str()
        .unwrap()
        .to_owned();
    // New arrivals and updates must not move a saved cursor or erase old rows.
    db.save_chat(
        &state(vec![json!({"id":"m-new","role":"user","text":"new"})]),
        Some(("receipt", "digest")),
    )
    .unwrap();
    db.save_chat(
        &state(vec![
            json!({"id":"m0","role":"user","text":"updated first","turnId":"moved-turn"}),
        ]),
        None,
    )
    .unwrap();
    assert_eq!(
        db.command_digest("receipt").unwrap().as_deref(),
        Some("digest")
    );
    drop(db);
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut ids: HashSet<String> = recent["thread"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().into())
        .collect();
    let mut cursor = Some(cursor);
    let mut pages = 0;
    while let Some(c) = cursor {
        let seq = c.rsplit(':').next().unwrap().parse().unwrap();
        let page = db.history_page("large", Some(seq), None).unwrap();
        assert!(page.to_string().len() < 180 * 1024);
        for r in page["messages"].as_array().unwrap() {
            assert!(
                ids.insert(r["id"].as_str().unwrap().into()),
                "duplicate cursor row"
            );
        }
        cursor = page["pageInfo"]["olderCursor"].as_str().map(str::to_owned);
        pages += 1;
    }
    assert_eq!(ids.len(), 5000);
    assert!(pages > 100);
    assert_eq!(
        db.history_row("large", "messages", "m0").unwrap().unwrap()["text"],
        "updated first"
    );
    assert!(db.history_row("other", "messages", "m0").unwrap().is_none());
    assert_eq!(
        db.history_page("large", None, Some("moved-turn")).unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let work = db.history_page("large", None, Some("t42")).unwrap();
    assert_eq!(work["messages"].as_array().unwrap().len(), 2);
}
#[test]
fn previous_blob_schema_migrates_without_losing_history_or_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE chats(id TEXT PRIMARY KEY,agent TEXT NOT NULL,data TEXT NOT NULL); CREATE TABLE chat_commands(id TEXT PRIMARY KEY,digest TEXT NOT NULL);INSERT INTO chat_commands VALUES('accepted','old-digest');").unwrap();
    let old = state(
        (0..200)
            .map(|i| json!({"id":format!("old-{i}"),"text":"legacy"}))
            .collect(),
    );
    db.execute(
        "INSERT INTO chats VALUES('large','codex',?1)",
        [old.to_string()],
    )
    .unwrap();
    drop(db);
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    db.init_chats().unwrap();
    assert!(db
        .history_row("large", "messages", "old-0")
        .unwrap()
        .is_some());
    assert_eq!(
        db.command_digest("accepted").unwrap().as_deref(),
        Some("old-digest")
    );
    assert!(
        db.chats().unwrap()[0]["thread"]["messages"]
            .as_array()
            .unwrap()
            .len()
            <= 24
    );
}

#[test]
fn plans_page_and_update_without_growing_chat_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut data = state(vec![]);
    data["thread"]["proposedPlans"]=json!((0..60).map(|i|json!({"id":format!("plan-{i}"),"turnId":"turn","planMarkdown":"p".repeat(10000),"implementedAt":null})).collect::<Vec<_>>());
    db.save_chat(&data, None).unwrap();
    drop(db);
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let recent = db.load_chat("large").unwrap();
    assert!(recent.to_string().len() < 180 * 1024);
    assert!(recent["thread"]["proposedPlans"].as_array().unwrap().len() < 24);
    let mut count = recent["thread"]["proposedPlans"].as_array().unwrap().len();
    let mut cursor = recent["thread"]["historyPage"]["olderCursor"]
        .as_str()
        .map(str::to_owned);
    while let Some(c) = cursor {
        let seq = c.rsplit(':').next().unwrap().parse().unwrap();
        let page = db.history_page("large", Some(seq), None).unwrap();
        count += page["proposedPlans"].as_array().unwrap().len();
        cursor = page["pageInfo"]["olderCursor"].as_str().map(str::to_owned);
    }
    assert_eq!(count, 60);
    assert!(db
        .history_row("large", "proposedPlans", "plan-0")
        .unwrap()
        .is_some());
}

#[test]
fn completed_duration_survives_spill_new_turn_stale_rows_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(dir.path()).unwrap();
    db.init_chats().unwrap();
    let mut s = state(vec![
        json!({"id":"original","turnId":"old","role":"user","text":"Keep this history"}),
    ]);
    let old = json!({"id":"old-tool","turnId":"old","kind":"tool.completed","payload":{"status":"completed","command":"echo hello"}});
    s["thread"]["activities"] = json!([old]);
    db.save_chat(&s, None).unwrap();
    let original = db
        .history_row("large", "messages", "original")
        .unwrap()
        .unwrap();
    for n in 0..60 {
        db.save_chat(
            &state(vec![
                json!({"id":format!("m{n}"),"turnId":"old","text":"Saved"}),
            ]),
            None,
        )
        .unwrap();
    }
    let mut recent = db.load_chat("large").unwrap();
    assert!(recent["thread"]["activities"]
        .as_array()
        .unwrap()
        .is_empty());
    recent["thread"]["latestTurn"] = json!({"turnId":"old","state":"interrupted","startedAt":"2026-01-01T01:00:00Z","completedAt":"2026-01-01T01:00:57Z"});
    db.save_chat(&recent, None).unwrap();
    let stamped = db
        .history_row("large", "activities", "old-tool")
        .unwrap()
        .unwrap();
    assert_eq!(stamped["payload"]["turnDurationMs"], 57_000);
    recent["thread"]["latestTurn"] = json!({"turnId":"new","state":"running"});
    // Simulate a cached row from before completion being saved by a later turn.
    recent["thread"]["activities"] = json!([old]);
    db.save_chat(&recent, None).unwrap();
    drop(db);
    let db = Store::open(dir.path()).unwrap();
    assert_eq!(
        db.history_row("large", "activities", "old-tool")
            .unwrap()
            .unwrap(),
        stamped
    );
    assert_eq!(
        db.history_row("large", "messages", "original")
            .unwrap()
            .unwrap(),
        original
    );
    let page = db.history_page("large", None, Some("old")).unwrap();
    assert!(page.to_string().len() < 128 * 1024);
}
