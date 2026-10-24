use remodex_connect_native::{chat::AgentConfig, codex_history_reader};
use serde_json::{json, Value};
use std::path::Path;
fn fixture(root: &Path, mode: &str) -> (AgentConfig, String) {
    let profile = root.join("profile");
    std::fs::create_dir(&profile).unwrap();
    std::fs::create_dir(profile.join("sessions")).unwrap();
    let native = uuid::Uuid::new_v4().to_string();
    let transcript = profile.join("sessions/rollout.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":native}})
        ),
    )
    .unwrap();
    std::fs::write(
        profile.join("fixture.json"),
        json!({"native":native,"path":transcript,"mode":mode}).to_string(),
    )
    .unwrap();
    let config=serde_json::from_value(json!({"executable":env!("CARGO_BIN_EXE_history-fixture"),"profile":profile,"model":"fixture"})).unwrap();
    (config, native)
}
#[tokio::test]
async fn capture_pages_without_ever_resuming_or_claiming_the_chat() {
    let root = tempfile::tempdir().unwrap();
    let (config, native) = fixture(root.path(), "complete");
    let output = root.path().join("saved.jsonl");
    let report = codex_history_reader::capture(&config, root.path(), &native, &output)
        .await
        .unwrap();
    assert_eq!(report["turns"], 2);
    assert_eq!(report["items"], 4);
    assert_eq!(report["historyOnly"], true);
    assert_eq!(report["ownerStateVerified"], false);
    let methods = std::fs::read_to_string(config.profile.join("methods.log")).unwrap();
    assert!(methods.lines().all(|m| matches!(
        m,
        "initialize" | "initialized" | "thread/read" | "thread/turns/list" | "thread/items/list"
    )));
    assert_eq!(methods.lines().filter(|m| *m == "thread/read").count(), 2);
    let rows: Vec<Value> = std::fs::read_to_string(&output)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.last().unwrap(), &report);
    assert!(
        codex_history_reader::capture(&config, root.path(), &native, &output)
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
#[tokio::test]
async fn changed_source_and_cyclic_pages_never_publish_completion() {
    for mode in ["changed", "cycle"] {
        let root = tempfile::tempdir().unwrap();
        let (config, native) = fixture(root.path(), mode);
        let output = root.path().join("saved.jsonl");
        assert!(
            codex_history_reader::capture(&config, root.path(), &native, &output)
                .await
                .is_err()
        );
        for line in std::fs::read_to_string(&output).unwrap().lines() {
            assert_ne!(
                serde_json::from_str::<Value>(line).unwrap()["type"],
                "complete"
            );
        }
    }
}
#[tokio::test]
async fn reader_cannot_capture_into_the_original_profile_or_follow_foreign_history() {
    let root = tempfile::tempdir().unwrap();
    let (config, native) = fixture(root.path(), "complete");
    assert!(codex_history_reader::capture(
        &config,
        root.path(),
        &native,
        &config.profile.join("saved.jsonl")
    )
    .await
    .is_err());
    let settings = config.profile.join("fixture.json");
    let mut setup: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    setup["path"] = json!(root.path().join("foreign.jsonl"));
    std::fs::write(&settings, setup.to_string()).unwrap();
    assert!(codex_history_reader::capture(
        &config,
        root.path(),
        &native,
        &root.path().join("saved.jsonl")
    )
    .await
    .is_err());
}
