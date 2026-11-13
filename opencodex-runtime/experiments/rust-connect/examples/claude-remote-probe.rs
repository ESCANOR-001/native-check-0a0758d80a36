//! Read-only protocol qualification against an explicitly supplied test profile.
//! Initialize asks for capabilities; no prompt, permission, setting or Stop is sent.
use remodex_connect_native::{claude_host::Identity, claude_remote::Remote};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 || !args[0].contains("/.tmp/") {
        return Err("Usage: probe ISOLATED_PROFILE NATIVE_UUID SHARED_ID".into());
    }
    let identity = Identity {
        native_session: args[1].clone(),
        bridge_session: args[2].clone(),
    };
    let mut remote = Remote::open(&PathBuf::from(&args[0]), identity, 0).await?;
    println!(
        "{}",
        json!({"connected":true,"workerStatus":remote.metadata["worker_status"],"workerEpoch":remote.metadata["worker_epoch"],"metadataKeys":remote.metadata.as_object().map(|o|o.keys().collect::<Vec<_>>())})
    );
    let request = format!("native-observer-probe-{}", uuid::Uuid::new_v4());
    remote.send(json!({"type":"control_request","request_id":request,"request":{"subtype":"initialize"}})).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut initialized = false;
    let mut system = false;
    for _ in 0..1000 {
        let envelope = tokio::time::timeout_at(deadline, remote.next())
            .await
            .map_err(|_| "Observer handshake timed out")??;
        let Some(value) = envelope.payload else {
            continue;
        };
        println!(
            "{}",
            json!({"sequence":envelope.sequence,"source":envelope.source,"type":value["type"],"subtype":value["subtype"],"hasNativeSession":value["session_id"]==args[1],"keys":value.as_object().map(|o|o.keys().collect::<Vec<_>>())})
        );
        if value["type"] == "control_response" && value["response"]["request_id"] == request {
            if value["response"]["subtype"] != "success" {
                return Err("Observer initialize rejected".into());
            }
            initialized = true;
            println!(
                "{}",
                json!({"inboundReceipts":value["response"]["response"]["inbound_read_receipts"],"sessionState":value["response"]["response"]["session_state"],"workerPid":value["response"]["response"]["pid"]})
            );
            println!(
                "{}",
                json!({"initializeKeys":value["response"]["response"].as_object().map(|o|o.keys().collect::<Vec<_>>())})
            );
        }
        if initialized && value["type"] == "system" && value["subtype"] == "init" {
            system = true;
        }
        if initialized && system {
            println!(
                "{}",
                json!({"initialized":true,"freshSharedSessionInitSeen":true,"promptSent":false})
            );
            return Ok(());
        }
    }
    Err("Observer handshake exceeded event budget".into())
}
