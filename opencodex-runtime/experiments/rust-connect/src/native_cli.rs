//! User-facing commands without a Node/Bun launcher. State discovery is read-only;
//! unreadable or conflicting records never become a new empty installation.
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallationKind {
    New,
    Native,
    Legacy,
}

fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect the saved installation; no data was changed".into()),
    }
}

pub fn installation_kind(installation: &Path) -> Result<InstallationKind, String> {
    let source = installation
        .parent()
        .ok_or("Invalid installation directory")?;
    let native = present(&installation.join("native-install.json"))?;
    let legacy = [
        installation.join("npm-upgrade.sqlite"),
        installation.join("npm-source.json"),
        source.join("android-remote.json"),
        source.join("native-upgrade-source-hold.json"),
    ]
    .iter()
    .map(|p| present(p))
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .any(|v| v);
    match (native, legacy) {
        (true, true) => Err("Conflicting installation records. Existing data was retained; use installation recovery".into()),
        (true, false) => Ok(InstallationKind::Native),
        (false, true) => Ok(InstallationKind::Legacy),
        (false, false) => Ok(InstallationKind::New),
    }
}

fn absolute(path: &str, cwd: &Path, home: &Path) -> PathBuf {
    let path = if path == "~" {
        home.to_owned()
    } else if path.starts_with("~/") || path.starts_with("~\\") {
        home.join(&path[2..])
    } else {
        PathBuf::from(path)
    };
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

pub fn installation_directory(
    override_root: Option<&str>,
    home: &Path,
    cwd: &Path,
) -> Result<PathBuf, String> {
    if let Some(root) = override_root.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(absolute(root, cwd, home).join("native-installation"));
    }
    let mut found = Vec::new();
    for name in [".remodex-connect", ".remodex", ".opencodex"] {
        let root = home.join(name);
        let mut known = false;
        for record in [
            "android-remote.json",
            "native-upgrade-source-hold.json",
            "native-installation/native-install.json",
            "native-installation/npm-source.json",
            "native-installation/npm-upgrade.sqlite",
        ] {
            known |= present(&root.join(record))?;
        }
        if known {
            found.push(root);
        }
    }
    if found.len() > 1 {
        return Err("Multiple Remodex installations were found. Set OPENCODEX_HOME to the installation you want; none was changed".into());
    }
    Ok(found
        .pop()
        .unwrap_or_else(|| home.join(".remodex-connect"))
        .join("native-installation"))
}

fn number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && value.parse::<u64>().is_ok()
}
fn port(value: &str) -> bool {
    number(value)
        && !value.starts_with('0')
        && value.parse::<u16>().is_ok_and(|p| (1..=65530).contains(&p))
}
fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Path cannot be represented by this command".into())
}

pub fn translate(
    args: &[String],
    installation: &Path,
    gui: &Path,
    cwd: &Path,
    home: &Path,
) -> Result<Vec<String>, String> {
    let a: Vec<_> = args.iter().map(String::as_str).collect();
    let root = path_text(installation)?;
    let gui = path_text(gui)?;
    let path = |p: &str| path_text(&absolute(p, cwd, home));
    let strings = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    match a.as_slice() {
        [] | ["help" | "--help" | "-h"] => Ok(strings(&["--help"])),
        ["version" | "--version" | "-v"] => Ok(strings(&["--version"])),
        ["features"] => Ok(strings(&["--features"])),
        ["tray"] => Ok(strings(&["--native-tray", "start", &root, &gui])),
        ["tray", "show"] if cfg!(feature = "tray-test-controls") => {
            Ok(strings(&["--native-tray", "show", &root, &gui]))
        }
        ["tray", op @ ("status" | "quit")] => Ok(strings(&["--native-tray", op, &root, &gui])),
        [op @ ("__tray-host" | "__tray-start" | "__tray-restart" | "terminal" | "preview"), ..] => {
            Ok(strings(&["--retired-feature", op]))
        }
        [op @ ("status" | "stop" | "restart")] => Ok(strings(&["--npm-upgrade", op, &root])),
        ["migrate"] => Ok(strings(&["--native-legacy-prepare", &root, &gui])),
        ["migrate", "--source", source] => Ok(strings(&[
            "--npm-upgrade",
            "prepare-explicit-source",
            &root,
            &path(source)?,
        ])),
        ["migrate", "--plan", plan] => {
            Ok(strings(&["--npm-upgrade", "prepare", &root, &path(plan)?]))
        }
        ["migrate", "cancel"] => Ok(strings(&["--npm-upgrade", "cancel", &root])),
        [op @ ("start" | "onboard"), rest @ ..] => {
            let kind = installation_kind(installation)?;
            if *op == "onboard" && kind == InstallationKind::Legacy {
                return Err("This PC already has a legacy installation. Run rmx start to upgrade it while retaining its chats and pairing".into());
            }
            let mut selected_port = None;
            let mut selected_config = None;
            let mut pairs = rest.chunks_exact(2);
            for pair in &mut pairs {
                match pair {
                    ["--port", p] if selected_port.is_none() && port(p) => selected_port = Some(*p),
                    ["--config", file] if *op == "onboard" && selected_config.is_none() => selected_config = Some(path(file)?),
                    _ => return Err("Usage: rmx start [--port PORT] | onboard [--port PORT] [--config FILE]; dashboard ports are 1–65530".into()),
                }
            }
            if !pairs.remainder().is_empty() {
                return Err("Installation option needs a value".into());
            }
            let mut out = if kind == InstallationKind::Legacy {
                strings(&["--native-legacy-start", &root, &gui])
            } else {
                strings(&["--native-install", op, &root, &gui])
            };
            if let Some(p) = selected_port {
                out.extend(strings(&["--port", p]));
            }
            if let Some(file) = selected_config {
                out.extend(strings(&["--config", &file]));
            }
            Ok(out)
        }
        ["config", "show"] => Ok(strings(&["--native-config", "show", &root])),
        ["config", "check", file] => {
            Ok(strings(&["--native-config", "check", &root, &path(file)?]))
        }
        ["config", op @ ("apply" | "rollback"), value, "--revision", revision]
            if number(revision) && (*op != "rollback" || number(value)) =>
        {
            let value = if *op == "apply" {
                path(value)?
            } else {
                (*value).into()
            };
            Ok(strings(&[
                "--native-config",
                op,
                &root,
                &value,
                "--revision",
                revision,
            ]))
        }
        ["remote", op @ ("local" | "quick"), "--revision", revision] if number(revision) => {
            Ok(strings(&[
                "--native-remote",
                op,
                &root,
                "--revision",
                revision,
            ]))
        }
        ["remote", "named", host, "--revision", revision] if number(revision) => Ok(strings(&[
            "--native-remote",
            "named",
            &root,
            host,
            "--revision",
            revision,
        ])),
        ["remote", "provision", account, zone, host, "--revision", revision]
            if number(revision) =>
        {
            Ok(strings(&[
                "--native-provision",
                &root,
                account,
                zone,
                host,
                "--revision",
                revision,
            ]))
        }
        ["service", "prepare", dir] => Ok(strings(&[
            "--native-service",
            "prepare",
            &path(dir)?,
            &root,
            &gui,
        ])),
        ["service", op @ ("install" | "start" | "status" | "uninstall"), dir] => {
            Ok(strings(&["--native-service", op, &path(dir)?]))
        }
        ["service", "replace", new, old] => Ok(strings(&[
            "--native-service",
            "replace",
            &path(new)?,
            &path(old)?,
        ])),
        ["package", destination, "--gui", directory, "--version", version] => Ok(strings(&[
            "--native-package",
            &path(destination)?,
            &path(directory)?,
            version,
        ])),
        ["package", destination, "--gui", directory, "--version", version, "--publishable-beta"] => {
            Ok(strings(&[
                "--native-package",
                &path(destination)?,
                &path(directory)?,
                version,
                "--publishable-beta",
            ]))
        }
        _ => Err(
            "Unknown command or invalid arguments. Run rmx --help; no old backend was started"
                .into(),
        ),
    }
}

/// Internal commands keep their explicit paths. Public commands use the same
/// persisted state selection as the previous launcher, without spawning it.
pub fn normalize(mut args: Vec<String>) -> Result<Vec<String>, String> {
    let exe = std::env::current_exe().map_err(|_| "Executable path is unavailable")?;
    crate::native_package::verify_if_packaged(&exe)?;
    // Preserve the explicit low-level isolated-server invocation used by
    // supervisors and fixtures; never interpret its absolute state path as a verb.
    if args.get(1).is_some_and(|a| {
        (a.starts_with("--") && !matches!(a.as_str(), "--help" | "--version"))
            || Path::new(a).is_absolute()
    }) {
        return Ok(args);
    }
    let cwd = std::env::current_dir().map_err(|_| "Current directory is unavailable")?;
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or("User home directory is unavailable")?;
    // Help and packaging do not depend on the user's saved profiles. They must
    // remain available when a user needs to resolve conflicting installations.
    let needs_installation = !matches!(
        args.get(1).map(String::as_str),
        None | Some(
            "help" | "--help" | "-h" | "version" | "--version" | "-v" | "features" | "package"
        )
    );
    let installation = if needs_installation {
        installation_directory(std::env::var("OPENCODEX_HOME").ok().as_deref(), &home, &cwd)?
    } else {
        home.join(".remodex-connect/native-installation")
    };
    let parent = exe.parent().ok_or("Executable directory is unavailable")?;
    let gui = std::env::var("REMODEX_GUI_DIR")
        .ok()
        .map(|p| absolute(&p, &cwd, &home))
        .unwrap_or_else(|| {
            if parent.join("gui/dist").is_dir() {
                parent.join("gui/dist")
            } else {
                parent.parent().unwrap_or(parent).join("gui/dist")
            }
        });
    let translated = translate(&args[1..], &installation, &gui, &cwd, &home)?;
    args.truncate(1);
    args.extend(translated);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn discovery_preserves_old_profiles_and_refuses_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        assert_eq!(
            installation_directory(None, home, home).unwrap(),
            home.join(".remodex-connect/native-installation")
        );
        fs::create_dir(home.join(".remodex")).unwrap();
        fs::write(
            home.join(".remodex/android-remote.json"),
            b"broken record still counts",
        )
        .unwrap();
        assert_eq!(
            installation_directory(None, home, home).unwrap(),
            home.join(".remodex/native-installation")
        );
        fs::create_dir(home.join(".opencodex")).unwrap();
        fs::write(home.join(".opencodex/android-remote.json"), b"{}").unwrap();
        assert!(installation_directory(None, home, home).is_err());
        assert_eq!(
            installation_directory(Some("~/.opencodex"), home, home).unwrap(),
            home.join(".opencodex/native-installation")
        );
    }
    #[test]
    fn public_commands_preserve_literal_paths_and_validate_options() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        let install = base.join("user data 测试/native-installation");
        let gui = base.join("gui/dist");
        let run = |a: &[&str]| translate(&args(a), &install, &gui, base, base);
        for op in ["status", "quit"] {
            assert_eq!(
                run(&["tray", op]).unwrap(),
                args(&[
                    "--native-tray",
                    op,
                    install.to_str().unwrap(),
                    gui.to_str().unwrap()
                ])
            );
        }
        assert_eq!(run(&["tray"]).unwrap()[1], "start");
        assert_eq!(
            run(&["tray", "show"]).is_ok(),
            cfg!(feature = "tray-test-controls")
        );
        assert!(run(&["tray", "stop"]).is_err());
        assert!(run(&["tray", "show", "extra"]).is_err());
        assert_eq!(
            run(&[
                "onboard",
                "--config",
                "settings & other.json",
                "--port",
                "22100"
            ])
            .unwrap(),
            args(&[
                "--native-install",
                "onboard",
                install.to_str().unwrap(),
                gui.to_str().unwrap(),
                "--port",
                "22100",
                "--config",
                base.join("settings & other.json").to_str().unwrap()
            ])
        );
        for invalid in [
            &["start", "--port", "65531"][..],
            &["start", "--port", "1", "--port", "2"],
            &["start", "--config", "x"],
            &["stop", "extra"],
            &["config", "rollback", "-1", "--revision", "0"],
            &["update"],
        ] {
            assert!(run(invalid).is_err());
        }
        fs::create_dir_all(&install).unwrap();
        fs::write(install.parent().unwrap().join("android-remote.json"), b"{}").unwrap();
        assert_eq!(run(&["start"]).unwrap()[0], "--native-legacy-start");
        assert_eq!(
            run(&["migrate"]).unwrap(),
            args(&[
                "--native-legacy-prepare",
                install.to_str().unwrap(),
                gui.to_str().unwrap()
            ])
        );
        assert_eq!(
            run(&["migrate", "--source", "old owner/source.json"])
                .unwrap()
                .last()
                .unwrap(),
            base.join("old owner").join("source.json").to_str().unwrap()
        );
        assert_eq!(
            run(&["migrate", "--plan", "plan.json"]).unwrap()[1],
            "prepare"
        );
        assert!(run(&["onboard"]).is_err());
        fs::write(install.join("native-install.json"), b"{}").unwrap();
        assert!(run(&["start"]).is_err());
    }
}
