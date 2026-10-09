//! The status tray's per-user autostart entry, `~/.config/autostart/io.unom.Punktfunk.Tray.desktop`.
//!
//! The host writes it from the `tray_autostart` setting. The tray's "Start at log-in" item and
//! the desktop's own startup settings change it too. An entry is off when it is missing, or
//! carries `Hidden=true` or `X-GNOME-Autostart-enabled=false`.

use std::path::{Path, PathBuf};

/// The file name. A same-named file in the system autostart folder would be shadowed by this one.
pub const ENTRY_FILE: &str = "io.unom.Punktfunk.Tray.desktop";

/// The tray beside `exe`, by absolute path: a session need not have `~/.local/bin` on `PATH`.
/// A Nix store path changes with every build, so there the profile's `PATH` finds the bare name.
pub fn tray_command(exe: &Path) -> String {
    let tray = exe.with_file_name("punktfunk-tray");
    if tray.starts_with("/nix/store") {
        return "punktfunk-tray".into();
    }
    tray.to_string_lossy().into_owned()
}

/// `TryExec` makes an entry an uninstall left behind inert.
fn entry(command: &str) -> String {
    let exec = if command.contains(' ') {
        format!("\"{command}\"")
    } else {
        command.to_string()
    };
    format!(
        "[Desktop Entry]
Type=Application
Name=Punktfunk tray
Comment=Shows the Punktfunk host's status next to the clock
Exec={exec} --autostart
TryExec={command}
Icon=punktfunk-tray
X-KDE-autostart-after=panel
X-GNOME-Autostart-enabled=true
Categories=Network;Utility;
"
    )
}

/// `$XDG_CONFIG_HOME/autostart/` + [`ENTRY_FILE`].
pub fn entry_path() -> PathBuf {
    crate::xdg_home("XDG_CONFIG_HOME", ".config")
        .join("autostart")
        .join(ENTRY_FILE)
}

/// `Some(on)` when the entry exists, `None` when it does not.
pub fn read() -> Option<bool> {
    std::fs::read_to_string(entry_path())
        .ok()
        .map(|text| is_enabled(&text))
}

/// Writes the entry for on, starting the tray beside `exe` ([`tray_command`]), and deletes it for
/// off. Deleting a missing entry is not an error.
pub fn write(on: bool, exe: &Path) -> std::io::Result<()> {
    let path = entry_path();
    if on {
        return crate::replace_file(&path, entry(&tray_command(exe)).as_bytes());
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether an entry's text starts the tray: off for `Hidden=true` or
/// `X-GNOME-Autostart-enabled=false` in its `[Desktop Entry]` group.
pub fn is_enabled(text: &str) -> bool {
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let off = match key.trim() {
            "Hidden" => value.eq_ignore_ascii_case("true"),
            "X-GNOME-Autostart-enabled" => value.eq_ignore_ascii_case("false"),
            _ => false,
        };
        if in_entry && off {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_entry_is_on() {
        let text = entry(&tray_command(Path::new("/usr/bin/punktfunk-host")));
        assert!(is_enabled(&text));
        assert!(text.contains("\nExec=/usr/bin/punktfunk-tray --autostart\n"));
        assert!(text.contains("\nTryExec=/usr/bin/punktfunk-tray\n"));
    }

    /// SteamOS runs from `~/.local/bin`, which an autostart session may not have on `PATH`.
    #[test]
    fn the_tray_is_named_by_path_except_in_the_nix_store() {
        let home = Path::new("/home/deck/.local/bin/punktfunk-host");
        assert_eq!(tray_command(home), "/home/deck/.local/bin/punktfunk-tray");
        let nix = Path::new("/nix/store/abc-punktfunk/bin/.punktfunk-host-wrapped");
        assert_eq!(tray_command(nix), "punktfunk-tray");
        let spaced = entry("/opt/my apps/punktfunk-tray");
        assert!(spaced.contains("\nExec=\"/opt/my apps/punktfunk-tray\" --autostart\n"));
    }

    #[test]
    fn each_desktop_off_switch_reads_as_off() {
        assert!(!is_enabled("[Desktop Entry]\nHidden=true\n"));
        assert!(!is_enabled(
            "[Desktop Entry]\nName=x\nX-GNOME-Autostart-enabled=false\n"
        ));
        assert!(!is_enabled("[Desktop Entry]\nHidden = True\n"));
        assert!(is_enabled("[Desktop Entry]\nHidden=false\n"));
    }

    #[test]
    fn keys_outside_the_entry_group_are_ignored() {
        assert!(is_enabled(
            "[Desktop Entry]\nName=x\n[Desktop Action off]\nHidden=true\n"
        ));
    }
}
