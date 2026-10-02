//! Hide/restore the shell's top-level window around a spawned session, so exactly ONE
//! Punktfunk window is visible at a time: the spawned stream/browse window IS the app
//! while it runs (hidden = no taskbar entry, no Alt-Tab ghost), and the shell reappears
//! the moment the child exits — every exit path funnels through the spawn reader's
//! `Exited` event (clean end, error, crash, Disconnect kill), so the shell can never stay
//! hidden with no child.
//!
//! windows-reactor exposes no window handle, so the HWND is resolved by its (unique)
//! title and cached — the same pattern as `app::apply_window_icon_when_ready` and
//! `stream::window_dpi`.

use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::windef::{HWND, RECT};
use windows::Win32::winuser::{
    FindWindowW, GetWindowRect, IsIconic, IsWindow, SetForegroundWindow, ShowWindow, SW_HIDE,
    SW_SHOW,
};

static SHELL_HWND: AtomicIsize = AtomicIsize::new(0);

fn shell_hwnd() -> Option<HWND> {
    // SAFETY: the cached value is an `HWND` this process obtained itself; it is re-validated with
    // `IsWindow` before use (a stale handle is treated as absent), and the lookup calls take only
    // static wide literals.
    unsafe {
        let cached = SHELL_HWND.load(Ordering::Relaxed);
        if cached != 0 {
            let h = HWND(cached as *mut _);
            if IsWindow(Some(h)).as_bool() {
                return Some(h);
            }
        }
        let h = FindWindowW(None, windows::core::w!("Punktfunk"));
        if h.0.is_null() {
            return None;
        }
        SHELL_HWND.store(h.0 as isize, Ordering::Relaxed);
        Some(h)
    }
}

/// Hide the shell while a spawned session window is up. Called on the child's
/// `{"ready":true}` (its window has presented — never earlier, so a failed connect keeps
/// the shell in view with its error banner).
pub(crate) fn hide() {
    if let Some(h) = shell_hwnd() {
        // SAFETY: `h` is the validated shell window handle from `shell_hwnd`; `ShowWindow` takes it
        // plus a plain flag and dereferences nothing.
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Bring the shell back (and to the foreground) when the child exits. Safe to call when
/// it was never hidden — showing a visible window is a no-op.
pub(crate) fn restore() {
    if let Some(h) = shell_hwnd() {
        // SAFETY: as `hide` — `h` is the validated shell window handle, and both calls take only
        // that handle plus a plain flag.
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
    }
}

/// The shell window's top-left in desktop coordinates — passed to the spawned session
/// (`--window-pos`) so its window opens on the SAME monitor, roughly where the shell is,
/// and the visibility handoff reads as one window changing content. `None` while minimized:
/// an iconic window's rect sits at (-32000, -32000), off every display.
pub(crate) fn position() -> Option<(i32, i32)> {
    let h = shell_hwnd()?;
    // SAFETY: `h` is the validated shell window handle.
    if unsafe { IsIconic(h) }.as_bool() {
        return None;
    }
    let mut r = RECT::default();
    // SAFETY: `h` is the validated shell window handle and `r` is a live local the call fills.
    if !unsafe { GetWindowRect(h, &mut r) }.as_bool() {
        return None;
    }
    Some((r.left, r.top))
}
