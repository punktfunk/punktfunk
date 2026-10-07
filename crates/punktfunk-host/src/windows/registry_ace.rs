//! A read ACE on a registry key for one SID, by key name: `MACHINE\SOFTWARE\…` as
//! `SetNamedSecurityInfoW` spells `HKLM`. The DACL is read, one entry merged, and written
//! back; a revoke drops every ACE the SID holds there. Works as SYSTEM, which owns the keys.

use anyhow::{Context, Result};
use windows::core::{Owned, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    ACCESS_MODE, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_REGISTRY_KEY, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
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

fn edit(key: &str, sid: &str, mode: ACCESS_MODE) -> Result<()> {
    let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let sid_w: Vec<u16> = sid.encode_utf16().chain(std::iter::once(0)).collect();
    let mut psid = PSID::default();
    // SAFETY: `sid_w` is NUL-terminated and outlives the call; `psid` is a live out-param that
    // receives a LocalAlloc'd SID, which `Owned` frees after its last use below.
    unsafe { ConvertStringSidToSidW(PCWSTR(sid_w.as_ptr()), &mut psid) }
        .with_context(|| format!("parse SID {sid}"))?;
    // SAFETY: `psid` is the LocalAlloc'd SID the call just returned; `Owned` frees it once.
    let _psid = unsafe { Owned::new(HLOCAL(psid.0)) };
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `key_w` is NUL-terminated; both out-params are live locals, and `sd` owns the
    // descriptor `old_dacl` points into until `_sd` drops.
    let read = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(key_w.as_ptr()),
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut sd,
        )
    };
    // SAFETY: `sd` is null or the one LocalAlloc'd descriptor the call returned; `Owned` frees
    // it once, after the last use of `old_dacl`.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    read.ok()
        .with_context(|| format!("read the DACL of {key}"))?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: KEY_READ.0,
        grfAccessMode: mode,
        grfInheritance: CONTAINER_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(psid.0.cast()),
        },
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: one entry whose SID stays allocated for the call; `old_dacl` is null or the DACL
    // `sd` still owns.
    unsafe {
        SetEntriesInAclW(
            Some(&[entry]),
            (!old_dacl.is_null()).then_some(old_dacl as *const ACL),
            &mut new_dacl,
        )
    }
    .ok()
    .context("SetEntriesInAclW")?;
    // SAFETY: `new_dacl` is the ACL SetEntriesInAclW just allocated; `Owned` frees it once.
    let _new_dacl = unsafe { Owned::new(HLOCAL(new_dacl.cast())) };
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
