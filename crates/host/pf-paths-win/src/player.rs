//! Rights the SYSTEM host gives the session's player on one file, in a folder that player
//! controls. Nothing here follows a path the player can bend: the checks and the change act on
//! one handle.

use std::path::Path;

use windows::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};

/// What a process needs of a DLL it loads: read, and execute to map it as an image.
pub const READ_EXECUTE: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;

/// Gives `sid` `access` on the file at `path`, when the file is what `path` names: opened
/// without following a link, with one name, and its real path `path` itself, so a junction or
/// link anywhere on the way reaches nothing. The ACE is explicit, on that file alone; granting
/// again changes nothing.
pub fn grant_on_file(path: &Path, sid: &str, access: u32) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::core::{Owned, HSTRING, PWSTR};
    use windows::Win32::Foundation::{HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID,
    };
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    let refused = |why: &str| std::io::Error::other(format!("{}: {why}", path.display()));
    let file = std::fs::OpenOptions::new()
        .access_mode((READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is `file`'s, open for this whole frame; `info` is a live out-param.
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(std::io::Error::other)?;
    let not_a_file = (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT).0;
    if info.dwFileAttributes & not_a_file != 0 || info.nNumberOfLinks != 1 {
        return Err(refused("a folder, a link, or a file with another name"));
    }
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `handle` is open; the call writes at most `buf.len()` units into `buf`.
    let n = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(std::io::Error::last_os_error());
    }
    if !names(&String::from_utf16_lossy(&buf[..n]), path) {
        return Err(refused("reached through a link"));
    }

    let mut trustee = PSID::default();
    // SAFETY: the `HSTRING` is NUL-terminated and outlives the call; `trustee` is a live out-param.
    unsafe { ConvertStringSidToSidW(&HSTRING::from(sid), &mut trustee) }
        .map_err(std::io::Error::other)?;
    // SAFETY: ConvertStringSidToSidW allocates with LocalAlloc; `Owned` frees it once.
    let _trustee = unsafe { Owned::new(HLOCAL(trustee.0)) };
    let (mut sd, mut dacl) = (PSECURITY_DESCRIPTOR::default(), std::ptr::null_mut::<ACL>());
    // SAFETY: `handle` was opened with READ_CONTROL; `dacl` points into `sd`, freed below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut sd),
        )
    };
    if status.is_err() {
        return Err(std::io::Error::from_raw_os_error(status.0 as i32));
    }
    // SAFETY: GetSecurityInfo returned one LocalAlloc'd descriptor; `Owned` frees it once.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    let grant = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: PWSTR(trustee.0.cast()),
        },
    };
    let mut merged = std::ptr::null_mut::<ACL>();
    // SAFETY: `grant` names `trustee`, alive in `_trustee`; `dacl` lives in `_sd`. The new ACL
    // is LocalAlloc'd into `merged`, freed below.
    let status = unsafe { SetEntriesInAclW(Some(&[grant]), Some(dacl.cast_const()), &mut merged) };
    if status.is_err() {
        return Err(std::io::Error::from_raw_os_error(status.0 as i32));
    }
    // SAFETY: SetEntriesInAclW allocated `merged` with LocalAlloc; `Owned` frees it once.
    let _merged = unsafe { Owned::new(HLOCAL(merged.cast())) };
    // SAFETY: `handle` was opened with WRITE_DAC; `merged` is a valid ACL alive in `_merged`.
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(merged.cast_const()),
            None,
        )
    };
    if status.is_err() {
        return Err(std::io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(())
}

/// `real`, a path as GetFinalPathNameByHandleW spells it, is `path`: the same letters in any
/// case once each loses its `\\?\` prefix.
fn names(real: &str, path: &Path) -> bool {
    fn plain(p: &str) -> String {
        let p = p.replace('/', "\\");
        let p = match p.strip_prefix(r"\\?\UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => p.strip_prefix(r"\\?\").unwrap_or(&p).to_string(),
        };
        p.to_lowercase()
    }
    plain(real) == plain(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_final_path_names_the_path_whatever_its_prefix_and_case() {
        let path =
            Path::new(r"C:\Users\Player\AppData\Roaming\RetroArch\cores\snes9x_libretro.dll");
        let real = r"\\?\C:\Users\player\AppData\Roaming\RetroArch\cores\snes9x_libretro.dll";
        assert!(names(real, path));
        assert!(names(
            r"\\?\UNC\nas\share\a.dll",
            Path::new(r"\\nas\share\a.dll")
        ));
        assert!(!names(r"\\?\C:\ProgramData\punktfunk\native-key", path));
        assert!(!names(
            r"\\?\D:\Users\Player\AppData\Roaming\RetroArch\cores\snes9x_libretro.dll",
            path
        ));
    }

    /// The explicit allow ACEs `icacls /save` lists for `file`, as SDDL strings.
    fn explicit_allows(file: &Path, saved: &Path) -> Vec<String> {
        let ok = std::process::Command::new("icacls")
            .arg(file)
            .arg("/save")
            .arg(saved)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "icacls /save {}", file.display());
        // `/save` writes the file's name and its SDDL as UTF-16LE.
        let raw = std::fs::read(saved).unwrap();
        let wide: Vec<u16> = raw
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        String::from_utf16_lossy(&wide)
            .split(')')
            .filter_map(|ace| ace.split_once('(').map(|(_, a)| a.to_string()))
            .filter(|ace| ace.starts_with("A;;"))
            .collect()
    }

    /// The file named by its own path gets one read-and-execute ACE for the trustee, however
    /// often it is granted.
    #[test]
    fn a_file_reached_by_its_own_name_gets_one_explicit_ace() {
        let base = tempfile_dir("grant");
        let file = base.join("snes9x_libretro.dll");
        std::fs::write(&file, b"core").unwrap();

        grant_on_file(&file, "S-1-5-32-545", READ_EXECUTE).unwrap();
        grant_on_file(&file, "S-1-5-32-545", READ_EXECUTE).unwrap();

        let users: Vec<String> = explicit_allows(&file, &base.join("acl"))
            .into_iter()
            .filter(|ace| ace.ends_with(";;;BU"))
            .collect();
        assert_eq!(users.len(), 1, "{users:?}");
        let rights = users[0].split(';').nth(2).unwrap_or_default();
        assert!(["FRFX", "FXFR", "0x1200a9"].contains(&rights), "{users:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A junction on the way, a second name and a folder get nothing.
    #[test]
    fn a_junction_a_second_name_or_a_folder_gets_nothing() {
        let base = tempfile_dir("refuse");
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let file = target.join("snes9x_libretro.dll");
        std::fs::write(&file, b"core").unwrap();
        let junction = base.join("cores");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .is_ok_and(|s| s.success());
        assert!(made, "mklink /J");

        let through = junction.join("snes9x_libretro.dll");
        assert!(grant_on_file(&through, "S-1-5-32-545", READ_EXECUTE).is_err());
        assert!(grant_on_file(&target, "S-1-5-32-545", READ_EXECUTE).is_err());
        std::fs::hard_link(&file, target.join("second-name.dll")).unwrap();
        assert!(grant_on_file(&file, "S-1-5-32-545", READ_EXECUTE).is_err());
        let none = explicit_allows(&file, &base.join("acl"));
        assert!(none.iter().all(|ace| !ace.ends_with(";;;BU")), "{none:?}");
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("pf-player-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // The path as the OS spells it, so a short `TEMP` name doesn't read as a link.
        dunce(std::fs::canonicalize(&base).unwrap())
    }

    /// `p` without the `\\?\` prefix `canonicalize` adds.
    fn dunce(p: std::path::PathBuf) -> std::path::PathBuf {
        let s = p.to_string_lossy();
        s.strip_prefix(r"\\?\")
            .map_or_else(|| p.clone(), std::path::PathBuf::from)
    }
}
