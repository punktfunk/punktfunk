//! Windows-only window branding for the SDL session window.
//!
//! SDL's window class carries a generic icon. [`stamp_window_icon`] copies the
//! exe's icon resource (ordinal 1, embedded by the session binary's `build.rs`)
//! onto the HWND via `WM_SETICON` at `SM_CXSMICON` / `SM_CXICON` so Win32 does
//! not scale at draw time.
//!
//! [`set_app_user_model_id`] stamps the shared AppUserModelID on unpackaged
//! runs so the shell and session windows group as one taskbar app. Packaged
//! processes already share MSIX identity; overriding it detaches the Start-menu
//! pin, so they are left alone.
//!
//! [`window_monitor`] names the monitor the window is on, the key the HDR volume
//! lookup matches DXGI outputs against.

use windows_sys::core::w;
use windows_sys::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    SM_CXICON, SM_CXSMICON, WM_SETICON,
};

/// Must match the WinUI shell (`clients/windows/src/main.rs`) or the two windows stop grouping.
const APP_USER_MODEL_ID: *const u16 = w!("unom.punktfunk.client");

/// Unpackaged runs only. Call before any window exists; later calls do not re-tag existing windows.
pub(crate) fn set_app_user_model_id() {
    // SAFETY: `GetCurrentPackageFullName` is called with `len = 0` and a NULL buffer, which is the
    // documented identity PROBE — it writes nothing and only reports whether the process is
    // packaged. `SetCurrentProcessExplicitAppUserModelID` takes a static wide literal.
    unsafe {
        let mut len: u32 = 0;
        if GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) != APPMODEL_ERROR_NO_PACKAGE {
            return; // packaged or indeterminate — do not override MSIX identity
        }
        let _ = SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID);
    }
}

/// The `HWND` SDL owns for this window; `None` before SDL has created it.
fn window_hwnd(window: &sdl3::video::Window) -> Option<HWND> {
    // SAFETY: the SDL property lookups only read the live `window` borrow's property set.
    let hwnd = unsafe {
        sdl3::sys::properties::SDL_GetPointerProperty(
            sdl3::sys::video::SDL_GetWindowProperties(window.raw()),
            sdl3::sys::video::SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        )
    } as HWND;
    (!hwnd.is_null()).then_some(hwnd)
}

/// The `HMONITOR` holding most of the window, as an integer for the DXGI output match.
pub(crate) fn window_monitor(window: &sdl3::video::Window) -> Option<isize> {
    let hwnd = window_hwnd(window)?;
    // SAFETY: `hwnd` is SDL's live handle; the call only reads it and returns a monitor handle.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    (!monitor.is_null()).then_some(monitor as isize)
}

/// Resource ordinal 1 onto this HWND. No-op when the exe embeds no icon.
pub(crate) fn stamp_window_icon(window: &sdl3::video::Window) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    // SAFETY: `hwnd` is the handle SDL owns for this live `window` borrow; the icon calls only
    // pass that handle and a resource ordinal back to Win32, and every result is checked before use.
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = GetSystemMetrics(metric);
            // MAKEINTRESOURCE(1): integer ordinal through the name pointer, never
            // dereferenced. `without_provenance` states that; `1 as *const u16` is a
            // dangling pointer to clippy.
            let icon = LoadImageW(
                module,
                std::ptr::without_provenance(1),
                IMAGE_ICON,
                px,
                px,
                LR_DEFAULTCOLOR,
            );
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as WPARAM, icon as LPARAM);
            }
        }
    }
}
