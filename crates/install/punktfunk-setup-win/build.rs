fn main() {
    // cfg(windows) is the HOST (skips the Linux/macOS stub build); CARGO_CFG_WINDOWS is the
    // TARGET — the same double gate clients/windows/build.rs uses.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Self-contained deployment: stage the full WinAppSDK runtime next to the exe. An
        // installer cannot assume the runtime it is about to install, so unlike the client
        // (framework-dependent + bootstrap()) this ships its own copy (S1's verdict). This
        // call also embeds reactor's activatable-class manifest via /MANIFEST:EMBED +
        // /MANIFESTINPUT — the self-contained marker S1 left the elevation question open on.
        windows_reactor_setup::as_self_contained();

        // The S1-deferred manifest answer. NOT a second /MANIFESTINPUT: link.exe always
        // merges its own default UAC fragment (level='asInvoker'), and mt.exe refuses two
        // snippets whose `level` disagree (c1010001) — /MANIFESTUAC replaces that default
        // instead. Per-bin on purpose: the client artifact's bin (M4) stays unelevated.
        println!(
            "cargo:rustc-link-arg-bin=punktfunk-setup-win=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );

        // The branded icon, ordinal 1 (the client's pattern): Apps & features + the taskbar.
        let icon = "../../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            .set_icon_with_id(icon, "1")
            // UAC names the installer by FileDescription (and by signtool's /d, which the
            // packers read from here).
            .set("FileDescription", "Punktfunk Setup")
            .set("ProductName", "Punktfunk")
            .set("CompanyName", "unom")
            .compile()
            .expect("embed the icon resource");
    }
}
