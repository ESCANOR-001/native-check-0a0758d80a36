//! Read-only configuration compatibility check, before claiming a state directory
//! or invalidating invitations. It never authenticates or launches an agent.
use crate::chat::Config;
use std::path::Path;

pub fn load(path: &Path) -> Result<Config, String> {
    let bytes = crate::profile_snapshot::bytes(path, 256 * 1024)
        .map_err(|_| "Cannot read a bounded, regular native configuration file")?;
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| {
        "Unsupported or invalid native configuration; no connection state was opened"
    })?;
    config.validate()?;
    Ok(config)
}
