//! Hide/restore the shell's top-level window around a spawned session, so exactly ONE
//! Punktfunk window is visible at a time: the spawned stream/browse window IS the app
//! while it runs (hidden = no taskbar entry, no Alt-Tab ghost), and the shell reappears
//! the moment the child exits — every exit path funnels through the spawn reader's
//! `Exited` event (clean end, error, crash, Disconnect kill), so the shell can never stay
//! hidden with no child.
//!
//! The root component hands the window to [`adopt`] once reactor has created it.

use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::windef::{HWND, RECT};
use windows::Win32::winuser::{
    GetWindowRect, IsIconic, IsWindow, SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW,
};

static SHELL_HWND: AtomicUsize = AtomicUsize::new(0);

/// Keep the shell window's handle and stamp the embedded app icon on it.
pub(crate) fn adopt(hwnd: *mut core::ffi::c_void) {
    SHELL_HWND.store(hwnd as usize, Ordering::Relaxed);
    set_icon(hwnd.cast());
}

fn shell_hwnd() -> Option<HWND> {
    let h = SHELL_HWND.load(Ordering::Relaxed) as HWND;
    // SAFETY: `IsWindow` accepts any value and dereferences nothing; a stale or unset handle
    // reads as absent.
    (!h.is_null() && unsafe { IsWindow(Some(h)) }.as_bool()).then_some(h)
}

/// The shell window's handle as a number, while the window lives.
pub(crate) fn raw() -> Option<usize> {
    shell_hwnd().map(|h| h as usize)
}

/// The exe's icon resource (build.rs, ordinal 1) as the title bar, Alt-Tab and unpackaged
/// taskbar icon. The MSIX taskbar and Start icons come from the package assets.
fn set_icon(hwnd: HWND) {
    use windows::Win32::libloaderapi::GetModuleHandleW;
    use windows::Win32::winuser::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };
    // MAKEINTRESOURCE(1): the "pointer" is the resource ordinal, so `ptr::dangling()` would
    // name a different resource.
    #[allow(clippy::manual_dangling_ptr)]
    let ordinal_1 = windows::core::PCWSTR(1 as *const u16);
    // SAFETY: `hwnd` is the live shell window; the module handle is this process's own and the
    // ordinal names a resource in it. Nothing here dereferences caller memory.
    unsafe {
        let module = GetModuleHandleW(None);
        if module.is_null() {
            return;
        }
        // Small (title bar) and big (Alt-Tab) at their native metrics from the multi-size .ico.
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = GetSystemMetrics(metric);
            let icon = LoadImageW(
                Some(module),
                ordinal_1,
                IMAGE_ICON as u32,
                px,
                px,
                LR_DEFAULTCOLOR as u32,
            );
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON as u32, which as usize, icon as isize);
            }
        }
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
