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
//! lookup matches DXGI outputs against. [`window_edid`] reads that monitor's EDID and
//! [`window_refresh_mhz`] its exact rate.

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
const APP_USER_MODEL_ID: *const u16 = w!("io.unom.punktfunk.client");

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
pub(crate) fn window_hwnd(window: &sdl3::video::Window) -> Option<HWND> {
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

/// The exact rate of the monitor the window is on, in millihertz, off its active display
/// path. SDL reads whole hertz here and guesses the fraction, so 59.80 Hz reads 59.94.
/// `None` when Windows names no path for the monitor.
pub(crate) fn window_refresh_mhz(window: &sdl3::video::Window) -> Option<u32> {
    let rate = window_path(window)?.targetInfo.refreshRate;
    let den = u64::from(rate.Denominator);
    (den > 0).then(|| ((u64::from(rate.Numerator) * 1000 + den / 2) / den) as u32)
}

/// The EDID of the monitor the window is on, as the registry holds it beside the monitor's
/// device. `None` when Windows names no path or monitor device, or stores no EDID.
pub(crate) fn window_edid(window: &sdl3::video::Window) -> Option<Vec<u8>> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
        SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW, DICS_FLAG_GLOBAL, DIREG_DEV,
        SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
    };
    use windows_sys::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ};
    let path = window_path(window)?;
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    target.header.adapterId = path.targetInfo.adapterId;
    target.header.id = path.targetInfo.id;
    // SAFETY: the header states the packet's own type and size, and `target` outlives the call.
    if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
        return None;
    }
    let mut iface = SP_DEVICE_INTERFACE_DATA {
        cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    let mut dev = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: the set starts empty and is destroyed on every path. `monitorDevicePath` is a
    // NUL-terminated array the call only reads; `iface` and `dev` are locals whose `cbSize`
    // names their own size, so the set's one device is the monitor the path opened.
    let key = unsafe {
        let set = SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut());
        if set == INVALID_HANDLE_VALUE as isize {
            return None;
        }
        let opened =
            SetupDiOpenDeviceInterfaceW(set, target.monitorDevicePath.as_ptr(), 0, &mut iface) != 0
                && SetupDiEnumDeviceInfo(set, 0, &mut dev) != 0;
        let key = if opened {
            SetupDiOpenDevRegKey(set, &dev, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ)
        } else {
            INVALID_HANDLE_VALUE
        };
        SetupDiDestroyDeviceInfoList(set);
        key
    };
    if key == INVALID_HANDLE_VALUE || key.is_null() {
        return None;
    }
    let value = w!("EDID");
    let mut len = 0u32;
    let mut edid = Vec::new();
    // SAFETY: `key` is open until the close below. The first call only writes the size into
    // `len`; the second writes at most `len` bytes into `edid`, which holds that many.
    let rc = unsafe {
        let mut rc = RegQueryValueExW(
            key,
            value,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        );
        if rc == 0 {
            edid.resize(len as usize, 0);
            rc = RegQueryValueExW(
                key,
                value,
                std::ptr::null(),
                std::ptr::null_mut(),
                edid.as_mut_ptr(),
                &mut len,
            );
        }
        RegCloseKey(key);
        rc
    };
    edid.truncate(len as usize);
    (rc == 0).then_some(edid)
}

/// The active display path whose source is the monitor the window is on. `None` when Windows
/// names none.
fn window_path(
    window: &sdl3::video::Window,
) -> Option<windows_sys::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO> {
    use windows_sys::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO, MONITORINFOEXW};
    let hwnd = window_hwnd(window)?;
    // SAFETY: MONITORINFOEXW is integers and a name array; all-zero is a valid value.
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `hwnd` is SDL's live handle. `info` is a local whose `cbSize` names the EX form,
    // which starts with the MONITORINFO the call is typed for.
    let named = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>())
    };
    if named == 0 {
        return None;
    }
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: both counts are live locals the call fills.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != 0
        || np == 0
    {
        return None;
    }
    // SAFETY: both records are integers and unions of integers; all-zero is a valid value.
    let (path, mode): (DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_MODE_INFO) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    let mut paths = vec![path; np as usize];
    let mut modes = vec![mode; nm as usize];
    // SAFETY: each array holds the count passed beside it, and the call writes no more. The
    // topology id is only written under `QDC_DATABASE_CURRENT`, so null is allowed here.
    let rc = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return None;
    }
    paths.truncate(np as usize);
    paths.into_iter().find(|p| {
        // SAFETY: the request packet is integers and a name array; all-zero is a valid value.
        let mut name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        name.header.adapterId = p.sourceInfo.adapterId;
        name.header.id = p.sourceInfo.id;
        // SAFETY: the header states the packet's own type and size, and `name` outlives the call.
        let found = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } == 0;
        found && name.viewGdiDeviceName == info.szDevice
    })
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
