//! A folder's read ACE and its files', with no walk. icacls and `SetNamedSecurityInfoW` rewrite the
//! whole tree below a folder however little changed, so a plugin that reads only a folder's own
//! files goes through `SetFileSecurityW`, which never descends: the folder, then each file in it.
//! A file made there later inherits the folder's ACE. A junction or symbolic link gets its ACE
//! through a handle opened on the link itself: a DACL set by name lands on the target. Never
//! `TreeSetNamedSecurityInfoW`: it lifts the protection off a child folder.

use super::registry_ace::{allowed_aces, parse_sid, read_dacl, rights_in, sid_trustee};
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Mutex;
use windows::core::{Owned, HSTRING};
use windows::Win32::Foundation::{HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    GetSecurityInfo, SetEntriesInAclW, EXPLICIT_ACCESS_W, SET_ACCESS, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    DeleteAce, EqualSid, GetSecurityDescriptorControl, InitializeSecurityDescriptor,
    SetFileSecurityW, SetKernelObjectSecurity, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
    NO_PROPAGATE_INHERIT_ACE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_DESCRIPTOR,
    SECURITY_DESCRIPTOR_CONTROL, SE_DACL_AUTO_INHERITED, SE_DACL_PROTECTED,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};

/// `(RX)`: FILE_GENERIC_READ | FILE_GENERIC_EXECUTE.
pub(crate) const READ_MASK: u32 = 0x0012_00A9;

/// Each merge starts from the DACL it read, so two at once would drop one of the ACEs.
static MERGE: Mutex<()> = Mutex::new(());

/// A parsed SID, allocated until its guard drops.
type Sid = (PSID, Owned<HLOCAL>);

/// Each SID may read `dir` and the files directly in it, its earlier ACE there replaced. A file
/// Windows refuses is logged and skipped.
pub(crate) fn place(dir: &Path, sids: &[&str]) -> Result<()> {
    let parsed = parse(sids)?;
    {
        let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
        let folder = ACE_FLAGS(OBJECT_INHERIT_ACE.0 | NO_PROPAGATE_INHERIT_ACE.0);
        merge(
            &Object::Named(&dir.to_string_lossy()),
            &entries(&parsed, folder),
        )?;
    }
    let own = entries(&parsed, NO_INHERITANCE);
    each_file(dir, |name| merge(&Object::Named(name), &own))
}

/// Each SID may read `link` itself: the junction or symbolic link object, nothing it points at.
/// A link has nothing below it, so the ACE inherits nowhere.
pub(crate) fn place_link(link: &Path, sids: &[&str]) -> Result<()> {
    let parsed = parse(sids)?;
    let handle = open_link(link, true)?;
    let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
    merge(&Object::Link(*handle), &entries(&parsed, NO_INHERITANCE))
}

/// Every ACE of each SID comes off `dir` and the files directly in it, inherited ones too: a file
/// made after the grant holds only that. A folder that is gone has nothing to remove.
pub(crate) fn take(dir: &Path, sids: &[&str]) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let parsed = parse(sids)?;
    let psids = psids(&parsed);
    {
        let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
        strip(&Object::Named(&dir.to_string_lossy()), &psids)?;
    }
    each_file(dir, |name| strip(&Object::Named(name), &psids))
}

/// Every ACE of each SID comes off `link` itself. A link that is gone has nothing to remove.
pub(crate) fn take_link(link: &Path, sids: &[&str]) -> Result<()> {
    if std::fs::symlink_metadata(link).is_err() {
        return Ok(());
    }
    let parsed = parse(sids)?;
    let handle = open_link(link, true)?;
    let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
    strip(&Object::Link(*handle), &psids(&parsed))
}

/// The access mask `link`'s own DACL gives `sid`, the link not followed.
pub(crate) fn link_rights(link: &Path, sid: &str) -> Result<u32> {
    let handle = open_link(link, false)?;
    let (dacl, _sd) = Object::Link(*handle).dacl()?;
    rights_in(dacl, sid)
}

fn parse(sids: &[&str]) -> Result<Vec<Sid>> {
    sids.iter().map(|s| parse_sid(s)).collect()
}

fn psids(parsed: &[Sid]) -> Vec<PSID> {
    parsed.iter().map(|(psid, _)| *psid).collect()
}

/// One read ACE per SID with `inherit`'s flags.
fn entries(parsed: &[Sid], inherit: ACE_FLAGS) -> Vec<EXPLICIT_ACCESS_W> {
    parsed
        .iter()
        .map(|(psid, _)| EXPLICIT_ACCESS_W {
            grfAccessPermissions: READ_MASK,
            grfAccessMode: SET_ACCESS,
            grfInheritance: inherit,
            Trustee: sid_trustee(*psid),
        })
        .collect()
}

/// `link` opened as itself, never followed, for its DACL: to change it with `write`, else to read.
fn open_link(link: &Path, write: bool) -> Result<Owned<HANDLE>> {
    let name = HSTRING::from(link.as_os_str());
    let access = if write {
        READ_CONTROL | WRITE_DAC
    } else {
        READ_CONTROL
    };
    // SAFETY: `name` is a NUL-terminated local that outlives the call; the handle that comes
    // back is this frame's alone, and `Owned` closes it once.
    let handle = unsafe {
        CreateFileW(
            &name,
            access.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .map(|handle| Owned::new(handle))
    }
    .with_context(|| format!("open the link {}", link.display()))?;
    Ok(handle)
}

fn each_file(dir: &Path, act: impl Fn(&str) -> Result<()>) -> Result<()> {
    let (mut refused, mut first) = (0u32, None);
    let listed = std::fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))?;
    for entry in listed.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if let Err(e) = act(&entry.path().to_string_lossy()) {
            refused += 1;
            first.get_or_insert(e);
        }
    }
    if let Some(e) = first {
        tracing::warn!(
            dir = %dir.display(),
            refused,
            error = %format!("{e:#}"),
            "folder files kept their old access"
        );
    }
    Ok(())
}

/// Where one object's DACL is read and written: by name, which follows a link, or through a
/// handle opened on the link itself.
enum Object<'a> {
    Named(&'a str),
    Link(HANDLE),
}

impl Object<'_> {
    /// This object's DACL, which stays valid until the returned guard frees its descriptor.
    fn dacl(&self) -> Result<(*mut ACL, Owned<HLOCAL>)> {
        let handle = match self {
            Object::Named(name) => return read_dacl(name, SE_FILE_OBJECT),
            Object::Link(handle) => *handle,
        };
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a live handle; both out-params are live locals, and `sd` owns the descriptor
        // `dacl` points into.
        let read = unsafe {
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
        // SAFETY: `sd` is null or the one LocalAlloc'd descriptor the call returned; `Owned`
        // frees it once, after the caller's last use of `dacl`.
        let owned = unsafe { Owned::new(HLOCAL(sd.0)) };
        read.ok().context("read the DACL of a link")?;
        Ok((dacl, owned))
    }

    /// `dacl` onto this object alone, keeping `old`'s control bits: a write that dropped them
    /// would turn a protected DACL into an inheriting one.
    fn put(&self, dacl: *const ACL, old: &Owned<HLOCAL>) -> Result<()> {
        const KEPT: u16 = SE_DACL_AUTO_INHERITED.0 | SE_DACL_PROTECTED.0;
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
        let (mut bits, mut revision) = (0u16, 0u32);
        // SAFETY: `old` holds a live descriptor; both out-params are locals.
        unsafe {
            GetSecurityDescriptorControl(PSECURITY_DESCRIPTOR(old.0), &mut bits, &mut revision)
        }
        .context("read a descriptor's control bits")?;
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = PSECURITY_DESCRIPTOR((&raw mut sd).cast());
        let (name, handle) = match self {
            Object::Named(name) => (Some(HSTRING::from(*name)), HANDLE::default()),
            Object::Link(handle) => (None, *handle),
        };
        // SAFETY: `psd` points at the live local `sd`, initialized first; `dacl` is live with the
        // caller past the last call, the only one that reads it through `sd`; `handle` is live
        // with the caller when it names the object.
        unsafe {
            InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION)
                .context("InitializeSecurityDescriptor")?;
            SetSecurityDescriptorDacl(psd, true, Some(dacl), false)
                .context("SetSecurityDescriptorDacl")?;
            SetSecurityDescriptorControl(
                psd,
                SECURITY_DESCRIPTOR_CONTROL(KEPT),
                SECURITY_DESCRIPTOR_CONTROL(bits & KEPT),
            )
            .context("SetSecurityDescriptorControl")?;
            match &name {
                Some(name) => SetFileSecurityW(name, DACL_SECURITY_INFORMATION, psd).ok(),
                None => SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, psd),
            }
        }
        .with_context(|| match self {
            Object::Named(name) => format!("write the DACL of {name}"),
            Object::Link(_) => "write the DACL of a link".to_string(),
        })
    }
}

/// `entries` merged into the object's DACL, on it alone.
fn merge(object: &Object, entries: &[EXPLICIT_ACCESS_W]) -> Result<()> {
    let (old, sd) = object.dacl()?;
    let mut new: *mut ACL = std::ptr::null_mut();
    // SAFETY: each entry's SID stays allocated with the caller; `old` is null or the DACL `sd`
    // keeps allocated.
    unsafe {
        SetEntriesInAclW(
            Some(entries),
            (!old.is_null()).then_some(old.cast_const()),
            &mut new,
        )
    }
    .ok()
    .context("SetEntriesInAclW")?;
    // SAFETY: `new` is the ACL SetEntriesInAclW just allocated; `Owned` frees it once.
    let _new = unsafe { Owned::new(HLOCAL(new.cast())) };
    object.put(new, &sd)
}

/// Every ACCESS_ALLOWED ACE of `sids` off the object alone. Nothing to take writes nothing.
fn strip(object: &Object, sids: &[PSID]) -> Result<()> {
    let (dacl, sd) = object.dacl()?;
    let named = |one: PSID| {
        sids.iter().copied().any(|s| {
            // SAFETY: `sids` are live with the caller; `one` lies in the DACL `sd` keeps.
            unsafe { EqualSid(s, one) }.is_ok()
        })
    };
    let held: Vec<u32> = allowed_aces(dacl)?
        .iter()
        .filter(|ace| named(ace.sid))
        .map(|ace| ace.index)
        .collect();
    if held.is_empty() {
        return Ok(());
    }
    for index in held.into_iter().rev() {
        // SAFETY: `dacl` is the writable ACL `sd` owns. Highest index first, so each one still
        // names its ACE.
        unsafe { DeleteAce(dacl, index) }.context("DeleteAce")?;
    }
    object.put(dacl, &sd)
}
