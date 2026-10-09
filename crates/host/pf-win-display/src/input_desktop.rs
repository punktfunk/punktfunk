//! Bind display-config writes to the input desktop.
//!
//! Windows refuses `ChangeDisplaySettingsEx` / `SetDisplayConfig` from a thread that is not on
//! the desktop currently receiving input. A UAC prompt, lock, or logon screen makes that desktop
//! `Winlogon`; a host thread still on `WinSta0\Default` then gets `DISP_CHANGE_FAILED` /
//! `ERROR_ACCESS_DENIED` for every write, and the session never sets the virtual display's mode.
//!
//! Retry mirrors `pf-inject`'s `sendinput.rs`: issue the write, and only on a wrong-desktop
//! failure rebind and retry once. The happy path is untouched.
//!
//! Unlike sendinput's dedicated injector thread, these helpers run on shared/task threads, so
//! the bind is scoped: [`InputDesktopBinding`] restores the original desktop on drop. A thread
//! left on a `Winlogon` desktop that is later destroyed (prompt dismissed) would fail every
//! later display write for the process lifetime.
//!
//! Also home of the process-wide secure-desktop verdict ([`secure_desktop`]), refreshed by the
//! display-events pump on every desktop-switch WinEvent and by the cursor poller as a fallback.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// `GENERIC_ALL` for the desktop open. The `windows` crate models desktop access as its own flag
/// type and doesn't export the generic rights, so spell it out (same constant `sendinput.rs` uses).
const GENERIC_ALL: u32 = 0x1000_0000;

/// `GetThreadDesktop` returns a borrowed handle (documented: creates no new handle, must not be
/// closed). Only the `OpenInputDesktop` handle is closed here, and only after the thread is off it.
pub(crate) struct InputDesktopBinding {
    previous: HDESK,
    input: HDESK,
    /// `UOI_NAME` of the bound desktop, for the rebound-write log. Read once at bind (failure path only).
    name: String,
}

impl InputDesktopBinding {
    /// `None` if the input desktop is unreachable (not privileged for `Winlogon`) or the rebind is
    /// refused; the caller keeps its existing result.
    pub(crate) fn bind() -> Option<Self> {
        // SAFETY: all four are FFI calls taking by-value args only. `GetThreadDesktop` yields a
        // borrowed handle for this thread (never closed here). `OpenInputDesktop` yields an owned
        // `HDESK` only on `Ok`; it is installed by `SetThreadDesktop` (then owned by the returned
        // guard, closed once in `Drop`) or closed here on failure — once on every path, never used
        // after close. `SetThreadDesktop` rebinds only the calling thread, which owns no windows
        // or hooks (display-config workers), so it cannot fail on that account.
        unsafe {
            let previous = GetThreadDesktop(GetCurrentThreadId()).ok()?;
            let input = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            )
            .ok()?;
            let name = desktop_name(input).unwrap_or_else(|| "<unnamed>".into());
            if SetThreadDesktop(input).is_err() {
                let _ = CloseDesktop(input);
                return None;
            }
            Some(Self {
                previous,
                input,
                name,
            })
        }
    }
}

impl Drop for InputDesktopBinding {
    fn drop(&mut self) {
        // SAFETY: `self.previous` is the borrowed desktop this thread started on and `self.input`
        // is the handle this guard uniquely owns. The thread is moved back first, so the handle is
        // no longer the thread's desktop when it is closed — closed exactly once, never used after.
        unsafe {
            let _ = SetThreadDesktop(self.previous);
            let _ = CloseDesktop(self.input);
        }
    }
}

/// `UOI_NAME` of the current input desktop — `Some("Winlogon")` while UAC / lock / logon owns
/// input, `Some("Default")` in normal operation, `None` when it cannot be opened.
pub(crate) fn input_desktop_name() -> Option<String> {
    // SAFETY: `OpenInputDesktop` yields an owned handle only on `Ok`, which is closed exactly once
    // below and not used after.
    unsafe {
        let h = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
        )
        .ok()?;
        let name = desktop_name(h);
        let _ = CloseDesktop(h);
        name
    }
}

/// The input desktop is `Winlogon` (UAC / lock / logon) or a screen-saver desktop. Written only
/// by [`refresh_secure_desktop`]; read by the capturer's hardware-cursor guard every tick.
static SECURE_DESKTOP: AtomicBool = AtomicBool::new(false);

/// `true` while the input desktop is secure, as of the last [`refresh_secure_desktop`].
pub fn secure_desktop() -> bool {
    SECURE_DESKTOP.load(Ordering::Relaxed)
}

/// Re-classify the input desktop into [`secure_desktop`]. An unopenable desktop keeps the last
/// verdict: flapping would stand the hardware-cursor declare down and up for nothing.
pub fn refresh_secure_desktop() {
    if let Some(name) = input_desktop_name() {
        SECURE_DESKTOP.store(!name.eq_ignore_ascii_case("Default"), Ordering::Relaxed);
    }
}

/// `UOI_NAME` of an already-open desktop handle. Borrows `h` — closing it stays the caller's job.
///
/// # Safety
/// `h` must be a live desktop handle for the duration of the call.
unsafe fn desktop_name(h: HDESK) -> Option<String> {
    let mut name = [0u16; 64]; // "Default" / "Winlogon" / "Screen-saver" all fit with room
    let mut needed = 0u32;
    // SAFETY: `h` is live per this fn's contract; `name`/`needed` are live out-params and the call
    // writes at most `nlength` bytes, exactly the size passed.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(h.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// `true` when the input desktop is not `Default` (`Winlogon` for UAC / lock / logon, or a
/// screen-saver). `false` when it is normal or unreadable: this only phrases a diagnostic, so an
/// unknown desktop must not claim the secure one is up.
pub(crate) fn input_desktop_is_secure() -> bool {
    input_desktop_name().is_some_and(|n| !n.eq_ignore_ascii_case("Default"))
}

/// `ERROR_ACCESS_DENIED` — how Windows refuses a `SetDisplayConfig` issued off the input desktop.
/// Narrower than "any error": the CCD path also returns `ERROR_INVALID_PARAMETER` (0x57) for an
/// unrelated exclusive-mode topology bug; re-issuing that on another desktop confuses its diagnosis.
const SDC_ACCESS_DENIED: i32 = 5;

pub(crate) fn retry_set_display_config(write: impl FnMut() -> i32) -> i32 {
    retry_on_input_desktop(|rc| *rc == SDC_ACCESS_DENIED, write)
}

/// Run a display-config write; on a wrong-desktop result, bind this thread to the current input
/// desktop and run it once more. `denied` must be that specific verdict so a bad mode or driver
/// refusal is not re-issued. The binding is dropped before return.
pub(crate) fn retry_on_input_desktop<T>(
    denied: impl Fn(&T) -> bool,
    mut write: impl FnMut() -> T,
) -> T {
    let first = write();
    if !denied(&first) {
        return first;
    }
    let Some(binding) = InputDesktopBinding::bind() else {
        return first;
    };
    let second = write();
    if !denied(&second) {
        // A silent success is indistinguishable from a write that never needed saving.
        tracing::info!(
            desktop = %binding.name,
            "display write was refused off the input desktop — retried bound to it and it applied \
             (a UAC prompt / lock screen owns input; the session continues normally)"
        );
    }
    second
}
