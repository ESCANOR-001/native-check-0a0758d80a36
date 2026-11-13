#[path = "build/windows_identity.rs"]
mod windows_identity;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/windows_identity.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // MSVC's default main-thread reserve is 1 MiB. The current-thread
        // executor's chat dispatch exceeds it in native Windows debug builds.
        // Match the usual Linux 8 MiB reserve; pages are committed on demand.
        // Apply to this server only, including supervised/restarted instances.
        println!("cargo:rustc-link-arg-bin=remodex-connect-native=/STACK:8388608");
        windows_identity::embed();
    }
}
