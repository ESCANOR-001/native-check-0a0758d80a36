use remodex_connect_native::desktop_ipc::Endpoint;
use serde_json::json;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        return Err(
            "Expected explicit private profile, endpoint, native identity and operation".into(),
        );
    }
    let endpoint = Endpoint {
        path: args[2].clone().into(),
    };
    let mut client = endpoint.connect(std::path::Path::new(&args[1])).await?;
    let owner = client.owner(&args[3]).await?;
    if args[4] == "owner" {
        println!("{}", json!({"originalOwner":true}));
    } else if args[4] == "start" {
        println!("{}",client.start(&owner,json!({"input":[{"type":"text","text":"Reply exactly DESKTOP_NATIVE_IPC_V128_OK. Do not use tools."}]}),&uuid::Uuid::new_v4().to_string()).await?);
    } else {
        return Err("Unsupported isolated operation".into());
    }
    Ok(())
}
