use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::sync::{Arc, Mutex};

const THREAD: &str = "12345678-1234-4234-8234-123456789abc";
type Log = Arc<Mutex<Vec<Value>>>;
async fn fixture(mut handler: impl FnMut(&Value) -> Value + Send + 'static) -> (Client, Log) {
    let (a, b) = tokio::io::duplex(65536);
    let log = Arc::new(Mutex::new(vec![]));
    let received = log.clone();
    tokio::spawn(async move {
        let mut stream: Box<dyn Stream> = Box::new(b);
        while let Ok(request) = read_frame(&mut stream).await {
            received.lock().unwrap().push(request.clone());
            let result = match request["method"].as_str().unwrap() {
                "initialize" => json!({"clientId":"reader"}),
                "thread-owner-discovery" => json!({"handledByClientId":"desktop"}),
                _ => handler(&request),
            };
            let response = json!({"type":"response","requestId":request["requestId"],"method":request["method"],
                "resultType":"success","handledByClientId":"desktop","result":result});
            if write_frame(&mut stream, &response).await.is_err() {
                break;
            }
        }
    });
    (
        Client::initialize(Box::new(a), Duration::from_millis(500))
            .await
            .unwrap(),
        log,
    )
}
fn page(request: &Value) -> Value {
    let older = request["params"]["direction"] == "older";
    json!({"revision":1,"conversationState":{"id":THREAD,"hostId":"local","requests":[],
        "turns":[{"turnId":if older {"first"} else {"second"},"status":"completed",
            "items":[{"type":"agentMessage","id":if older {"i1"} else {"i2"},"text":"original"}]}],
        "historyPage":{"kind":"desktop-history-page","sourceRevision":"revision-one","order":"oldest-to-newest",
            "hasOlder":!older,"hasNewer":older,"olderPageToken":if older {Value::Null} else {json!("older-token")},
            "newerPageToken":null,"itemsOmitted":0,"inlineFieldsMovedToHandles":0,"completeOrExplicitlyPartial":true}}})
}
fn rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn read_only(log: &Log) {
    assert!(log.lock().unwrap().iter().all(|r| matches!(
        r["method"].as_str().unwrap(),
        "initialize"
            | "thread-owner-discovery"
            | "thread-follower-load-history-page"
            | "thread-follower-read-content-chunk"
    )));
}
#[tokio::test]
async fn complete_export_reads_every_page_and_rechecks_revision_without_touching_owner() {
    let (mut client, log) = fixture(page).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.jsonl");
    let receipt = crate::desktop_snapshot::capture(&mut client, THREAD, &path)
        .await
        .unwrap();
    assert_eq!(receipt["pages"], 2);
    assert_eq!(receipt["turns"], 2);
    let saved = rows(&path);
    assert_eq!(saved.len(), 3);
    assert_eq!(saved[0]["state"]["turns"][0]["turnId"], "second");
    assert_eq!(saved[1]["state"]["turns"][0]["turnId"], "first");
    assert_eq!(saved[2], receipt);
    use sha2::{Digest, Sha256};
    let raw = std::fs::read(&path).unwrap();
    let prefix = &raw[..receipt["bytes"].as_u64().unwrap() as usize];
    assert_eq!(receipt["sha256"], format!("{:x}", Sha256::digest(prefix)));
    assert!(crate::desktop_snapshot::capture(&mut client, THREAD, &path)
        .await
        .is_err());
    assert_eq!(raw, std::fs::read(&path).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    read_only(&log);
}
#[tokio::test]
async fn expanded_content_is_complete_and_preview_is_never_a_substitute() {
    let content = "actual full content, including Unicode 🦀"
        .as_bytes()
        .to_vec();
    let expected = content.clone();
    let (mut client,log)=fixture(move |request| {
        if request["method"]=="thread-follower-read-content-chunk" {
            assert_eq!(request["params"]["sourceRevision"],"revision-one");
            let offset=request["params"]["offset"].as_u64().unwrap() as usize;
            let end=(offset+7).min(content.len()); let chunk=&content[offset..end];
            return json!({"kind":"chunk","handle":"h1","offset":offset,"nextOffset":end,
                "totalByteLength":content.len(),"chunkByteLength":chunk.len(),"mediaType":"text/plain",
                "encoding":"base64","data":STANDARD.encode(chunk),"complete":end==content.len()});
        }
        let mut value=page(request);
        value["conversationState"]["turns"][0]["items"][0]["text"]=json!({"kind":"handle","handle":"h1",
            "preview":"TRUNCATED","byteLength":content.len(),"mediaType":"text/plain","truncatedInline":true});
        value
    }).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history");
    crate::desktop_snapshot::capture(&mut client, THREAD, &path)
        .await
        .unwrap();
    for row in rows(&path).into_iter().take(2) {
        assert_eq!(
            row["state"]["turns"][0]["items"][0]["text"],
            String::from_utf8(expected.clone()).unwrap()
        );
    }
    read_only(&log);
}
#[tokio::test]
async fn incomplete_changed_or_cyclic_pages_never_get_a_completion_receipt() {
    for fault in 0..7 {
        let mut calls = 0;
        let (mut client, log) = fixture(move |r| {
            calls += 1;
            let mut v = page(r);
            let s = &mut v["conversationState"];
            match fault {
                0 => s["historyPage"]["itemsOmitted"] = json!(1),
                1 => {
                    s["turns"][0]["items"][0]["text"] =
                        json!({"kind":"missing","reason":"not-retained"})
                }
                2 if calls == 2 => s["historyPage"]["sourceRevision"] = json!("changed"),
                3 if calls == 3 => s["historyPage"]["sourceRevision"] = json!("changed"),
                4 if calls == 2 => {
                    s["historyPage"]["hasOlder"] = json!(true);
                    s["historyPage"]["olderPageToken"] = json!("older-token");
                }
                5 => s["requests"] = json!([{"id":"approval"}]),
                6 if calls == 3 => s["turns"][0]["status"] = json!("inProgress"),
                _ => {}
            };
            v
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        assert!(
            crate::desktop_snapshot::capture(&mut client, THREAD, &path)
                .await
                .is_err(),
            "fault {fault}"
        );
        assert!(path.exists());
        assert!(rows(&path).iter().all(|r| r["type"] != "complete"));
        read_only(&log);
    }
}
#[tokio::test]
async fn invalid_content_chunks_never_publish_partial_text() {
    for fault in 0..7 {
        let (mut client,log)=fixture(move |r| {
            if r["method"]=="thread-follower-read-content-chunk" {
                let mut v=json!({"kind":"chunk","handle":"h1","offset":0,"nextOffset":4,
                    "totalByteLength":4,"chunkByteLength":4,"mediaType":"text/plain","encoding":"base64",
                    "data":STANDARD.encode(b"full"),"complete":true});
                match fault {
                    0=>v["handle"]=json!("foreign"),1=>v["offset"]=json!(1),2=>v["nextOffset"]=json!(3),
                    3=>v["totalByteLength"]=json!(5),4=>v["data"]=json!("INVALID"),5=>v["complete"]=json!(false),
                    _=>v=json!({"kind":"missing","reason":"source-missing"})
                };return v;
            }
            let mut v=page(r);v["conversationState"]["turns"][0]["items"][0]["text"]=json!({"kind":"handle",
                "handle":"h1","preview":"wrong","byteLength":4,"mediaType":"text/plain","truncatedInline":true});v
        }).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        assert!(crate::desktop_snapshot::capture(&mut client, THREAD, &path)
            .await
            .is_err());
        assert!(rows(&path).is_empty());
        read_only(&log);
    }
}
