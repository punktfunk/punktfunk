fn main() {
    // cfg(windows) is the HOST (skips the Linux/macOS stub build); CARGO_CFG_WINDOWS is the
    // TARGET — the same double gate clients/windows/build.rs uses.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Self-contained: stage the full WinAppSDK runtime next to the exe. An installer cannot
        // assume the runtime it is about to install, so unlike the client (framework-dependent
        // + bootstrap()) this ships its own copy. This call also embeds reactor's
        // activatable-class manifest via /MANIFEST:EMBED + /MANIFESTINPUT.
        windows_reactor_setup::as_self_contained();

        // The elevation manifest. NOT a second /MANIFESTINPUT: link.exe always merges its own
        // default UAC fragment (level='asInvoker'), and mt.exe (10.0.26100) refuses two snippets
        // whose `level` disagree (c1010001) — /MANIFESTUAC replaces that default instead.
        // Per-bin on purpose: the client artifact's bin stays unelevated.
        println!(
            "cargo:rustc-link-arg-bin=punktfunk-setup-win=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );

        // reactor imports two package-dependency functions Windows 10 lacks. A self-contained
        // wizard never calls them; delay-loading their API set lets it start there.
        for bin in ["punktfunk-setup-win", "punktfunk-setup-client"] {
            for arg in [
                "/DELAYLOAD:api-ms-win-appmodel-runtime-l1-1-5.dll",
                "delayimp.lib",
            ] {
                println!("cargo:rustc-link-arg-bin={bin}={arg}");
            }
        }

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
