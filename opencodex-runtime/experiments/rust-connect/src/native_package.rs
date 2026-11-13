//! Produce a self-contained native distribution without installation scripts or
//! a JavaScript launcher. Dashboard JavaScript is static browser UI, not a server.
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

const ERROR: &str = "Native package could not be verified; existing output was not replaced";
const MAX_BINARY: u64 = 128 * 1024 * 1024;
fn version(value: &str) -> bool {
    let (core, beta) = match value.split_once("-beta.") {
        Some((core, beta)) => (core, Some(beta)),
        None => (value, None),
    };
    let number = |v: &str| {
        !v.is_empty()
            && (v == "0" || !v.starts_with('0'))
            && v.bytes().all(|c| c.is_ascii_digit())
            && v.parse::<u64>().is_ok()
    };
    value.len() <= 48
        && core.split('.').count() == 3
        && core.split('.').all(number)
        && beta.is_none_or(number)
}
fn plain(path: &Path, directory: bool) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ERROR)?;
    if metadata.is_symlink()
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
    {
        return Err(ERROR.into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(ERROR.into());
        }
    }
    Ok(metadata)
}
fn digest(path: &Path, limit: u64) -> Result<(String, u64), String> {
    let (mut file, before) = crate::profile_snapshot::open(path, limit).map_err(|_| ERROR)?;
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut bytes = [0u8; 65536];
    loop {
        let read = file.read(&mut bytes).map_err(|_| ERROR)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > before.len() {
            return Err(ERROR.into());
        }
        hash.update(&bytes[..read]);
    }
    if count == 0 || count != before.len() {
        return Err(ERROR.into());
    }
    Ok((format!("{:x}", hash.finalize()), count))
}
fn write(path: &Path, value: &Value) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| ERROR)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|_| ERROR)?)
        .map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR.into())
}
fn copy_ui(
    source: &Path,
    destination: &Path,
    files: &mut usize,
    total: &mut u64,
) -> Result<(), String> {
    plain(source, true)?;
    fs::create_dir(destination).map_err(|_| ERROR)?;
    for entry in fs::read_dir(source).map_err(|_| ERROR)? {
        let entry = entry.map_err(|_| ERROR)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| ERROR)?;
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            copy_ui(&path, &target, files, total)?;
        } else {
            let metadata = plain(&path, false)?;
            *files += 1;
            *total += metadata.len();
            if *files > 20_000 || *total > 512 * 1024 * 1024 {
                return Err(ERROR.into());
            }
            fs::copy(&path, &target).map_err(|_| ERROR)?;
            // Zero-byte static assets are valid. Nonempty files must match.
            if metadata.len() > 0
                && digest(&path, 512 * 1024 * 1024)? != digest(&target, 512 * 1024 * 1024)?
            {
                return Err(ERROR.into());
            }
        }
    }
    Ok(())
}
pub fn build(
    binary: &Path,
    gui: &Path,
    output: &Path,
    release_version: &str,
) -> Result<Value, String> {
    build_distribution(binary, gui, output, release_version, false)
}

/// Publication is deliberately separate from the default private package.
/// Only explicitly requested Windows x64 beta versions may be published.
pub fn build_beta(
    binary: &Path,
    gui: &Path,
    output: &Path,
    release_version: &str,
) -> Result<Value, String> {
    if !cfg!(all(windows, target_arch = "x86_64"))
        || !version(release_version)
        || !release_version.contains("-beta.")
    {
        return Err("Publishable packages require Windows x64 and an X.Y.Z-beta.N version".into());
    }
    build_distribution(binary, gui, output, release_version, true)
}

fn build_distribution(
    binary: &Path,
    gui: &Path,
    output: &Path,
    release_version: &str,
    publishable: bool,
) -> Result<Value, String> {
    if cfg!(feature = "tray-test-controls") {
        return Err("Developer tray controls cannot be packaged. Rebuild without tray-test-controls for users".into());
    }
    if !version(release_version) || !output.is_absolute() || fs::symlink_metadata(output).is_ok() {
        return Err(ERROR.into());
    }
    plain(gui, true)?;
    plain(&gui.join("index.html"), false)?;
    let before = digest(binary, MAX_BINARY)?;
    let startup = if cfg!(windows) {
        let source = binary.parent().ok_or(ERROR)?.join("remodex-startup.exe");
        Some((source.clone(), digest(&source, MAX_BINARY)?))
    } else {
        None
    };
    let parent = output.parent().ok_or(ERROR)?;
    plain(parent, true)?;
    let resolved_output = parent
        .canonicalize()
        .map_err(|_| ERROR)?
        .join(output.file_name().ok_or(ERROR)?);
    let resolved_gui = gui.canonicalize().map_err(|_| ERROR)?;
    if resolved_output.starts_with(&resolved_gui) || resolved_gui.starts_with(&resolved_output) {
        return Err(ERROR.into());
    }
    fs::create_dir(output).map_err(|_| ERROR)?;
    let package = output.join("package");
    fs::create_dir(&package).map_err(|_| ERROR)?;
    let executable = if cfg!(windows) { "rmx.exe" } else { "rmx" };
    let copied = package.join(executable);
    fs::copy(binary, &copied).map_err(|_| ERROR)?;
    if digest(binary, MAX_BINARY)? != before || digest(&copied, MAX_BINARY)? != before {
        return Err(ERROR.into());
    }
    let startup_metadata = if let Some((source, expected)) = startup {
        let target = package.join("remodex-startup.exe");
        fs::copy(&source, &target).map_err(|_| ERROR)?;
        if digest(&source, MAX_BINARY)? != expected || digest(&target, MAX_BINARY)? != expected {
            return Err(ERROR.into());
        }
        json!({"executable":"remodex-startup.exe","sha256":expected.0,"bytes":expected.1})
    } else {
        Value::Null
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&copied, fs::Permissions::from_mode(0o755)).map_err(|_| ERROR)?;
    }
    fs::create_dir(package.join("gui")).map_err(|_| ERROR)?;
    let (mut files, mut total) = (0, 0);
    copy_ui(gui, &package.join("gui/dist"), &mut files, &mut total)?;
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    write(
        &package.join("native.json"),
        &json!({"formatVersion":2,"version":release_version,"target":target,"executable":executable,"sha256":before.0,"bytes":before.1,"backend":"rust","launcher":"rust","startup":startup_metadata}),
    )?;
    let os = if cfg!(windows) {
        "win32"
    } else {
        std::env::consts::OS
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        std::env::consts::ARCH
    };
    let mut included_files = vec![
        executable,
        "native.json",
        "gui/dist",
        "README.md",
        "LICENSE",
        "THIRD_PARTY_NOTICES.txt",
    ];
    if cfg!(windows) {
        included_files.push("remodex-startup.exe");
    }
    fs::write(package.join("README.md"), include_str!("../NPM-BETA.md")).map_err(|_| ERROR)?;
    fs::write(package.join("LICENSE"), include_str!("../../../LICENSE")).map_err(|_| ERROR)?;
    fs::write(
        package.join("THIRD_PARTY_NOTICES.txt"),
        include_str!("../THIRD_PARTY_NOTICES.txt"),
    )
    .map_err(|_| ERROR)?;
    let mut manifest = json!({"name":"@remodex/rmx","version":release_version,"private":!publishable,
        "description":"Remodex native Rust server for Windows — experimental beta","os":[os],"cpu":[arch],
        "license":"MIT",
        "bin":{"rmx":executable,"remodex":executable,"opencodex":executable,"ocx":executable},
        "files":included_files});
    if publishable {
        manifest["publishConfig"] =
            json!({"registry":"https://registry.npmjs.org/","access":"public","tag":"beta"});
    }
    write(&package.join("package.json"), &manifest)?;
    let archive = output.join(format!("remodex-rmx-{release_version}-{target}.tgz"));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive)
        .map_err(|_| ERROR)?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    tar.append_dir_all("package", &package).map_err(|_| ERROR)?;
    let file = tar
        .into_inner()
        .map_err(|_| ERROR)?
        .finish()
        .map_err(|_| ERROR)?;
    file.sync_all().map_err(|_| ERROR)?;
    Ok(
        json!({"package":package,"archive":archive,"version":release_version,"target":target,"runtime":"rust","launcher":"rust","nodeRequired":false,"bunRequired":false,"sha256":before.0,"dashboardFiles":files}),
    )
}

/// Verify an installed package before user commands. A bare developer/service
/// executable has its existing service receipt instead of a package manifest.
pub fn verify_if_packaged(binary: &Path) -> Result<Option<String>, String> {
    let metadata = binary.parent().ok_or(ERROR)?.join("native.json");
    match fs::symlink_metadata(&metadata) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ERROR.into()),
        Ok(_) => {}
    }
    let raw = crate::profile_snapshot::bytes(&metadata, 4096).map_err(|_| ERROR)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| ERROR)?;
    let release_version = value["version"]
        .as_str()
        .filter(|s| version(s))
        .ok_or(ERROR)?;
    if value["formatVersion"] != 2
        || value["backend"] != "rust"
        || value["launcher"] != "rust"
        || value["target"] != format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        || Some(value["executable"].as_str().ok_or(ERROR)?)
            != binary.file_name().and_then(|s| s.to_str())
    {
        return Err(ERROR.into());
    }
    let (hash, size) = digest(binary, MAX_BINARY)?;
    if value["sha256"] != hash || value["bytes"] != size {
        return Err("Native executable checksum failed; reinstall the package".into());
    }
    if !value["startup"].is_null() {
        if !cfg!(windows) || value["startup"]["executable"] != "remodex-startup.exe" {
            return Err(ERROR.into());
        }
        let (hash, size) = digest(
            &binary.parent().ok_or(ERROR)?.join("remodex-startup.exe"),
            MAX_BINARY,
        )?;
        if value["startup"]["sha256"] != hash || value["startup"]["bytes"] != size {
            return Err("Native startup helper checksum failed; reinstall the package".into());
        }
    }
    Ok(Some(release_version.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn beta_versions_are_explicit_and_cannot_publish_stable_versions() {
        for valid in ["0.0.0", "1.2.27", "2.0.0-beta.1", "2.0.0-beta.0"] {
            assert!(version(valid), "{valid}");
        }
        for invalid in [
            "2.0.0-beta",
            "2.0.0-beta.01",
            "2.0.0-beta.1.extra",
            "2.0.0-rc.1",
            "02.0.0",
            "../1.0.0",
            "2.0.0-beta.1/path",
            "2.0.0-beta.",
        ] {
            assert!(!version(invalid), "{invalid}");
        }
        let root = tempfile::tempdir().unwrap();
        assert!(build_beta(root.path(), root.path(), &root.path().join("out"), "2.0.0").is_err());
        assert!(!root.path().join("out").exists());
    }
    #[test]
    fn package_has_native_entry_and_rejects_changed_executable() {
        let root = tempfile::tempdir().unwrap();
        if cfg!(feature = "tray-test-controls") {
            let output = root.path().join("must-not-exist");
            let error = build(root.path(), root.path(), &output, "1.2.3").unwrap_err();
            assert!(error.contains("Developer tray controls cannot be packaged"));
            assert!(!output.exists());
            return;
        }
        let root = root.path();
        let binary = root.join("test.exe");
        fs::write(&binary, b"native test fixture").unwrap();
        if cfg!(windows) {
            fs::write(root.join("remodex-startup.exe"), b"startup fixture").unwrap();
        }
        let gui = root.join("gui");
        fs::create_dir(&gui).unwrap();
        fs::write(gui.join("index.html"), b"<html></html>").unwrap();
        fs::write(gui.join("empty.css"), b"").unwrap();
        let output = root.join("package 测试");
        let result = build(&binary, &gui, &output, "1.2.3").unwrap();
        assert_eq!(result["nodeRequired"], false);
        let package = output.join("package");
        let name = if cfg!(windows) { "rmx.exe" } else { "rmx" };
        assert_eq!(
            verify_if_packaged(&package.join(name)).unwrap(),
            Some("1.2.3".into())
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(package.join("package.json")).unwrap()).unwrap();
        assert_eq!(manifest["bin"]["rmx"], name);
        assert!(manifest["scripts"].is_null());
        assert!(manifest["dependencies"].is_null());
        assert_eq!(manifest["private"], true);
        assert!(manifest["publishConfig"].is_null());
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let beta_output = root.join("beta");
            build_beta(&binary, &gui, &beta_output, "2.0.0-beta.1").unwrap();
            let beta = beta_output.join("package");
            let manifest: Value =
                serde_json::from_slice(&fs::read(beta.join("package.json")).unwrap()).unwrap();
            assert_eq!(manifest["private"], false);
            assert_eq!(manifest["publishConfig"]["tag"], "beta");
            assert_eq!(
                manifest["publishConfig"]["registry"],
                "https://registry.npmjs.org/"
            );
            assert_eq!(manifest["os"], json!(["win32"]));
            assert!(manifest["scripts"].is_null());
            assert!(manifest["dependencies"].is_null());
            assert!(fs::read_to_string(beta.join("LICENSE"))
                .unwrap()
                .contains("MIT License"));
            assert!(fs::read_to_string(beta.join("README.md"))
                .unwrap()
                .contains("1.2.27"));
            assert_eq!(
                verify_if_packaged(&beta.join(name)).unwrap(),
                Some("2.0.0-beta.1".into())
            );
        }
        assert!(build(&binary, &gui, &output, "1.2.3").is_err());
        if cfg!(windows) {
            let helper = package.join("remodex-startup.exe");
            fs::write(&helper, b"altered startup helper").unwrap();
            assert!(verify_if_packaged(&package.join(name)).is_err());
            fs::write(&helper, b"startup fixture").unwrap();
            assert!(verify_if_packaged(&package.join(name)).is_ok());
        }
        fs::write(package.join(name), b"changed executable").unwrap();
        assert!(verify_if_packaged(&package.join(name)).is_err());
    }
}
