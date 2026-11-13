//! Explicit, bounded repository cloning. A durable intent prevents replay after
//! cancellation/restart; an ambiguous or failed clone keeps any files in place.
use crate::{auth_service::AuthService, chat_projection::s, vcs, workspace};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

fn remote(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.starts_with('-')
    {
        return Err("Enter an HTTPS or SSH Git clone URL".into());
    }
    if value.contains("://") {
        let url = reqwest::Url::parse(value).map_err(|_| "Invalid Git URL")?;
        if !["http", "https", "ssh", "git"].contains(&url.scheme())
            || url.host_str().is_none()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (url.scheme() != "ssh" && !url.username().is_empty())
        {
            return Err("Use a Git URL without embedded credentials or query parameters".into());
        }
    } else {
        let (host, path) = value
            .split_once(':')
            .ok_or("Enter an HTTPS or SSH Git clone URL")?;
        let (user, host) = host
            .split_once('@')
            .ok_or("Enter an HTTPS or SSH Git clone URL")?;
        if user.is_empty()
            || user.starts_with('-')
            || !user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            || host.is_empty()
            || host.starts_with('-')
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
            || path.is_empty()
            || path.starts_with('-')
        {
            return Err("Invalid SSH Git URL".into());
        }
    }
    Ok(value.to_owned())
}
fn destination(base: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 || raw.chars().any(char::is_control) {
        return Err("Select a clone destination".into());
    }
    let target = base.join(raw);
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Invalid clone destination".into());
    }
    let name = target
        .file_name()
        .ok_or("Select a new folder inside the workspace")?;
    if name.to_string_lossy().starts_with('-') {
        return Err("Invalid clone folder name".into());
    }
    let parent = workspace::directory(
        base,
        target
            .parent()
            .and_then(Path::to_str)
            .ok_or("Invalid destination")?,
        true,
    )?;
    let target = parent.join(name);
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("The clone destination is not an ordinary folder".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("The clone destination cannot be a linked folder".into());
            }
        }
    }
    Ok(target)
}
fn empty(path: &Path) -> Result<(), String> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_none() {
                Ok(())
            } else {
                Err("The clone destination already exists and is not empty".into())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("The clone destination already exists and is not empty".into()),
    }
}
async fn verified(target: &Path, url: &str) -> Result<(), String> {
    let top = vcs::git(target, &["rev-parse", "--show-toplevel"]).await?;
    let origin = vcs::git(target, &["config", "--get", "remote.origin.url"]).await?;
    if top.0 != 0 || origin.0 != 0 || String::from_utf8_lossy(&origin.1).trim() != url {
        return Err("The cloned repository could not be verified".into());
    }
    let path = PathBuf::from(String::from_utf8_lossy(&top.1).trim());
    if path.canonicalize().ok() != target.canonicalize().ok() {
        return Err("The cloned repository folder changed".into());
    }
    Ok(())
}
pub(crate) async fn run(db: AuthService, base: &Path, p: Value) -> Result<Value, String> {
    let url = remote(s(&p["remoteUrl"]))?;
    let target = destination(base, s(&p["destinationPath"]))?;
    clone_validated(db, target, url).await
}
async fn clone_validated(db: AuthService, target: PathBuf, url: String) -> Result<Value, String> {
    let identity = vcs::path_argument(&target)?;
    #[cfg(windows)]
    let identity = identity.to_lowercase();
    let key = format!("native_clone_{}", crate::auth::digest(&identity));
    let k = key.clone();
    let previous = db
        .call(move |a| {
            a.store
                .db
                .query_row("SELECT value FROM metadata WHERE key=?1", [k], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
        })
        .await?
        .map_err(|_| "Cannot read clone receipt")?;
    if let Some(value) = previous {
        let saved: Value = serde_json::from_str(&value).map_err(|_| "Invalid clone receipt")?;
        if saved["remoteUrl"] != url {
            return Err("This destination belongs to another clone request".into());
        }
        if saved["state"] != "done" {
            return Err("The earlier clone did not finish conclusively. Its files were kept; inspect the folder or choose another destination. It will not be retried automatically.".into());
        }
        verified(&target, &url).await?;
        return Ok(json!({"cwd":target,"remoteUrl":url,"repository":null}));
    }
    empty(&target)?;
    let record = json!({"remoteUrl":url,"path":target,"state":"started"}).to_string();
    let k = key.clone();
    db.call(move |a| {
        let tx = a.store.db.transaction()?;
        let n: i64 = tx.query_row(
            "SELECT count(*) FROM metadata WHERE key GLOB 'native_clone_*'",
            [],
            |r| r.get(0),
        )?;
        if n >= 128 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![k, record],
        )?;
        tx.commit()
    })
    .await?
    .map_err(|_| "Cannot reserve a clone; no Git command was started")?;
    let folder = target
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid clone folder name")?;
    let parent = target.parent().ok_or("Invalid clone destination")?;
    // Disable hooks/submodules and external protocol helpers. Credentials may be
    // used noninteractively; no browser, password dialog or visible terminal.
    let (code, _) = vcs::git_with_timeout(
        parent,
        &[
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.git.allow=always",
            "clone",
            "--quiet",
            "--no-recurse-submodules",
            "--",
            &url,
            folder,
        ],
        Duration::from_secs(120),
        true,
    )
    .await?;
    if code != 0 {
        return Err("Git clone failed. Check the URL and sign-in on this PC. Existing files were kept; choose another destination before retrying.".into());
    }
    verified(&target, &url).await?;
    let record = json!({"remoteUrl":url,"path":target,"state":"done"}).to_string();
    db.call(move |a| {
        let n = a.store.db.execute(
            "UPDATE metadata SET value=?2 WHERE key=?1",
            rusqlite::params![key, record],
        )?;
        if n != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    })
    .await?
    .map_err(|_| {
        "Clone finished but its receipt could not be saved; inspect the folder before retrying"
    })?;
    Ok(json!({"cwd":target,"remoteUrl":url,"repository":null}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Auth, store::Store};
    #[test]
    fn remote_urls_reject_helpers_options_credentials_and_controls() {
        for value in [
            "https://example.test/repo.git",
            "ssh://git@example.test/repo.git",
            "git@example.test:repo.git",
            "git://localhost/repo",
        ] {
            assert_eq!(remote(value).unwrap(), value);
        }
        for value in [
            "",
            "--upload-pack=bad",
            "ext::bad",
            "file:///private/repo",
            "/private/repo",
            "https://user:secret@example.test/repo",
            "https://secret@example.test/repo",
            "https://example.test/repo?secret=x",
            "ssh://git@example.test/repo\nother",
            "git@-host:repo",
            "git@host:-flag",
        ] {
            assert!(remote(value).is_err(), "{value}");
        }
    }
    #[test]
    fn destination_is_scoped_and_nonempty_content_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let p = destination(dir.path(), "nested/folder with spaces").unwrap();
        assert!(p.parent().unwrap().is_dir());
        assert!(empty(&p).is_ok());
        fs::create_dir(&p).unwrap();
        fs::write(p.join("keep"), "original").unwrap();
        assert!(empty(&p).is_err());
        assert_eq!(fs::read_to_string(p.join("keep")).unwrap(), "original");
        assert!(destination(dir.path(), "../outside").is_err());
        assert!(destination(dir.path(), "-flag").is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert!(destination(dir.path(), "link/repo").is_err());
            assert!(destination(dir.path(), "link").is_err());
        }
    }
    #[tokio::test]
    async fn interrupted_receipt_never_replays_or_reuses_another_remote() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let dest = destination(dir.path(), "interrupted").unwrap();
        let identity = vcs::path_argument(&dest).unwrap();
        #[cfg(windows)]
        let identity = identity.to_lowercase();
        let key = format!("native_clone_{}", crate::auth::digest(&identity));
        store
            .db
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![
                    key,
                    json!({"state":"started","remoteUrl":"https://example.test/repo"}).to_string()
                ],
            )
            .unwrap();
        let db = AuthService::new(Auth::new(store)).unwrap();
        let err = run(
            db.clone(),
            dir.path(),
            json!({"remoteUrl":"https://example.test/repo","destinationPath":dest}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not be retried"));
        assert!(!dest.exists());
        let err = run(
            db,
            dir.path(),
            json!({"remoteUrl":"https://example.test/another","destinationPath":dest}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("another clone"));
        assert!(!dest.exists());
    }
    #[tokio::test]
    async fn git_clone_and_completed_receipt_keep_history_and_local_edits() {
        use std::process::Stdio;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        assert_eq!(vcs::git(&source, &["init"]).await.unwrap().0, 0);
        fs::write(source.join("README.md"), "fixture repository").unwrap();
        assert_eq!(vcs::git(&source, &["add", "README.md"]).await.unwrap().0, 0);
        assert_eq!(
            vcs::git(
                &source,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-m",
                    "fixture"
                ]
            )
            .await
            .unwrap()
            .0,
            0
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = tokio::process::Command::new("git");
        command
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", vcs::path_argument(root.path()).unwrap()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut daemon = command.spawn().unwrap();
        let tree = crate::process_tree::ProcessTree::attach(&daemon).unwrap();
        let mut ready = false;
        for _ in 0..400 {
            if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "fixture Git daemon did not start");
        let db =
            AuthService::new(Auth::new(Store::open(&root.path().join("state")).unwrap())).unwrap();
        let target = root.path().join("nested").join("clone with spaces");
        let url = format!("git://127.0.0.1:{port}/source");
        let p = json!({"remoteUrl":url,"destinationPath":target});
        let first = run(db.clone(), root.path(), p.clone()).await.unwrap();
        assert!(PathBuf::from(s(&first["cwd"])).join("README.md").is_file());
        fs::write(target.join("README.md"), "local edit").unwrap();
        let result = run(db.clone(), root.path(), p.clone()).await.unwrap();
        assert_eq!(result, first);
        assert_eq!(
            fs::read_to_string(target.join("README.md")).unwrap(),
            "local edit"
        );
        // Durable receipts still work after the endpoint's storage is reopened.
        let reopened =
            AuthService::new(Auth::new(Store::open(&root.path().join("state")).unwrap())).unwrap();
        assert_eq!(run(reopened, root.path(), p).await.unwrap(), first);
        let bad = json!({"remoteUrl":format!("git://127.0.0.1:{port}/missing"),"destinationPath":root.path().join("failed")});
        assert!(run(db.clone(), root.path(), bad.clone()).await.is_err());
        assert!(run(db, root.path(), bad)
            .await
            .unwrap_err()
            .contains("not be retried"));
        drop(tree);
        let _ = daemon.kill().await;
        let _ = daemon.wait().await;
    }
}
