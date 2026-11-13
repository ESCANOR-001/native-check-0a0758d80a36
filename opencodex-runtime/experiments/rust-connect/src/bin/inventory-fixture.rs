//! Strict fixture: discovery must never navigate, resume or write a native chat.
use serde_json::{json, Value};
use std::io::{BufRead, Write};
fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CODEX_HOME").unwrap());
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = v["method"].as_str().unwrap();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("requests.jsonl"))
            .unwrap()
            .write_all(format!("{v}\n").as_bytes())
            .unwrap();
        let result = match method {
            "initialize" => json!({}),
            "initialized" => continue,
            "thread/list" => {
                let p = &v["params"];
                assert_eq!(p["limit"], 64);
                assert_eq!(p["useStateDbOnly"], true);
                assert_eq!(p["sortKey"], "updated_at");
                assert_eq!(p["modelProviders"], json!([]));
                assert!(p.get("sectionId").is_none());
                assert!(p.get("cwd").is_none());
                let rows: Vec<Value> =
                    serde_json::from_slice(&std::fs::read(root.join("inventory.json")).unwrap())
                        .unwrap();
                let offset = p["cursor"]
                    .as_str()
                    .map(|s| s.parse::<usize>().unwrap())
                    .unwrap_or(0);
                json!({"data":rows.iter().skip(offset).take(64).collect::<Vec<_>>(),"nextCursor":(offset+64<rows.len()).then(||(offset+64).to_string())})
            }
            "model/list" => json!({"data":[]}),
            "thread/read" | "thread/turns/list" | "account/rateLimits/read" => {
                println!(
                    "{}",
                    json!({"id":v["id"],"error":{"code":-32000,"message":"owner unavailable"}})
                );
                std::io::stdout().flush().unwrap();
                continue;
            }
            _ => panic!("Unexpected native method: {method}"),
        };
        println!("{}", json!({"id":v["id"],"result":result}));
        std::io::stdout().flush().unwrap();
    }
}
