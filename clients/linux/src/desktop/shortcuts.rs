//! "Create shortcut…" — a desktop entry that boots straight into one host (optionally with a
//! preset or a game), design/client-deep-links.md §5.
//!
//! The shortcut is a **container for a URL**, not a second launch mechanism: it invokes the
//! client with a positional `punktfunk://…`, which is the same door xdg-open and a browser
//! prompt use. That is deliberate — it keeps working if scheme registration is lost or the
//! host store changes, because the URL itself carries the stable id, the address and the pin.
//!
//! Under flatpak the sandbox cannot write `~/.local/share/applications`, so this offers the
//! URL to copy instead. The `org.freedesktop.portal.DynamicLauncher` route (which exists for
//! exactly this, with its own confirmation) is the intended upgrade there.

use std::path::PathBuf;

/// Are we inside the flatpak sandbox? `/.flatpak-info` is present in every flatpak run and
/// nowhere else — the standard check, and the one the portal docs use.
pub fn sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Write `~/.local/share/applications/punktfunk-<slug>.desktop` for this URL and return the
/// path. Best-effort `update-desktop-database` afterwards: an entry nothing indexes still
/// works from a file manager, it just won't show up in search straight away.
pub fn write_desktop_entry(label: &str, url: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME isn't set".to_string())?;
    let dir = PathBuf::from(home).join(".local/share/applications");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The URL, not just the label: two hosts can share a name, and the file name was the only
    // thing telling their shortcuts apart — the second silently replaced the first. The same
    // host always resolves to the same URL, so re-creating its shortcut still overwrites its own.
    let path = dir.join(format!(
        "punktfunk-{}-{:08x}.desktop",
        file_slug(label),
        url_tag(url)
    ));
    // Desktop-entry values are line-oriented: a newline in a host name would end the Name
    // key and turn the rest into an unparsable line (or, worse, another key).
    let name = one_line(label);
    // `%` starts a field code in Exec (`%u`, `%f`), and the URL is percent-encoded — a literal
    // one has to be doubled or the launcher eats it and the pair of characters after it.
    let exec_url = url.replace('%', "%%");
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment=Stream from this Punktfunk host\n\
         Exec=punktfunk-client \"{exec_url}\"\n\
         Icon=io.unom.Punktfunk\n\
         Terminal=false\n\
         Categories=Game;Network;\n\
         StartupNotify=true\n"
    );
    std::fs::write(&path, entry).map_err(|e| format!("{}: {e}", path.display()))?;
    // Some desktops only offer a `.desktop` as a launchable icon when it is executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(path)
}

/// A short stable tag for a URL — FNV-1a, so the same host names the same file on every
/// build. Only has to separate one host's shortcut from another's.
fn url_tag(url: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in url.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// A filename-safe slug: ASCII alphanumerics and `-`, everything else collapsed to one `-`,
/// capped so a long host+preset pair can't produce a name the filesystem rejects.
fn file_slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(48).collect();
    if capped.is_empty() {
        "host".to_string()
    } else {
        capped
    }
}

/// Flatten to one line — see [`write_desktop_entry`] on why a newline here is not cosmetic.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names become safe filenames, and a name that survives to `Name=` can't break the file.
    #[test]
    fn labels_are_sanitised_both_ways() {
        assert_eq!(file_slug("Living Room PC"), "living-room-pc");
        assert_eq!(file_slug("Büro · Mac"), "b-ro-mac");
        assert_eq!(file_slug("Desk · Work"), "desk-work");
        assert_eq!(file_slug("////"), "host");
        assert_eq!(file_slug(""), "host");
        assert!(file_slug(&"x".repeat(200)).len() <= 48);
        // The classic injection: a newline would end the Name key and start a new one.
        assert_eq!(
            one_line("Desk\nExec=rm -rf ~"),
            "Desk Exec=rm -rf ~".to_string()
        );
    }
}
