//! Windows file-owner and DACL checks, asked before a directory or secret is trusted.
//!
//! `%ProgramData%` grants Users add-subdirectory + CREATOR OWNER, so anything created there
//! before the first elevated run belongs to whoever created it. These readers answer who owns
//! a path and whether anyone unprivileged can write to a directory; the callers decide what a
//! plant means for them, or [`quarantine_planted_tree`] moves a whole tree's plants aside. Ask
//! BEFORE `pf_paths::create_private_dir`: its first pass re-owns the contents and erases the
//! evidence.

#![cfg(windows)]

pub mod player;

use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, std::io::Error>;

fn other(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

/// `path` exists and an unprivileged account owns it. The two rename-aside sites that adopt
/// an Administrators-owned file from a prior install go through this. An unreadable owner
/// counts as planted: a planter can deny admins `READ_CONTROL` on their own file.
pub fn planted_by_non_admin(path: &Path) -> bool {
    path.exists() && is_admin_owned(path) != Some(true)
}

/// How many names `rename_aside` tries: `<path>.untrusted`, then `.untrusted.1` and on.
const ASIDE_NAMES: u32 = 64;

/// Rename a planted entry to `<path>.untrusted`, replacing an earlier file there. An earlier
/// directory stays, since Windows renames nothing over a non-empty directory: the entry takes
/// the next free `<path>.untrusted.N`. Best-effort by contract: the caller's guarantee is what
/// it does when the entry is still there.
pub fn rename_aside(path: &Path) -> Result<PathBuf> {
    let mut base = path.to_path_buf().into_os_string();
    base.push(".untrusted");
    let _ = std::fs::remove_file(&base);
    let mut refused = None;
    for n in 0..ASIDE_NAMES {
        let mut name = base.clone();
        if n > 0 {
            name.push(format!(".{n}"));
        }
        let aside = PathBuf::from(name);
        match std::fs::rename(path, &aside) {
            Ok(()) => return Ok(aside),
            // The name is taken: try the next. Anything else will not change with the name.
            Err(e) if std::fs::symlink_metadata(&aside).is_ok() => refused = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(refused.unwrap_or_else(|| other(format!("no free name beside {}", path.display()))))
}

/// Rename `path` aside when it is a reparse point or its owner is not SYSTEM, Administrators
/// or TrustedInstaller, an unreadable owner included. `true` when it went.
pub fn set_aside_if_planted(path: &Path) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| other(format!("read {}: {e}", path.display())))?;
    if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        && is_admin_owned(path) == Some(true)
    {
        return Ok(false);
    }
    let aside =
        rename_aside(path).map_err(|e| other(format!("rename {} aside: {e}", path.display())))?;
    tracing::warn!(
        path = %path.display(), aside = %aside.display(),
        "entry owned by a non-admin account renamed aside"
    );
    Ok(true)
}

/// Every entry below `dir` through [`set_aside_if_planted`]: a plant goes aside whole and is
/// never entered, an admin-owned directory is walked. Lock `dir` first, or what lands during
/// the walk is missed. `Err` names the entry that stayed; harden nothing it could not move.
pub fn quarantine_planted_tree(dir: &Path) -> Result<()> {
    set_aside_tree(dir, false, None)
}

/// [`quarantine_planted_tree`] that also sets aside every file, whoever owns it, but `keep`
/// directly in `dir`. For a tree whose ACL let others modify, rename or hold open what admins
/// own: there an owner proves nothing about a file, and a handle opened before the lock follows
/// its file aside.
pub fn clear_open_tree(dir: &Path, keep: &Path) -> Result<()> {
    set_aside_tree(dir, true, Some(keep))
}

/// The marker a folder holds from its lock until what it held is proven: this name, or this
/// name and `.N` when an entry already holds it. A pass that finds any has that work to do,
/// whatever the folder's ACL says by then.
pub const UNPROVEN: &str = ".pf-unproven";

/// The `n`th marker name: [`UNPROVEN`], then [`UNPROVEN`]`.1` and on.
fn marker_name(n: u32) -> String {
    if n == 0 {
        UNPROVEN.to_owned()
    } else {
        format!("{UNPROVEN}.{n}")
    }
}

/// `name` is a marker name, in any case. A marker set aside as `.untrusted` is not one.
fn is_marker(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.strip_prefix(UNPROVEN).is_some_and(|rest| {
        rest.is_empty()
            || rest
                .strip_prefix('.')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn list_names(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    std::fs::read_dir(dir)
        .and_then(|it| it.map(|e| e.map(|e| e.file_name())).collect())
        .map_err(|e| other(format!("list {}: {e}", dir.display())))
}

/// `dir` holds a marker, whoever owns it. A listing that fails counts as one.
pub fn is_unproven(dir: &Path) -> bool {
    list_names(dir).map_or(true, |names| names.iter().any(|n| is_marker(n)))
}

/// Leave a new marker in `dir`, locked just before, at the first marker name no entry holds,
/// and return it. An entry already there may have a handle from before the lock, so none is
/// taken as this pass's marker.
pub fn mark_unproven(dir: &Path) -> Result<PathBuf> {
    let taken: Vec<String> = list_names(dir)?
        .iter()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .collect();
    // A listed name is skipped, not probed: a plant pending delete refuses a create there.
    for name in (0..u32::MAX).map(marker_name) {
        if taken.contains(&name) {
            continue;
        }
        let path = dir.join(&name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(other(format!("create {}: {e}", path.display()))),
        }
    }
    Err(other(format!("no free marker name in {}", dir.display())))
}

/// Take every marker out of `dir` once its walk has passed: a file at a marker name is removed,
/// anything else there goes aside, and `ours` goes last. On `Err` `ours` is still there.
pub fn mark_proven(dir: &Path, ours: &Path) -> Result<()> {
    for name in list_names(dir)?.into_iter().filter(|n| is_marker(n)) {
        let path = dir.join(name);
        if path == ours {
            continue;
        }
        if path.is_file() {
            std::fs::remove_file(&path)
                .map_err(|e| other(format!("remove {}: {e}", path.display())))?;
        } else {
            rename_aside(&path).map_err(|e| other(format!("set {} aside: {e}", path.display())))?;
        }
    }
    std::fs::remove_file(ours).map_err(|e| other(format!("remove {}: {e}", ours.display())))
}

fn set_aside_tree(dir: &Path, files: bool, keep: Option<&Path>) -> Result<()> {
    // Listed in full first: a listing still open could hand back the name just renamed.
    let entries = std::fs::read_dir(dir)
        .and_then(|it| it.map(|e| e.map(|e| e.path())).collect::<Result<Vec<_>>>())
        .map_err(|e| other(format!("list {}: {e}", dir.display())))?;
    for entry in entries {
        if keep == Some(entry.as_path()) {
            continue;
        }
        let gone = if files && entry.is_file() {
            rename_aside(&entry)
                .map_err(|e| other(format!("rename {} aside: {e}", entry.display())))?;
            true
        } else {
            set_aside_if_planted(&entry)?
        };
        if !gone && entry.is_dir() {
            set_aside_tree(&entry, files, None)?;
        }
    }
    Ok(())
}

/// Refuse a staging directory anyone but SYSTEM/Administrators/TrustedInstaller can write.
///
/// Both: the owner is in that set (an owner always retains `WRITE_DAC` and can restore their
/// own ACE), and no allow ACE grants a write-shaped right to anyone else. `CREATOR OWNER` is
/// outside — a non-admin who created the dir under `%ProgramData%` keeps control through it.
///
/// Reads the security descriptor; `icacls` output uses localized account names.
pub fn ensure_admin_only_source(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Err(other(format!("{} is not a directory", dir.display())));
    }
    let info = SecurityInfo::read(dir, true)?;
    let privileged = privileged_sids()?;
    if info.owner_in(&privileged) != Some(true) {
        return Err(other(
            "the directory is owned by a non-administrative account, which retains WRITE_DAC \
             and can restore its own access at any time"
                .into(),
        ));
    }
    match info.foreign_writer(&privileged, false)? {
        Some(why) => Err(other(why)),
        None => Ok(()),
    }
}

/// An ACE `dir` inherits from its parent lets an account outside SYSTEM, Administrators and
/// TrustedInstaller write in it, so what it holds was open to that account. An explicit ACE is
/// the choice of whoever held `WRITE_DAC`, such as a plugin runner grant, and does not count.
/// Unreadable or missing counts as open.
pub fn inherits_open_acl(dir: &Path) -> bool {
    let read = || -> Result<bool> {
        let info = SecurityInfo::read(dir, true)?;
        Ok(info.foreign_writer(&privileged_sids()?, true)?.is_some())
    };
    read().unwrap_or(true)
}

/// No allow ACE on `path`, inherited or explicit, lets an account outside SYSTEM, Administrators
/// and TrustedInstaller write it: what a file moved out of an [`ADMIN_ONLY`] folder carries.
/// Unreadable or missing counts as not.
pub fn admin_only_acl(path: &Path) -> bool {
    let read = || -> Result<bool> {
        let info = SecurityInfo::read(path, true)?;
        Ok(info.foreign_writer(&privileged_sids()?, false)?.is_none())
    };
    read().unwrap_or(false)
}

/// Lets `sid` read the file at `path`, when it is `inside`'s own: a file, not a link, with one
/// name, whose real path lies inside `inside`. The checks and the grant act on one open handle,
/// so a link swapped in after a caller's check reaches nothing. `inside` is compared as given,
/// never resolved: pass it canonical, under a folder no less-trusted account can replace, and a
/// link swapped in for `inside` itself fails the check too. The read ACE is explicit, on that
/// file alone; granting again changes nothing.
pub fn grant_read_inside(path: &Path, inside: &Path, sid: &str) -> Result<()> {
    use std::os::windows::ffi::OsStringExt;
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
        FILE_GENERIC_READ, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode((READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is `file`'s, open for this whole frame; `info` is a live out-param.
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(win32_io)?;
    let not_a_file = (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT).0;
    if info.dwFileAttributes & not_a_file != 0 || info.nNumberOfLinks != 1 {
        return Err(other(format!(
            "{} is a folder, a link, or a file with another name",
            path.display()
        )));
    }
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `handle` is open; the call writes at most `buf.len()` units into `buf`.
    let n = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(std::io::Error::last_os_error());
    }
    let real = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
    if !real.starts_with(inside) {
        return Err(other(format!(
            "{} is not inside {}",
            real.display(),
            inside.display()
        )));
    }

    let mut trustee = PSID::default();
    // SAFETY: the `HSTRING` is NUL-terminated and outlives the call; `trustee` is a live out-param.
    unsafe { ConvertStringSidToSidW(&HSTRING::from(sid), &mut trustee) }.map_err(win32_io)?;
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
    let read = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_GENERIC_READ.0,
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
    // SAFETY: `read` names `trustee`, alive in `_trustee`; `dacl` lives in `_sd`. The new ACL
    // is LocalAlloc'd into `merged`, freed below.
    let status = unsafe { SetEntriesInAclW(Some(&[read]), Some(dacl), &mut merged) };
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

/// Owner Administrators; SYSTEM and Administrators full control, inherited below; nothing else,
/// and protected from the parent's ACL: what [`ensure_admin_only_source`] passes.
pub const ADMIN_ONLY: &str = "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// Create the directory `path` with `sddl` as its descriptor from its first instant, so nothing
/// lands in it under the parent's ACL. Fails if `path` exists.
pub fn create_dir_with(path: &Path, sddl: &str) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;
    let attributes = SecurityAttributes::new(sddl).map_err(win32_io)?;
    // SAFETY: the `HSTRING` is NUL-terminated and `attributes` owns its descriptor; both outlive
    // the call, which only reads them.
    unsafe { CreateDirectoryW(&HSTRING::from(path.as_os_str()), Some(attributes.as_ptr())) }
        .map_err(win32_io)
}

/// Give `path` [`ADMIN_ONLY`]'s owner and protected DACL in one call: no moment between the old
/// ACL and the new one is open. The DACL replaces the old one whole, explicit ACEs included, and
/// what lies below re-inherits from it; a child's explicit ACE stays. An account that cannot
/// make Administrators the owner changes nothing.
pub fn lock_admin_only(path: &Path) -> Result<()> {
    use windows::core::{BOOL, HSTRING};
    use windows::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };

    let parsed = SecurityAttributes::new(ADMIN_ONLY).map_err(win32_io)?;
    let (mut owner, mut dacl) = (PSID::default(), std::ptr::null_mut::<ACL>());
    let (mut present, mut defaulted) = (BOOL::default(), BOOL::default());
    // SAFETY: `parsed` owns a valid descriptor for this whole frame; both calls only write
    // pointers into it to live locals.
    unsafe {
        GetSecurityDescriptorOwner(parsed.descriptor(), &mut owner, &mut defaulted)
            .and_then(|()| {
                GetSecurityDescriptorDacl(
                    parsed.descriptor(),
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            })
            .map_err(win32_io)?;
    }
    // SAFETY: the `HSTRING` is NUL-terminated; `owner` and `dacl` point into `parsed`, which
    // outlives the call. The group and SACL are not passed and stay as they are.
    let status = unsafe {
        SetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl.cast_const()),
            None,
        )
    };
    if status.is_err() {
        return Err(std::io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(())
}

/// `e` as the `io::Error` std makes of the same Win32 code, so `kind()` reads true.
fn win32_io(e: windows::core::Error) -> std::io::Error {
    use windows::Win32::Foundation::WIN32_ERROR;
    match WIN32_ERROR::from_error(&e) {
        Some(code) => std::io::Error::from_raw_os_error(code.0 as i32),
        None => std::io::Error::other(e),
    }
}

/// A path's security descriptor. `owner` and `dacl` point into `sd`, the one block
/// GetNamedSecurityInfoW allocates, which Drop frees.
struct SecurityInfo {
    sd: windows::Win32::Security::PSECURITY_DESCRIPTOR,
    owner: windows::Win32::Security::PSID,
    /// Null unless read with the DACL, and null for a NULL DACL.
    dacl: *mut windows::Win32::Security::ACL,
}

impl SecurityInfo {
    fn read(path: &Path, with_dacl: bool) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let (what, label) = if with_dacl {
            (
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                "owner + DACL",
            )
        } else {
            (OWNER_SECURITY_INFORMATION, "owner")
        };
        let mut info = SecurityInfo {
            sd: Default::default(),
            owner: Default::default(),
            dacl: std::ptr::null_mut(),
        };
        // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are fields of
        // `info`, whose Drop frees the returned descriptor that `owner` and `dacl` point into.
        unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                what,
                Some(&raw mut info.owner),
                None,
                with_dacl.then_some(&raw mut info.dacl),
                None,
                &mut info.sd,
            )
        }
        .ok()
        .map_err(|e| other(format!("GetNamedSecurityInfoW({label}): {e}")))?;
        Ok(info)
    }

    /// `Some(true)` when the owner is one of `sids`, `None` when the owner is not a valid SID.
    fn owner_in(&self, sids: &[Vec<u8>]) -> Option<bool> {
        // SAFETY: `owner` is null or points into `self.sd`, which lives as long as `self`.
        unsafe { sid_in(self.owner, sids) }
    }

    /// Why someone outside `privileged` may write here: the first allow ACE with a write-shaped
    /// right for another trustee, or a NULL DACL. `None` when nobody else can. `inherited_only`
    /// looks only at ACEs that came from the parent. Read with the DACL.
    fn foreign_writer(
        &self,
        privileged: &[Vec<u8>],
        inherited_only: bool,
    ) -> Result<Option<String>> {
        use windows::Win32::Security::{
            GetAce, ACCESS_ALLOWED_ACE, ACE_HEADER, INHERITED_ACE, PSID,
        };

        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        /// Rights that replace what we are about to trust and execute. GENERIC_ALL/WRITE are
        /// checked unmapped in case an ACE stores them without mapping.
        const WRITE_MASK: u32 = 0x0000_0002 // FILE_WRITE_DATA / FILE_ADD_FILE
            | 0x0000_0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
            | 0x0000_0010 // FILE_WRITE_EA
            | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
            | 0x0000_0040 // FILE_DELETE_CHILD
            | 0x0001_0000 // DELETE
            | 0x0004_0000 // WRITE_DAC
            | 0x0008_0000 // WRITE_OWNER
            | 0x1000_0000 // GENERIC_ALL
            | 0x4000_0000; // GENERIC_WRITE

        let dacl = self.dacl;
        // A NULL DACL grants everyone everything; an absent one is not "no access".
        if dacl.is_null() {
            return Ok(Some(
                "the directory has a NULL DACL (everyone has full control)".into(),
            ));
        }
        // SAFETY: `dacl` is a valid ACL inside `self`'s descriptor; AceCount bounds the GetAce index.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..count as u32 {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: i < AceCount, and `ace` is a live out-param.
            unsafe { GetAce(dacl, i, &mut ace) }.map_err(|e| other(format!("GetAce: {e}")))?;
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue; // deny ACEs only ever subtract; audit ACEs grant nothing
            }
            if inherited_only && u32::from(header.AceFlags) & INHERITED_ACE.0 == 0 {
                continue;
            }
            // SAFETY: an allow ACE is an ACCESS_ALLOWED_ACE, whose SidStart begins the trustee SID.
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            if allowed.Mask & WRITE_MASK == 0 {
                continue; // no write-shaped right
            }
            let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut core::ffi::c_void);
            // SAFETY: `sid` is the trustee inside this ACE, inside `self`'s live descriptor.
            if unsafe { sid_in(sid, privileged) } != Some(true) {
                return Ok(Some(format!(
                    "a non-administrative trustee has write access (ACE {i}, mask {:#010x}) — \
                     anything staged here can be replaced before it is trusted or executed",
                    allowed.Mask
                )));
            }
        }
        Ok(None)
    }
}

impl Drop for SecurityInfo {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        // SAFETY: `sd` is null or the one LocalAlloc'd descriptor GetNamedSecurityInfoW returned;
        // `owner` and `dacl` die with `self`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.sd.0)));
        }
    }
}

/// `Some(true)` when `sid` equals one of `sids`, `None` when it is null or not a valid SID.
///
/// # Safety
/// `sid` is null or points at SID bytes that stay readable for the call.
unsafe fn sid_in(sid: windows::Win32::Security::PSID, sids: &[Vec<u8>]) -> Option<bool> {
    use windows::Win32::Security::{EqualSid, IsValidSid, PSID};
    // SAFETY: the caller keeps `sid` readable; IsValidSid is itself the probe.
    if sid.is_invalid() || !unsafe { IsValidSid(sid) }.as_bool() {
        return None;
    }
    Some(sids.iter().any(|p| {
        // SAFETY: `sid` passed IsValidSid; `p` is an owned, length-exact SID copy.
        unsafe { EqualSid(sid, PSID(p.as_ptr().cast_mut().cast())) }.is_ok()
    }))
}

/// SYSTEM, `BUILTIN\Administrators`, and TrustedInstaller (owns `%ProgramFiles%`).
fn privileged_sids() -> Result<Vec<Vec<u8>>> {
    use windows::core::{Owned, PCWSTR};
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{GetLengthSid, PSID};

    [
        "S-1-5-18",
        "S-1-5-32-544",
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    ]
    .iter()
    .map(|s| {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid = PSID::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; psid is a live out-param.
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid) }
            .map_err(|e| other(format!("ConvertStringSidToSidW({s}): {e}")))?;
        // SAFETY: ConvertStringSidToSidW allocates with LocalAlloc; `Owned` frees it once.
        let _psid = unsafe { Owned::new(HLOCAL(psid.0)) };
        // SAFETY: psid is a valid SID; copy it out so the caller owns plain bytes.
        let len = unsafe { GetLengthSid(psid) } as usize;
        // SAFETY: GetLengthSid just measured exactly `len` readable bytes at `psid`.
        Ok(unsafe { std::slice::from_raw_parts(psid.0 as *const u8, len) }.to_vec())
    })
    .collect()
}

/// `Some(true)` SYSTEM/Administrators/TrustedInstaller, `Some(false)` any other owner, `None`
/// if the owner cannot be read. Call before `create_private_dir` re-owns the file and erases
/// the signal. Distrusts a `host.env` / `web-password` a non-admin planted under `%ProgramData%`.
pub fn is_admin_owned(path: &Path) -> Option<bool> {
    let info = SecurityInfo::read(path, false).ok()?;
    info.owner_in(&privileged_sids().ok()?)
}

/// SID of the account this process runs as, as plain bytes.
///
/// A per-user install (`PUNKTFUNK_CONFIG_DIR` into a profile) legitimately owns its own
/// secrets; only a *foreign* unprivileged owner is a plant.
fn current_user_sid() -> Option<Vec<u8>> {
    use windows::core::Owned;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle needing no close; `token` is a live
    // out-param.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: the open succeeded, so `token` is a handle this frame alone owns.
    let token = unsafe { Owned::new(token) };
    let mut len = 0u32;
    // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and sets `len`.
    // SAFETY: the null buffer with len 0 is the documented sizing form.
    unsafe { GetTokenInformation(*token, TokenUser, None, 0, &mut len) }.ok();
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` holds exactly the `len` bytes the sizing call asked for.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: on success `buf` starts with a TOKEN_USER whose Sid points inside it. A
    // `Vec<u8>` is only byte-aligned, so the struct is read unaligned.
    let sid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
        .User
        .Sid;
    if sid.is_invalid() {
        return None;
    }
    // SAFETY: `sid` is valid; GetLengthSid measures exactly the readable bytes, which live
    // until `buf` drops.
    let n = unsafe { GetLengthSid(sid) } as usize;
    // SAFETY: `n` bytes at `sid` are readable and copied out before `buf` drops.
    Some(unsafe { std::slice::from_raw_parts(sid.0 as *const u8, n) }.to_vec())
}

/// True when `path`'s owner is the account this process runs as.
pub fn owned_by_current_user(path: &Path) -> bool {
    let Some(me) = current_user_sid() else {
        return false;
    };
    SecurityInfo::read(path, false).is_ok_and(|info| info.owner_in(&[me]) == Some(true))
}

/// True when `path` holds a secret an unprivileged account owns, and renames it aside.
///
/// `%ProgramData%` grants Users add-subdirectory + CREATOR OWNER, so a token, key or cert
/// dropped there before the first elevated run belongs to whoever dropped it. Adopting one
/// hands that account the host's own credential.
///
/// Call BEFORE `pf_paths::create_private_dir`: its first pass re-owns the contents
/// (`icacls /setowner /T`) and erases the only evidence. A `true` verdict must make the
/// caller mint a fresh secret through `write_secret_file`, which unlinks and creates new:
/// a planted file the planter still holds open fails that mint instead of receiving it.
/// An unreadable owner counts as planted.
pub fn quarantine_planted_secret(path: &Path) -> bool {
    if !path.exists() || is_admin_owned(path) == Some(true) || owned_by_current_user(path) {
        return false;
    }
    match rename_aside(path) {
        Ok(aside) => tracing::warn!(
            path = %path.display(), aside = %aside.display(),
            "secret was owned by a non-admin account (planted before install) — renamed aside; minting a fresh one"
        ),
        Err(e) => tracing::error!(
            error = %e, path = %path.display(),
            "secret is non-admin-owned and could not be renamed aside — refusing to adopt it"
        ),
    }
    true
}

/// A `SECURITY_ATTRIBUTES` parsed from SDDL, owning the descriptor it points at.
///
/// The conversion `LocalAlloc`s the descriptor and `Drop` frees it, so the attributes cannot
/// outlive this value: [`as_ptr`](Self::as_ptr) only lends them. An object copies its
/// descriptor when it is created, so dropping this once the create call has returned is
/// sound. `bInheritHandle` is false. Moving is fine: the pointer targets the heap, not a field.
pub struct SecurityAttributes {
    attributes: windows::Win32::Security::SECURITY_ATTRIBUTES,
    descriptor: windows::Win32::Security::PSECURITY_DESCRIPTOR,
}

impl SecurityAttributes {
    /// `sddl` with a NUL in it is refused: the conversion would read it as ending there, and a
    /// descriptor cut short is still a valid one.
    pub fn new(sddl: &str) -> windows::core::Result<Self> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::E_INVALIDARG;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        if sddl.contains('\0') {
            return Err(E_INVALIDARG.into());
        }
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the `HSTRING` is NUL-terminated and outlives the call; `descriptor` is a
        // live out-parameter, written only on success, with the one block `drop` frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            },
            descriptor,
        })
    }

    /// A create call's `lpSecurityAttributes`. Valid while `self` lives.
    pub fn as_ptr(&self) -> *const windows::Win32::Security::SECURITY_ATTRIBUTES {
        &self.attributes
    }

    /// The descriptor alone, for a call that takes one without the attributes around it.
    pub fn descriptor(&self) -> windows::Win32::Security::PSECURITY_DESCRIPTOR {
        self.descriptor
    }
}

impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        // SAFETY: `descriptor` is the one block the conversion allocated; nothing else owns
        // it, and `as_ptr` and `descriptor` only lend it for as long as `self` lives.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

/// A job object whose last handle closing kills every process still in it. `extra` adds
/// limits to `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; `JOB_OBJECT_LIMIT(0)` adds none.
///
/// Unnamed, default security. The handle is owned before the limits are set, so a failure
/// there closes the job instead of leaking one that kills nothing.
pub fn kill_on_close_job(
    extra: windows::Win32::System::JobObjects::JOB_OBJECT_LIMIT,
) -> windows::core::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: null attributes and a null name ask for a fresh unnamed job, default security.
    let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
    // SAFETY: the handle just created, owned nowhere else.
    let job = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | extra;
    // SAFETY: a live job; `info` is the type this class expects, passed with its own size,
    // and the kernel copies it before the call returns.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }?;
    Ok(job)
}

#[cfg(test)]
mod rename_aside_tests {
    /// A directory set aside twice keeps both: the second takes the next name instead of
    /// failing to replace the first, and nothing stays at the original path.
    #[test]
    fn a_second_planted_directory_takes_the_next_name() {
        let root = std::env::temp_dir().join(format!("pf-rename-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let planted = root.join("data");
        for round in ["first", "second"] {
            std::fs::create_dir_all(&planted).unwrap();
            std::fs::write(planted.join(round), "x").unwrap();
            super::rename_aside(&planted).unwrap();
            assert!(!planted.exists(), "{round} stayed at the trusted path");
        }
        assert!(root.join("data.untrusted").join("first").exists());
        assert!(root.join("data.untrusted.1").join("second").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file still replaces an earlier file at the first name.
    #[test]
    fn a_planted_file_replaces_an_earlier_one() {
        let root = std::env::temp_dir().join(format!("pf-rename-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let planted = root.join("host.env");
        for body in ["old", "new"] {
            std::fs::write(&planted, body).unwrap();
            super::rename_aside(&planted).unwrap();
        }
        let kept = std::fs::read_to_string(root.join("host.env.untrusted")).unwrap();
        assert_eq!(kept, "new");
        assert!(!root.join("host.env.untrusted.1").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod planted_tree_tests {
    /// A folder this process made moves aside whole when a non-admin owns it (an unelevated
    /// run) and stays, walked, when an admin does (an elevated one).
    #[test]
    fn a_folder_moves_aside_exactly_when_a_non_admin_owns_it() {
        let root = std::env::temp_dir().join(format!("pf-planted-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("host.env"), "x").unwrap();
        let planted = super::is_admin_owned(&root.join("sub")) != Some(true);
        super::quarantine_planted_tree(&root).unwrap();
        assert_eq!(root.join("sub").join("host.env").exists(), !planted);
        assert_eq!(
            root.join("sub.untrusted").join("host.env").exists(),
            planted
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An open tree keeps its marker and its folders, those only when an admin owns them: every
    /// other file goes aside whoever owns it, and a junction without its target being entered.
    #[test]
    fn an_open_tree_keeps_no_file_and_never_follows_a_junction() {
        let tag = format!("pf-open-tree-{}", std::process::id());
        let (root, target) = (
            std::env::temp_dir().join(&tag),
            std::env::temp_dir().join(format!("{tag}-target")),
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(root.join("version.dll"), "x").unwrap();
        std::fs::write(root.join("sub").join("layer.dll"), "x").unwrap();
        std::fs::write(target.join("kept.dll"), "x").unwrap();
        let link = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(link.success(), "mklink /J must work for a standard user");
        let admin = super::is_admin_owned(&root.join("sub")) == Some(true);
        let marker = super::mark_unproven(&root).unwrap();

        super::clear_open_tree(&root, &marker).unwrap();
        assert!(marker.is_file(), "the marker went aside");
        assert!(!root.join("version.dll").exists());
        assert!(root.join("version.dll.untrusted").exists());
        assert!(!root.join("link").exists());
        assert!(
            target.join("kept.dll").exists(),
            "the walk entered the junction"
        );
        assert_eq!(root.join("sub").is_dir(), admin);
        let layer = if admin {
            root.join("sub").join("layer.dll.untrusted")
        } else {
            root.join("sub.untrusted").join("layer.dll")
        };
        assert!(layer.exists(), "{}", layer.display());
        std::fs::remove_dir(root.join("link.untrusted")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target);
    }
}

#[cfg(test)]
mod marker_tests {
    /// Only a name `mark_unproven` may take counts, in any case; a marker set aside does not.
    #[test]
    fn only_marker_names_count_as_unproven() {
        for name in [".pf-unproven", ".pf-unproven.1", ".PF-Unproven.12"] {
            assert!(super::is_marker(name.as_ref()), "{name}");
        }
        for name in [
            ".pf-unproven.untrusted",
            ".pf-unproven.1.untrusted",
            ".pf-unproven.",
            ".pf-unprovenx",
            "pf-unproven",
        ] {
            assert!(!super::is_marker(name.as_ref()), "{name}");
        }
    }
}

#[cfg(test)]
mod open_acl_tests {
    /// A write grant inherited from the parent reads as open; the same grant made explicit on
    /// the folder does not, though a staging check still refuses it.
    #[test]
    fn only_an_inherited_grant_reads_as_open() {
        let icacls = |path: &std::path::Path, args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let base = std::env::temp_dir().join(format!("pf-open-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("child")).unwrap();
        assert!(icacls(&base, &["/grant", "*S-1-5-32-545:(OI)(CI)(M)"]));
        assert!(super::inherits_open_acl(&base.join("child")));
        assert!(super::inherits_open_acl(&base.join("missing")));
        // Copies every inherited ACE onto the folder as an explicit one.
        assert!(icacls(&base.join("child"), &["/inheritance:d"]));
        assert!(!super::inherits_open_acl(&base.join("child")));
        assert!(super::ensure_admin_only_source(&base.join("child")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file only SYSTEM may write reads as admin-only; a Users grant, even explicit, opens it.
    #[test]
    fn only_a_file_no_one_else_may_write_reads_admin_only() {
        let icacls = |path: &std::path::Path, args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let base = std::env::temp_dir().join(format!("pf-admin-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("core.dll");
        std::fs::write(&file, b"").unwrap();
        let grant = [
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(F)",
            "*S-1-5-32-545:(M)",
        ];
        assert!(icacls(&file, &grant));
        assert!(!super::admin_only_acl(&file));
        assert!(icacls(&file, &["/remove:g", "*S-1-5-32-545"]));
        assert!(super::admin_only_acl(&file));
        assert!(!super::admin_only_acl(&base.join("missing")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// One file inside gets an explicit read ACE; a second name, a file outside and a folder
    /// get nothing.
    #[test]
    fn only_a_file_with_one_name_inside_is_opened_to_a_reader() {
        let base = std::env::temp_dir().join(format!("pf-grant-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let own = base.join("own");
        std::fs::create_dir_all(&own).unwrap();
        let own = own.canonicalize().unwrap();
        let file = own.join("PS3UPDAT.PUP");
        std::fs::write(&file, b"fw").unwrap();
        std::fs::write(base.join("outside.bin"), b"x").unwrap();

        super::grant_read_inside(&file, &own, "S-1-5-32-545").unwrap();
        super::grant_read_inside(&file, &own, "S-1-5-32-545").unwrap();
        let saved = base.join("acl");
        let ok = std::process::Command::new("icacls")
            .arg(&file)
            .arg("/save")
            .arg(&saved)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let raw = std::fs::read(&saved).unwrap();
        let wide: Vec<u16> = raw
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let sddl = String::from_utf16_lossy(&wide);
        // FILE_GENERIC_READ is `FR` in SDDL; the inherited ACEs stay as they were.
        assert_eq!(sddl.matches("(A;;FR;;;BU)").count(), 1, "{sddl}");
        assert!(sddl.contains("(A;ID;FA;;;SY)"), "{sddl}");

        assert!(super::grant_read_inside(&base.join("outside.bin"), &own, "S-1-5-32-545").is_err());
        assert!(super::grant_read_inside(&own, &own, "S-1-5-32-545").is_err());
        std::fs::hard_link(&file, own.join("second-name")).unwrap();
        assert!(super::grant_read_inside(&file, &own, "S-1-5-32-545").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The state folder swapped for a junction after the caller resolved it: the file the
    /// junction reaches gets nothing.
    #[test]
    fn a_state_folder_swapped_for_a_junction_is_refused() {
        let base = std::env::temp_dir().join(format!("pf-grant-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("secret"), b"s").unwrap();
        let own = base.canonicalize().unwrap().join("plugin");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(base.join("plugin"))
            .arg(&elsewhere)
            .status()
            .is_ok_and(|s| s.success());
        assert!(made);

        assert!(super::grant_read_inside(&own.join("secret"), &own, "S-1-5-32-545").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod security_attributes_tests {
    use super::SecurityAttributes;

    /// The attributes point at the descriptor the SDDL parsed to; bad SDDL is an error.
    #[test]
    fn sddl_parses_into_attributes_that_own_their_descriptor() {
        let sa = SecurityAttributes::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)").expect("valid SDDL");
        // SAFETY: `as_ptr` lends the attributes while `sa` lives.
        let attributes = unsafe { &*sa.as_ptr() };
        assert_eq!(attributes.lpSecurityDescriptor, sa.descriptor().0);
        assert!(!sa.descriptor().0.is_null());
        assert!(!attributes.bInheritHandle.as_bool());
        assert!(SecurityAttributes::new("not sddl").is_err());
        assert!(SecurityAttributes::new("D:P(A;;GA;;;SY)\0(A;;GA;;;WD)").is_err());
    }

    /// Closing the job's last handle ends a process assigned to it.
    #[test]
    fn closing_the_job_kills_what_is_in_it() {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::{AssignProcessToJobObject, JOB_OBJECT_LIMIT};

        let job = super::kill_on_close_job(JOB_OBJECT_LIMIT(0)).expect("a job");
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn cmd");
        // SAFETY: both handles are live and owned by this test for the call.
        unsafe {
            AssignProcessToJobObject(HANDLE(job.as_raw_handle()), HANDLE(child.as_raw_handle()))
        }
        .expect("assign");
        assert!(
            child.try_wait().expect("poll").is_none(),
            "still waiting on stdin"
        );
        drop(job);
        // The kill is asynchronous. Its exit code is the kernel's, so only the exit is asserted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().expect("poll").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the job's close left its process running"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
