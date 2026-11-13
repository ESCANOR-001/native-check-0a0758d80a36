//! Isolated Windows-service test child. Never opens profiles or network ports.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 || args[1] != "--native-service" {
        return Err("fixture arguments".into());
    }
    let root = std::path::Path::new(&args[3]);
    if std::fs::read(root.join("scm-fixture.marker"))? != b"isolated-remodex-service-test-v1" {
        return Err("fixture marker".into());
    }
    match args[2].as_str() {
        "launch" => {
            std::fs::write(root.join("child-started"), b"ready")?;
            let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
            while !root.join("child-stop").exists() {
                if std::time::Instant::now() > until {
                    return Err("fixture timeout".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            std::fs::write(root.join("child-graceful-exit"), b"closed")?;
        }
        "halt" => {
            std::fs::write(root.join("child-stop"), b"stop")?;
        }
        _ => return Err("fixture operation".into()),
    }
    Ok(())
}
