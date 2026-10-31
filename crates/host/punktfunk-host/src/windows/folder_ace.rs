//! A folder's read ACE and its files', with no walk. icacls and `SetNamedSecurityInfoW` rewrite the
//! whole tree below a folder however little changed, so a plugin that reads only a folder's own
//! files goes through `SetFileSecurityW`, which never descends: the folder, then each file in it.
//! A file made there later inherits the folder's ACE. Never `TreeSetNamedSecurityInfoW`: it lifts
//! the protection off a child folder.

use super::registry_ace::{allowed_aces, parse_sid, read_dacl, sid_trustee};
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Mutex;
use windows::core::{Owned, HSTRING};
use windows::Win32::Foundation::HLOCAL;
use windows::Win32::Security::Authorization::{
    SetEntriesInAclW, EXPLICIT_ACCESS_W, SET_ACCESS, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    DeleteAce, EqualSid, GetSecurityDescriptorControl, InitializeSecurityDescriptor,
    SetFileSecurityW, SetSecurityDescriptorControl, SetSecurityDescriptorDacl, ACE_FLAGS, ACL,
    DACL_SECURITY_INFORMATION, NO_INHERITANCE, NO_PROPAGATE_INHERIT_ACE, OBJECT_INHERIT_ACE,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR_CONTROL,
    SE_DACL_AUTO_INHERITED, SE_DACL_PROTECTED,
};

/// `(RX)`: FILE_GENERIC_READ | FILE_GENERIC_EXECUTE.
pub(crate) const READ_MASK: u32 = 0x0012_00A9;

/// Each merge starts from the DACL it read, so two at once would drop one of the ACEs.
static MERGE: Mutex<()> = Mutex::new(());

/// Each SID may read `dir` and the files directly in it, its earlier ACE there replaced. A file
/// Windows refuses is logged and skipped.
pub(crate) fn place(dir: &Path, sids: &[&str]) -> Result<()> {
    let parsed = sids
        .iter()
        .map(|s| parse_sid(s))
        .collect::<Result<Vec<_>>>()?;
    let entries = |inherit: ACE_FLAGS| -> Vec<EXPLICIT_ACCESS_W> {
        parsed
            .iter()
            .map(|(psid, _)| EXPLICIT_ACCESS_W {
                grfAccessPermissions: READ_MASK,
                grfAccessMode: SET_ACCESS,
                grfInheritance: inherit,
                Trustee: sid_trustee(*psid),
            })
            .collect()
    };
    {
        let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
        let folder = ACE_FLAGS(OBJECT_INHERIT_ACE.0 | NO_PROPAGATE_INHERIT_ACE.0);
        merge(&dir.to_string_lossy(), &entries(folder))?;
    }
    let own = entries(NO_INHERITANCE);
    each_file(dir, |name| merge(name, &own))
}

/// Every ACE of each SID comes off `dir` and the files directly in it, inherited ones too: a file
/// made after the grant holds only that. A folder that is gone has nothing to remove.
pub(crate) fn take(dir: &Path, sids: &[&str]) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let parsed = sids
        .iter()
        .map(|s| parse_sid(s))
        .collect::<Result<Vec<_>>>()?;
    let psids: Vec<PSID> = parsed.iter().map(|(psid, _)| *psid).collect();
    {
        let _one = MERGE.lock().unwrap_or_else(|poison| poison.into_inner());
        strip(&dir.to_string_lossy(), &psids)?;
    }
    each_file(dir, |name| strip(name, &psids))
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

/// `entries` merged into `name`'s DACL, on `name` alone.
fn merge(name: &str, entries: &[EXPLICIT_ACCESS_W]) -> Result<()> {
    let (old, sd) = read_dacl(name, SE_FILE_OBJECT)?;
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
    write(name, new, &sd)
}

/// Every ACCESS_ALLOWED ACE of `sids` off `name` alone. Nothing to take writes nothing.
fn strip(name: &str, sids: &[PSID]) -> Result<()> {
    let (dacl, sd) = read_dacl(name, SE_FILE_OBJECT)?;
    // SAFETY: `sids` are live with the caller; `one` lies in the DACL `sd` keeps.
    let named = |one: PSID| {
        sids.iter()
            .copied()
            .any(|s| unsafe { EqualSid(s, one) }.is_ok())
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
    write(name, dacl, &sd)
}

/// `dacl` onto `name` alone, keeping `old`'s control bits: a write that dropped them would turn a
/// protected DACL into an inheriting one.
fn write(name: &str, dacl: *const ACL, old: &Owned<HLOCAL>) -> Result<()> {
    const KEPT: u16 = SE_DACL_AUTO_INHERITED.0 | SE_DACL_PROTECTED.0;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    let (mut bits, mut revision) = (0u16, 0u32);
    // SAFETY: `old` holds a live descriptor from GetNamedSecurityInfoW; both out-params are locals.
    unsafe { GetSecurityDescriptorControl(PSECURITY_DESCRIPTOR(old.0), &mut bits, &mut revision) }
        .context("read a descriptor's control bits")?;
    let mut sd = SECURITY_DESCRIPTOR::default();
    let psd = PSECURITY_DESCRIPTOR((&raw mut sd).cast());
    // SAFETY: `psd` points at the live local `sd`, initialized first; `dacl` is live with the
    // caller past the last call, the only one that reads it through `sd`.
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
        SetFileSecurityW(&HSTRING::from(name), DACL_SECURITY_INFORMATION, psd)
    }
    .ok()
    .with_context(|| format!("write the DACL of {name}"))
}
