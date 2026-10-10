//! Do not `cargo:rustc-link-lib=nvencodeapi`. `nvEncodeAPI64.dll` ships only
//! with the NVIDIA driver; a link-time import aborts the all-vendor host before
//! `main`. `pf-encode` loads the entry points at runtime (`LoadLibraryExW`).

/// Windows hands a DPI-unaware or system-aware process the cursor bitmap and GDI
/// metrics for the DPI it was LAUNCHED at, whatever a thread's awareness says: a host
/// started on a 300 % desktop kept forwarding a 96 px pointer onto the 96 DPI virtual
/// display. Per-monitor v2 from the first instruction keeps every thread on the
/// display's live DPI; a runtime call cannot beat the DLLs that initialise before `main`.
#[cfg(windows)]
const MANIFEST: &str = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#;

fn main() {
    // Not git-derived: RPM packaging uses a `git archive` tarball with no `.git`.
    let version = std::env::var("PUNKTFUNK_BUILD_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()));
    println!("cargo:rustc-env=PUNKTFUNK_VERSION={version}");
    println!("cargo:rerun-if-env-changed=PUNKTFUNK_BUILD_VERSION");

    // cfg(windows) is the HOST (Linux packaging skips this); `CARGO_CFG_WINDOWS` is
    // the TARGET. Task Manager / Explorer show FileDescription, not the exe name.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = "../../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            .set_icon_with_id(icon, "1")
            .set("FileDescription", "Punktfunk Host")
            .set("ProductName", "Punktfunk")
            .set("CompanyName", "unom")
            .set_manifest(MANIFEST)
            .compile()
            .expect("embed windows icon/version/manifest resources");
    }
}
