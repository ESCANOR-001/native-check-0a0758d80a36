//! Local test child. No agent/network/account access.
use std::io::{self, BufRead, Write};
fn main() {
    let behavior = std::env::var("REMODEX_FIXTURE_BEHAVIOR").unwrap_or_default();
    if behavior.starts_with("claude-host") {
        claude_host(&behavior);
        return;
    }
    if behavior == "wait" {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if behavior == "tree-exit" || behavior == "tree-hold" {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        // Intentionally orphan this disposable child to exercise supervisor
        // cleanup after a CLI leader exits. The test also kills it on failure.
        #[allow(clippy::zombie_processes)]
        let child = command
            .env("REMODEX_FIXTURE_BEHAVIOR", "wait")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        println!("{}", serde_json::json!({"childPid":child.id()}));
        io::stdout().flush().unwrap();
        if behavior == "tree-hold" {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        return;
    }
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.contains("fixtureExit") {
            break;
        }
        println!("{line}");
        let _ = io::stdout().flush();
    }
}

fn claude_host(behavior: &str) {
    use serde_json::{json, Value};
    let native = std::env::var("REMODEX_FIXTURE_NATIVE").unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("host-commands.jsonl")
        .unwrap();
    let out = |value: Value| {
        println!("{value}");
        io::stdout().flush().unwrap();
    };
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let v: Value = serde_json::from_str(&line).unwrap();
        log.write_all(format!("{v}\n").as_bytes()).unwrap();
        log.flush().unwrap();
        if v["request"]["subtype"] == "initialize" {
            out(
                json!({"type":"control_response","response":{"request_id":v["request_id"],"subtype":"success","response":{"models":[{"value":"fixture"}]}}}),
            );
        } else if v["request"]["subtype"] == "remote_control" && v["request"]["enabled"] == true {
            assert_eq!(v["request"]["keep_session_on_exit"], true);
            if behavior == "claude-host-hang" {
                continue;
            }
            if behavior == "claude-host-overflow" {
                for _ in 0..65 {
                    out(json!({"type":"system","subtype":"status"}));
                }
                continue;
            }
            if behavior == "claude-host-auth" {
                out(
                    json!({"type":"control_response","response":{"request_id":v["request_id"],"subtype":"error","error":"SECRET_WORK_TOKEN at https://private.invalid/signin"}}),
                );
                continue;
            }
            let bridge = if behavior == "claude-host-wrong-link" {
                "cse_wrong_fixture"
            } else {
                v["request"]["reattach_session_id"]
                    .as_str()
                    .unwrap_or("cse_fixture_link")
            };
            out(
                json!({"type":"system","subtype":"init","session_id":if behavior == "claude-host-foreign" {"00000000-0000-4000-8000-000000000009"} else {&native}}),
            );
            out(
                json!({"type":"control_response","response":{"request_id":v["request_id"],"subtype":"success","response":{"bridge_session_id":bridge}}}),
            );
        } else if v["request"]["subtype"] == "remote_control" {
            out(
                json!({"type":"control_response","response":{"request_id":v["request_id"],"subtype":"success","response":{}}}),
            );
        } else if v["type"] == "user" {
            assert_eq!(v["session_id"], native);
            out(
                json!({"type":"user","uuid":v["uuid"],"session_id":native,"isReplay":true,"parent_tool_use_id":null,"message":v["message"]}),
            );
            if v["message"]["content"] == "exit" {
                break;
            }
            if v["message"]["content"] == "hold" {
                continue;
            }
            out(
                json!({"type":"assistant","session_id":native,"user_message_uuid":v["uuid"],"user_message_uuids":[v["uuid"]],"message":{"id":"fixture-assistant","content":[{"type":"text","text":"HOST_REPLY"}]}}),
            );
            out(
                json!({"type":"result","session_id":native,"user_message_uuids":[v["uuid"]],"is_error":false}),
            );
        } else if v["request"]["subtype"] == "fixture_external" {
            let id = "00000000-0000-4000-8000-000000000010";
            out(
                json!({"type":"user","uuid":id,"session_id":native,"parent_tool_use_id":null,"message":{"role":"user","content":"DESKTOP_INPUT"}}),
            );
            out(
                json!({"type":"result","session_id":native,"user_message_uuids":[id],"is_error":false}),
            );
        }
    }
}
