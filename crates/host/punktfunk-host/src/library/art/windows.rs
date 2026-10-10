//! Windows art roots and the handle-resolved path check: Steam's install (art lives under it,
//! not the users base), a portable Playnite beside its exe, and `GetFinalPathNameByHandleW`.

use super::*;

/// Roots beyond the users base.
pub(super) fn extra_roots() -> Vec<PathBuf> {
    let mut roots = steam_art_roots();
    // Portable Playnite keeps `library\files\…` beside the exe, outside every profile.
    roots.extend(super::super::launch::playnite_art_roots());
    roots
}

/// Windows: Steam install roots. Steam art lives under the install, not the users base
/// (`appcache\librarycache\…` and `userdata\<id>\config\grid\`). POSIX needs no equivalent —
/// native and Flatpak layouts are already under `$HOME`.
fn steam_art_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        // `is_dir` before dedup: `%ProgramFiles%` and `%ProgramW6432%` are the same directory
        // on a 64-bit host, and the registry often repeats whichever Steam sits in.
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            push(PathBuf::from(pf).join("Steam"));
        }
    }
    // Off-default Steam (second drive) is only in the registry. HKLM, not HKCU: the host is
    // SYSTEM and its own hive does not know where the operator installed anything.
    for key in [r"SOFTWARE\WOW6432Node\Valve\Steam", r"SOFTWARE\Valve\Steam"] {
        if let Some(p) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(key)
            .ok()
            .and_then(|k| k.get_value::<String, _>("InstallPath").ok())
        {
            push(PathBuf::from(p));
        }
    }
    out
}

/// Windows twin of Linux `final_path_of`: `GetFinalPathNameByHandleW` in normalized DOS form,
/// which carries the same `\\?\` prefix `canonicalize` produces, so `starts_with` matches.
pub(super) fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, GETFINALPATHNAMEBYHANDLE_FLAGS,
    };
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the handle is a live open File for the whole call, and `buf` is a valid
        // mutable u16 buffer of the length the API is told about.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(f.as_raw_handle()),
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(0), // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(std::ffi::OsString::from_wide(&buf[..n]).into());
        }
        buf.resize(n + 1, 0); // n = required length (incl. NUL) when the buffer was too small
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{file_url, ArtRootsEnv, PNG};
    use super::*;

    /// A Steam cover under Program Files is servable with no `PUNKTFUNK_LIBRARY_ART_ROOTS`.
    /// Hermetic: `%ProgramFiles(x86)%` is pointed at a temp tree. Asserting over whatever
    /// Steam this box has would pass vacuously on CI.
    #[test]
    fn steam_librarycache_cover_is_servable_without_configuration() {
        let base = std::env::temp_dir().join(format!("pf-art-steam-{}", std::process::id()));
        let hero = base
            .join("Steam")
            .join("appcache")
            .join("librarycache")
            .join("570")
            .join("abcdef")
            .join("library_hero.jpg");
        std::fs::create_dir_all(hero.parent().unwrap()).unwrap();
        std::fs::write(&hero, PNG).unwrap();

        // No configured roots; `%ProgramFiles(x86)%` is a real variable later tests may read.
        let _env = ArtRootsEnv::set(&[
            ("PUNKTFUNK_LIBRARY_ART_ROOTS", None),
            ("ProgramFiles(x86)", Some(&base)),
        ]);

        let steam_root = base.join("Steam");
        assert!(
            steam_art_roots().contains(&steam_root),
            "the Program Files probe must find the Steam install"
        );
        assert!(
            art_roots().contains(&steam_root),
            "the DEFAULT art roots must include it — the whole point is that no env var is needed"
        );

        let url = file_url(&hero);
        assert!(art_path_is_servable(&url), "{url} must be servable");
        assert!(
            validate_art_paths(&Artwork {
                hero: Some(url.clone()),
                ..Default::default()
            })
            .is_ok(),
            "a Steam-shaped payload must reconcile"
        );
        assert!(
            sanitize_art_paths(&mut Artwork {
                hero: Some(url.clone()),
                ..Default::default()
            })
            .is_empty(),
            "and nothing about it is dropped"
        );
        assert_eq!(
            local_art_bytes(&url).expect("read time serves it too").0,
            PNG
        );

        let secret = base.join("Steam").join("config").join("config.vdf");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, b"\"Accounts\"\n{\n\"user\" \"token\"\n}\n").unwrap();
        assert!(
            local_art_bytes(secret.to_str().unwrap()).is_none(),
            "Steam's own credential blob must not be servable from an art root"
        );
        let disguised = base.join("Steam").join("config.png");
        std::fs::write(&disguised, b"\"Accounts\" { \"user\" \"token\" }").unwrap();
        assert!(
            local_art_bytes(disguised.to_str().unwrap()).is_none(),
            "an image extension is still not enough — the bytes must BE an image"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Playnite roots must be in the default confinement with no `PUNKTFUNK_LIBRARY_ART_ROOTS`.
    /// Vacuous on a box with no Playnite — the registry half cannot be faked from here.
    #[test]
    fn playnite_roots_reach_the_art_confinement() {
        let _env = ArtRootsEnv::set(&[("PUNKTFUNK_LIBRARY_ART_ROOTS", None)]);
        let roots = art_roots();
        for root in crate::library::launch::playnite_art_roots() {
            assert!(root.is_dir(), "{root:?} is offered as an art root");
            assert!(
                roots.contains(&root),
                "{root:?} must be an allowed art root with no env var set"
            );
        }
    }
}
