//! DDC/CI panel power for the experimental `ddc_power_off` axis (Windows).
//!
//! VESA MCCS VCP 0xD6 over the video-cable I²C / DP-AUX bus. We command `0x04` (DPMS off —
//! panel and backlight dark, firmware still listening) so a cooperating monitor stops
//! no-signal auto-scan. Never `0x05` (power-button off): many monitors kill their DDC
//! controller and need a physical button to come back.
//!
//! Best-effort, warn-and-continue. Monitors without DDC, OSD-disabled DDC, docks/KVMs
//! that drop the channel, and laptop-internal panels (ACPI backlight) probe as
//! unsupported and are skipped. Each transaction can block for tens of ms — callers
//! run at session acquire/teardown, never on the frame path. `HMONITOR` is live only
//! while the display is on the desktop, so off runs before an Exclusive CCD isolate
//! and on after the restore.

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

const VCP_POWER_MODE: u8 = 0xD6;
const POWER_ON: u32 = 0x01;
/// 0x04 DPMS off (DDC stays up). Not 0x05: power-button off, many panels need a physical press after it.
const POWER_OFF: u32 = 0x04;

struct ActiveMonitor {
    hmon: HMONITOR,
    device: String,
}

/// `HMONITOR` is live only while the display is on the desktop — off before isolate, on after restore.
fn active_monitors() -> Vec<ActiveMonitor> {
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut windows::Win32::Foundation::RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: `data` is the `&mut Vec<ActiveMonitor>` passed by `active_monitors` below,
        // valid for the duration of the synchronous EnumDisplayMonitors call that invokes us.
        let out = unsafe { &mut *(data.0 as *mut Vec<ActiveMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // Pass the whole `MONITORINFOEXW`, not `&mut info.monitorInfo`. `cbSize` is 104 bytes;
        // `&mut info.monitorInfo` only covers the 40-byte prefix, so `szDevice` is out of
        // provenance and the compiler may keep the zeroed name. An empty name matches no
        // exclusion and `panel_off_except` would darken the spared panel.
        let p = (&raw mut info).cast::<windows::Win32::Graphics::Gdi::MONITORINFO>();
        // SAFETY: `hmon` is the live monitor handle the enumeration just handed us. `p` carries the
        // provenance of the full `MONITORINFOEXW`, so the OS may write all `cbSize` bytes it was
        // promised; `info` is a live local that outlives this synchronous call.
        if unsafe { GetMonitorInfoW(hmon, p) }.as_bool() {
            let len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            out.push(ActiveMonitor {
                hmon,
                device: String::from_utf16_lossy(&info.szDevice[..len]),
            });
        }
        true.into() // FALSE stops the walk
    }

    let mut out: Vec<ActiveMonitor> = Vec::new();
    // SAFETY: `collect` matches MONITORENUMPROC; `&mut out` outlives the synchronous enumeration
    // and is only dereferenced inside the callback (single-threaded — user32 invokes it inline).
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut Vec<ActiveMonitor> as isize),
        )
    };
    out
}

/// `device` is log-only.
fn set_power(hmon: HMONITOR, device: &str, value: u32) -> u32 {
    let mut n = 0u32;
    // SAFETY: `hmon` is a live monitor handle from the enumeration; `n` is a valid out-param.
    if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n) }.is_err() || n == 0 {
        return 0;
    }
    let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
    // SAFETY: `phys` is sized to exactly the count the API just reported for this handle.
    if unsafe { GetPhysicalMonitorsFromHMONITOR(hmon, &mut phys) }.is_err() {
        return 0;
    }
    let mut acked = 0u32;
    for p in &phys {
        // `PHYSICAL_MONITOR` is packed(1). Copy fields by value; a ref into a packed field is UB.
        let handle = p.hPhysicalMonitor;
        let desc_raw = p.szPhysicalMonitorDescription;
        let len = desc_raw
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc_raw.len());
        let desc = String::from_utf16_lossy(&desc_raw[..len]);
        // Probe first. A silent bus (no DDC, OSD-off, dock drop) fails here — never write unread.
        let (mut current, mut max) = (0u32, 0u32);
        // SAFETY: `handle` is the live physical-monitor handle (valid until
        // DestroyPhysicalMonitors below); the value pointers are valid locals ('None' for the
        // code-type out-param we don't need).
        let probe = unsafe {
            GetVCPFeatureAndVCPFeatureReply(
                handle,
                VCP_POWER_MODE,
                None,
                &mut current,
                Some(&mut max),
            )
        };
        if probe == 0 {
            tracing::debug!(
                device,
                monitor = desc,
                "DDC/CI: no reply to the power-mode (0xD6) probe — skipping (no DDC/CI, \
                 disabled in the OSD, or not passed through)"
            );
            continue;
        }
        // SAFETY: as the probe above — same live physical-monitor handle, plain value args.
        let set = unsafe { SetVCPFeature(handle, VCP_POWER_MODE, value) };
        if set == 0 {
            tracing::warn!(
                device,
                monitor = desc,
                value,
                "DDC/CI: power-mode set failed after a successful probe"
            );
        } else {
            tracing::info!(
                device,
                monitor = desc,
                from = current,
                to = value,
                "DDC/CI: panel power mode commanded"
            );
            acked += 1;
        }
    }
    // SAFETY: `phys` holds exactly the handles GetPhysicalMonitorsFromHMONITOR opened for us;
    // each is destroyed once, here.
    if let Err(e) = unsafe { DestroyPhysicalMonitors(&phys) } {
        tracing::debug!(device, "DDC/CI: DestroyPhysicalMonitors failed: {e}");
    }
    acked
}

/// Call while the physical displays are still on the desktop, immediately before Exclusive isolate.
pub fn panel_off_except(exclude_gdi: &str) -> u32 {
    let mut acked = 0;
    for m in active_monitors() {
        if m.device.eq_ignore_ascii_case(exclude_gdi) {
            continue;
        }
        acked += set_power(m.hmon, &m.device, POWER_OFF);
    }
    if acked == 0 {
        // INFO: the user opted into this axis, so a no-op is an answer. Laptop eDP/LVDS
        // has no DDC; brightness runs on the driver's own channel.
        tracing::info!(
            "DDC/CI: no panel accepted the DPMS-off command — the ddc_power_off axis did \
             nothing on this display set (internal eDP/LVDS panels expose no DDC/CI; external \
             monitors may have it disabled in the OSD or dropped by a dock/KVM)"
        );
    }
    acked
}

/// Call after CCD restore. Returning signal wakes most firmware; this covers the rest.
pub fn panel_on_all() -> u32 {
    let mut acked = 0;
    for m in active_monitors() {
        acked += set_power(m.hmon, &m.device, POWER_ON);
    }
    acked
}
