//! Linux JSON-lines round trips. Same fixture/payload as benchmark.mjs.
//! This is a transport subset, not an end-to-end bridge RAM comparison.
use remodex_connect_native::agent::{Agent, Connection, Event, Launch};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Instant};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || !cfg!(target_os = "linux") {
        return Err("benchmark ABSOLUTE_FIXTURE SIZE_MIB (Linux only)".into());
    }
    let size: usize = args[2].parse()?;
    if size == 0 || size > 256 {
        return Err("Size outside benchmark bounds".into());
    }
    let mut connection = Connection::start(Launch {
        agent: Agent::Codex,
        executable: PathBuf::from(&args[1]),
        cwd: std::env::current_dir()?,
        env: HashMap::new(),
        extra_args: vec![],
    })?;
    let payload = "x".repeat(32 * 1024);
    let started = Instant::now();
    for index in 0..size * 32 {
        let value = json!({"index":index,"payload":payload});
        connection.send(&value).await?;
        match connection.events.recv().await {
            Some(Event::Message(actual)) if actual == value => {}
            _ => return Err("Lost or changed benchmark frame".into()),
        }
    }
    let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let peak: f64 = status
        .lines()
        .find(|s| s.starts_with("VmHWM:"))
        .and_then(|s| s.split_whitespace().nth(1))
        .ok_or("Missing high water mark")?
        .parse()?;
    connection.close().await;
    println!(
        "{}",
        json!({"mode":"native-rust","sizeMiB":size,"milliseconds":milliseconds,"parentPeakMiB":peak/1024.0,"fixtureExcluded":true})
    );
    Ok(())
}
