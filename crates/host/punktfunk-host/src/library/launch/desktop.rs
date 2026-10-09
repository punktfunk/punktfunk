//! `launch.kind == "desktop_id"`: run an installed `.desktop` entry by its id.
//!
//! The plugin that enumerates the menu sends only the id. The command comes from the entry the
//! distribution (or Flatpak) installed, read from the XDG data dirs the host searches itself, so
//! nothing a plugin says becomes a command line.
//!
//! The command still runs through the session shell, exactly as the desktop menu would run it —
//! the difference is that its text is the system's, not a plugin's.

use std::path::PathBuf;

/// A desktop entry's id: the basename, with or without the `.desktop` suffix. No separators, so
/// an id can never walk out of the directories below.
pub fn valid_desktop_id(value: &str) -> bool {
    let id = value.strip_suffix(".desktop").unwrap_or(value);
    !id.is_empty()
        && id.len() <= 255
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

/// `Exec=` of the entry `id` names, with the field codes removed, plus its `Path=` as cwd.
pub fn desktop_command(id: &str) -> Option<(String, Option<PathBuf>)> {
    if !valid_desktop_id(id) {
        return None;
    }
    let files = id_files(id.strip_suffix(".desktop").unwrap_or(id));
    let path = applications_dirs()
        .into_iter()
        .flat_map(|dir| files.iter().map(move |f| dir.join(f)))
        .find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&path).ok()?;
    parse_entry(&text)
}

/// `Exec=` of the first entry, in XDG order, that handles `mime` (`x-scheme-handler/<scheme>`).
/// Finds an app by the link it registers, wherever it is installed: an integrated AppImage's
/// entry has no fixed id. Two installs resolve by XDG order, not by mimeapps.list's default.
#[cfg(target_os = "linux")]
pub fn mime_handler_command(mime: &str) -> Option<String> {
    applications_dirs().into_iter().find_map(|dir| {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
            .collect();
        files.sort();
        files.into_iter().find_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            handles_mime(&text, mime)
                .then(|| parse_entry(&text))
                .flatten()
                .map(|(cmd, _)| cmd)
        })
    })
}

/// Does the entry list `mime` in its `MimeType=`?
#[cfg(target_os = "linux")]
fn handles_mime(text: &str, mime: &str) -> bool {
    entry_keys(text).any(|(k, v)| k == "MimeType" && v.split(';').any(|m| m.trim() == mime))
}

/// Where an id's file may sit below an `applications` dir. XDG joins a subdirectory into the id
/// with `-` (`kde-foo` is `kde/foo.desktop`), so each `-` is also tried as that one separator.
fn id_files(stem: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(format!("{stem}.desktop"))];
    for (i, _) in stem.match_indices('-') {
        let (dir, rest) = (&stem[..i], &stem[i + 1..]);
        if !matches!(dir, "" | "." | "..") && !rest.is_empty() {
            out.push(PathBuf::from(dir).join(format!("{rest}.desktop")));
        }
    }
    out
}

/// The trimmed `key=value` pairs of the `[Desktop Entry]` group; other groups and comments skipped.
fn entry_keys(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut in_group = false;
    text.lines().filter_map(move |line| {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            return None;
        }
        if !in_group || line.starts_with('#') {
            return None;
        }
        line.split_once('=').map(|(k, v)| (k.trim(), v.trim()))
    })
}

/// Parse the `[Desktop Entry]` group: its `Exec` and `Path`. `None` for an entry that is hidden,
/// is not an application, or has no `Exec` — the same entries a menu would not show.
fn parse_entry(text: &str) -> Option<(String, Option<PathBuf>)> {
    let (mut exec, mut cwd, mut hidden, mut kind) = (None, None, false, None);
    for (key, value) in entry_keys(text) {
        match key {
            "Exec" if exec.is_none() => exec = Some(value.to_string()),
            "Path" if cwd.is_none() => cwd = Some(PathBuf::from(value)),
            "Type" if kind.is_none() => kind = Some(value.to_string()),
            "Hidden" | "NoDisplay" => hidden |= value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if hidden || kind.as_deref() != Some("Application") {
        return None;
    }
    let exec = strip_field_codes(&exec?);
    (!exec.trim().is_empty()).then_some((exec, cwd.filter(|p| p.is_absolute())))
}

/// Drop the `%f %F %u %U %i %c %k` placeholders a launcher would fill in. `%%` is a literal `%`.
fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // `%%` is a literal percent; every other code is one we have no value for, so it goes.
        if chars.next() == Some('%') {
            out.push('%');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `$XDG_DATA_HOME`, `$XDG_DATA_DIRS`, and the Flatpak and snap exports, each `+ /applications`.
fn applications_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(d) => dirs.push(d),
        None => dirs.extend(home.as_ref().map(|h| h.join(".local/share"))),
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    if data_dirs.trim().is_empty() {
        dirs.push(PathBuf::from("/usr/local/share"));
        dirs.push(PathBuf::from("/usr/share"));
    } else {
        dirs.extend(
            data_dirs
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    dirs.extend(home.map(|h| h.join(".local/share/flatpak/exports/share")));
    dirs.push(PathBuf::from("/var/lib/snapd/desktop"));
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

#[cfg(test)]
mod desktop_tests {
    use super::*;

    #[test]
    fn an_id_cannot_leave_the_applications_dirs() {
        assert!(valid_desktop_id("org.videolan.VLC"));
        assert!(valid_desktop_id("steam.desktop"));
        assert!(!valid_desktop_id("../../etc/passwd"));
        assert!(!valid_desktop_id("sub/dir"));
        assert!(!valid_desktop_id(""));
        // `..-passwd` would be `../passwd.desktop` if a segment were not checked.
        let files = id_files("..-passwd");
        assert_eq!(files, vec![PathBuf::from("..-passwd.desktop")]);
    }

    #[test]
    fn an_id_finds_an_entry_one_subdirectory_down() {
        assert_eq!(
            id_files("wine-Quail"),
            vec![
                PathBuf::from("wine-Quail.desktop"),
                PathBuf::from("wine/Quail.desktop"),
            ]
        );
        assert_eq!(
            id_files("org.example.Flap"),
            vec![PathBuf::from("org.example.Flap.desktop")]
        );
    }

    #[test]
    fn the_exec_line_loses_its_field_codes() {
        let entry = "[Desktop Entry]\nType=Application\nName=VLC\nExec=/usr/bin/vlc --started-from-file %U\nPath=/opt/vlc\n";
        let (cmd, cwd) = parse_entry(entry).unwrap();
        assert_eq!(cmd, "/usr/bin/vlc --started-from-file");
        assert_eq!(cwd, Some(PathBuf::from("/opt/vlc")));
        assert_eq!(strip_field_codes("prog %% %f x"), "prog % x");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_scheme_handler_is_read_from_the_entry_group_only() {
        let handles = |text: &str| handles_mime(text, "x-scheme-handler/hydralauncher");
        assert!(handles(
            "[Desktop Entry]\nType=Application\nExec=/opt/Hydra/hydralauncher %U\nMimeType=x-scheme-handler/hydralauncher;\n"
        ));
        assert!(!handles(
            "[Desktop Entry]\nMimeType=x-scheme-handler/hydralauncherx;\n[Desktop Action a]\nMimeType=x-scheme-handler/hydralauncher;\n"
        ));
    }

    #[test]
    fn hidden_and_non_application_entries_do_not_launch() {
        assert!(parse_entry("[Desktop Entry]\nType=Application\nExec=x\nHidden=true\n").is_none());
        assert!(parse_entry("[Desktop Entry]\nType=Link\nExec=x\n").is_none());
        assert!(parse_entry("[Desktop Entry]\nType=Application\n").is_none());
        // A key outside the group is not the entry's.
        assert!(
            parse_entry("[Desktop Action new]\nExec=x\n[Desktop Entry]\nType=Application\n")
                .is_none()
        );
    }
}
