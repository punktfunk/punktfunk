//! A seat's own Steam client: what of the box's install it copies and how it starts.
//!
//! On Windows, Steam runs once per IPC name on a machine: a second start under the default name
//! closes whoever runs it, in any session and from any folder. So each Windows seat gets a copy
//! of the box's client in its own profile and starts it only under [`ipc_name`]; the box keeps
//! the default. The games stay the box's: the copy lists the box's library folders after its own.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The folder beside a library's `steamapps` that holds each Linux seat's own folder for it.
pub const SEAT_ROOTS: &str = ".punktfunk-seats";

/// Top-level entries of a Steam install that a seat's copy leaves behind, on Windows and Linux:
/// the box's sign-ins, library, caches and logs, Steam Guard's sentry files, and the seats'
/// folders for the library there ([`SEAT_ROOTS`]). Names compare without case.
pub fn skipped(name: &str) -> bool {
    const DIRS: [&str; 7] = [
        "config",
        "userdata",
        "appcache",
        "steamapps",
        "logs",
        "dumps",
        "depotcache",
    ];
    let name = name.to_ascii_lowercase();
    DIRS.contains(&name.as_str())
        || name == SEAT_ROOTS
        || name.ends_with(".vdf")
        || name.starts_with("ssfn")
        || name == "clientregistry.blob"
}

/// The `-master_ipc_name_override` every start of seat `seat_id`'s copy carries.
pub fn ipc_name(seat_id: &str) -> String {
    format!("pfseat{seat_id}")
}

/// The arguments that start a seat's copy under its own name.
pub fn ipc_args(ipc: &str) -> String {
    format!("-master_ipc_name_override {ipc}")
}

/// A Windows command line that starts `exe` under `ipc`, then `rest`.
pub fn seat_command(exe: &str, ipc: &str, rest: &str) -> String {
    let rest = rest.trim();
    let mut line = format!("\"{exe}\" {}", ipc_args(ipc));
    if !rest.is_empty() {
        line.push(' ');
        line.push_str(rest);
    }
    line
}

/// Steam's autostart line pointed at the seat's copy under `ipc`, its other switches kept. Steam
/// writes `"<its steam.exe>" -silent`, which would start the copy under the box's name.
pub fn autostart_line(line: &str, exe: &str, ipc: &str) -> String {
    let line = line.trim();
    let rest = match line.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"').map_or("", |(_, rest)| rest),
        None => line
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest),
    };
    let mut kept = Vec::new();
    let mut words = rest.split_whitespace();
    while let Some(word) = words.next() {
        if word.eq_ignore_ascii_case("-master_ipc_name_override") {
            words.next();
        } else {
            kept.push(word);
        }
    }
    seat_command(exe, ipc, &kept.join(" "))
}

/// Copies `src` to `dst` without the [`skipped`] entries. The copy lands in `<dst>.partial` and
/// is renamed when whole, so a stopped copy never passes for a finished one. Links are not
/// followed: a folder every local user writes may hold one that points anywhere.
pub fn copy_install(src: &Path, dst: &Path) -> io::Result<()> {
    let mut partial = OsString::from(dst.as_os_str());
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if !skipped(&entry.file_name().to_string_lossy()) {
            copy_tree(&entry.path(), &partial.join(entry.file_name()))?;
        }
    }
    std::fs::rename(&partial, dst)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let kind = std::fs::symlink_metadata(src)?.file_type();
    if kind.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if kind.is_file() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// The `path` of every folder a `libraryfolders.vdf` lists, in order.
pub fn library_paths(vdf: &str) -> Vec<String> {
    vdf.lines()
        .filter_map(|line| match quoted(line).as_slice() {
            [key, value] if key.eq_ignore_ascii_case("path") => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// The quoted strings on one VDF line, unescaped.
fn quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => value.extend(chars.next()),
                c => value.push(c),
            }
        }
        out.push(value);
    }
    out
}

/// The smallest `libraryfolders.vdf` Steam accepts: one numbered entry per folder.
pub fn library_folders_vdf(paths: &[String]) -> String {
    let mut out = String::from("\"libraryfolders\"\n{\n");
    for (i, path) in paths.iter().enumerate() {
        out.push_str(&folder_entry(i, path));
    }
    out.push_str("}\n");
    out
}

fn folder_entry(i: usize, path: &str) -> String {
    let path = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{path}\"\n\t}}\n")
}

/// `vdf` with the folders `managed` claims listed exactly as `wanted`: a managed folder `wanted`
/// lacks is dropped, a `wanted` one not listed is added. Every other entry, and every field Steam
/// keeps in a folder (`contentid`, `apps`), stays as written; folders are renumbered from 0.
/// `None` when `vdf` isn't a library list.
pub fn merge_library_folders(
    vdf: &str,
    managed: impl Fn(&str) -> bool,
    wanted: &[String],
) -> Option<String> {
    let tokens = tokens(vdf);
    let [first, second, ..] = tokens.as_slice() else {
        return None;
    };
    match (&first.kind, &second.kind) {
        (Kind::Text(name), Kind::Open) if name.eq_ignore_ascii_case("libraryfolders") => {}
        _ => return None,
    }
    let mut out = String::from("\"libraryfolders\"\n{\n");
    let mut listed = Vec::new();
    let mut i = 2;
    loop {
        let key = tokens.get(i)?;
        let name = match &key.kind {
            Kind::Close => break,
            Kind::Open => return None,
            Kind::Text(name) => name,
        };
        let value = tokens.get(i + 1)?;
        let end = match value.kind {
            Kind::Text(_) => i + 1,
            Kind::Open => closing(&tokens, i + 1)?,
            Kind::Close => return None,
        };
        let numbered = !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
        if value.kind == Kind::Open && numbered {
            let path = path_of(&tokens[i + 2..end]);
            if path
                .as_deref()
                .is_none_or(|p| !managed(p) || wanted.iter().any(|w| w == p))
            {
                let n = listed.len();
                out.push_str(&format!("\t\"{n}\"{}\n", &vdf[key.end..tokens[end].end]));
                listed.push(path);
            }
        } else {
            out.push_str(&format!("\t{}\n", &vdf[key.start..tokens[end].end]));
        }
        i = end + 1;
    }
    for path in wanted {
        if !listed.iter().any(|p| p.as_deref() == Some(path.as_str())) {
            out.push_str(&folder_entry(listed.len(), path));
            listed.push(Some(path.clone()));
        }
    }
    out.push_str("}\n");
    Some(out)
}

/// A Windows seat copy's library list: `own`, the copy, then each folder of `shared` it lacks.
/// Steam reads entry `0` as its own install whatever path stands there, so a folder listed there
/// is lost. Steam's entries and fields stay as written. A list `own` doesn't lead is one Steam
/// never wrote, and is rebuilt.
pub fn seat_library_list(vdf: Option<&str>, own: &str, shared: &[String]) -> String {
    let same = |a: &str, b: &str| {
        a.trim_end_matches('\\')
            .eq_ignore_ascii_case(b.trim_end_matches('\\'))
    };
    let listed = vdf.map(library_paths).unwrap_or_default();
    if listed.first().is_some_and(|first| same(first, own)) {
        let missing: Vec<String> = shared
            .iter()
            .filter(|path| !listed.iter().any(|have| same(have, path)))
            .cloned()
            .collect();
        if let Some(merged) = vdf.and_then(|vdf| merge_library_folders(vdf, |_| false, &missing)) {
            return merged;
        }
    }
    let mut folders = vec![own.to_owned()];
    folders.extend(shared.iter().filter(|path| !same(path, own)).cloned());
    library_folders_vdf(&folders)
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Text(String),
    Open,
    Close,
}

/// One VDF token and the byte range it covers.
#[derive(Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// Quoted or bare strings, unescaped, and braces. `//` comments are skipped.
fn tokens(vdf: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = vdf.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let kind = match c {
            '{' => Kind::Open,
            '}' => Kind::Close,
            '/' if chars.peek().is_some_and(|&(_, next)| next == '/') => {
                while chars.next_if(|&(_, next)| next != '\n').is_some() {}
                continue;
            }
            c if c.is_whitespace() => continue,
            '"' => {
                let mut text = String::new();
                while let Some((_, c)) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => text.extend(chars.next().map(|(_, c)| c)),
                        c => text.push(c),
                    }
                }
                Kind::Text(text)
            }
            c => {
                let mut text = String::from(c);
                while let Some((_, c)) = chars
                    .next_if(|&(_, next)| !next.is_whitespace() && !matches!(next, '{' | '}' | '"'))
                {
                    text.push(c);
                }
                Kind::Text(text)
            }
        };
        let end = chars.peek().map_or(vdf.len(), |&(at, _)| at);
        out.push(Token { kind, start, end });
    }
    out
}

/// The index of the brace that closes the one at `open`.
fn closing(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            Kind::Open => depth += 1,
            Kind::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            Kind::Text(_) => {}
        }
    }
    None
}

/// A folder block's own `path`, not one in a nested block.
fn path_of(inner: &[Token]) -> Option<String> {
    let mut depth = 0_usize;
    let mut i = 0;
    while let Some(token) = inner.get(i) {
        match &token.kind {
            Kind::Open => depth += 1,
            Kind::Close => depth = depth.saturating_sub(1),
            Kind::Text(key) if depth == 0 => {
                if let Some(Kind::Text(value)) = inner.get(i + 1).map(|t| &t.kind) {
                    if key.eq_ignore_ascii_case("path") {
                        return Some(value.clone());
                    }
                    i += 1;
                }
            }
            Kind::Text(_) => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\pf_seat1\Steam\steam.exe";

    #[test]
    fn a_seat_command_names_the_copy_and_its_ipc_name() {
        assert_eq!(
            seat_command(EXE, "pfseatab", r#"-- "%1""#),
            r#""C:\Users\pf_seat1\Steam\steam.exe" -master_ipc_name_override pfseatab -- "%1""#
        );
        assert_eq!(
            seat_command(EXE, "pfseatab", " "),
            r#""C:\Users\pf_seat1\Steam\steam.exe" -master_ipc_name_override pfseatab"#
        );
    }

    /// Steam's own line, the box's install, a bare path and our own line all come out as one
    /// start of the copy under the seat's name, with `-silent` kept.
    #[test]
    fn an_autostart_line_starts_the_copy_under_the_seats_name() {
        let want =
            r#""C:\Users\pf_seat1\Steam\steam.exe" -master_ipc_name_override pfseatab -silent"#;
        for line in [
            r#""C:\Users\pf_seat1\Steam\steam.exe" -silent"#,
            r#""C:\Program Files (x86)\Steam\steam.exe" -silent"#,
            r#"C:\Users\pf_seat1\Steam\steam.exe -silent"#,
            want,
            r#""C:\Users\pf_seat1\Steam\steam.exe" -master_ipc_name_override pfseatold -silent"#,
        ] {
            assert_eq!(autostart_line(line, EXE, "pfseatab"), want, "{line}");
        }
    }

    #[test]
    fn a_copy_leaves_the_boxs_account_library_and_logs() {
        for name in [
            "config",
            "userdata",
            "appcache",
            "steamapps",
            "SteamApps",
            "logs",
            "dumps",
            "depotcache",
            "ssfn1234567890",
            "ClientRegistry.blob",
            "local.vdf",
        ] {
            assert!(skipped(name), "{name}");
        }
        for name in [
            "bin",
            "steam.exe",
            "package",
            "steamui",
            "public",
            "tenfoot",
        ] {
            assert!(!skipped(name), "{name}");
        }
    }

    #[test]
    fn a_copy_is_whole_or_absent() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("box");
        std::fs::create_dir_all(src.join("bin/cef")).unwrap();
        std::fs::create_dir_all(src.join("config")).unwrap();
        std::fs::write(src.join("steam.exe"), "exe").unwrap();
        std::fs::write(src.join("bin/cef/helper.dll"), "dll").unwrap();
        std::fs::write(src.join("config/loginusers.vdf"), "secret").unwrap();
        std::fs::write(src.join("ssfn42"), "sentry").unwrap();
        let dst = root.path().join("seat/Steam");
        std::fs::create_dir_all(root.path().join("seat/Steam.partial/stale")).unwrap();

        copy_install(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("steam.exe")).unwrap(),
            "exe"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("bin/cef/helper.dll")).unwrap(),
            "dll"
        );
        assert!(!dst.join("config").exists());
        assert!(!dst.join("ssfn42").exists());
        assert!(!root.path().join("seat/Steam.partial").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_never_follows_a_link() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("box");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("private"), "x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, src.join("bin")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("private"), src.join("steam.exe")).unwrap();
        let dst = root.path().join("Steam");

        copy_install(&src, &dst).unwrap();

        assert!(!dst.join("bin").exists());
        assert!(!dst.join("steam.exe").exists());
    }

    #[test]
    fn library_paths_read_back_what_was_written() {
        let paths = vec![
            r"C:\Program Files (x86)\Steam".to_owned(),
            r#"D:\Games "shared""#.to_owned(),
            "/var/lib/punktfunk/games".to_owned(),
        ];
        assert_eq!(library_paths(&library_folders_vdf(&paths)), paths);
    }

    #[test]
    fn library_paths_read_a_file_steam_wrote() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\
                   \t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"1\"\n\t\t}\n\t}\n\
                   \t\"1\"\n\t{\n\t\t\"path\"\t\t\"E:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            library_paths(vdf),
            [r"C:\Program Files (x86)\Steam", r"E:\SteamLibrary"]
        );
    }

    /// A Linux box owner's list as Steam writes it: a home library and drives with spaces.
    const OWNER: &str = "\"libraryfolders\"\n{\n\
        \t\"0\"\n\t{\n\t\t\"path\"\t\t\"/var/home/bazzite/.local/share/Steam\"\n\
        \t\t\"label\"\t\t\"\"\n\t\t\"contentid\"\t\t\"8774309875119513024\"\n\
        \t\t\"totalsize\"\t\t\"0\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"44350\"\t\t\"10346097338\"\n\t\t}\n\t}\n\
        \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/run/media/bazzite/QEMU HARDDISK\"\n\
        \t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"730\"\t\t\"73402644130\"\n\t\t}\n\t}\n\
        \t\"2\"\n\t{\n\t\t\"path\"\t\t\"/run/media/bazzite/QEMU HARDDISK1\"\n\
        \t\t\"label\"\t\t\"QEMU HARDDISK\"\n\t\t\"apps\"\n\t\t{\n\t\t}\n\t}\n}\n";

    #[test]
    fn library_paths_read_a_linux_list_with_spaces() {
        assert_eq!(
            library_paths(OWNER),
            [
                "/var/home/bazzite/.local/share/Steam",
                "/run/media/bazzite/QEMU HARDDISK",
                "/run/media/bazzite/QEMU HARDDISK1",
            ]
        );
    }

    /// A seat's list: its own Steam, a drive it adopted before `/run/media` was hidden, a box
    /// library still wanted and one the owner dropped, around a top-level field.
    const SEAT: &str = "\"libraryfolders\"\n{\n\
        \t\"contentstatsid\"\t\t\"-123\"\n\
        \t\"0\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/home/.local/share/Steam\"\n\
        \t\t\"contentid\"\t\t\"2720844216766770910\"\n\
        \t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"0\"\n\t\t}\n\t}\n\
        \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/run/media/system/QEMU_HARDDISK\"\n\
        \t\t\"apps\"\n\t\t{\n\t\t\t\"12210\"\t\t\"23927789464\"\n\t\t}\n\t}\n\
        \t\"2\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/libraries/kept\"\n\
        \t\t\"contentid\"\t\t\"77\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"730\"\t\t\"1\"\n\t\t}\n\t}\n\
        \t\"3\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/libraries/gone\"\n\t}\n}\n";

    fn managed(path: &str) -> bool {
        path.starts_with("/var/lib/punktfunk/seats/s/libraries/") || path.starts_with("/run/media/")
    }

    #[test]
    fn a_merge_keeps_steam_s_entries_and_lists_exactly_the_box_s() {
        let wanted = [
            "/var/lib/punktfunk/seats/s/libraries/kept".to_owned(),
            "/var/lib/punktfunk/seats/s/libraries/new".to_owned(),
        ];
        let merged = merge_library_folders(SEAT, managed, &wanted).unwrap();
        assert_eq!(
            merged,
            "\"libraryfolders\"\n{\n\
             \t\"contentstatsid\"\t\t\"-123\"\n\
             \t\"0\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/home/.local/share/Steam\"\n\
             \t\t\"contentid\"\t\t\"2720844216766770910\"\n\
             \t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"0\"\n\t\t}\n\t}\n\
             \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/libraries/kept\"\n\
             \t\t\"contentid\"\t\t\"77\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"730\"\t\t\"1\"\n\t\t}\n\t}\n\
             \t\"2\"\n\t{\n\t\t\"path\"\t\t\"/var/lib/punktfunk/seats/s/libraries/new\"\n\t}\n}\n"
        );
        assert_eq!(
            merge_library_folders(&merged, managed, &wanted),
            Some(merged)
        );
    }

    #[test]
    fn a_list_with_nothing_to_change_merges_to_itself() {
        assert_eq!(
            merge_library_folders(OWNER, |_| false, &[]).as_deref(),
            Some(OWNER)
        );
        let all_gone = merge_library_folders(OWNER, |_| true, &[]).unwrap();
        assert_eq!(all_gone, "\"libraryfolders\"\n{\n}\n");
    }

    #[test]
    fn a_merge_refuses_text_that_is_not_a_library_list() {
        for text in [
            "",
            "\"config\"\n{\n}\n",
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/a\"\n",
            "\"libraryfolders\"\n{\n\t\"0\"\n}\n",
        ] {
            assert_eq!(merge_library_folders(text, |_| true, &[]), None, "{text:?}");
        }
    }

    #[test]
    fn the_library_list_numbers_its_folders_in_order() {
        let list = library_folders_vdf(&["/home/s/.local/share/Steam".to_owned(), "/g".to_owned()]);
        assert_eq!(
            list,
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/s/.local/share/Steam\"\n\t}\n\
             \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/g\"\n\t}\n}\n"
        );
    }

    /// The copy leads its list whatever was there, and Steam's own fields survive.
    #[test]
    fn a_seat_copy_leads_its_library_list() {
        let own = r"C:\Users\pf_seat1\Steam";
        let shared = [
            r"C:\Program Files (x86)\Steam".to_owned(),
            r"E:\SteamLibrary".to_owned(),
        ];
        let fresh = library_folders_vdf(&[own.to_owned(), shared[0].clone(), shared[1].clone()]);
        assert_eq!(seat_library_list(None, own, &shared), fresh);
        // The order Steam discards: the box's folder at entry 0.
        let led_by_box = library_folders_vdf(&[shared[0].clone(), own.to_owned()]);
        assert_eq!(seat_library_list(Some(&led_by_box), own, &shared), fresh);
        // What Steam wrote back after discarding it: the copy alone, with Steam's fields.
        let steam_wrote = "\"libraryfolders\"\n{\n\
            \t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Users\\\\pf_seat1\\\\Steam\"\n\
            \t\t\"contentid\"\t\t\"5678596761891394462\"\n\
            \t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"0\"\n\t\t}\n\t}\n}\n";
        let healed = seat_library_list(Some(steam_wrote), own, &shared);
        assert_eq!(
            library_paths(&healed),
            [own, shared[0].as_str(), shared[1].as_str()]
        );
        assert!(healed.contains("5678596761891394462"));
        assert_eq!(seat_library_list(Some(&healed), own, &shared), healed);
        // Steam spells a folder its own way: the same folder is not listed twice.
        let respelled = healed.replace("Program Files", "PROGRAM FILES");
        assert_eq!(seat_library_list(Some(&respelled), own, &shared), respelled);
    }

    #[test]
    fn each_seat_has_its_own_ipc_name() {
        assert_eq!(
            ipc_name("0123456789abcdef0123456789abcdef"),
            "pfseat0123456789abcdef0123456789abcdef"
        );
    }
}
