//! ACEs for one SID on an object named as `SetNamedSecurityInfoW` spells it: `MACHINE\SOFTWARE\…`
//! for `HKLM`, a path for a folder. A registry key gets a read ACE merged into its DACL, or every
//! ACE the SID holds there dropped; any object answers the rights its DACL gives the SID. Works
//! as SYSTEM, which owns the keys.

use anyhow::{Context, Result};
use windows::core::{Owned, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    ACCESS_MODE, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_OBJECT_TYPE, SE_REGISTRY_KEY, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, ACCESS_ALLOWED_ACE, ACL,
    ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE,
    PSECURITY_DESCRIPTOR, PSID,
};
use windows::Win32::System::Registry::KEY_READ;

/// `sid` (`S-1-…`) may read `key` and every key under it.
pub(crate) fn grant_read(key: &str, sid: &str) -> Result<()> {
    edit(key, sid, GRANT_ACCESS)
}

/// The edit failed because the key does not exist: its launcher is not installed yet.
pub(crate) fn is_absent(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<windows::core::Error>()
            .is_some_and(|w| w.code() == ERROR_FILE_NOT_FOUND.to_hresult())
    })
}

/// Every ACE `sid` holds on `key` goes.
pub(crate) fn revoke(key: &str, sid: &str) -> Result<()> {
    edit(key, sid, REVOKE_ACCESS)
}

/// The access mask `name`'s DACL gives `sid` by name, inherited ACEs included. Reads one descriptor;
/// nothing below `name` is touched. A group the SID may sit in doesn't count: an AppContainer needs
/// its capability's own ACE, and a `Users:(F)` beside it doesn't stand in for that.
pub(crate) fn rights(name: &str, object: SE_OBJECT_TYPE, sid: &str) -> Result<u32> {
    let (dacl, _sd) = read_dacl(name, object)?;
    rights_in(dacl, sid)
}

/// [`rights`] over a DACL already read, which stays allocated for the call.
pub(super) fn rights_in(dacl: *const ACL, sid: &str) -> Result<u32> {
    let (psid, _psid) = parse_sid(sid)?;
    let mut mask = 0u32;
    for ace in allowed_aces(dacl)? {
        // SAFETY: `_psid` keeps `psid` allocated; `ace.sid` lies in the DACL the caller keeps.
        if !ace.inherit_only && unsafe { EqualSid(psid, ace.sid) }.is_ok() {
            mask |= ace.mask;
        }
    }
    Ok(mask)
}

/// One ACCESS_ALLOWED ACE of a DACL. `sid` points into that DACL.
pub(super) struct AllowedAce {
    pub index: u32,
    pub inherit_only: bool,
    pub mask: u32,
    pub sid: PSID,
}

/// The ACCESS_ALLOWED ACEs of `dacl`, null for none.
pub(super) fn allowed_aces(dacl: *const ACL) -> Result<Vec<AllowedAce>> {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    if dacl.is_null() {
        return Ok(Vec::new());
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is a live ACL; `size` is a live out-param of the length passed.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .context("GetAclInformation")?;
    let mut out = Vec::new();
    for index in 0..size.AceCount {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `index` is below the ACE count; `ace` is a live out-param.
        unsafe { GetAce(dacl, index, &mut ace) }.context("GetAce")?;
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: `GetAce` points into the live DACL, and any ACE spans this header and mask. The
        // mask means what it says only on an allowed ACE, checked before it is read.
        let head = unsafe { &*ace };
        if head.Header.AceType == ACCESS_ALLOWED_ACE_TYPE {
            let sid = ace.wrapping_byte_add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart));
            out.push(AllowedAce {
                index,
                inherit_only: u32::from(head.Header.AceFlags) & INHERIT_ONLY_ACE.0 != 0,
                mask: head.Mask,
                sid: PSID(sid.cast()),
            });
        }
    }
    Ok(out)
}

/// The SID `S-1-…` names, allocated until the returned guard drops.
pub(super) fn parse_sid(sid: &str) -> Result<(PSID, Owned<HLOCAL>)> {
    let sid_w: Vec<u16> = sid.encode_utf16().chain(std::iter::once(0)).collect();
    let mut psid = PSID::default();
    // SAFETY: `sid_w` is NUL-terminated and outlives the call; `psid` is a live out-param that
    // receives a LocalAlloc'd SID, which the returned `Owned` frees.
    unsafe { ConvertStringSidToSidW(PCWSTR(sid_w.as_ptr()), &mut psid) }
        .with_context(|| format!("parse SID {sid}"))?;
    // SAFETY: `psid` is the LocalAlloc'd SID the call just returned; `Owned` frees it once.
    let owned = unsafe { Owned::new(HLOCAL(psid.0)) };
    Ok((psid, owned))
}

/// `name`'s DACL, which stays valid until the returned guard frees its descriptor.
pub(super) fn read_dacl(name: &str, object: SE_OBJECT_TYPE) -> Result<(*mut ACL, Owned<HLOCAL>)> {
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `name_w` is NUL-terminated; both out-params are live locals, and `sd` owns the
    // descriptor `dacl` points into.
    let read = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name_w.as_ptr()),
            object,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    // SAFETY: `sd` is null or the one LocalAlloc'd descriptor the call returned; `Owned` frees
    // it once, after the caller's last use of `dacl`.
    let owned = unsafe { Owned::new(HLOCAL(sd.0)) };
    read.ok()
        .with_context(|| format!("read the DACL of {name}"))?;
    Ok((dacl, owned))
}

pub(super) fn sid_trustee(psid: PSID) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: PWSTR(psid.0.cast()),
    }
}

fn edit(key: &str, sid: &str, mode: ACCESS_MODE) -> Result<()> {
    let (psid, _psid) = parse_sid(sid)?;
    let (old_dacl, _sd) = read_dacl(key, SE_REGISTRY_KEY)?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: KEY_READ.0,
        grfAccessMode: mode,
        grfInheritance: CONTAINER_INHERIT_ACE,
        Trustee: sid_trustee(psid),
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: one entry whose SID stays allocated for the call; `old_dacl` is null or the DACL
    // `_sd` still owns.
    unsafe {
        SetEntriesInAclW(
            Some(&[entry]),
            (!old_dacl.is_null()).then_some(old_dacl.cast_const()),
            &mut new_dacl,
        )
    }
    .ok()
    .context("SetEntriesInAclW")?;
    // SAFETY: `new_dacl` is the ACL SetEntriesInAclW just allocated; `Owned` frees it once.
    let _new_dacl = unsafe { Owned::new(HLOCAL(new_dacl.cast())) };
    let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `key_w` is NUL-terminated and `new_dacl` stays allocated for the call.
    unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(key_w.as_ptr()),
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    }
    .ok()
    .with_context(|| format!("write the DACL of {key}"))
}
