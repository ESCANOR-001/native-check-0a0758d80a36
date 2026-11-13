use remodex_connect_native::store::Store;
use serde_json::json;
fn main() {
    let args: Vec<_> = std::env::args().collect();
    // Operate only on an explicit scratch copy, never the running store.
    assert!(args[1].contains("/.tmp/"));
    let store = Store::open(std::path::Path::new(&args[1])).unwrap();
    let at = std::time::Instant::now();
    let display = store.load_chat_display(&args[2], &[]).unwrap();
    let thread = &display["thread"];
    let turns: Vec<_> = thread["activities"].as_array().unwrap().iter()
        .filter(|r| r["kind"] == "work.deferred")
        .map(|m| json!({"turn":m["turnId"],"marker":m["payload"],
            "activities":thread["activities"].as_array().unwrap().iter().filter(|r|r["turnId"]==m["turnId"] && r["kind"]!="work.deferred").count(),
            "messages":thread["messages"].as_array().unwrap().iter().filter(|r|r["turnId"]==m["turnId"]).count()})).collect();
    println!(
        "{}",
        json!({"milliseconds":at.elapsed().as_millis(),"bytes":display.to_string().len(),"turns":turns})
    );
}
