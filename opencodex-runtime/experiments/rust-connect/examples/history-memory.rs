//! Compare whole-blob decoding and paged loading of the same disposable history.
use remodex_connect_native::store::Store;
use serde_json::{json, Value};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::Path::new(&args[2]);
    if args[1] == "prepare" {
        let mut store = Store::open(path).unwrap();
        store.init_chats().unwrap();
        for batch in 0..50 {
            let rows:Vec<_>=(0..100).map(|n|json!({"id":format!("m{}",batch*100+n),"role":"user","text":"x".repeat(4096)})).collect();
            store.save_chat(&json!({"agent":"codex","thread":{"id":"bench","messages":rows,"activities":[]}}),None).unwrap();
        }
        let db = rusqlite::Connection::open(path.join("native-connect.sqlite")).unwrap();
        let rows: Vec<Value> = db
            .prepare("SELECT data FROM chat_rows ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect();
        std::fs::write(
            path.join("legacy.json"),
            json!({"thread":{"messages":rows}}).to_string(),
        )
        .unwrap();
    } else if args[1] == "legacy" {
        let raw = std::fs::read(path.join("legacy.json")).unwrap();
        let parsed: Value = serde_json::from_slice(&raw).unwrap();
        println!(
            "messages={}",
            std::hint::black_box(&parsed)["thread"]["messages"]
                .as_array()
                .unwrap()
                .len()
        );
    } else {
        let store = Store::open(path).unwrap();
        let rows = store.chats().unwrap();
        println!(
            "messages={}",
            std::hint::black_box(&rows)[0]["thread"]["messages"]
                .as_array()
                .unwrap()
                .len()
        );
    }
}
