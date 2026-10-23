//! Pinned helper acquisition for the isolated native profile. Downloads stream
//! to disk; only a verified complete installation is published or executed.
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const VERSION: &str = "2026.8.3";
const MAX_BYTES: u64 = 200 * 1024 * 1024;
const FAILED: &str = "cloudflared_unavailable";
#[derive(Clone, Copy)]
struct Release {
    name: &'static str,
    sha256: &'static str,
    archive: bool,
}
fn release(os: &str, arch: &str) -> Option<Release> {
    let (name, sha256, archive) = match (os, arch) {
        ("linux", "x86_64") => (
            "cloudflared-linux-amd64",
            "f29324fe934d1e100617484c78deef803c4dc2cd351d645bbde42e96b4fccc5e",
            false,
        ),
        ("linux", "aarch64") => (
            "cloudflared-linux-arm64",
            "4bcfd35521a7cbc545ebfd5d57334a71ee180e2a64874981f374c81472118391",
            false,
        ),
        ("macos", "x86_64") => (
            "cloudflared-darwin-amd64.tgz",
            "61e1316266a00fd70ce40da011d612badc805367fb65293dd1925f938f704c99",
            true,
        ),
        ("macos", "aarch64") => (
            "cloudflared-darwin-arm64.tgz",
            "40c9144d86df8937c5b43293a1f7d2d2107029aa74725023dd46b1b27154352f",
            true,
        ),
        ("windows", "x86_64" | "aarch64") => (
            "cloudflared-windows-amd64.exe",
            "83e726ed18ea78c5ad5213c4c3a3a27051393950d2bc8ed4de69bec12d14eaae",
            false,
        ),
        _ => return None,
    };
    Some(Release {
        name,
        sha256,
        archive,
    })
}
fn name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}
pub(crate) fn managed_path(directory: &Path) -> PathBuf {
    directory
        .join("tools")
        .join("cloudflared")
        .join(VERSION)
        .join(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
        .join(name())
}
pub(crate) fn is_managed(executable: &Path, directory: &Path) -> bool {
    directory.is_absolute() && executable == managed_path(directory)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn clear_interrupted_installs(parent: &Path) -> io::Result<()> {
    // One manager owns this locked profile. Recover only our bounded, known
    // staging layout; unrelated files and linked directories are left intact.
    for entry in fs::read_dir(parent)?.take(64) {
        let entry = entry?;
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|s| s.strip_prefix(".install-"))
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_dir() || !crate::profile_snapshot::regular(&meta) {
            continue;
        }
        let files = fs::read_dir(entry.path())?
            .take(4)
            .collect::<io::Result<Vec<_>>>()?;
        let known = files.len() <= 2
            && files.iter().all(|f| {
                matches!(
                    f.file_name().to_str(),
                    Some("cloudflared" | "cloudflared.exe" | "release.tgz")
                ) && fs::symlink_metadata(f.path())
                    .is_ok_and(|m| m.is_file() && crate::profile_snapshot::regular(&m))
            });
        if known {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}
fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
fn digest(reader: &mut impl Read, output: &mut impl Write, limit: u64) -> io::Result<String> {
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    let mut hash = Sha256::new();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > limit {
            return Err(invalid());
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if total == 0 {
        return Err(invalid());
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn file_digest(path: &Path, limit: u64) -> io::Result<String> {
    let mut file = crate::storage_stage::open(path, false)?;
    if file.metadata()?.len() > limit {
        return Err(invalid());
    }
    digest(&mut file, &mut io::sink(), limit)
}
fn archive_digest(path: &Path, output: &mut impl Write, limit: u64) -> io::Result<String> {
    let file = crate::storage_stage::open(path, false)?;
    let decoder = flate2::read::GzDecoder::new(file).take(limit + 1024 * 1024);
    let mut archive = tar::Archive::new(decoder);
    let mut found = None;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= 32 {
            return Err(invalid());
        }
        let mut entry = entry?;
        let path = entry.path_bytes();
        if entry.header().entry_type().is_dir() && matches!(path.as_ref(), b"." | b"./") {
            continue;
        }
        if found.is_some()
            || !entry.header().entry_type().is_file()
            || !matches!(path.as_ref(), b"cloudflared" | b"./cloudflared")
            || entry.size() > limit
        {
            return Err(invalid());
        }
        found = Some(digest(&mut entry, output, limit)?);
    }
    found.ok_or_else(invalid)
}
fn cached(directory: &Path, release: Release) -> io::Result<PathBuf> {
    let meta = fs::symlink_metadata(directory)?;
    if !meta.is_dir() || !crate::profile_snapshot::regular(&meta) {
        return Err(invalid());
    }
    let executable = directory.join(name());
    let actual = file_digest(&executable, MAX_BYTES)?;
    if release.archive {
        let archive = directory.join("release.tgz");
        if file_digest(&archive, MAX_BYTES)? != release.sha256
            || archive_digest(&archive, &mut io::sink(), MAX_BYTES)? != actual
        {
            return Err(invalid());
        }
    } else if actual != release.sha256 {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&executable)?.permissions().mode() & 0o111 == 0 {
            return Err(invalid());
        }
    }
    Ok(executable)
}
async fn download(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    sha256: &str,
    limit: u64,
) -> Result<(), &'static str> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "download_failed")?;
    if !response.status().is_success() || response.content_length().is_some_and(|n| n > limit) {
        return Err("download_failed");
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await.map_err(|_| FAILED)?;
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "download_failed")?;
        total += chunk.len() as u64;
        if total > limit {
            return Err("download_failed");
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(|_| FAILED)?;
    }
    file.flush().await.map_err(|_| FAILED)?;
    file.sync_all().await.map_err(|_| FAILED)?;
    if total == 0 || format!("{:x}", hash.finalize()) != sha256 {
        return Err("invalid_checksum");
    }
    Ok(())
}
async fn executable_check(path: &Path) -> Result<(), &'static str> {
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| FAILED)?;
    let _tree = crate::process_tree::ProcessTree::attach(&child).map_err(|_| FAILED)?;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(FAILED)
        }
    }
}
pub(crate) async fn resolve(executable: &Path, directory: &Path) -> Result<PathBuf, &'static str> {
    if !is_managed(executable, directory) {
        return Ok(executable.to_owned());
    }
    let release =
        release(std::env::consts::OS, std::env::consts::ARCH).ok_or("unsupported_platform")?;
    let mut parent = directory.to_owned();
    for component in ["tools", "cloudflared", VERSION] {
        parent.push(component);
        crate::private_directory::prepare(&parent).map_err(|_| FAILED)?;
    }
    clear_interrupted_installs(&parent).map_err(|_| FAILED)?;
    let target = executable.parent().ok_or(FAILED)?.to_owned();
    if target.try_exists().map_err(|_| FAILED)? {
        let path = tokio::task::spawn_blocking(move || cached(&target, release))
            .await
            .map_err(|_| FAILED)?
            .map_err(|_| "invalid_checksum")?;
        executable_check(&path).await?;
        return Ok(path);
    }
    let temporary = Temporary(parent.join(format!(".install-{}", uuid::Uuid::new_v4())));
    crate::private_directory::prepare(&temporary.0).map_err(|_| FAILED)?;
    let downloaded = temporary.0.join(if release.archive {
        "release.tgz"
    } else {
        name()
    });
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| FAILED)?;
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{VERSION}/{}",
        release.name
    );
    download(&client, &url, &downloaded, release.sha256, MAX_BYTES).await?;
    let bin = temporary.0.join(name());
    let working = temporary.0.clone();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        if release.archive {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options.open(working.join(name()))?;
            archive_digest(&downloaded, &mut output, MAX_BYTES)?;
            output.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(working.join(name()), fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    })
    .await
    .map_err(|_| FAILED)?
    .map_err(|_| FAILED)?;
    executable_check(&bin).await?;
    // Only one manager owns this locked profile. Never replace an existing cache
    // entry, including a corrupt cache: preserve it and report the failure.
    if target.symlink_metadata().is_ok() {
        return Err(FAILED);
    }
    fs::rename(&temporary.0, &target).map_err(|_| FAILED)?;
    #[cfg(unix)]
    crate::storage_stage::open(&parent, true)
        .and_then(|f| f.sync_all())
        .map_err(|_| FAILED)?;
    Ok(target.join(name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Downloads the pinned platform helper; run explicitly for release qualification"]
    async fn pinned_platform_helper_installs_and_reuses_verified_cache() {
        let root = tempfile::tempdir().unwrap();
        let path = managed_path(root.path());
        assert_eq!(resolve(&path, root.path()).await.unwrap(), path);
        let original = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(resolve(&path, root.path()).await.unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), original);
        assert!(root.path().join("tools/cloudflared").is_dir());
    }
    #[test]
    fn interrupted_installs_are_cleaned_but_unknown_content_survives() {
        let root = tempfile::tempdir().unwrap();
        let own = root
            .path()
            .join(format!(".install-{}", uuid::Uuid::new_v4()));
        let unknown = root
            .path()
            .join(format!(".install-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&own).unwrap();
        fs::create_dir(&unknown).unwrap();
        fs::write(own.join(name()), b"PARTIAL").unwrap();
        fs::write(unknown.join("personal"), b"KEEP").unwrap();
        clear_interrupted_installs(root.path()).unwrap();
        assert!(!own.exists());
        assert_eq!(fs::read(unknown.join("personal")).unwrap(), b"KEEP");
    }
    #[test]
    fn platform_release_selection_and_paths_are_explicit() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let asset = release(os, arch).unwrap();
            assert_eq!(asset.sha256.len(), 64);
            assert_eq!(asset.archive, os == "macos");
        }
        assert!(release("linux", "x86").is_none());
        assert!(release("freebsd", "x86_64").is_none());
        let root = tempfile::tempdir().unwrap();
        assert!(is_managed(&managed_path(root.path()), root.path()));
        assert!(!is_managed(&root.path().join("cloudflared"), root.path()));
    }
    fn gzip(root: &Path, mode: &str) -> PathBuf {
        let path = root.join("release.tgz");
        let gzip = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gzip);
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o700);
        if mode == "link" {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_link_name("outside").unwrap();
        }
        h.set_cksum();
        let bytes: &[u8] = if mode == "link" { b"" } else { b"BIN" };
        archive
            .append_data(
                &mut h,
                if mode == "extra" {
                    "other"
                } else {
                    "cloudflared"
                },
                bytes,
            )
            .unwrap();
        if mode == "duplicate" {
            archive.append_data(&mut h, "cloudflared", bytes).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        path
    }
    #[test]
    fn archives_extract_only_one_regular_bounded_executable() {
        for mode in ["ok", "link", "extra", "duplicate"] {
            let root = tempfile::tempdir().unwrap();
            let path = gzip(root.path(), mode);
            let mut bytes = vec![];
            let result = archive_digest(&path, &mut bytes, 10);
            assert_eq!(result.is_ok(), mode == "ok", "{mode}");
            if mode == "ok" {
                assert_eq!(bytes, b"BIN");
                assert!(archive_digest(&path, &mut io::sink(), 2).is_err());
            }
        }
    }
    #[tokio::test]
    async fn download_rejects_wrong_digest_empty_and_oversized_bodies() {
        use axum::{routing::get, Router};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                server,
                Router::new()
                    .route("/ok", get(|| async { "BIN" }))
                    .route("/empty", get(|| async { "" })),
            )
            .await
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let hash = format!("{:x}", Sha256::digest(b"BIN"));
        assert!(download(
            &client,
            &format!("http://{address}/ok"),
            &root.path().join("ok"),
            &hash,
            3
        )
        .await
        .is_ok());
        assert!(download(
            &client,
            &format!("http://{address}/ok"),
            &root.path().join("size"),
            &hash,
            2
        )
        .await
        .is_err());
        assert!(download(
            &client,
            &format!("http://{address}/ok"),
            &root.path().join("bad"),
            "wrong",
            3
        )
        .await
        .is_err());
        assert!(download(
            &client,
            &format!("http://{address}/empty"),
            &root.path().join("empty"),
            "wrong",
            3
        )
        .await
        .is_err());
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    async fn chunked_download_is_bounded_and_cancellation_discards_partial_install() {
        use axum::{body::Body, routing::get, Router};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                server,
                Router::new()
                    .route(
                        "/large",
                        get(|| async {
                            Body::from_stream(futures_util::stream::iter([
                                Ok::<_, std::convert::Infallible>(vec![b'x'; 3]),
                                Ok(vec![b'x'; 3]),
                            ]))
                        }),
                    )
                    .route(
                        "/slow",
                        get(|| async {
                            Body::from_stream(
                                futures_util::stream::once(async {
                                    Ok::<_, std::convert::Infallible>(vec![b'x'; 3])
                                })
                                .chain(futures_util::stream::pending()),
                            )
                        }),
                    ),
            )
            .await
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        assert!(download(
            &reqwest::Client::new(),
            &format!("http://{address}/large"),
            &root.path().join("large"),
            "unused",
            4
        )
        .await
        .is_err());
        let temporary = Temporary(root.path().join("partial"));
        fs::create_dir(&temporary.0).unwrap();
        let path = temporary.0.join("download");
        let watched = path.clone();
        let install = tokio::spawn(async move {
            let _owned = temporary;
            download(
                &reqwest::Client::new(),
                &format!("http://{address}/slow"),
                &path,
                "unused",
                4,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fs::metadata(&watched).is_ok_and(|m| m.len() == 3) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        install.abort();
        let _ = install.await;
        assert!(!root.path().join("partial").exists());
        task.abort();
        let _ = task.await;
    }
    #[test]
    fn cache_is_verified_against_the_pinned_archive_and_changed_files_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let archive = gzip(root.path(), "ok");
        let hash = file_digest(&archive, MAX_BYTES).unwrap();
        let asset = Release {
            name: "fixture",
            sha256: Box::leak(hash.into_boxed_str()),
            archive: true,
        };
        let bin = root.path().join(name());
        fs::write(&bin, b"BIN").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(cached(root.path(), asset).unwrap(), bin);
        fs::write(&bin, b"EDIT").unwrap();
        assert!(cached(root.path(), asset).is_err());
        assert_eq!(fs::read(&bin).unwrap(), b"EDIT");
    }
    #[cfg(unix)]
    #[test]
    fn linked_cache_files_are_not_read_or_changed() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"KEEP").unwrap();
        let cache = root.path().join("cache");
        fs::create_dir(&cache).unwrap();
        std::os::unix::fs::symlink(&outside, cache.join(name())).unwrap();
        assert!(cached(&cache, release("linux", "x86_64").unwrap()).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"KEEP");
    }
}
