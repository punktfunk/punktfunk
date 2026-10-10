//! Win32 half of [`crate::gamelease`]: the termination ladder (`WM_CLOSE`, then `TerminateProcess`),
//! and whether the game's window is on the input desktop yet ([`visible_window`]).
//!
//! Kept out of `procscan` (read-only) and `gamelease` (platform-neutral).
//!
//! `EnumWindows` sees only the calling thread's desktop. The host is SYSTEM in the interactive
//! session ([`super::service`]) but not on the user's desktop, so without
//! `OpenInputDesktop`/`SetThreadDesktop` the polite pass is empty and every game is killed.
//! A UAC prompt, lock, or Ctrl-Alt-Del swaps that desktop out from under us — same bind
//! `pf-inject`'s `sendinput.rs` uses for `SendInput`.
//!
//! Pin: [`request_close`], [`kill`]. Evidence: [`crate::gamelease`].

use windows::core::{Owned, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, HWND, LPARAM, PROPERTYKEY, RECT, WPARAM,
};
use windows::Win32::Storage::Packaging::Appx::{
    GetApplicationUserModelId, APPLICATION_USER_MODEL_ID_MAX_LENGTH,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, EnumDesktopWindows, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, HDESK,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, GW_OWNER, WM_CLOSE,
};

/// Non-zero so a reader can tell a kill from a clean quit.
const KILLED_EXIT_CODE: u32 = 1;

/// `GENERIC_ALL`. The `windows` crate's desktop-rights type does not export generic rights, so
/// spell it (same constant `pf-inject`'s `sendinput.rs` uses).
const DESKTOP_GENERIC_ALL: u32 = 0x1000_0000;

/// The calling thread bound to the input desktop. `Drop` rebinds the thread's previous desktop
/// before the handle closes: `CloseDesktop` fails on a desktop a thread is still bound to.
pub(super) struct InputDesktop {
    #[allow(dead_code, reason = "held so the handle closes after Drop rebinds")]
    desk: Owned<HDESK>,
    prev: HDESK,
}

impl InputDesktop {
    /// `None` if the input desktop cannot be opened or bound (unprivileged, or a secure desktop).
    /// Callers skip the polite pass; the kill pass still works.
    pub(super) fn attach() -> Option<Self> {
        // SAFETY: FFI by-value args only. `GetThreadDesktop` returns this thread's desktop, which
        // needs no close. `OpenInputDesktop` yields an owned `HDESK` only on `Ok`, adopted by
        // `Owned` at once. `SetThreadDesktop` rebinds only the calling thread, which owns no
        // windows or hooks.
        unsafe {
            let prev = GetThreadDesktop(GetCurrentThreadId()).ok()?;
            let desk = Owned::new(
                OpenInputDesktop(
                    DESKTOP_CONTROL_FLAGS(0),
                    false,
                    DESKTOP_ACCESS_FLAGS(DESKTOP_GENERIC_ALL),
                )
                .ok()?,
            );
            SetThreadDesktop(*desk).ok()?;
            Some(Self { desk, prev })
        }
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        // SAFETY: `prev` was this thread's desktop before `attach` and is still open. Rebinding it
        // releases `desk`, which its `Owned` closes after this body.
        unsafe {
            let _ = SetThreadDesktop(self.prev);
        }
    }
}

/// `EnumWindows` `LPARAM`. Owns the pid list: the value round-trips through a raw pointer, and a
/// borrowed lifetime there has no upside for one small allocation per termination.
struct CloseCtx {
    pids: Vec<u32>,
    posted: usize,
}

/// The window X, so the game can save. Zero is ordinary (no window yet, or off the input
/// desktop). The caller waits, then kills; it must not treat zero as success.
pub fn request_close(pids: &[u32]) -> usize {
    if pids.is_empty() {
        return 0;
    }
    // Held for the whole enumeration. Failure skips the polite pass (session 0 has no game windows).
    let Some(_desktop) = InputDesktop::attach() else {
        tracing::debug!(
            "could not bind to the input desktop — skipping the polite close and going straight to \
             the kill pass"
        );
        return 0;
    };
    let mut ctx = CloseCtx {
        pids: pids.to_vec(),
        posted: 0,
    };
    // SAFETY: `EnumWindows` calls `enum_close` synchronously and returns before this frame exits,
    // so the `&mut ctx` in `LPARAM` stays valid and unaliased for the whole call.
    unsafe {
        let _ = EnumWindows(Some(enum_close), LPARAM(&mut ctx as *mut CloseCtx as isize));
    }
    ctx.posted
}

/// Visible top-level windows only: message-only and tool windows swallow `WM_CLOSE` and would
/// inflate the count.
unsafe extern "system" fn enum_close(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lparam` is the `&mut CloseCtx` from `request_close`, valid for the enumeration;
    // the callback runs synchronously on the same thread, so this is the only live reference.
    let ctx = unsafe { &mut *(lparam.0 as *mut CloseCtx) };
    let mut pid = 0u32;
    // SAFETY: `hwnd` is the window the enumeration handed us; `pid` is a live local we own.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if ctx.pids.contains(&pid) && IsWindowVisible(hwnd).as_bool() {
            // Posted, not sent: a `SendMessage` would block this thread on a hung game's message pump.
            if PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
                ctx.posted += 1;
            }
        }
    }
    true.into() // keep enumerating — a game can own several windows
}

/// `EnumDesktopWindows` `LPARAM` for [`visible_window`].
struct FindCtx {
    pids: Vec<u32>,
    /// AppUserModelIDs of the packaged processes among `pids`.
    apps: Vec<String>,
    title: Option<String>,
}

/// `System.AppUserModel.ID`: the app a frame window hosts.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// Title of a visible, unowned, non-empty top-level window of one of `pids` on the input desktop —
/// the one the player sees. `None` when there is none yet, or that desktop cannot be read.
///
/// A packaged (UWP) app's own window never enumerates here. What the player sees is an
/// `ApplicationFrameWindow` owned by `ApplicationFrameHost`, which counts when it hosts the app's
/// AppUserModelID.
///
/// Enumerates the desktop by handle instead of binding this thread to it: the lease watcher asks
/// every second, and a desktop a thread is bound to cannot be closed.
pub fn visible_window(pids: &[u32]) -> Option<String> {
    if pids.is_empty() {
        return None;
    }
    // SAFETY: `OpenInputDesktop` yields an owned `HDESK` only on `Ok`, closed once below. The
    // enumeration calls `enum_find` synchronously, so `&mut ctx` in `LPARAM` stays valid and
    // unaliased for the whole call. A successful `CoInitializeEx` is balanced below.
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
        let mut ctx = FindCtx {
            pids: pids.to_vec(),
            apps: pids
                .iter()
                .filter_map(|&pid| app_user_model_id(pid))
                .collect(),
            title: None,
        };
        // A frame's property store is COM. A repeat init succeeds too (S_FALSE).
        let com = !ctx.apps.is_empty() && CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let _ = EnumDesktopWindows(
            Some(desk),
            Some(enum_find),
            LPARAM(&mut ctx as *mut FindCtx as isize),
        );
        if com {
            CoUninitialize();
        }
        let _ = CloseDesktop(desk);
        ctx.title
    }
}

/// Stops at the first match. Owned windows (dialogs, splash tool windows) and zero-size ones are
/// not the game's main window.
unsafe extern "system" fn enum_find(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lparam` is the `&mut FindCtx` from `visible_window`, valid for the enumeration;
    // the callback runs synchronously on the same thread, so this is the only live reference.
    let ctx = unsafe { &mut *(lparam.0 as *mut FindCtx) };
    let mut pid = 0u32;
    let mut rect = RECT::default();
    // SAFETY: `hwnd` is the window the enumeration handed us; `pid`, `rect` and `buf` are live
    // locals we own, and `GetWindowTextW` writes at most `buf.len()` units.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let candidate = IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && GetWindowRect(hwnd, &mut rect).is_ok()
            && rect.right > rect.left
            && rect.bottom > rect.top
            && (ctx.pids.contains(&pid) || hosts_one_of(hwnd, &ctx.apps));
        if !candidate {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        ctx.title = Some(String::from_utf16_lossy(&buf[..len]));
    }
    false.into()
}

/// The AppUserModelID of a packaged process. `None` for an ordinary Win32 process.
fn app_user_model_id(pid: u32) -> Option<String> {
    let mut buf = [0u16; APPLICATION_USER_MODEL_ID_MAX_LENGTH as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `OpenProcess` yields an owned handle only on `Ok`, closed once below. The call
    // writes at most `len` units into `buf`, which it owns, and sets `len` to the units written.
    let rc = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let rc = GetApplicationUserModelId(process, &mut len, Some(PWSTR(buf.as_mut_ptr())));
        let _ = CloseHandle(process);
        rc
    };
    // `len` counts the terminating NUL.
    (rc == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]))
}

/// Whether `hwnd` is an `ApplicationFrameWindow` hosting one of `apps`.
fn hosts_one_of(hwnd: HWND, apps: &[String]) -> bool {
    if apps.is_empty() {
        return false;
    }
    let mut class = [0u16; 32];
    // SAFETY: `GetClassNameW` writes at most `class.len()` units into a buffer we own. The store
    // and the variant it returns are ours; the variant clears on drop, and its string is only
    // read when `vt` says `VT_LPWSTR` and the pointer is non-null.
    unsafe {
        let len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        if class[..len] != *windows::core::w!("ApplicationFrameWindow").as_wide() {
            return false;
        }
        let Ok(store) = SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) else {
            return false;
        };
        let Ok(pv) = store.GetValue(&PKEY_APP_USER_MODEL_ID) else {
            return false;
        };
        let inner = &pv.Anonymous.Anonymous;
        inner.vt == VT_LPWSTR
            && !inner.Anonymous.pwszVal.is_null()
            && inner
                .Anonymous
                .pwszVal
                .to_string()
                .is_ok_and(|app| apps.contains(&app))
    }
}

/// Whether `pid` runs in this process's session. A pid a plugin reported may name anything;
/// SYSTEM must never terminate a service or another session's process on its word.
pub fn in_our_session(pid: u32) -> bool {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut ours = 0u32;
    let mut theirs = 0u32;
    // SAFETY: both out-params are live locals; the calls read a session id by pid and touch
    // no memory of ours. A failed lookup (pid gone, no access) leaves the default and fails.
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut ours).is_ok()
            && ProcessIdToSessionId(pid, &mut theirs).is_ok()
            && ours == theirs
    }
}

/// Caller must have re-verified start time ([`crate::procscan::Scanner::alive`]) immediately
/// before: Windows recycles pids.
pub fn kill(pids: &[u32]) -> usize {
    let mut killed = 0;
    for &pid in pids {
        // SAFETY: `OpenProcess` yields an owned handle only on `Ok`, which is closed exactly once
        // below; `TerminateProcess` takes it by value plus a plain exit code.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                if TerminateProcess(h, KILLED_EXIT_CODE).is_ok() {
                    killed += 1;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    killed
}
