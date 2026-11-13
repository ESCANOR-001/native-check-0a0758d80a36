//! Honest Windows file identity and an explicit ordinary-user manifest.
//! These resources are not a signature or antivirus clearance.
use std::{env, fs, path::PathBuf, process::Command};

pub fn embed() {
    let version = env::var("CARGO_PKG_VERSION").expect("package version");
    let core = ["MAJOR", "MINOR", "PATCH"].map(|part| {
        env::var(format!("CARGO_PKG_VERSION_{part}"))
            .expect("version component")
            .parse::<u16>()
            .expect("Windows file version components must fit in 16 bits")
    });
    let prerelease = env::var("CARGO_PKG_VERSION_PRE").expect("prerelease version");
    let revision = if prerelease.is_empty() {
        0
    } else {
        prerelease
            .strip_prefix("beta.")
            .expect("Windows prereleases use beta.N")
            .parse::<u16>()
            .expect("Windows beta number must fit in 16 bits")
    };
    let [major, minor, patch] = core;
    let number = format!("{major}.{minor}.{patch}.{revision}");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"));
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .join("../../src/tray/assets/remodex-tray.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let icon_name = icon
        .to_str()
        .expect("icon path must be Unicode")
        .replace('\\', "/")
        .replace('"', "\"\"");
    // SDK discovery only reads registry/environment data. It does not run a
    // shell or a compiler probe. All resource compilation stays windowless.
    let sdk = find_msvc_tools::find_windows_sdk(env::consts::ARCH)
        .expect("Install the Windows SDK to build Windows program resources");
    let compiler = sdk
        .path()
        .map(|p| p.join("rc.exe"))
        .find(|p| p.is_file())
        .expect("Windows SDK resource compiler rc.exe is missing");
    for (binary, identity, description) in [
        (
            "remodex-connect-native",
            "Remodex.Server",
            "Remodex Rust server",
        ),
        (
            "remodex-startup",
            "Remodex.Startup",
            "Remodex background startup",
        ),
    ] {
        let manifest = out.join(format!("{binary}.manifest"));
        fs::write(
            &manifest,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="{identity}" version="{number}"/>
  <description>{description}</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges>
      <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
    </requestedPrivileges></security>
  </trustInfo>
</assembly>
"#
            ),
        )
        .expect("write program manifest");
        let manifest_name = manifest
            .to_str()
            .expect("resource path must be Unicode")
            .replace('\\', "/")
            .replace('"', "\"\"");
        let resource = out.join(format!("{binary}.rc"));
        let compiled = out.join(format!("{binary}.res"));
        let flags = if prerelease.is_empty() { 0 } else { 2 };
        fs::write(
            &resource,
            format!(
                r#"#pragma code_page(65001)
1 24 "{manifest_name}"
101 ICON "{icon_name}"
1 VERSIONINFO
 FILEVERSION {major},{minor},{patch},{revision}
 PRODUCTVERSION {major},{minor},{patch},{revision}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS {flags}
 FILEOS 0x40004L
 FILETYPE 1
 FILESUBTYPE 0
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "Remodex\0"
   VALUE "FileDescription", "{description}\0"
   VALUE "FileVersion", "{version}\0"
   VALUE "InternalName", "{binary}\0"
   VALUE "OriginalFilename", "{binary}.exe\0"
   VALUE "ProductName", "Remodex\0"
   VALUE "ProductVersion", "{version}\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#
            ),
        )
        .expect("write program identity resource");
        let mut command = Command::new(&compiler);
        command
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled)
            .arg(&resource);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command.output().expect("start Windows resource compiler");
        assert!(
            result.status.success(),
            "Windows resource compilation failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        // The embedded manifest above owns resource ID 1. Do not let the linker
        // synthesize a different manifest or request administrator rights.
        println!("cargo:rustc-link-arg-bin={binary}=/MANIFEST:NO");
        println!("cargo:rustc-link-arg-bin={binary}={}", compiled.display());
    }
}
