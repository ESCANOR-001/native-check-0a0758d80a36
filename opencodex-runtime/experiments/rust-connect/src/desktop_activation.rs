//! Activate an existing Desktop route only after conclusive absent-owner
//! discovery. Never start a CLI, send a prompt, or use this for background reads.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Activation {
    System,
    /// Explicit local launcher for installations using a custom Desktop profile.
    /// The URL is appended as one argument; nothing is interpreted by a shell.
    Command {
        executable: PathBuf,
        arguments: Vec<String>,
    },
}
const ERROR: &str = "The original Desktop chat could not be loaded; its history is intact and no replacement CLI was started";
impl Activation {
    pub fn validate(&self, profile: &Path) -> Result<(), String> {
        match self {
            Self::System => {
                let root = profile.canonicalize().map_err(|_| ERROR)?;
                let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .ok_or(ERROR)?;
                if PathBuf::from(home)
                    .join(".codex")
                    .canonicalize()
                    .map_err(|_| ERROR)?
                    != root
                {
                    return Err(
                        "A custom Desktop profile needs its own explicit activation launcher"
                            .into(),
                    );
                }
            }
            Self::Command {
                executable,
                arguments,
            } => {
                if !executable.is_absolute()
                    || !executable.is_file()
                    || arguments.len() > 16
                    || arguments.iter().any(|s| s.len() > 4096 || s.contains('\0'))
                {
                    return Err(ERROR.into());
                }
            }
        }
        Ok(())
    }
    pub async fn open(&self, profile: &Path, thread: &str) -> Result<(), String> {
        self.validate(profile)?;
        let id = uuid::Uuid::parse_str(thread).map_err(|_| ERROR)?;
        let url = format!(
            "codex://threads/{id}?opencodex-reactivate={}",
            uuid::Uuid::new_v4()
        );
        let (executable, mut args) = match self {
            Self::Command {
                executable,
                arguments,
            } => (executable.clone(), arguments.clone()),
            Self::System => system_command()?,
        };
        args.push(url);
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().map_err(|_| ERROR)?;
        let status = tokio::time::timeout(Duration::from_secs(8), child.wait())
            .await
            .map_err(|_| ERROR)?
            .map_err(|_| ERROR)?;
        if !status.success() {
            return Err(ERROR.into());
        }
        Ok(())
    }
}
fn system_command() -> Result<(PathBuf, Vec<String>), String> {
    #[cfg(target_os = "linux")]
    {
        Ok((PathBuf::from("/usr/bin/xdg-open"), vec![]))
    }
    #[cfg(target_os = "macos")]
    {
        Ok((
            PathBuf::from("/usr/bin/open"),
            vec!["-b".into(), "com.openai.codex".into()],
        ))
    }
    #[cfg(windows)]
    {
        let root = PathBuf::from(std::env::var_os("SystemRoot").ok_or(ERROR)?);
        if !root.is_absolute() {
            return Err(ERROR.into());
        }
        Ok((
            root.join("System32/rundll32.exe"),
            vec!["url.dll,FileProtocolHandler".into()],
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(ERROR.into())
    }
}
