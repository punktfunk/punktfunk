//! Cross-process ownership guards for pf-vdisplay management.
//!
//! The console host retains the `Global\punktfunk-vdisplay-manager` mutex.
//! A trusted seats reservation gives each explicit seat connector its own
//! mutex, so distinct reserved slots can coexist but duplicate owners fail.
//! Every claim is held for the process lifetime and the OS releases it on
//! exit. Failed claims are not memoized.
//!
//! DACL is SYSTEM + Administrators only. Tests pin which owner SIDs count
//! as a sibling host versus a squat.

use super::*;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// Process-global held mutex (`None` until claimed). The serve path claims
/// its console or fixed-seat scope before a session opens the backend.
static INSTANCE: Mutex<Option<OwnedHandle>> = Mutex::new(None);

fn instance_name_for(plan: crate::identity::WindowsSlotPlan) -> String {
    match plan.seat_slot() {
        Some(slot) => format!(r"Global\punktfunk-vdisplay-manager-seat-{slot}"),
        None => r"Global\punktfunk-vdisplay-manager".to_string(),
    }
}

fn instance_name() -> Result<String> {
    process_slot_plan()
        .map(instance_name_for)
        .map_err(anyhow::Error::new)
}

pub(super) fn claim_instance() -> Result<()> {
    let mut g = INSTANCE.lock().unwrap();
    if g.is_none() {
        let name = instance_name()?;
        *g = Some(acquire_single_instance(&name)?);
    }
    Ok(())
}

/// Claims early so a duplicate console or seat owner loses before a client
/// arrives. Failure stays visible and the later backend open fails again.
pub fn claim_instance_eagerly() {
    if let Err(e) = claim_instance() {
        tracing::warn!("pf-vdisplay instance claim failed at startup: {e:#}");
    }
}

/// Holds one console or seat-slot mutex for the process lifetime. A second
/// owner of the same scope fails while owners of distinct reserved slots can
/// coexist.
fn acquire_single_instance(name: &str) -> Result<OwnedHandle> {
    let in_use = format!(
        "another punktfunk-host process already owns pf-vdisplay scope `{name}` — refusing to \
         touch that connector scope until the other process exits"
    );
    // `Global\` is creatable by any SeCreateGlobalPrivilege holder (includes LocalService).
    // Default DACL from the creating token lets a squatter deny SYSTEM and look like
    // "another instance". Explicit DACL so lesser principals cannot open ours; check
    // OWNER of an existing name so a squat is reported as a squat.
    let sd = security_descriptor()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let wide_name = windows::core::HSTRING::from(name);
    // SAFETY: `wide_name`, `sa`, and its descriptor outlive the call. The checked handle has one
    // `OwnedHandle` owner, and `GetLastError` is read immediately after `CreateMutexW`.
    unsafe {
        let h = match CreateMutexW(Some(&sa), false, windows::core::PCWSTR(wide_name.as_ptr())) {
            Ok(h) => h,
            // ACCESS_DENIED cannot distinguish a live protected owner, a squat,
            // or a token that cannot create a Global object.
            Err(e) if e.code().0 == 0x8007_0005u32 as i32 => anyhow::bail!(
                "{in_use}. If no matching punktfunk-host is running, this process lacks rights \
                 on `{name}` (run elevated or as the installed service account), or that name is \
                 squatted by another process. Sysinternals `handle.exe -a \
                 punktfunk-vdisplay-manager` tells a holder from a missing privilege"
            ),
            Err(e) => {
                return Err(e).with_context(|| format!("CreateMutexW({name})"));
            }
        };
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = OwnedHandle::from_raw_handle(h.0 as _);
        if already {
            // DACL access does not identify the creator. An owner outside the
            // privileged host accounts is a squat, not a sibling host.
            if let Some(owner) = object_owner_sid(h) {
                if !is_privileged_sid(&owner) {
                    anyhow::bail!(
                        "pf-vdisplay scope `{name}` is held by a non-administrative process (owner \
                         SID {owner}); treating the name as squatted and refusing driver access"
                    );
                }
            }
            anyhow::bail!(in_use);
        }
        Ok(owned)
    }
}

/// Protected DACL (`D:P`): Full to SYSTEM and BUILTIN\Administrators only.
/// A LocalService plugin runner is neither, so it cannot open our object.
fn security_descriptor() -> Result<LocalSd> {
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::Authorization::SDDL_REVISION_1;
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is NUL-terminated (`w!`), and `psd` is a live out-param whose
    // allocation is taken over by `LocalSd` below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)"),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .context("build the pf-vdisplay single-instance security descriptor")?;
    Ok(LocalSd(psd.0))
}

struct LocalSd(*mut core::ffi::c_void);

impl Drop for LocalSd {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
            // which documents LocalFree as the matching deallocation.
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0),
                ));
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

/// Owner SID of a kernel object as SDDL. `None` when unreadable (handle
/// lacks READ_CONTROL) — treated as unknown, never as fine.
fn object_owner_sid(h: HANDLE) -> Option<String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSID};

    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `h` is the live mutex handle; the out-params are live locals; `sd` is the single
    // allocation and is LocalFree'd below.
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    let out = if rc.is_ok() && !owner.is_invalid() {
        let mut sid_str = windows::core::PWSTR::null();
        // SAFETY: `owner` points into `sd` and is a valid SID; `sid_str` is a live out-param whose
        // LocalAlloc'd string is freed immediately below.
        unsafe {
            if ConvertSidToStringSidW(owner, &mut sid_str).is_ok() && !sid_str.is_null() {
                let text = sid_str.to_string().unwrap_or_default();
                let _ = LocalFree(Some(HLOCAL(sid_str.0 as _)));
                Some(text)
            } else {
                None
            }
        }
    } else {
        None
    };
    // SAFETY: `sd` is the LocalAlloc'd descriptor GetSecurityInfo returned (null when it failed,
    // which LocalFree tolerates).
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    out
}

/// SYSTEM, BUILTIN\Administrators, or an NT SERVICE SID (`S-1-5-80-…`).
///
/// Narrow on purpose: this decides sibling host vs squat. A `S-1-5-32-`
/// prefix or any `S-1-5-21-…` domain account reclassifies a non-admin
/// squatter as one of ours. LocalService (`S-1-5-19`) and NetworkService
/// (`S-1-5-20`) are excluded: the plugin runner is forced to LocalService,
/// so a name owned by it is a plugin, not a host.
fn is_privileged_sid(sid: &str) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544") || sid.starts_with("S-1-5-80-")
}

#[cfg(test)]
mod tests {
    use super::{instance_name_for, is_privileged_sid};
    use crate::identity::WindowsSlotPlan;

    #[test]
    fn console_and_seat_owners_have_disjoint_mutex_scopes() {
        let console = r"Global\punktfunk-vdisplay-manager";
        assert_eq!(instance_name_for(WindowsSlotPlan::Unreserved), console);
        assert_eq!(instance_name_for(WindowsSlotPlan::ReservedConsole), console);
        assert_eq!(
            instance_name_for(WindowsSlotPlan::Seat(12)),
            r"Global\punktfunk-vdisplay-manager-seat-12"
        );
        assert_ne!(
            instance_name_for(WindowsSlotPlan::Seat(12)),
            instance_name_for(WindowsSlotPlan::Seat(13))
        );
    }

    /// Widening is silent: a squat starts reading as a sibling host.
    #[test]
    fn is_privileged_sid_accepts_system_admins_and_service_sids_only() {
        assert!(is_privileged_sid("S-1-5-18"), "SYSTEM");
        assert!(is_privileged_sid("S-1-5-32-544"), "BUILTIN\\Administrators");
        assert!(
            is_privileged_sid("S-1-5-80-3139157870-2983391045-3678747466-658725712-1809340420"),
            "an NT SERVICE\\… per-service SID"
        );

        assert!(!is_privileged_sid("S-1-5-32-545"), "BUILTIN\\Users");
        assert!(
            !is_privileged_sid("S-1-5-21-1004336348-1177238915-682003330-1001"),
            "a local/domain user account"
        );
        assert!(!is_privileged_sid("S-1-5-19"), "LocalService");
        assert!(!is_privileged_sid("S-1-5-20"), "NetworkService");
        assert!(
            !is_privileged_sid(""),
            "an unreadable owner is never 'fine'"
        );
        // `S-1-5-80` without the trailing dash is a different SID string;
        // `S-1-5-8` (Proxy) must not slip in under a loosened prefix.
        assert!(!is_privileged_sid("S-1-5-8"), "Proxy");
        assert!(!is_privileged_sid("S-1-5-800-1"), "not a service SID");
    }
}
