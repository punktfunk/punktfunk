//! Embed version-info and icons into `punktfunk-tray.exe`.
//!
//! Ordinal 1 is the exe/file icon. Ordinals 2–6 are the status-variant tray
//! icons `src/win.rs` loads by id (running / stopped / error / streaming /
//! degraded). Same winresource pattern as `clients/windows/build.rs`.

fn main() {
    // `cfg(windows)` is the host (skip the Linux/macOS workspace stub).
    // `CARGO_CFG_WINDOWS` is the target, same as the Windows client's build.rs.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let branding = "../../../packaging/windows/branding";
        let icons = [
            (format!("{branding}/punktfunk.ico"), "1"),
            (format!("{branding}/punktfunk-tray-running.ico"), "2"),
            (format!("{branding}/punktfunk-tray-stopped.ico"), "3"),
            (format!("{branding}/punktfunk-tray-error.ico"), "4"),
            (format!("{branding}/punktfunk-tray-streaming.ico"), "5"),
            (format!("{branding}/punktfunk-tray-degraded.ico"), "6"),
        ];
        let mut res = winresource::WindowsResource::new();
        for (path, id) in &icons {
            println!("cargo:rerun-if-changed={path}");
            res.set_icon_with_id(path, id);
        }
        // Task Manager / Explorer identity. Matches the host's "Punktfunk Host".
        res.set("FileDescription", "Punktfunk Tray");
        res.set("ProductName", "Punktfunk");
        res.set("CompanyName", "unom");
        // PerMonitorV2. Without a DPI manifest the process is virtualized and GDI-stretches the menu.
        res.set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10/11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#,
        );
        res.compile().expect("embed windows icon resources");
    }
}
