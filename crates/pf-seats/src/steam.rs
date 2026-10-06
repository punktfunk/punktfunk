//! A seat's own Steam client: what of the box's install it copies and how it starts.
//!
//! On Windows, Steam runs once per IPC name on a machine: a second start under the default name
//! closes whoever runs it, in any session and from any folder. So each Windows seat gets a copy
//! of the box's client in its own profile and starts it only under [`ipc_name`]; the box keeps
//! the default. The games stay the box's: the copy lists the box's library folders first.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// Top-level entries of a Windows install that a copy leaves behind: the box's sign-ins, library,
/// caches and logs, and Steam Guard's sentry files.
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
        || name.ends_with(".vdf")
        || name.starts_with("ssfn")
        || name == "clientregistry.blob"
}

/// The `-master_ipc_name_override` every start of seat `seat_id`'s copy carries.
pub fn ipc_name(seat_id: &str) -> String {
    format!("pfseat{seat_id}")
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
        let path = path.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!(
            "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{path}\"\n\t}}\n"
        ));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_library_list_names_the_shared_folder_first() {
        let list = library_folders_vdf(&["/g".to_owned(), "/home/s/.local/share/Steam".to_owned()]);
        assert_eq!(
            list,
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/g\"\n\t}\n\
             \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/home/s/.local/share/Steam\"\n\t}\n}\n"
        );
    }

    #[test]
    fn each_seat_has_its_own_ipc_name() {
        assert_eq!(
            ipc_name("0123456789abcdef0123456789abcdef"),
            "pfseat0123456789abcdef0123456789abcdef"
        );
    }
}
