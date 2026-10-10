//! Compile the shell's embedded assets (`data/` — the host-card OS-mark symbolic icons)
//! into a gresource bundle, registered at startup via `gio::resources_register_include!`,
//! and stamp the build version the update check compares against.

fn main() {
    // Build provenance, as in crates/host/punktfunk-host/build.rs: packaging sets
    // PUNKTFUNK_BUILD_VERSION to the full package version; a plain `cargo build` uses the crate's.
    // `--version` prints it and `--check-update` compares it with the signed manifest. Canary
    // channels compare the CI run number in its suffix (pf_update_check::version).
    let version = std::env::var("PUNKTFUNK_BUILD_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()));
    println!("cargo:rustc-env=PUNKTFUNK_VERSION={version}");
    println!("cargo:rerun-if-env-changed=PUNKTFUNK_BUILD_VERSION");

    // Host cfg gate mirrors this crate's `#[cfg(target_os = "linux")]` modules: on any other
    // host the crate compiles to an empty stub and `glib-compile-resources` may not exist.
    #[cfg(target_os = "linux")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        glib_build_tools::compile_resources(
            &["data"],
            "data/resources.gresource.xml",
            "punktfunk-client.gresource",
        );
    }
}
