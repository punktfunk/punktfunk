//! Windows display-topology helpers. Leaf peer for the IDD-push capturer (`pf-capture`) and the
//! pf-vdisplay host backend; Windows-only, empty lib elsewhere.
//!
//! - [`win_display`]: CCD/GDI path activation, mode-setting, HDR advanced-colour toggles, and the
//!   source-desktop geometry the capturer duplicates.
//! - [`monitor_devnode`]: PnP monitor devnode enable/disable (the parallel-display isolation lever).
//! - [`display_events`]: the display ACTOR — `WM_DISPLAYCHANGE` / device-arrival watch that
//!   publishes the cached [`snapshot::DisplaySnapshot`] every hot reader takes instead of the
//!   display-config lock, and timestamps events so a capture stall can say whether an OS display
//!   event coincided with it.
//! - [`snapshot`]: the platform-neutral snapshot types and cache rules (tested everywhere).
//! - [`console_display`]: the console display's power state and the wake a new monitor needs.
//! - [`compose_probe`]: the recovery canary — a 1-pixel window repaint, never input.
//! - [`open_wudfhost`]: the WUDFHost a driver channel duplicates handles into, image-path proved.

#[cfg(target_os = "windows")]
pub mod adl_emul;
/// Typed per-target CCD packets behind one SAFETY proof.
#[cfg(target_os = "windows")]
mod ccd_info;
#[cfg(target_os = "windows")]
pub mod compose_probe;
#[cfg(target_os = "windows")]
pub mod console_display;
#[cfg(target_os = "windows")]
pub mod display_events;
/// Bind display-config writes to the input desktop so a UAC / lock screen can't refuse them.
#[cfg(target_os = "windows")]
mod input_desktop;
#[cfg(target_os = "windows")]
pub use input_desktop::{refresh_secure_desktop, secure_desktop};
#[cfg(target_os = "windows")]
pub mod monitor_devnode;
/// Display identity, inventory and the snapshot cache — pure std, unit-tested on every platform.
pub mod snapshot;

/// A `REG_MULTI_SZ` value in UTF-16 units: each string NUL-terminated, the list NUL-terminated
/// again. Pure std, so it is tested on every platform.
pub fn multi_sz(items: &[&str]) -> Vec<u16> {
    let mut units: Vec<u16> = items
        .iter()
        .flat_map(|s| s.encode_utf16().chain([0]))
        .collect();
    units.push(0);
    units
}
/// Cross-crate "topology churn in flight" latch. Pure std — no Windows surface, so compiled and
/// unit-tested on every platform.
pub mod topology_churn;
#[cfg(target_os = "windows")]
pub mod win_display;
#[cfg(target_os = "windows")]
mod wudfhost;
#[cfg(target_os = "windows")]
pub use wudfhost::{open_wudfhost, verify_is_wudfhost};

/// Whether the machine-level seats marker reserves connector slots.
/// Key existence is the signal; HKLM keeps an unprivileged seat process from
/// enabling cross-process driver management through its own environment.
#[cfg(target_os = "windows")]
pub fn seats_addon_reserves_display_slots() -> bool {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY,
    };

    static RESERVED: OnceLock<bool> = OnceLock::new();
    *RESERVED.get_or_init(|| {
        let mut key = HKEY::default();
        // SAFETY: the subkey is a NUL-terminated literal and `key` is a live out-param.
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                windows::core::w!(r"SOFTWARE\Punktfunk\Seats"),
                None,
                KEY_READ | KEY_WOW64_64KEY,
                &mut key,
            )
        };
        if rc.is_ok() {
            // SAFETY: `key` was initialized by the successful open and is closed once here.
            let _ = unsafe { RegCloseKey(key) };
            true
        } else {
            if rc != ERROR_FILE_NOT_FOUND && rc != ERROR_PATH_NOT_FOUND {
                tracing::warn!(
                    error = rc.0,
                    r"HKLM\SOFTWARE\Punktfunk\Seats not read — seat display-slot reservation stays disabled"
                );
            }
            false
        }
    })
}

/// The machine's hardware-accelerated GPU scheduling setting: `HwSchMode` under
/// `GraphicsDrivers` is 2 for on and 1 for off, and absent where the driver's default stands.
/// It applies at boot, so this is the setting, not proof of the running state.
#[cfg(target_os = "windows")]
pub fn hags_setting() -> &'static str {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};

    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated literals; `value`/`size` are live out-params
    // sized for the DWORD the flags demand.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!(r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers"),
            windows::core::w!("HwSchMode"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(value).cast()),
            Some(&mut size),
        )
    };
    match (rc == ERROR_SUCCESS, value) {
        (false, _) => "unset",
        (true, 2) => "on",
        (true, 1) => "off",
        (true, _) => "unknown",
    }
}

/// The first Windows build with SDR wide colour on demand (`SET_WCG_STATE`) and the colour
/// report that names it: Windows 11 24H2.
pub const WCG_MIN_BUILD: u32 = 26100;

/// The OS build number (`CurrentBuildNumber`), `0` when unreadable.
#[cfg(target_os = "windows")]
pub fn os_build() -> u32 {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let mut buf = [0u16; 16];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: both strings are NUL-terminated literals; `buf`/`size` are live out-params and
    // `size` is the buffer's byte length, which the call never writes past.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            windows::core::w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return 0;
    }
    String::from_utf16_lossy(&buf)
        .trim_end_matches('\0')
        .parse()
        .unwrap_or(0)
}

/// Returns both session ids when this process is outside the active console.
/// That usually predicts inaccessible console display state. A seats host can
/// intentionally own an active RDP desktop instead, so callers decide how to
/// phrase the mismatch without hiding later display-operation errors.
#[cfg(target_os = "windows")]
pub fn console_session_mismatch() -> Option<(u32, u32)> {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    let own = own_session_id()?;
    // SAFETY: takes no arguments and returns the console session id by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    (console != 0xFFFF_FFFF && own != console).then_some((own, console))
}

/// The session this process runs in; `None` when Windows can't say.
#[cfg(target_os = "windows")]
pub fn own_session_id() -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut own: u32 = 0;
    // SAFETY: `own` is a live local out-param for this synchronous call; no pointer escapes it.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut own) }
        .ok()
        .map(|()| own)
}

#[cfg(test)]
mod tests {
    #[test]
    fn multi_sz_terminates_each_string_and_the_list() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let want = [w("a"), vec![0], w("bc"), vec![0, 0]].concat();
        assert_eq!(super::multi_sz(&["a", "bc"]), want);
    }
}
