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

/// Parse the `[Desktop Entry]` group: its `Exec` and `Path`. `None` for an entry that is hidden,
/// is not an application, or has no `Exec` — the same entries a menu would not show.
fn parse_entry(text: &str) -> Option<(String, Option<PathBuf>)> {
    let mut in_group = false;
    let (mut exec, mut cwd, mut hidden, mut kind) = (None, None, false, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" if exec.is_none() => exec = Some(value.trim().to_string()),
            "Path" if cwd.is_none() => cwd = Some(PathBuf::from(value.trim())),
            "Type" if kind.is_none() => kind = Some(value.trim().to_string()),
            "Hidden" | "NoDisplay" => hidden |= value.trim().eq_ignore_ascii_case("true"),
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

/// `$XDG_DATA_HOME`, `$XDG_DATA_DIRS`, and the Flatpak exports, each `+ /applications`.
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
