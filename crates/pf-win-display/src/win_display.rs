//! CCD (`QueryDisplayConfig`) and GDI helpers shared by virtual-display backends
//! and capturers: GDI-name resolution, HDR get/set, active-mode set, topology
//! isolate/restore.
//!
//! A pf-vdisplay `target_id` is a real OS target. Call topology mutators under
//! the manager `state` lock (serialization, not soundness). Pin callers on
//! [`force_extend_topology`], [`isolate_displays_ccd`], and
//! [`restore_displays_ccd`]. Evidence: `design/display-management.md`.

// Helpers are safe: Copy/borrowed in, owned out, FFI discharged inside.
// Unlocked reads race the topology mutator (stale answer, not UB).

// SAFETY: CCD contract — every Query/GetBufferSizes/Set below is an instance.
// GetDisplayConfigBufferSizes writes np/nm; QueryDisplayConfig fills buffers of
// those lengths via as_mut_ptr() and the same &mut counts. Vecs are locals; the
// API is synchronous and retains neither pointer. truncate is correctness, not
// safety. Arrays come only from QueryDisplayConfig or a SavedConfig this module
// built. SetDisplayConfig is serialized under the manager lock;
// retry_set_display_config binds the input desktop.
//
// UNION READS. modeInfoIdx / advanced-color `.value` overlay a u32 with a
// same-sized POD bitfield — every bit pattern is valid; modes.get(idx) is
// correctness. sourceMode is discriminated by sibling infoType; every read is
// guarded by infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE on the same
// DISPLAYCONFIG_MODE_INFO. Move that guard and the access is unjustified.
use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::QUERY_DISPLAY_CONFIG_FLAGS;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_2DREGION,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_RATIONAL,
    DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ALL_PATHS,
    QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_FORCE_MODE_ENUMERATION,
    SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, POINTL};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_RESET, CDS_TEST, CDS_UPDATEREGISTRY,
    DEVMODEW, DISP_CHANGE_FAILED, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

use punktfunk_core::Mode;

// The identity + inventory types live in the platform-neutral `snapshot` module (WP8) so the
// cache rules test everywhere; re-exported here so every existing `win_display::` path still works.
pub use crate::snapshot::{pack_luid_parts, CcdTargetKey, TargetInventory};

/// The key of the TARGET side of a CCD path.
pub(crate) fn path_target_key(p: &DISPLAYCONFIG_PATH_INFO) -> CcdTargetKey {
    CcdTargetKey::from_luid_parts(
        p.targetInfo.adapterId.LowPart,
        p.targetInfo.adapterId.HighPart,
        p.targetInfo.id,
    )
}

/// How a CCD read failed. A QUERY FAILURE is a distinct answer from an empty topology, and no
/// caller may fold the two: a watchdog that reads a failed query as "stable" mutates on unknown
/// state (see `isolate_displays_ccd`'s verify).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcdError {
    /// `GetDisplayConfigBufferSizes` failed (raw WIN32 code).
    Sizing(u32),
    /// `QueryDisplayConfig` failed, after the bounded resize budget (raw WIN32 code).
    Query(u32),
}

/// The one robust CCD reader every helper in this module goes through: size, allocate, query —
/// retrying `ERROR_INSUFFICIENT_BUFFER` from a FRESH sizing call (topology churn between the two
/// calls legitimately grows the arrays) under a bounded budget. Zero active paths is an ANSWER
/// (`Ok` with empty vecs), never a failure: `QueryDisplayConfig` REJECTS a zero-count call rather
/// than returning an empty set, so asking anyway would turn "nothing is active" (every panel
/// off/standby, a KVM switched away, headless between adapter arrival and first monitor) into
/// "the query failed". (Measured on .173: sizing succeeds with numPaths=0, the query then
/// returns 0x57 ERROR_INVALID_PARAMETER — 0x5 from session 0.)
pub fn query_display_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<SavedConfig, CcdError> {
    // 4 attempts: churn that grows the buffer between sizing and query several times in a row is
    // already pathological; the budget keeps a lying driver from looping us forever.
    let mut last = ERROR_INSUFFICIENT_BUFFER.0;
    for _ in 0..4 {
        let mut np = 0u32;
        let mut nm = 0u32;
        // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
        // locals the OS fills with the counts it wants for these flags.
        let rc = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if rc.is_err() {
            return Err(CcdError::Sizing(rc.0));
        }
        if np == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
        // elements from the sizing call above, and are handed over with those same counts.
        let rc = unsafe {
            QueryDisplayConfig(
                flags,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            last = rc.0;
            continue;
        }
        if rc.is_err() {
            return Err(CcdError::Query(rc.0));
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok((paths, modes));
    }
    Err(CcdError::Query(last))
}

/// Force the desktop into EXTEND topology - the programmatic equivalent of the Win+P / DisplaySwitch
/// "Extend" shortcut. Windows defaults a FRESHLY-ADDED monitor into CLONE/duplicate mode when a
/// physical display is already active (e.g. a laptop panel): a cloned IddCx output shares the panel's
/// source, so the OS never commits a distinct path for it, never calls ASSIGN_SWAPCHAIN, and capture
/// sees no frames (`resolve_gdi_name` stays `None` and the session fails "not an active display path").
/// Applying the EXTEND preset across the live set of connected displays makes the new IddCx monitor its
/// OWN active path, so the rest of bring-up (`resolve_gdi_name` -> `set_active_mode` ->
/// `isolate_displays_ccd`) proceeds. Best-effort + idempotent: a no-op on a single-display (already
/// sole/extended) box, so it is safe to call unconditionally. `rc == 0` is success.
pub fn force_extend_topology() {
    let rc = crate::input_desktop::retry_set_display_config(|| {
        // SAFETY: both arrays are None — the OS recomputes the preset; no
        // buffer/count pair. `retry_set_display_config` binds the input desktop.
        unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) }
    });
    if rc == 0 {
        tracing::info!(
            "display topology forced to EXTEND (a new IddCx monitor would otherwise be CLONED onto the \
             existing panel -> no distinct source -> no frames)"
        );
    } else {
        tracing::warn!("display force-EXTEND topology: SetDisplayConfig rc={rc:#x}");
    }
}

/// EXPLICITLY activate `target_id` into its own display path — the last-resort fallback when neither
/// the OS auto-activate nor the EXTEND topology preset lights a freshly-ADDed IDD target. Observed on
/// a lid-closed laptop (field report, Intel iGPU): the clamshell lid policy makes Windows skip the
/// new-monitor auto-activation AND the `SDC_TOPOLOGY_EXTEND` preset returns success without ever
/// committing a path for the IDD, so the target sits connected-but-inactive for the whole retry
/// budget (RDP/Parsec don't need a new console display path, which is why they still work there).
///
/// This is the supplied-config apply Windows' own display Settings uses to turn a monitor on: query
/// ALL paths, keep every currently-active path verbatim, and append the target's inactive path with a
/// source not already driving another display — mode indices invalidated so `SDC_ALLOW_CHANGES` lets
/// the OS pick modes for the new path. Returns `true` when the apply reports success; the caller
/// still re-polls [`resolve_gdi_name`] to confirm the path actually committed.
pub fn activate_target_path(key: CcdTargetKey) -> bool {
    let Ok((paths, modes)) = query_display_config(QDC_ALL_PATHS) else {
        return false;
    };

    // Active paths stay verbatim so their mode indices stay valid against `modes`.
    let mut supplied: Vec<DISPLAYCONFIG_PATH_INFO> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .copied()
        .collect();
    if supplied.iter().any(|p| path_target_key(p) == key) {
        return true; // already active — we raced the OS auto-activate
    }

    // Free source only: sharing one clones the IDD (no distinct source, no frames).
    let Some(cand) = paths.iter().find(|p| {
        path_target_key(p) == key
            && p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
            && !supplied.iter().any(|a| {
                (
                    a.sourceInfo.adapterId.LowPart,
                    a.sourceInfo.adapterId.HighPart,
                    a.sourceInfo.id,
                ) == (
                    p.sourceInfo.adapterId.LowPart,
                    p.sourceInfo.adapterId.HighPart,
                    p.sourceInfo.id,
                )
            })
    }) else {
        tracing::warn!(
            target = %key,
            "explicit path activation: no inactive path with a free source for this target"
        );
        return false;
    };
    let mut new_path = *cand;
    new_path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    new_path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    new_path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    supplied.push(new_path);

    // Persist so the next same-identity ADD auto-activates and skips this fallback.

    // SAFETY: CCD contract — slices, so pointer and length agree; both outlive
    // this synchronous call. `retry_set_display_config` binds the input desktop.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(supplied.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
        )
    });
    if rc == 0 {
        tracing::info!(
            target = %key,
            "explicit path activation: supplied-config apply succeeded (target committed alongside {} active path(s))",
            supplied.len() - 1
        );
        true
    } else {
        tracing::warn!(
            target = %key,
            "explicit path activation: SetDisplayConfig rc={rc:#x}"
        );
        false
    }
}

/// Resolve the `\\.\DisplayN` GDI name for a virtual-display target id via the CCD API. Returns `None`
/// until the OS activates the target into the desktop topology (needs a real WDDM GPU; on a
/// GPU-less box this stays `None` even though ADD succeeded).
pub fn resolve_gdi_name(key: CcdTargetKey) -> Option<String> {
    let (paths, _modes) = query_display_config(QDC_ONLY_ACTIVE_PATHS).ok()?;
    for p in &paths {
        if path_target_key(p) == key {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            // SAFETY: `header.size` is this struct's size_of; the OS may touch
            // that many bytes. The local outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } == 0 {
                let name = String::from_utf16_lossy(&src.viewGdiDeviceName);
                return Some(name.trim_end_matches('\u{0}').to_string());
            }
        }
    }
    None
}

/// The virtual display's CURRENT active resolution `(width, height)` via the GDI/CCD API, or `None` if the
/// target isn't an active display yet / the query fails. The IDD-push capturer sizes its ring to this
/// ACTUAL mode and polls it to recreate the ring when it changes — a fullscreen game can change the
/// virtual display's mode out from under the session-negotiated one (game-capture bug GB1).
///
/// Safe to call from any thread.
pub fn active_resolution(key: CcdTargetKey) -> Option<(u32, u32)> {
    active_mode(key).map(|(w, h, _)| (w, h))
}

/// The target's CURRENT active mode as `(width, height, refresh_hz)` — what the OS actually
/// committed, which is not always what was asked for: `set_active_mode` deliberately falls back to
/// the highest advertised refresh <= requested rather than losing the client's resolution. Callers
/// that RECORD a mode must record this, or they claim a refresh the display is not running.
pub fn active_mode(key: CcdTargetKey) -> Option<(u32, u32, u32)> {
    let gdi = resolve_gdi_name(key)?;
    let wname: Vec<u16> = gdi.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `wname` is a live NUL-terminated UTF-16 name; `&mut dm` is a
    // size-stamped DEVMODEW. The query only reads the name and fills `dm`.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool();
    if !ok || dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
        return None;
    }
    Some((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency))
}

/// Verified-state topology-settle wait (latency plan P0.2): poll the CCD state until the target is
/// actually COMMITTED — an active path exists (the GDI name resolves) and the active resolution
/// equals the requested one — instead of sleeping a fixed interval. The conditions are exactly what
/// `resolve_gdi_name`/`set_active_mode` already established once; this waits until the OS reports
/// them stable. `ceiling` (the old fixed sleep) is the worst-case bound: a mode the driver rejected
/// (`set_active_mode` left the OS default) or a slow third-party CCD-lock holder (SteelSeries
/// class) burns the ceiling and proceeds — behavior identical to the fixed sleep it replaces.
/// Returns `true` when the state verified (typical: one or two 25 ms polls), `false` on ceiling.
///
/// Call under the manager `state` lock like the callers it serve — a *serialization* requirement,
/// not a soundness one: reading topology unlocked races the mutator and yields a stale answer.
pub fn wait_mode_settled(key: CcdTargetKey, mode: Mode, ceiling: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        // `&&` short-circuits, so the second CCD query still does not run while the target has no
        // active path at all — this polls every 25 ms. (It was nested only so each call could carry
        // its own `unsafe` proof; both are safe fns now.)
        if resolve_gdi_name(key).is_some()
            && active_resolution(key) == Some((mode.width, mode.height))
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Re-commit with `SDC_FORCE_MODE_ENUMERATION` so the OS re-queries IddCx
/// modes after `IddCxMonitorUpdateModes2`. Call under the manager `state` lock.
pub fn force_mode_reenumeration() -> bool {
    let Some((paths, modes)) = query_active_config() else {
        return false;
    };
    // SAFETY: CCD contract — slices, so pointer and length agree; both outlive
    // this synchronous call. `retry_set_display_config` binds the input desktop.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
    if rc != 0 {
        tracing::debug!("force mode re-enumeration: SetDisplayConfig rc={rc:#x}");
    }
    rc == 0
}

/// Distinct resolutions `gdi_name` advertises (fallback when the request is absent).
pub fn advertised_resolutions(gdi_name: &str) -> Vec<(u32, u32)> {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut set = std::collections::BTreeSet::new();
    let mut i = 0u32;
    loop {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `wname` is a live NUL-terminated UTF-16 name; `&mut dm` is a
        // size-stamped DEVMODEW the API fills for index `i`. Both outlive the call.
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(wname.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        }
        .as_bool();
        if !ok {
            break;
        }
        set.insert((dm.dmPelsWidth, dm.dmPelsHeight));
        i += 1;
    }
    set.into_iter().collect()
}

/// Wait until `gdi_name` enumerates `mode`'s WxH@Hz, or `ceiling`. IddCx modes
/// land asynchronously after `IddCxMonitorUpdateModes2`. Refresh is part of
/// the match — WxH-only would skip a rate-only update.
pub fn wait_mode_advertised(gdi_name: &str, mode: Mode, ceiling: std::time::Duration) -> bool {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        let mut i = 0u32;
        loop {
            let mut dm = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: `wname` is a live NUL-terminated UTF-16 name; `&mut dm` is a
            // size-stamped DEVMODEW the API fills for index `i`. Both outlive the call.
            let ok = unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(wname.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(i),
                    &mut dm,
                )
            }
            .as_bool();
            if !ok {
                break;
            }
            if dm.dmPelsWidth == mode.width
                && dm.dmPelsHeight == mode.height
                && dm.dmDisplayFrequency == mode.refresh_hz
            {
                return true;
            }
            i += 1;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Monitor-departure wait (latency plan P0.3): after a REMOVE, poll until the target has left the
/// ACTIVE CCD set — two consecutive absent samples, so one transient query failure mid-teardown
/// can't read as "gone" — instead of sleeping the fixed departure settle. `ceiling` (the old fixed
/// sleep) bounds the worst case. The OS-side departure may still be finishing driver-side when the
/// CCD stops listing the target; the ADD path's ghost-reap retry (pf_vdisplay) remains the backstop
/// for that rare race, exactly as it was for a settle that expired. Returns `true` when departure
/// was observed, `false` on ceiling.
///
/// Call under the manager `state` lock like the callers it serves (serialization, not soundness).
pub fn wait_target_departed(key: CcdTargetKey, ceiling: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + ceiling;
    let mut absent_streak = 0u32;
    loop {
        // A failed query is unknown, not absent: it neither advances nor resets the streak.
        match query_display_config(QDC_ONLY_ACTIVE_PATHS) {
            Ok((paths, _)) if paths.iter().any(|p| path_target_key(p) == key) => {
                absent_streak = 0;
            }
            Ok(_) => {
                absent_streak += 1;
                if absent_streak >= 2 {
                    return true;
                }
            }
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Toggle the virtual-display target's advanced-color (HDR) state via the CCD API. Disabling HDR while on the
/// secure (Winlogon) desktop makes it render SDR/composed so DXGI Desktop Duplication can capture it
/// (the HDR fullscreen independent-flip otherwise storms `ACCESS_LOST` → black); re-enable on return so
/// WGC keeps HDR on the normal desktop. Returns true on a successful `DisplayConfigSetDeviceInfo`.
pub fn set_advanced_color(key: CcdTargetKey, enable: bool) -> bool {
    let Ok((paths, _modes)) = query_display_config(QDC_ONLY_ACTIVE_PATHS) else {
        return false;
    };
    for p in &paths {
        if path_target_key(p) == key {
            let mut s = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE::default();
            s.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
            s.header.size = size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32;
            s.header.adapterId = p.targetInfo.adapterId;
            s.header.id = p.targetInfo.id;
            s.Anonymous.value = enable as u32; // bit 0 = enableAdvancedColor
                                               // SAFETY: `header.size` is this struct's size_of; adapterId/id copied
                                               // from the matched path. The OS reads that many bytes and retains nothing.
            let rc = unsafe { DisplayConfigSetDeviceInfo(&s.header) };
            tracing::debug!(
                target = %key,
                enable,
                rc,
                "virtual-display set advanced-color (HDR) state"
            );
            return rc == 0;
        }
    }
    tracing::warn!(
        target = %key,
        "virtual-display advanced-color: target not in active paths"
    );
    false
}

/// `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` bits: 1 = advancedColorEnabled, 2 = wideColorEnforced.
/// Advanced colour with wide colour enforced is SDR auto colour management, not HDR.
fn hdr_active(bits: u32) -> bool {
    bits & 0x2 != 0 && bits & 0x4 == 0
}

/// Read the virtual-display target's CURRENT advanced-color (HDR) state via the CCD API — i.e. whether HDR is
/// actually ON for the virtual display right now (e.g. because the user toggled it in Windows display
/// settings). The capture/encode pipeline follows the monitor's real colorspace (WGC → FP16 → NVENC
/// Main10 BT.2020 PQ), so this is the authoritative "is this an HDR session" signal — NOT the
/// handshake-negotiated bit depth. `None` when the query fails or the target isn't in the active-path
/// list (both happen transiently during a display-topology re-probe): the caller decides the fallback —
/// the capture loop's poller keeps the last known value, since reading a blip as "HDR off" used to cost
/// an HDR session TWO spurious ring recreates (false, then true again a poll later).
pub fn advanced_color_enabled(key: CcdTargetKey) -> Option<bool> {
    let (paths, _modes) = query_display_config(QDC_ONLY_ACTIVE_PATHS).ok()?;
    for p in &paths {
        if path_target_key(p) == key {
            let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            info.header.size = size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            info.header.adapterId = p.targetInfo.adapterId;
            info.header.id = p.targetInfo.id;
            // SAFETY: `header.size` is this struct's size_of; the OS may touch
            // that many bytes. The local outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0 {
                // SAFETY: POD union — `value` overlays a same-sized bitfield.
                return Some(hdr_active(unsafe { info.Anonymous.value }));
            }
            return None;
        }
    }
    None
}

/// Re-apply the current mode with `CDS_RESET` — a same-mode write is otherwise
/// a no-op. Restarts presentation after DWM stops composing to a virtual
/// display. Same input-desktop retry as [`set_active_mode`].
pub fn force_mode_reset(gdi_name: &str) -> bool {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `wname` is a live NUL-terminated UTF-16 name; `&mut dm` is a
    // size-stamped DEVMODEW. The query only reads the name and fills `dm`.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool();
    if !ok {
        tracing::warn!("{gdi_name}: force_mode_reset — no current mode to re-apply");
        return false;
    }
    // SAFETY: same liveness as the query; CDS_RESET re-applies the identical
    // mode; trailing args are null; the API only reads. Off the input desktop
    // CDS returns DISP_CHANGE_FAILED, so the retry binds that desktop.
    let rc = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_RESET, None)
        },
    );
    if rc != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!(
            result = rc.0,
            "{gdi_name}: force_mode_reset rejected ({})",
            disp_change_reason(rc.0)
        );
        return false;
    }
    tracing::info!("{gdi_name}: forced same-mode reset applied (presentation restart)");
    true
}

/// Put `key`'s path on `mode` through CCD, supplying the source mode outright.
///
/// [`set_active_mode`] goes through GDI, which can only pick a mode `EnumDisplaySettings`
/// already lists — and the OS pins that list when a monitor arrives. A driver that has just
/// published a new list (`IOCTL_UPDATE_MODES`) is therefore unreachable that way, which is why a
/// resize used to need a hotplug. CCD takes the source size as data instead of choosing from a
/// list, so the mode lands on the live path: same target, same swap chain, no re-arrival.
///
/// Both halves are supplied — source size and target timing — because the OS answers
/// `ERROR_BAD_CONFIGURATION` rather than searching for whatever the caller left out.
/// `false` if the target has no active path or the apply failed.
pub fn set_active_mode_ccd(key: CcdTargetKey, mode: Mode) -> bool {
    let Ok((mut paths, mut modes)) = query_display_config(QDC_ONLY_ACTIVE_PATHS) else {
        return false;
    };
    let Some(pi) = paths.iter().position(|p| path_target_key(p) == key) else {
        tracing::warn!(target = %key, "ccd mode set: no active path for this target");
        return false;
    };
    // SAFETY: the CCD contract at the top of this file — on an ACTIVE path the union carries
    // `modeInfoIdx`, and this path came back from `QDC_ONLY_ACTIVE_PATHS`.
    let src_idx = unsafe { paths[pi].sourceInfo.Anonymous.modeInfoIdx } as usize;
    if src_idx >= modes.len() || modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
        tracing::warn!(target = %key, "ccd mode set: the path carries no source mode to rewrite");
        return false;
    }
    // The `infoType` guard above is what makes `sourceMode` the live arm of this union; move it
    // and these writes land on a target mode instead.
    modes[src_idx].Anonymous.sourceMode.width = mode.width;
    modes[src_idx].Anonymous.sourceMode.height = mode.height;
    // Supply the target timing too. Leaving it out makes the OS search for a workable one and
    // answer ERROR_BAD_CONFIGURATION (0x64a) when it cannot; these are the numbers the driver
    // advertises for this mode, so the search is not needed. `v_sync_freq_divider` is 1 for a
    // target mode, per the DDI contract the driver builds its own mode list against.
    // SAFETY: the CCD contract — an ACTIVE path carries `modeInfoIdx` in this union.
    let tgt_idx = unsafe { paths[pi].targetInfo.Anonymous.modeInfoIdx } as usize;
    if tgt_idx < modes.len() && modes[tgt_idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET {
        let region = DISPLAYCONFIG_2DREGION {
            cx: mode.width,
            cy: mode.height,
        };
        let hz = u64::from(mode.refresh_hz);
        // SAFETY: guarded by `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET` immediately above.
        unsafe {
            let si = &mut modes[tgt_idx].Anonymous.targetMode.targetVideoSignalInfo;
            si.pixelRate = hz * u64::from(mode.width) * u64::from(mode.height);
            si.hSyncFreq = DISPLAYCONFIG_RATIONAL {
                Numerator: u32::try_from(hz * u64::from(mode.height)).unwrap_or(u32::MAX),
                Denominator: 1,
            };
            si.vSyncFreq = DISPLAYCONFIG_RATIONAL {
                Numerator: mode.refresh_hz,
                Denominator: 1,
            };
            si.totalSize = region;
            si.activeSize = region;
            si.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
            si.Anonymous.videoStandard = 255 | (1 << 16);
        }
    } else {
        paths[pi].targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    }
    // SAFETY: CCD contract — slices, so pointer and length agree, and both outlive this
    // synchronous call. `retry_set_display_config` binds the input desktop.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    });
    if rc != 0 {
        tracing::warn!(
            target = %key,
            "ccd mode set to {}x{}: SetDisplayConfig rc={rc:#x}{}",
            mode.width,
            mode.height,
            sdc_access_denied_hint(rc)
        );
        return false;
    }
    true
}

/// Force `gdi_name` to `mode`. ADD only advertises; Windows otherwise lights
/// an IDD at 1280×720. `CDS_TEST` first so an unadvertised mode leaves the
/// default instead of failing the session. A refresh the OS does not list
/// clamps to the nearest lower one, and warns: the client asked for a rate
/// this display will not run. `false` if nothing was written — callers using
/// this as [`set_active_mode_ccd`]'s fallback owe that a log, or a display
/// left on the wrong mode looks like it was never asked.
pub fn set_active_mode(gdi_name: &str, mode: Mode) -> bool {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();

    // Prefer same WxH: exact Hz, else highest advertised ≤ requested, else
    // highest at that resolution. A clamped pixel-rate must not collapse to
    // the 1280×720 OS default.
    let mut at_res: Vec<u32> = Vec::new();
    let mut res_set: std::collections::BTreeSet<(u32, u32)> = std::collections::BTreeSet::new();
    let mut i = 0u32;
    loop {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `wname` is a live NUL-terminated UTF-16 name; `&mut dm` is a
        // size-stamped DEVMODEW filled for index `i`. Both outlive the call.
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(wname.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        }
        .as_bool();
        if !ok {
            break;
        }
        i += 1;
        res_set.insert((dm.dmPelsWidth, dm.dmPelsHeight));
        if dm.dmPelsWidth == mode.width && dm.dmPelsHeight == mode.height {
            at_res.push(dm.dmDisplayFrequency);
        }
    }
    let chosen_hz = if at_res.contains(&mode.refresh_hz) {
        mode.refresh_hz
    } else if let Some(hz) = at_res
        .iter()
        .copied()
        .filter(|&hz| hz <= mode.refresh_hz)
        .max()
    {
        hz
    } else if let Some(hz) = at_res.iter().copied().max() {
        hz
    } else {
        mode.refresh_hz // not advertised; attempt anyway (likely OS default)
    };
    if at_res.is_empty() {
        tracing::warn!(
            "{gdi_name}: driver advertises no {}x{} mode (top advertised: {:?}); attempting @{} anyway",
            mode.width,
            mode.height,
            res_set.iter().rev().take(8).collect::<Vec<_>>(),
            mode.refresh_hz
        );
    } else if chosen_hz != mode.refresh_hz {
        tracing::warn!(
            "{gdi_name}: {}x{}@{} not advertised; using {}x{}@{} (advertised refreshes here: {:?})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            mode.width,
            mode.height,
            chosen_hz,
            at_res
        );
    }

    // This output only: size/refresh/bpp, no DM_POSITION, no PRIMARY.
    // CDS_SET_PRIMARY while another display is live storms DXGI_ERROR_MODE_CHANGE_IN_PROGRESS.
    let dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL,
        dmBitsPerPel: 32,
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: chosen_hz,
        ..Default::default()
    };
    // SAFETY: `wname` is a live NUL-terminated UTF-16 name and `&dm` a live
    // DEVMODEW; both outlive the call. CDS_TEST only validates; trailing args
    // are null; the API only reads. DISP_CHANGE_FAILED off the input desktop
    // (UAC/lock) is not "unsupported" — retry bound to that desktop.
    let test = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_TEST, None)
        },
    );
    if test != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!(
            result = test.0,
            "{gdi_name}: mode-set {}x{}@{} rejected ({}) — leaving OS default",
            mode.width,
            mode.height,
            chosen_hz,
            disp_change_reason(test.0)
        );
        return false;
    }
    // SAFETY: same inputs as the CDS_TEST above; both outlive the call.
    // CDS_UPDATEREGISTRY applies the already-validated mode; API only reads.
    // The two calls bind independently if the secure desktop comes up between them.
    let apply = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wname.as_ptr()),
                Some(&dm),
                None,
                CDS_UPDATEREGISTRY,
                None,
            )
        },
    );
    if apply == DISP_CHANGE_SUCCESSFUL {
        tracing::info!(
            "{gdi_name}: active mode set to {}x{}@{}",
            mode.width,
            mode.height,
            chosen_hz
        );
        return true;
    }
    tracing::warn!(
        result = apply.0,
        "{gdi_name}: {}x{}@{} not applied ({})",
        mode.width,
        mode.height,
        chosen_hz,
        disp_change_reason(apply.0)
    );
    false
}

/// Decode a failed `ChangeDisplaySettingsExW`. `BADMODE` = not advertised;
/// `FAILED` = write rejected. `FAILED` splits: no console session vs wrong
/// desktop (UAC/lock). See [`sdc_access_denied_hint`].
fn disp_change_reason(rc: i32) -> &'static str {
    match rc {
        -1 if crate::input_desktop::input_desktop_is_secure() => {
            "DISP_CHANGE_FAILED: the SECURE desktop owns input — a UAC consent prompt, the lock \
             screen or the logon screen is up, and display writes are refused off it. Dismiss the \
             prompt on the host"
        }
        -1 => {
            "DISP_CHANGE_FAILED: the display write was rejected — a host without console-session \
             access (disconnected RDP session / non-console session) fails ALL display writes \
             this way"
        }
        -2 => "DISP_CHANGE_BADMODE: the display does not advertise this mode",
        -3 => "DISP_CHANGE_NOTUPDATED: registry write failed",
        -4 => "DISP_CHANGE_BADFLAGS",
        -5 => "DISP_CHANGE_BADPARAM",
        _ => "unrecognized DISP_CHANGE code",
    }
}

/// Extra text for `SetDisplayConfig` `ERROR_ACCESS_DENIED` (0x5) only. The
/// same rc is disconnected-RDP *or* off the input desktop; naming only the
/// first chases the wrong fix. Seeing this means the input-desktop retry failed too.
fn sdc_access_denied_hint(rc: i32) -> &'static str {
    if rc != 5 {
        return "";
    }
    if crate::input_desktop::input_desktop_is_secure() {
        " (ERROR_ACCESS_DENIED: the SECURE desktop owns input — a UAC consent prompt, the lock \
         screen or the logon screen is up, and display writes are refused off it. Dismiss the \
         prompt on the host)"
    } else {
        " (ERROR_ACCESS_DENIED: the host has no console-session access — disconnected RDP \
         session? run via the installed service so it tracks the console session)"
    }
}

/// Saved active topology, restored on teardown.
pub type SavedConfig = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

/// `DISPLAYCONFIG_PATH_ACTIVE` (wingdi.h). Not exported by the `windows` crate.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

/// `DISPLAYCONFIG_PATH_MODE_IDX_INVALID` (wingdi.h) — no mode pinned; with
/// `SDC_ALLOW_CHANGES` the OS picks. Not exported by the `windows` crate.
const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xffff_ffff;

/// Current active paths + modes. `None` on API failure.
fn query_active_config() -> Option<SavedConfig> {
    // The zero-path answer (empty-but-`Some`) and the insufficient-buffer retry both live in
    // `query_display_config`; this wrapper keeps the legacy `Option` shape for the callers whose
    // `None` genuinely means "the query failed" — and is the ONE place that failure is logged.
    match query_display_config(QDC_ONLY_ACTIVE_PATHS) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(?e, "CCD active-config query failed");
            None
        }
    }
}

/// Count currently-ACTIVE display paths whose target id is not in `keep_target_ids` — i.e. displays
/// that would still be lit besides the managed virtual set. `None` on query failure. Used to VERIFY
/// isolation actually took, and (in the `primary` topology) to detect a physical that is ALREADY
/// active so we can skip a force-EXTEND that would reset its refresh.
pub fn count_other_active(keep: &[CcdTargetKey]) -> Option<u32> {
    let (paths, _) = query_active_config()?;
    Some(
        paths
            .iter()
            .filter(|p| {
                !keep.contains(&path_target_key(p)) && p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
            })
            .count() as u32,
    )
}

/// EDID manufacturer id as it appears in the CCD path (`\\?\DISPLAY#PNK…`).
/// Driver stamps `"PNK"` into EDID bytes 8-9 (`pf-vdisplay` `edid.rs`).
const PF_EDID_MANUFACTURER: &str = "PNK";

/// True when the CCD monitor path is one of ours (`PNK` in the PnP id).
/// Our IddCx target declares HDMI, so [`output_tech_class`] would count it
/// as a physical panel and the dark-desk backstop would never fire. Unknown
/// is not ours (do not adopt a third-party virtual display).
fn is_our_virtual_display(monitor_device_path: &str) -> bool {
    monitor_device_path
        .to_ascii_uppercase()
        .contains(PF_EDID_MANUFACTURER)
}

/// `(external physical?, log label)`. Allowlist: unknown/indirect is not
/// external, so a third-party virtual display is never a physical suspect.
fn output_tech_class(tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> (bool, &'static str) {
    match tech {
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI => (true, "HDMI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL => (true, "DisplayPort"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI => (true, "DVI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15 => (true, "VGA"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL => (true, "UDI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI => (true, "SDI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO => (true, "component"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO => (true, "composite"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO => (true, "S-Video"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE => (true, "TV-dongle"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => (false, "internal-panel"),
        _ => (false, "virtual/other"),
    }
}

fn utf16z_str(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Connected targets (`QDC_ALL_PATHS`, unique by adapter+id). Read-only CCD;
/// can serialize on the display-config lock — keep it off the capture thread. Empty on a failed
/// query; the actor ([`crate::display_events`]) uses [`target_inventory_checked`] to tell that
/// apart from an empty topology.
pub fn target_inventory() -> Vec<TargetInventory> {
    target_inventory_checked().unwrap_or_default()
}

/// [`target_inventory`] with the query failure kept distinct from "no targets" (`Err`) — the
/// display actor keeps its last-known-good snapshot on `Err` and publishes an empty one on `Ok`.
pub fn target_inventory_checked() -> Result<Vec<TargetInventory>, CcdError> {
    let (paths, modes) = query_display_config(QDC_ALL_PATHS)?;
    // Targets driven by an ACTIVE path, by complete key (target ids are only unique per adapter).
    let active: Vec<CcdTargetKey> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .map(path_target_key)
        .collect();
    let mut seen: Vec<CcdTargetKey> = Vec::new();
    let mut out = Vec::new();
    for p in &paths {
        let t = &p.targetInfo;
        let key = path_target_key(p);
        // `targetAvailable` == a monitor is connected; an ACTIVE target is included regardless
        // (the flag reads FALSE transiently right after a removal).
        if (!t.targetAvailable.as_bool() && !active.contains(&key)) || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        req.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        req.header.adapterId = t.adapterId;
        req.header.id = t.id;
        // SAFETY: `header.size` is this struct's size_of; the OS may touch
        // that many bytes. The local outlives this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
            continue; // no queryable monitor — nothing to attribute
        }
        let monitor_device_path = utf16z_str(&req.monitorDevicePath);
        let (mut external_physical, mut tech) = output_tech_class(req.outputTechnology);
        // Our IddCx monitor claims HDMI; connector class would call it a panel.
        let ours = is_our_virtual_display(&monitor_device_path);
        if ours {
            external_physical = false;
            tech = "punktfunk-virtual";
        }
        let is_active = active.contains(&key);
        // Inactive paths have INVALID modeInfoIdx — stay zeroed, do not index 0xffffffff.
        let (mut gdi_name, mut x, mut y, mut width, mut height) =
            (String::new(), 0i32, 0i32, 0u32, 0u32);
        let (mut hdr, mut source_id, mut source_adapter_luid) = (None, 0u32, 0i64);
        let mut sdr_white_level = None;
        if is_active {
            source_id = p.sourceInfo.id;
            source_adapter_luid = pack_luid_parts(
                p.sourceInfo.adapterId.LowPart,
                p.sourceInfo.adapterId.HighPart,
            );
            let mut ac = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            ac.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            ac.header.size = size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            ac.header.adapterId = t.adapterId;
            ac.header.id = t.id;
            // SAFETY: `header.size` is this struct's size_of; the OS may touch that many bytes.
            // The local outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut ac.header) } == 0 {
                // SAFETY: POD union — `value` overlays a same-sized bitfield.
                hdr = Some(hdr_active(unsafe { ac.Anonymous.value }));
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            white.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            white.header.size = size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
            white.header.adapterId = t.adapterId;
            white.header.id = t.id;
            // SAFETY: `header.size` is this struct's size_of; the OS may touch that many bytes.
            // The local outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } == 0
                && white.SDRWhiteLevel > 0
            {
                sdr_white_level = Some(white.SDRWhiteLevel);
            }
            // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
            // every bit pattern is valid. Bounds-checked index below.
            let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
            if let Some(m) = modes.get(idx) {
                if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    // SAFETY: `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE`
                    // on this same entry is the discriminant for `sourceMode`.
                    let sm = unsafe { m.Anonymous.sourceMode };
                    x = sm.position.x;
                    y = sm.position.y;
                    width = sm.width;
                    height = sm.height;
                }
            }
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            // SAFETY: `header.size` is this struct's size_of; the OS may write
            // that many bytes. The local outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } == 0 {
                gdi_name = utf16z_str(&src.viewGdiDeviceName);
            }
        }
        // mHz keeps 59.94 distinct from 60 without a float.
        let refresh_mhz = match t.refreshRate.Denominator {
            0 => 0,
            d => (u64::from(t.refreshRate.Numerator) * 1000 / u64::from(d)) as u32,
        };
        out.push(TargetInventory {
            key,
            target_id: t.id,
            active: is_active,
            external_physical,
            internal_panel: tech == "internal-panel",
            tech,
            friendly: utf16z_str(&req.monitorFriendlyDeviceName),
            monitor_device_path,
            ours,
            gdi_name,
            primary: is_active && x == 0 && y == 0,
            x,
            y,
            width,
            height,
            refresh_mhz,
            hdr,
            sdr_white_level,
            source_id,
            source_adapter_luid,
        });
    }
    Ok(out)
}

/// Crash-recovery journal for exclusive isolate: a marker so a fresh host can
/// undo a dead one's deactivated physicals.
///
/// The pre-isolate snapshot is process memory only; the isolated topology is
/// never written to the CCD database. A crash therefore leaves the desk dark.
/// Same shape as [`monitor_devnode`](crate::monitor_devnode): mark while live,
/// clear on a clean restore, re-light at startup if a marker survived.
///
/// Recover with the EXTEND preset, not a saved CCD blob: that blob pins the
/// dead virtual's target ids (`ERROR_BAD_CONFIGURATION`), and those ids are
/// stale after a reboot.
pub mod isolate_journal {
    use std::sync::Mutex;

    use super::CcdTargetKey;

    /// What we last wrote, so the exclusive re-assert watchdog's repeat isolates don't rewrite the
    /// file every couple of seconds. `None` = "no marker known to be on disk".
    static LAST: Mutex<Option<Vec<CcdTargetKey>>> = Mutex::new(None);

    fn path() -> std::path::PathBuf {
        pf_paths::config_dir().join("display-isolate-active.json")
    }

    /// Record that `deactivated` physicals are off for a live exclusive isolate.
    /// Best-effort: a journal we cannot write costs crash recovery, not the session.
    ///
    /// The on-disk schema is the KEYED one — `[[adapter_luid, target_id], …]`. The pre-key
    /// `[target_id, …]` shape is still READ (one compatibility release, see [`pending`]) but
    /// never written.
    pub fn mark(deactivated: &[CcdTargetKey]) {
        if deactivated.is_empty() {
            return; // nothing deactivated — nothing for a later host to put back
        }
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() == Some(deactivated) {
            return;
        }
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = pf_paths::create_private_dir(dir);
        }
        let rows: Vec<(i64, u32)> = deactivated
            .iter()
            .map(|k| (k.adapter_luid, k.target_id))
            .collect();
        match std::fs::write(&p, serde_json::to_vec_pretty(&rows).unwrap_or_default()) {
            Ok(()) => *last = Some(deactivated.to_vec()),
            Err(e) => tracing::warn!(
                error = %e,
                "display isolate: could not write the crash-recovery journal — if this host dies \
                 mid-session the deactivated panels will stay dark"
            ),
        }
    }

    /// Drop the marker. Idempotent.
    pub fn clear() {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::remove_file(path());
        *last = None;
    }

    /// If a previous host left exclusive isolate live, re-light with EXTEND.
    /// Call once, early in `serve`, before any session. Gated on the marker,
    /// not "is anything active", so a headless host is never forced awake.
    pub fn startup_recover() {
        let Some(targets) = pending() else {
            return;
        };
        tracing::warn!(
            deactivated = ?targets,
            "display isolate: a previous host exited with the operator's display(s) deactivated for \
             an EXCLUSIVE session and never restored them — forcing the EXTEND preset so the desk is \
             not left dark"
        );
        super::force_extend_topology();
        clear();
    }

    /// The marker a previous host left behind, if any (its deactivated targets) — the *decision*
    /// half of [`startup_recover`], split out so the recovery rule is testable without driving a
    /// real `SetDisplayConfig` against the machine running the test.
    ///
    /// Reads the keyed schema first, then the pre-key `Vec<u32>` one (a marker written by the
    /// previous release; its keys carry `adapter_luid = 0`, which is fine — the ids only feed a
    /// log line before the EXTEND recovery). The old decode goes away after one release.
    pub fn pending() -> Option<Vec<CcdTargetKey>> {
        let bytes = std::fs::read(path()).ok()?;
        if let Ok(rows) = serde_json::from_slice::<Vec<(i64, u32)>>(&bytes) {
            return Some(
                rows.into_iter()
                    .map(|(luid, id)| CcdTargetKey::new(luid, id))
                    .collect(),
            );
        }
        let old: Vec<u32> = serde_json::from_slice(&bytes).unwrap_or_default();
        Some(old.into_iter().map(|id| CcdTargetKey::new(0, id)).collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `PUNKTFUNK_CONFIG_DIR` and `LAST` are process-global — cases must not interleave.
        static ENV: Mutex<()> = Mutex::new(());

        fn with_temp_dir(name: &str, f: impl FnOnce(&std::path::Path)) {
            let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("pf-isolate-journal-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            // SAFETY: `_g` holds ENV, which serializes every test that
            // reads or writes `PUNKTFUNK_CONFIG_DIR` in this binary.
            unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", &dir) };
            clear(); // reset LAST and any leftover marker
            f(&dir);
            clear();
            // SAFETY: still under `_g` — same serialization as the set above.
            unsafe { std::env::remove_var("PUNKTFUNK_CONFIG_DIR") };
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The crash path: a host marks what it switched off and dies. The next start must see the
        /// marker (and which targets), which is what makes it force the desk back on.
        fn k(luid: i64, id: u32) -> CcdTargetKey {
            CcdTargetKey::new(luid, id)
        }

        #[test]
        fn a_mark_survives_for_the_next_host_and_clear_retracts_it() {
            with_temp_dir("roundtrip", |_| {
                assert_eq!(pending(), None, "a clean box owes no recovery");
                mark(&[k(7, 101), k(9, 202)]);
                assert_eq!(
                    pending(),
                    Some(vec![k(7, 101), k(9, 202)]),
                    "a crashed host's marker must be readable by the next start"
                );
                clear();
                assert_eq!(pending(), None, "a clean teardown retracts the marker");
            });
        }

        /// A marker written by the PREVIOUS release (`[target_id, …]`) must still trigger
        /// recovery — read for one compatibility release, mapped to zero-LUID keys (the ids
        /// only feed a log line).
        #[test]
        fn an_old_bare_id_marker_still_asks_for_recovery() {
            with_temp_dir("oldschema", |dir| {
                std::fs::write(dir.join("display-isolate-active.json"), b"[101, 202]").unwrap();
                assert_eq!(pending(), Some(vec![k(0, 101), k(0, 202)]));
            });
        }

        /// An isolate that deactivated nothing (single-display box: the virtual output is already
        /// the only head) owes the next start no force-EXTEND — marking there would re-arrange a
        /// desk we never touched.
        #[test]
        fn deactivating_nothing_writes_no_marker() {
            with_temp_dir("empty", |_| {
                mark(&[]);
                assert_eq!(pending(), None);
            });
        }

        /// The keyed schema is what lands on disk: `[[adapter_luid, target_id], …]`.
        #[test]
        fn the_marker_is_written_keyed() {
            with_temp_dir("keyed", |dir| {
                mark(&[k(0x1f, 4352)]);
                let bytes = std::fs::read(dir.join("display-isolate-active.json")).unwrap();
                let rows: Vec<(i64, u32)> = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(rows, vec![(0x1f, 4352)]);
            });
        }

        /// The re-assert watchdog re-isolates every couple of seconds while something fights it;
        /// that must not mean a disk write per cycle.
        #[test]
        fn repeating_the_same_mark_does_not_rewrite_the_file() {
            with_temp_dir("cached", |dir| {
                let file = dir.join("display-isolate-active.json");
                mark(&[k(1, 7)]);
                // Overwrite behind the journal's back rather than comparing mtimes — a filesystem
                // whose timestamp resolution is coarser than two back-to-back writes would let an
                // mtime assertion pass without proving anything.
                std::fs::write(&file, b"SENTINEL").unwrap();
                mark(&[k(1, 7)]);
                assert_eq!(
                    std::fs::read(&file).unwrap(),
                    b"SENTINEL",
                    "an unchanged mark must not rewrite the journal"
                );
                // A CHANGED set still lands — the group grew/shrank and recovery must follow it.
                mark(&[k(1, 7), k(1, 8)]);
                assert_eq!(pending(), Some(vec![k(1, 7), k(1, 8)]));
            });
        }

        /// File existence is the signal; contents are diagnostics. Corrupt
        /// still asks for recovery.
        #[test]
        fn an_unparseable_marker_still_asks_for_recovery() {
            with_temp_dir("corrupt", |dir| {
                std::fs::write(dir.join("display-isolate-active.json"), b"{ not json").unwrap();
                assert_eq!(pending(), Some(Vec::new()));
            });
        }
    }
}

/// Robust display isolation via the CCD API. The naive GDI approach (EnumDisplayDevices +
/// ChangeDisplaySettings) MISSES displays on a hybrid box — an iGPU-attached physical monitor isn't
/// flagged `ATTACHED_TO_DESKTOP` in the GDI enum, so it's never detached and the secure desktop /
/// lock screen lands on IT while our virtual output freezes. `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`
/// sees every active path; we deactivate all of them EXCEPT the managed virtual target **set**
/// (`design/display-management.md` §6.1: "exclusive" means the managed set stays active — with
/// parallel displays a sibling slot is never deactivated), leaving the virtual display(s) as the sole
/// desktop so ALL content (incl. Winlogon) renders to them. Apollo isolates the same way (CCD).
/// Re-issued with the grown/shrunk set on each slot add/remove while the group lives; the FIRST call's
/// returned config is what teardown restores (the caller keeps it on the group record and discards
/// later returns). Returns the original active config to restore on teardown.
// pub so vdisplay::pf_vdisplay can reuse this backend-neutral CCD isolation helper
// (it operates on real OS target ids — a pf-vdisplay monitor's target_id qualifies).
pub fn isolate_displays_ccd(keep: &[CcdTargetKey]) -> Option<SavedConfig> {
    isolate_displays_ccd_checked(keep).map(|(saved, _)| saved)
}

/// What [`isolate_displays_ccd_checked`] observed — the truthful input a re-assert watchdog or a
/// recovery generation needs (immunity plan WP10 item 4: a recovery generation moves only after
/// an OBSERVED change, never because a `SetDisplayConfig` was attempted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolateOutcome {
    /// The verification read showed only the keep set active. `deactivated` is how many paths
    /// THIS call switched off on the successful attempt (0 = the desktop already matched).
    Verified { attempts: u32, deactivated: u32 },
    /// No display path was active at all — nothing to isolate.
    NothingActive,
    /// Every attempt left a non-keep path active or its verification read failed: state UNKNOWN.
    Unverified { attempts: u32 },
}

/// [`isolate_displays_ccd`] with the outcome kept distinct from the restore snapshot: `None` only
/// when the initial CCD read failed (nothing was attempted); otherwise the pre-isolate config plus
/// what the verification observed. Callers that mutate on the strength of the result (the
/// exclusive re-assert watchdog) read the outcome; acquire keeps the `Option<SavedConfig>` shape.
pub fn isolate_displays_ccd_checked(
    keep: &[CcdTargetKey],
) -> Option<(SavedConfig, IsolateOutcome)> {
    // Snapshot the ORIGINAL active config ONCE for restore-on-teardown, before any changes.
    let saved = query_active_config()?;

    // Empty snapshot: nothing to deactivate. A zero-path SetDisplayConfig is
    // rejected; retries would only sleep on a topology we already want.
    if saved.0.is_empty() {
        tracing::info!(
            "display isolate (CCD): no display path is active — nothing to isolate for target set \
             {keep:?} (every panel off/standby, or a headless host)"
        );
        return Some((saved, IsolateOutcome::NothingActive));
    }

    // Journal what we are about to switch off BEFORE the first apply, not after a verified one: the
    // window this exists to cover includes dying mid-apply. `saved.0` is the ACTIVE path set
    // (QDC_ONLY_ACTIVE_PATHS), so everything in it outside the keep set is exactly what teardown
    // owes the operator back. See `isolate_journal`.
    let doomed: Vec<CcdTargetKey> = saved
        .0
        .iter()
        .map(path_target_key)
        .filter(|k| !keep.contains(k))
        .collect();
    isolate_journal::mark(&doomed);

    // Re-query and re-apply until only the keep set is active. One apply can
    // leave a panel lit; the lock screen must not land there.
    for attempt in 1..=4u32 {
        // An earlier attempt may already have switched panels off, so `saved` must survive a
        // failed re-query: the churn it caused is why this loop retries at all.
        let Some((mut paths, mut modes)) = query_active_config() else {
            tracing::warn!(
                "display isolate (CCD): re-query FAILED before attempt {attempt}/4 — retrying"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
            continue;
        };
        let mut others = 0u32;
        for p in paths.iter_mut() {
            if keep.contains(&path_target_key(p)) {
                continue;
            }
            if p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                // Inactive AND unpin both mode indexes — leaving them pointing
                // at queried entries rejects the whole config with 0x57. The
                // all-ones sentinel also invalidates cloneGroupId.
                p.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
                p.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                p.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                others += 1;
            }
        }
        // The doomed display may have held (0,0). An origin-less desktop is
        // rejected 0x57 regardless of array shape.
        if others > 0 {
            anchor_kept_sources_at_origin(&paths, &mut modes);
        }
        // Re-commit even when nothing was deactivated: a GDI mode-set does not
        // drive IddCx COMMIT_MODES. SAVE_TO_DATABASE only for a sole path.
        // Pick the supplied shape once so a desktop retry does not log twice.
        let keep_only = (others > 0 && attempt >= 2).then(|| {
            // Attempt 2+: keep-only arrays. Last attempt also drops
            // SDC_FORCE_MODE_ENUMERATION — some drivers reject the flag with a
            // topology change, and a real path removal already drives COMMIT_MODES.
            let (kp, km) = keep_only_supplied(&paths, &modes);
            let mut esc = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
            if attempt < 4 {
                esc |= SDC_FORCE_MODE_ENUMERATION;
            }
            tracing::info!(
                "display isolate (CCD): escalating to a keep-only supplied config (attempt {attempt}/4, paths {}→{}, modes {}→{})",
                paths.len(), kp.len(), modes.len(), km.len()
            );
            (kp, km, esc)
        });
        // SAFETY: CCD contract — both arms hand over slices that outlive the
        // call; `retry_set_display_config` binds the input desktop. `keep_only`
        // is a SavedConfig this module built, never caller-supplied.
        let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
            match &keep_only {
                Some((kp, km, esc)) => {
                    SetDisplayConfig(Some(kp.as_slice()), Some(km.as_slice()), *esc)
                }
                None => {
                    let mut flags = SDC_APPLY
                        | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                        | SDC_ALLOW_CHANGES
                        | SDC_FORCE_MODE_ENUMERATION;
                    if others == 0 {
                        flags |= SDC_SAVE_TO_DATABASE;
                    }
                    SetDisplayConfig(Some(paths.as_slice()), Some(modes.as_slice()), flags)
                }
            }
        });
        // Log a failed apply even when verify is vacuously true: the re-commit
        // still has to drive COMMIT_MODES.
        if rc != 0 {
            tracing::warn!(
                "display isolate (CCD): SetDisplayConfig rc={rc:#x}{} — the re-commit did NOT \
                 apply (COMMIT_MODES/ASSIGN_SWAPCHAIN may not fire for the virtual display)",
                sdc_access_denied_hint(rc)
            );
        }

        // VERIFY the OUTCOME (rc alone lies — a "successful" apply can leave a panel active): re-query
        // and confirm no non-keep display survived. Only then is the virtual set truly the sole
        // desktop. A FAILED verification query is UNKNOWN, never success: the old `unwrap_or(0)`
        // here reported "SOLE active desktop" on the strength of a query that answered nothing.
        match count_other_active(keep) {
            Some(0) => {
                tracing::info!(
                    "display isolate (CCD): target set {keep:?} is the SOLE active desktop (attempt {attempt}/4, deactivated {others}, rc={rc:#x})"
                );
                return Some((
                    saved,
                    IsolateOutcome::Verified {
                        attempts: attempt,
                        deactivated: others,
                    },
                ));
            }
            Some(survivors) => tracing::warn!(
                "display isolate (CCD): {survivors} display(s) STILL active after attempt {attempt}/4 (deactivated {others}, rc={rc:#x}) — re-querying + retrying"
            ),
            None => tracing::warn!(
                "display isolate (CCD): verification query FAILED after attempt {attempt}/4 (rc={rc:#x}) — isolation state UNKNOWN, retrying"
            ),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // Name survivors (kind + friendly); a sibling virtual is a common leftover.
    let survivors: Vec<String> = target_inventory()
        .iter()
        .filter(|t| t.active && !keep.contains(&t.key))
        .map(|t| format!("{} {} \"{}\"", t.key, t.tech, t.friendly))
        .collect();
    tracing::error!(
        "display isolate (CCD): target set {keep:?} not isolated after 4 attempts — still active or unverifiable: [{}] (field-reported exclusive-mode bug)",
        survivors.join(", ")
    );
    Some((saved, IsolateOutcome::Unverified { attempts: 4 }))
}

// Do not add an eviction without SDC_FORCE_MODE_ENUMERATION: a topology change
// still bounces the swap-chain and then stops presenting. Eviction always
// goes through [`isolate_displays_ccd`].

/// Keep-only supplied config: ACTIVE paths (caller already cleared doomed
/// ones) and the mode entries they reference, indexes remapped. Some
/// validators reject the inactive entries with 0x57.
fn keep_only_supplied(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>) {
    let mut out_paths = Vec::new();
    let mut out_modes = Vec::new();
    // Old index → new. Clone pairs share one source mode; each mode appears once.
    let mut remap = std::collections::HashMap::new();
    for p in paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let mut q = *p;
        q.sourceInfo.Anonymous.modeInfoIdx = remap_mode_idx(
            // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield.
            unsafe { q.sourceInfo.Anonymous.modeInfoIdx },
            modes,
            &mut out_modes,
            &mut remap,
        );
        q.targetInfo.Anonymous.modeInfoIdx = remap_mode_idx(
            // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield.
            unsafe { q.targetInfo.Anonymous.modeInfoIdx },
            modes,
            &mut out_modes,
            &mut remap,
        );
        out_paths.push(q);
    }
    (out_paths, out_modes)
}

/// Move `modes[old]` into `out` (once — `remap` dedups). INVALID and
/// out-of-range stay INVALID; `SDC_ALLOW_CHANGES` fills the gap.
fn remap_mode_idx(
    old: u32,
    modes: &[DISPLAYCONFIG_MODE_INFO],
    out: &mut Vec<DISPLAYCONFIG_MODE_INFO>,
    remap: &mut std::collections::HashMap<u32, u32>,
) -> u32 {
    if old == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        return old;
    }
    let Some(m) = modes.get(old as usize) else {
        return DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    };
    *remap.entry(old).or_insert_with(|| {
        out.push(*m);
        (out.len() - 1) as u32
    })
}

/// Translate kept sources so one sits at `(0,0)`. Deactivating the old primary
/// leaves an origin-less desktop, which Windows rejects with 0x57. Rigid shift:
/// relative layout kept. A set that already covers the origin is untouched.
fn anchor_kept_sources_at_origin(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &mut [DISPLAYCONFIG_MODE_INFO],
) {
    // Unique source-mode entries of kept paths — a clone pair shares one.
    let mut idxs: Vec<usize> = Vec::new();
    for p in paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let Some(m) = modes.get(idx) else { continue };
        if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE && !idxs.contains(&idx) {
            idxs.push(idx);
        }
    }
    let positions: Vec<(i32, i32)> = idxs
        .iter()
        .map(|&i| {
            // SAFETY: `idxs` was filtered on
            // `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` when built.
            let pos = unsafe { modes[i].Anonymous.sourceMode.position };
            (pos.x, pos.y)
        })
        .collect();
    if positions.contains(&(0, 0)) {
        return; // kept set already holds the primary
    }
    // Lexicographic min: the anchor is a kept source, so one lands on (0,0).
    let Some((ax, ay)) = positions.iter().copied().min() else {
        return; // no pinned sources — SDC_ALLOW_CHANGES places them
    };
    for &i in &idxs {
        // SAFETY: same `idxs`, same `infoType == SOURCE` filter.
        let sm = unsafe { &mut modes[i].Anonymous.sourceMode };
        sm.position.x -= ax;
        sm.position.y -= ay;
    }
    tracing::info!(
        "display isolate (CCD): kept source(s) re-anchored onto the desktop origin (primary) — the doomed display held (0,0) delta=({},{})",
        -ax,
        -ay
    );
}

/// The desktop-space rectangle `(x, y, w, h)` of `target_id`'s SOURCE — where this display's
/// region lives in the desktop coordinate space. `None` while the target isn't an active path.
/// Used by the IDD-push compose kick to dirty THE TARGET display: with parallel displays the
/// cursor sits on ONE of them, and a cursor wiggle only dirties that one — a sibling display's
/// kick must first know where to send the cursor (Stage W3 on-glass finding).
pub fn source_desktop_rect(key: CcdTargetKey) -> Option<(i32, i32, i32, i32)> {
    // SAFETY: `query_active_config` is this module's own CCD helper: it takes nothing and returns owned
    // `Vec`s built from a fresh `QueryDisplayConfig`, so it has no caller obligation at all.
    let (paths, modes) = query_active_config()?;
    for p in &paths {
        if path_target_key(p) != key || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let m = modes.get(idx)?;
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        // SAFETY: `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` on this
        // same entry is the discriminant for `sourceMode`.
        let sm = unsafe { m.Anonymous.sourceMode };
        return Some((
            sm.position.x,
            sm.position.y,
            sm.width as i32,
            sm.height as i32,
        ));
    }
    None
}

/// Adapter LUID + VidPn source of an active path, preferring a physical head
/// (`physical == true`). `D3DKMTGetScanLine` needs a real scan-out; exclusive
/// topology falls back to our IDD so the report does not over-read scanline values.
pub fn active_scanline_target() -> Option<(u32, i32, u32, bool)> {
    let (paths, _modes) = query_active_config()?;
    let mut fallback: Option<(u32, i32, u32, bool)> = None;
    for p in &paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let candidate = (
            p.sourceInfo.adapterId.LowPart,
            p.sourceInfo.adapterId.HighPart,
            p.sourceInfo.id,
            false,
        );
        let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        req.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        req.header.adapterId = p.targetInfo.adapterId;
        req.header.id = p.targetInfo.id;
        // SAFETY: `header.size` is this struct's size_of; the local outlives
        // this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
            fallback.get_or_insert(candidate);
            continue;
        }
        if is_our_virtual_display(&utf16z_str(&req.monitorDevicePath)) {
            fallback.get_or_insert(candidate);
            continue;
        }
        return Some((candidate.0, candidate.1, candidate.2, true));
    }
    fallback
}

/// Union of every active source rect, `(x, y, w, h)`. CCD, not
/// `GetSystemMetrics(SM_*VIRTUALSCREEN)`: GDI is per-session, CCD is the
/// console layout. Maps HID `0..=32767` onto the virtual screen.
pub fn desktop_bounds() -> Option<(i32, i32, i32, i32)> {
    let (paths, modes) = query_active_config()?;
    let mut acc: Option<(i32, i32, i32, i32)> = None; // (x0, y0, x1, y1) exclusive end
    for p in &paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let Some(m) = modes.get(idx) else { continue };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        // SAFETY: `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` on this
        // same entry is the discriminant for `sourceMode`.
        let sm = unsafe { m.Anonymous.sourceMode };
        let (x0, y0) = (sm.position.x, sm.position.y);
        let (x1, y1) = (x0 + sm.width as i32, y0 + sm.height as i32);
        acc = Some(match acc {
            None => (x0, y0, x1, y1),
            Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
        });
    }
    acc.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
}

/// Place each managed virtual target's SOURCE at the given desktop-space origin, as ONE atomic CCD
/// `SetDisplayConfig` (design `display-management.md` §6.2 — the Windows arm of the pure
/// `vdisplay/layout.rs` arrangement; positions come from `arrange`, this only commits them). Windows
/// treats the source at `(0,0)` as primary, so auto-row's first member lands primary — the group's
/// designated member. Paths not named stay where they are. Best-effort: a failure leaves the OS
/// placement (mouse crossing may not match the layout table until the next apply).
pub fn apply_source_positions(positions: &[(CcdTargetKey, i32, i32)]) {
    if positions.len() < 2 {
        return; // a single (or no) member already sits at the origin
    }
    let Some((paths, mut modes)) = query_active_config() else {
        return;
    };
    // Dedup source-mode indices (a cloned group shares one).
    let mut done = std::collections::HashSet::new();
    let mut moved = 0u32;
    for p in paths.iter() {
        let Some(&(_, x, y)) = positions.iter().find(|(t, _, _)| *t == path_target_key(p)) else {
            continue;
        };
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        if !done.insert(idx) {
            continue;
        }
        let Some(m) = modes.get_mut(idx) else {
            continue;
        };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        m.Anonymous.sourceMode.position = POINTL { x, y };
        moved += 1;
    }
    if moved == 0 {
        return;
    }
    // SAFETY: CCD contract — slices, so pointer and length agree; both outlive
    // this synchronous call. `retry_set_display_config` binds the input desktop.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
    if rc == 0 {
        tracing::info!(
            ?positions,
            "display layout (CCD): group source origins applied"
        );
    } else {
        tracing::warn!(
            ?positions,
            "display layout (CCD): SetDisplayConfig rc={rc:#x}"
        );
    }
}

/// **Primary (topology=primary)** — make the virtual output the PRIMARY display while KEEPING every
/// other display ACTIVE (unlike [`isolate_displays_ccd`], which deactivates them). Windows treats the
/// display whose source sits at the desktop origin `(0,0)` as primary, so we move the virtual's source
/// to `(0,0)` and shift every other active source to its right — all paths stay active. Done as ONE
/// atomic CCD `SetDisplayConfig` (NOT GDI `CDS_SET_PRIMARY`, which storms
/// `DXGI_ERROR_MODE_CHANGE_IN_PROGRESS` when another display is live — see [`set_active_mode`]).
/// Returns the original config to restore on teardown.
pub fn set_virtual_primary_ccd(keep: CcdTargetKey) -> Option<SavedConfig> {
    // Through the shared query, not a private copy of it. This was a verbatim duplicate of
    // `query_active_config` — same flags, same shape — and so the one CCD entry point that did not
    // inherit its zero-path fix (the seam asymmetry this crate keeps producing: N-1 of N sibling
    // paths share a helper and the Nth open-codes it).
    let (paths, mut modes) = query_active_config()?;
    let saved = (paths.clone(), modes.clone());

    let virt_width = paths.iter().find_map(|p| {
        if path_target_key(p) != keep {
            return None;
        }
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let m = modes.get(idx)?;
        // SAFETY: POD u32 union read — `then_some` is eager, so `sourceMode.width`
        // is read even when infoType is not SOURCE. Every u32 bit pattern is valid.
        let width = unsafe { m.Anonymous.sourceMode.width } as i32;
        (m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE).then_some(width)
    })?;
    let others = paths.len().saturating_sub(1);

    // Virtual to (0,0); others packed left-to-right from its right edge.
    // Shifting each by virt_width leaves a hole when EXTEND already placed
    // them to the right. Dedup cloned source-mode indices.
    let mut next_x = virt_width;
    let mut done = std::collections::HashSet::new();
    for p in paths.iter() {
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        if !done.insert(idx) {
            continue;
        }
        let Some(m) = modes.get_mut(idx) else {
            continue;
        };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        if path_target_key(p) == keep {
            // (A union field ASSIGNMENT needs no `unsafe` — only reads do.)
            m.Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
        } else {
            // SAFETY: `infoType == SOURCE` checked immediately above; `width` is a u32.
            let w = unsafe { m.Anonymous.sourceMode.width } as i32;
            m.Anonymous.sourceMode.position = POINTL { x: next_x, y: 0 };
            next_x += w;
        }
    }

    // SAFETY: CCD contract — slices, so pointer and length agree; both outlive
    // this synchronous call. `retry_set_display_config` binds the input desktop.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
    if rc == 0 {
        tracing::info!(
            "display primary (CCD): virtual target {keep} set PRIMARY at (0,0); {others} other display(s) kept ACTIVE + packed to its right"
        );
    } else {
        tracing::warn!(
            "display primary (CCD): SetDisplayConfig failed rc={rc:#x}{} (virtual {keep} primary, physicals kept)",
            sdc_access_denied_hint(rc)
        );
    }
    Some(saved)
}

/// The dark-sink futility latch: `(target_id, monitor_device_path)` keys of connected external
/// displays that stayed dark THROUGH a force-EXTEND. A sink the EXTEND preset demonstrably
/// cannot light (an off/standby TV, a KVM switched away) will not light on the next teardown
/// either — without the latch [`restore_displays_ccd`]'s backstop re-warns and re-forces on
/// EVERY teardown, forever (field: the off-TV box). Process-lifetime on purpose: a host restart
/// re-probes once, in case the sink meanwhile became lightable.
static DARK_SINKS_FUTILE: std::sync::Mutex<Vec<(CcdTargetKey, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Restore the topology saved by [`isolate_displays_ccd`] (teardown, before
/// the virtual is removed).
pub fn restore_displays_ccd(saved: &SavedConfig) {
    // The marker goes only once the desk is known lit (or nothing external is connected). A
    // host that dies mid-restore, or a restore that lit nothing, leaves it for the next start.
    if restore_displays_ccd_inner(saved) {
        isolate_journal::clear();
    } else {
        tracing::warn!(
            "display isolate (CCD): the restore left every external display dark — keeping the \
             crash-recovery marker so the next host start forces EXTEND"
        );
    }
}

/// Every display target that still EXISTS right now — complete [`CcdTargetKey`]s from a full
/// `QDC_ALL_PATHS` sweep, counting a target present when the OS says a monitor is attached
/// (`targetAvailable`) OR an active path drives it (the flag reads FALSE transiently right after
/// a removal — same rule as [`target_inventory`]). `None` when the CCD query itself fails, so the
/// caller can fall back to trusting its snapshot verbatim.
fn available_target_keys() -> Option<Vec<CcdTargetKey>> {
    let (paths, _modes) = query_display_config(QDC_ALL_PATHS).ok()?;
    let mut keys: Vec<CcdTargetKey> = Vec::new();
    for p in &paths {
        let key = path_target_key(p);
        let present =
            p.targetInfo.targetAvailable.as_bool() || p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0;
        if present && !keys.contains(&key) {
            keys.push(key);
        }
    }
    Some(keys)
}

/// Drop snapshot paths whose target is gone from `avail` and rebuild the mode
/// table for survivors. One stale path or orphaned mode fails the whole
/// `SetDisplayConfig` with 0x57.
fn prune_saved_config_for_targets(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
    avail: &[CcdTargetKey],
) -> (
    Vec<DISPLAYCONFIG_PATH_INFO>,
    Vec<DISPLAYCONFIG_MODE_INFO>,
    usize,
) {
    let mut kept: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::with_capacity(paths.len());
    let mut new_modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::with_capacity(modes.len());
    // Old index → new, memoized: clone configs share a source mode; it lands once.
    let mut remap: Vec<Option<u32>> = vec![None; modes.len()];
    let take =
        |idx: u32, new_modes: &mut Vec<DISPLAYCONFIG_MODE_INFO>, remap: &mut Vec<Option<u32>>| {
            if idx == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
                return DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            }
            match modes.get(idx as usize) {
                // Out-of-range could never have applied — unpin rather than
                // ship a table the whole submission fails on.
                None => DISPLAYCONFIG_PATH_MODE_IDX_INVALID,
                Some(m) => match remap[idx as usize] {
                    Some(n) => n,
                    None => {
                        let n = new_modes.len() as u32;
                        new_modes.push(*m);
                        remap[idx as usize] = Some(n);
                        n
                    }
                },
            }
        };
    let mut dropped = 0usize;
    for p in paths {
        if !avail.contains(&path_target_key(p)) {
            dropped += 1;
            continue;
        }
        let mut p = *p;
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid. Used as bounds-checked indices.
        let (src_idx, tgt_idx) = unsafe {
            (
                p.sourceInfo.Anonymous.modeInfoIdx,
                p.targetInfo.Anonymous.modeInfoIdx,
            )
        };
        p.sourceInfo.Anonymous.modeInfoIdx = take(src_idx, &mut new_modes, &mut remap);
        p.targetInfo.Anonymous.modeInfoIdx = take(tgt_idx, &mut new_modes, &mut remap);
        kept.push(p);
    }
    (kept, new_modes, dropped)
}

/// `true` when no connected external display is left dark — the restore lit one, the backstop
/// did, or there is none to light.
fn restore_displays_ccd_inner(saved: &SavedConfig) -> bool {
    let (saved_paths, saved_modes) = saved;
    if saved_paths.is_empty() {
        return true;
    }
    // Prune absent targets first: one stale path (or orphaned mode) makes
    // SetDisplayConfig reject the whole array with 0x57 — nothing restores.
    let (kept, pruned_modes, dropped);
    let (paths, modes): (&Vec<_>, &Vec<_>) = match available_target_keys() {
        Some(avail) => {
            (kept, pruned_modes, dropped) =
                prune_saved_config_for_targets(saved_paths, saved_modes, &avail);
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    kept = kept.len(),
                    "display isolate (CCD): snapshot references target(s) that are no longer \
                     attached (unplugged mid-session?) — pruned them so the survivors can restore \
                     (a verbatim replay fails whole with rc=0x57)"
                );
            }
            (&kept, &pruned_modes)
        }
        // Availability query failed — replay verbatim.
        None => (saved_paths, saved_modes),
    };
    let mut apply_rc = 0i32; // 0 also when replay was skipped
    if paths.is_empty() {
        tracing::warn!(
            "display isolate (CCD): nothing from the topology snapshot is still attached — \
             skipping the replay (the dark-desk backstop decides what lights up)"
        );
    } else {
        // SAFETY: CCD contract — slices, so pointer and length agree; both outlive
        // this synchronous call. `retry_set_display_config` binds the input desktop.
        let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
            SetDisplayConfig(
                Some(paths.as_slice()),
                Some(modes.as_slice()),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
            )
        });
        apply_rc = rc;
        if rc == 0 {
            tracing::info!("display isolate (CCD): restored original topology");
        } else {
            tracing::warn!(
                "display isolate (CCD): topology restore failed rc={rc:#x}{} — physical displays may be left deactivated",
                sdc_access_denied_hint(rc)
            );
        }
    }
    // If every connected external is still dark after apply, force EXTEND. Internals do not
    // count as lightable — a closed clamshell must stay off. rc=0 can still re-light nothing.
    // Checked, because an unwrapped query failure reads as "nothing is connected" and skips
    // the backstop; not-restored keeps the caller's recovery marker instead.
    let inventory = match target_inventory_checked() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                "display isolate (CCD): cannot read the target inventory after the restore — \
                 leaving the desk unverified rather than assuming nothing is connected"
            );
            return false;
        }
    };
    let (connected, lit) = inventory
        .iter()
        .filter(|t| t.external_physical)
        .fold((0u32, 0u32), |(c, a), t| (c + 1, a + u32::from(t.active)));
    if connected > 0 && lit == 0 {
        let dark: Vec<(CcdTargetKey, String)> = inventory
            .iter()
            .filter(|t| t.external_physical && !t.active)
            .map(|t| (t.key, t.monitor_device_path.clone()))
            .collect();
        // Same dark set already survived EXTEND: unlightable sink, not a
        // failed restore. A panel that can light succeeds the first force.
        if *DARK_SINKS_FUTILE.lock().unwrap() == dark {
            tracing::debug!(
                connected,
                "display isolate (CCD): the connected external display(s) stayed dark through \
                 an earlier force-EXTEND — an unlightable sink (off/standby TV), not a failed \
                 restore; leaving the topology be"
            );
            return false;
        }
        tracing::warn!(
            "display isolate (CCD): no external physical display active after the restore (rc={apply_rc:#x}, connected={connected}) — forcing the EXTEND preset so the desk is not left dark"
        );
        force_extend_topology();
        // Still dark after EXTEND: remember the set and stop re-forcing.
        // Anything lit clears the latch.
        let lit_after = target_inventory()
            .iter()
            .any(|t| t.external_physical && t.active);
        *DARK_SINKS_FUTILE.lock().unwrap() = if lit_after { Vec::new() } else { dark };
        return lit_after;
    }
    true
}

/// Live CCD queries. `#[ignore]` so an un-instrumented run is `ignored`, not a
/// vacuous `ok`. `cargo test -p pf-win-display -- --ignored` on hardware.
///
/// Read-only: do not call `isolate_displays_ccd(&[])` — empty keep deactivates everything.
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn auto_colour_management_is_not_hdr() {
        assert!(hdr_active(0b0011), "enabled, not enforced: HDR");
        assert!(!hdr_active(0b0111), "wide colour enforced: SDR under ACM");
        assert!(!hdr_active(0b0001), "supported only");
    }

    /// Path match is case-insensitive; unknown is not ours.
    #[test]
    fn our_own_virtual_display_is_never_an_external_physical() {
        assert!(super::is_our_virtual_display(
            r"\\?\DISPLAY#PNK0000#5&1234abcd&0&UID257#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
        ));
        // The OS is not consistent about the path's case.
        assert!(super::is_our_virtual_display(
            r"\\?\display#pnk0000#5&1&0&uid257#{guid}"
        ));
        assert!(!super::is_our_virtual_display(
            r"\\?\DISPLAY#GSM83CD#5&367fb4cb&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
        ));
        assert!(!super::is_our_virtual_display(
            r"\\?\DISPLAY#SMVD0001#5&1&0&UID999#{guid}"
        ));
    }

    /// Read-only against a live host: IddCx create/destroy wedges the slot pool.
    /// Our display must not count as `external_physical` (it declares HDMI). The
    /// inventory must hold one, or the loop below asserts over an empty set.
    #[test]
    #[ignore = "hardware: reads the live display topology"]
    fn our_own_display_is_excluded_from_the_operators_physicals_on_real_hardware() {
        let inv = target_inventory();
        let ours: Vec<_> = inv
            .iter()
            .filter(|t| is_our_virtual_display(&t.monitor_device_path))
            .collect();
        assert!(
            !ours.is_empty(),
            "no punktfunk virtual display among the {} live target(s) — bring one up before \
             running this, or the check below proves nothing",
            inv.len()
        );
        for t in ours {
            assert!(
                !t.external_physical,
                "our own display {} ({:?}) is still counted as one of the operator's physical \
                 panels — `restore_displays_ccd`'s dark-desk backstop keys on exactly this set and \
                 would never fire",
                t.target_id, t.friendly
            );
        }
    }

    /// `numPaths = 0` is an ordinary state, not query failure. Without the
    /// short-circuit `isolate_displays_ccd` returns `None` on a healthy
    /// headless box.
    #[test]
    #[ignore = "hardware: reads the live display topology"]
    fn a_host_with_nothing_lit_reports_zero_actives_rather_than_a_failed_query() {
        let n = count_other_active(&[]).expect(
            "count_other_active returned None, i.e. the CCD query was reported as FAILED. On a \
             host with no active display path that is the zero-path conflation, and it is what \
             makes isolate_displays_ccd yield None on a perfectly healthy machine",
        );
        tracing::info!("live CCD query: {n} active display path(s)");
    }

    /// LIVE (console session, `PUNKTFUNK_CONFIG_DIR` at a journal a killed host left behind):
    /// the isolate journal's startup leg relights the desktop — afterwards an external physical
    /// panel is active and the marker is gone. The S4/WP11 kill-recovery gate; a box with no
    /// leftover passes on the panel check alone.
    #[test]
    #[ignore = "hardware: writes the live display topology; needs a console session"]
    fn live_startup_recover_relights_the_desktop_after_a_kill() {
        let pending = isolate_journal::pending();
        eprintln!("isolate journal before: {pending:?}");
        isolate_journal::startup_recover();
        assert!(
            isolate_journal::pending().is_none(),
            "the isolate marker must be cleared by the replay"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let inv = target_inventory();
            let lit: Vec<_> = inv
                .iter()
                .filter(|t| t.active && t.external_physical)
                .map(|t| (t.target_id, t.friendly.clone()))
                .collect();
            if !lit.is_empty() {
                eprintln!("physical panel(s) active after the replay: {lit:?}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no external physical panel came back within 8 s of the replay: {inv:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod prune_saved_config_tests {
    //! Remap arithmetic for `prune_saved_config_for_targets`: a stale path
    //! vanishes, its modes must not orphan (0x57), clone-shared modes land once.
    use super::*;

    fn path(
        luid_low: u32,
        target_id: u32,
        src_mode: u32,
        tgt_mode: u32,
    ) -> DISPLAYCONFIG_PATH_INFO {
        let mut p = DISPLAYCONFIG_PATH_INFO::default();
        p.targetInfo.adapterId.LowPart = luid_low;
        p.targetInfo.id = target_id;
        p.sourceInfo.adapterId.LowPart = luid_low;
        p.sourceInfo.Anonymous.modeInfoIdx = src_mode;
        p.targetInfo.Anonymous.modeInfoIdx = tgt_mode;
        p
    }

    fn mode(marker: u32) -> DISPLAYCONFIG_MODE_INFO {
        DISPLAYCONFIG_MODE_INFO {
            id: marker,
            ..Default::default()
        }
    }

    fn k(luid_low: u32, target_id: u32) -> CcdTargetKey {
        CcdTargetKey::from_luid_parts(luid_low, 0, target_id)
    }

    fn indices(p: &DISPLAYCONFIG_PATH_INFO) -> (u32, u32) {
        // SAFETY: POD union — `modeInfoIdx` overlays a same-sized bitfield;
        // every bit pattern is valid.
        unsafe {
            (
                p.sourceInfo.Anonymous.modeInfoIdx,
                p.targetInfo.Anonymous.modeInfoIdx,
            )
        }
    }

    #[test]
    fn everything_attached_survives_with_dense_indices() {
        let paths = vec![path(1, 100, 0, 1), path(1, 200, 2, 3)];
        let modes = vec![mode(10), mode(11), mode(12), mode(13)];
        let avail = vec![k(1, 100), k(1, 200)];
        let (kept, new_modes, dropped) = prune_saved_config_for_targets(&paths, &modes, &avail);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
        assert_eq!(new_modes.len(), 4);
        assert_eq!(indices(&kept[0]), (0, 1));
        assert_eq!(indices(&kept[1]), (2, 3));
        assert_eq!(new_modes[3].id, 13, "mode entries follow their paths");
    }

    #[test]
    fn a_gone_target_drops_its_path_and_modes() {
        let paths = vec![path(1, 100, 0, 1), path(1, 200, 2, 3)];
        let modes = vec![mode(10), mode(11), mode(12), mode(13)];
        let avail = vec![k(1, 100)];
        let (kept, new_modes, dropped) = prune_saved_config_for_targets(&paths, &modes, &avail);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].targetInfo.id, 100);
        assert_eq!(
            new_modes.len(),
            2,
            "the dropped path's modes must not orphan"
        );
        assert_eq!((new_modes[0].id, new_modes[1].id), (10, 11));
        assert_eq!(indices(&kept[0]), (0, 1));
    }

    #[test]
    fn a_clone_shared_source_mode_lands_exactly_once() {
        let paths = vec![path(1, 100, 0, 1), path(1, 200, 0, 2)];
        let modes = vec![mode(10), mode(11), mode(12)];
        let avail = vec![k(1, 100), k(1, 200)];
        let (kept, new_modes, dropped) = prune_saved_config_for_targets(&paths, &modes, &avail);
        assert_eq!(dropped, 0);
        assert_eq!(new_modes.len(), 3);
        let (a_src, _) = indices(&kept[0]);
        let (b_src, _) = indices(&kept[1]);
        assert_eq!(a_src, b_src, "shared source mode keeps one table entry");
    }

    #[test]
    fn unpinned_and_corrupt_indices_stay_unpinned() {
        let paths = vec![path(1, 100, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, 99)];
        let modes = vec![mode(10)];
        let avail = vec![k(1, 100)];
        let (kept, new_modes, dropped) = prune_saved_config_for_targets(&paths, &modes, &avail);
        assert_eq!(dropped, 0);
        assert!(new_modes.is_empty());
        assert_eq!(
            indices(&kept[0]),
            (
                DISPLAYCONFIG_PATH_MODE_IDX_INVALID,
                DISPLAYCONFIG_PATH_MODE_IDX_INVALID
            )
        );
    }

    #[test]
    fn different_adapters_do_not_alias_the_same_target_id() {
        // Target ids are unique per adapter — matching ids on another LUID
        // must not keep a stale path.
        let paths = vec![path(1, 100, 0, 1)];
        let modes = vec![mode(10), mode(11)];
        let avail = vec![k(2, 100)];
        let (kept, _, dropped) = prune_saved_config_for_targets(&paths, &modes, &avail);
        assert_eq!((kept.len(), dropped), (0, 1));
    }
}
