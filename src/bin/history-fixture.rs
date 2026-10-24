//! Strict saved-history reader fixture. Any ownership mutation fails the test.
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CODEX_HOME").unwrap());
    let setup: Value =
        serde_json::from_slice(&std::fs::read(root.join("fixture.json")).unwrap()).unwrap();
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = request["method"].as_str().unwrap();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("methods.log"))
            .unwrap();
        writeln!(log, "{method}").unwrap();
        if method == "initialized" {
            continue;
        }
        let live: Value = std::fs::read(root.join("desktop-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({}),
            "thread/read" => {
                assert_eq!(request["params"]["includeTurns"], false);
                json!({"thread":{"id":setup["native"],"path":setup["path"],"turns":[],"status":{"type":"notLoaded"}}})
            }
            "thread/turns/list" => {
                assert_eq!(request["params"]["limit"], 1);
                if setup["mode"] == "desktop" {
                    println!(
                        "{}",
                        json!({"id":request["id"],"result":{"data":[{"id":live["turn"],"status":if live["status"]=="inProgress" {json!("interrupted")}else{live["status"].clone()},"items":[]}],"nextCursor":null}})
                    );
                    io::stdout().flush().unwrap();
                    continue;
                }
                let second = request["params"]["cursor"] == "page-2";
                json!({"data":[{"id":if second {"turn-2"}else{"turn-1"},"status":"completed","items":[]}],"nextCursor":if second {Value::Null}else{json!("page-2")}})
            }
            "thread/items/list" => {
                assert_eq!(request["params"]["limit"], 8);
                if setup["mode"] == "desktop" {
                    println!(
                        "{}",
                        json!({"id":request["id"],"result":{"data":[{"turnId":live["turn"],"completedAtMs":if live["status"]=="completed"{json!(10)}else{Value::Null},"item":{"id":"reply","type":"agentMessage","text":"OWNER_REPLY"}}],"nextCursor":null}})
                    );
                    io::stdout().flush().unwrap();
                    continue;
                }
                if setup["mode"] == "changed" {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(setup["path"].as_str().unwrap())
                        .unwrap()
                        .write_all(b"{}\n")
                        .unwrap();
                }
                let second = request["params"]["cursor"] == "items-2";
                json!({"data":[{"item":{"id":if second {"message-2"}else{"message-1"},"type":"agentMessage","text":"Synthetic saved history"}}],"nextCursor":if second && setup["mode"]!="cycle" {Value::Null}else{json!("items-2")}})
            }
            _ => panic!("Ownership or unsupported operation sent to saved-history reader"),
        };
        println!("{}", json!({"id":request["id"],"result":result}));
        io::stdout().flush().unwrap();
    }
}
