//! The IddCx client-config callbacks + the PnP `EvtDeviceD0Entry`.
//!
//! Every callback is `unsafe extern "C"` to match the wdk-sys `PFN_IDD_CX_*` types. The driver builds
//! with `panic = "abort"`, so a panic in any of them — or in the swap-chain worker thread — takes
//! WUDFHost down instead of unwinding across the FFI boundary.
//!
//! The `*2` (HDR) mode and commit callbacks are registered ALONGSIDE their v1 twins, not instead of
//! them: the OS prefers the `*2` on IddCx 1.10 and falls back down-level, and the adapter's
//! `CAN_PROCESS_FP16` cap obligates the whole `*2` + gamma/HDR-metadata set. Each mode pair shares one
//! fill helper ([`fill_monitor_modes`], [`fill_target_modes`]); only the emitted struct differs.

use std::sync::{Arc, Mutex};

use wdk_sys::iddcx;
use wdk_sys::{NTSTATUS, WDFDEVICE, WDFOBJECT, WDFREQUEST, call_unsafe_wdf_function_binding};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::worker::Worker;

/// The seat presenter thread, so device cleanup can join it: detached, it would call IddCx DDIs
/// and create monitors against a device the framework is deleting.
static SEAT_PRESENTER: Mutex<Option<Worker>> = Mutex::new(None);

use crate::{
    STATUS_BUFFER_TOO_SMALL, STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED,
    STATUS_SUCCESS,
};

/// PnP `EvtDeviceD0Entry` (not an IddCx config callback). Adapter creation is deferred to the first D0
/// (the adapter object is only valid after D0), not driver_add.
pub unsafe extern "C" fn device_d0_entry(
    device: WDFDEVICE,
    previous_state: wdk_sys::WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    dbglog!("[pf-vd] device_d0_entry (previous_state={previous_state})");
    // A resume from a REAL low-power state (D1/D2/D3 — the initial start reports D3Final): the
    // cached adapter handle belongs to the pre-power-cycle incarnation, and `init_adapter` would
    // short-circuit on it forever, leaving every later IOCTL_ADD pointed at a stale adapter. The
    // MS sample re-inits on every D0 entry; we clear-and-reinit only on genuine resumes so the
    // common re-entrant D0 (no power cycle) stays the cheap no-op the doc above promises.
    if matches!(
        previous_state,
        wdk_sys::_WDF_POWER_DEVICE_STATE::WdfPowerDeviceD1
            | wdk_sys::_WDF_POWER_DEVICE_STATE::WdfPowerDeviceD2
            | wdk_sys::_WDF_POWER_DEVICE_STATE::WdfPowerDeviceD3
    ) {
        dbglog!("[pf-vd] device_d0_entry: power-cycle resume — re-initializing the adapter");
        // The monitors belong to the incarnation being retired: kept, they hold its ids (a
        // re-ADD is refused its stable id) and their departure would hit a dead handle.
        crate::monitor::cleanup_for_device_removal();
        crate::adapter::clear_adapter();
    }
    // SAFETY: `device` is the framework's live WDFDEVICE for this D0 entry.
    unsafe { crate::adapter::init_adapter(device) }
}

/// Async completion of `IddCxAdapterInitAsync`: stash the adapter for later DDIs — IFF the init
/// actually SUCCEEDED — and arm the host-gone watchdog, since this is the point from which
/// monitors can exist.
pub unsafe extern "C" fn adapter_init_finished(
    adapter: iddcx::IDDCX_ADAPTER,
    p_in: *const iddcx::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    // SAFETY: the framework supplies a valid, live input-args pointer for the call.
    let status = unsafe { (*p_in).AdapterInitStatus };
    dbglog!("[pf-vd] adapter_init_finished (AdapterInitStatus={status:#010x})");
    crate::adapter::init_settled();
    // The MS sample gates on NT_SUCCESS(AdapterInitStatus). An adapter whose async init FAILED is a
    // husk the contract forbids using: monitors created on it arrive but are never activated (no
    // swap-chain ever assigned) — every session then black-screens with no visible cause. Leaving
    // the ADAPTER unset makes `create_monitor` fail the ADD cleanly (host-visible + retryable), and
    // a re-entrant D0 retries the init (`init_adapter` only short-circuits once the stash is set).
    if status < 0 {
        return STATUS_SUCCESS; // the callback itself succeeded; the failure is in NOT adopting
    }
    crate::adapter::set_adapter(adapter);
    crate::watchdog::start();
    // A seat adapter must carry a display or the remoting stack drops the session, and this runs
    // OFF the callback thread: the stack is waiting on adapter init, and arriving a monitor inline
    // blocks it long enough to lose the display anyway.
    if crate::adapter::is_seat_role() {
        let old = std::mem::replace(
            &mut *crate::registry::lock(&SEAT_PRESENTER),
            Worker::spawn("pf-vd-seat", present_seat_display),
        );
        drop(old);
    }
    STATUS_SUCCESS
}

/// Give the seat adapter the display the remoting stack expects, then publish the configuration
/// the OS needs before it will assign a swap chain. Runs on its own thread (see the call site);
/// `stop` is device cleanup asking it to leave.
fn present_seat_display(stop: HANDLE) {
    let Some(adapter) = crate::adapter::adapter() else {
        return;
    };
    // `PFVD_SEAT_MODE` is `<w>x<h>@<hz>`. The remoting stack matches the client's requested
    // desktop against this, so the two have to agree until the seat learns the client's size.
    let (w, h, hz) = crate::log::knob("PFVD_SEAT_MODE")
        .and_then(|v| {
            let (wh, hz) = v.split_once('@')?;
            let (w, h) = wh.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?, hz.parse().ok()?))
        })
        .unwrap_or((1920u32, 1080u32, 60u32));
    // The id becomes the monitor's EDID serial, so two seats sharing it present one monitor
    // identity to the OS and only the first is usable.
    let monitor_id = crate::log::knob("PFVD_SEAT_MONITOR_ID")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    // Driver-owned, not a host's: the OS started the adapter, and pid 0 is never a requestor, so
    // this cannot collide with a real host. It stays for the session's life (see
    // `SEAT_PLACEHOLDER_OWNER`) and out of the owner-gone reap: it lives with the adapter.
    let made = crate::monitor::create_monitor(
        crate::monitor::SEAT_PLACEHOLDER_OWNER,
        &pf_driver_proto::control::AddRequest {
            session_id: crate::monitor::SEAT_PLACEHOLDER_SESSION,
            width: w,
            height: h,
            refresh_hz: hz,
            preferred_monitor_id: monitor_id,
            max_luminance_nits: 0,
            max_frame_avg_nits: 0,
            min_luminance_millinits: 0,
            hw_cursor: 0,
        },
    );
    dbglog!(
        "[pf-vd] seat adapter: presented {w}x{h}@{hz} -> {}",
        made.is_some()
    );
    // A remote adapter only gets swap chains for the configuration it publishes here, so the
    // arrival alone leaves the display dark. One path, the monitor we just made.
    if let Some((id, ..)) = made
        && let Some(object) = crate::registry::find(|m| m.id == id).and_then(|m| m.object())
    {
        let mut path = iddcx::IDDCX_DISPLAYCONFIGPATH2 {
            Size: core::mem::size_of::<iddcx::IDDCX_DISPLAYCONFIGPATH2>() as u32,
            Flags: iddcx::IDDCX_DISPLAYCONFIGPATH2_FLAGS::IDDCX_DISPLAYCONFIGPATH2_FLAGS_MODE_VALID
                | iddcx::IDDCX_DISPLAYCONFIGPATH2_FLAGS::IDDCX_DISPLAYCONFIGPATH2_FLAGS_MONITOR_SCALE_FACTOR_VALID,
            MonitorObject: object,
            ..Default::default()
        };
        path.Mode.Resolution.cx = w;
        path.Mode.Resolution.cy = h;
        path.Mode.Rotation = 1; // DISPLAYCONFIG_ROTATION_IDENTITY
        path.Mode.RefreshRate.Numerator = hz;
        path.Mode.RefreshRate.Denominator = 1;
        path.Mode.VSyncFreqDivider = 1;
        path.Mode.MonitorColorMode =
            iddcx::IDDCX_DISPLAYCONFIG_MONITOR_COLORMODE::IDDCX_DISPLAYCONFIG_MONITOR_COLORMODE_SDR;
        path.MonitorScaleFactor = 100;
        let args = iddcx::IDARG_IN_ADAPTERDISPLAYCONFIGUPDATE2 {
            PathCount: 1,
            pPaths: &raw mut path,
        };
        // The session's display stack is still coming up, and a config published before it is
        // ready is accepted and then ignored: no swap chain, and the stack discards the display.
        // Republishing is free once one has landed, so repeat until it does.
        for attempt in 0..8 {
            // SAFETY: `adapter` is the stashed adapter object; `args`/`path` are live locals the
            // DDI reads synchronously.
            let st = unsafe { wdk_iddcx::IddCxAdapterDisplayConfigUpdate2(adapter, &args) };
            let live = crate::registry::find(|m| m.id == id).is_some_and(|m| m.has_swap_chain());
            dbglog!("[pf-vd] seat: display config #{attempt} -> {st:#x} (swap={live})");
            if live {
                break;
            }
            // SAFETY: `stop` is the worker's stop event, alive until the worker joins.
            if unsafe { WaitForSingleObject(stop, 400) } == WAIT_OBJECT_0 {
                return;
            }
        }
    }
}

/// `EvtCleanupCallback` on the WDFDEVICE (E1): the device is being removed (PnP / driver unload) — drop
/// every monitor's swap-chain worker so the worker threads don't linger into teardown. IddCx-free (the
/// framework tears the monitors down with the departing device); see
/// [`crate::monitor::cleanup_for_device_removal`].
pub unsafe extern "C" fn device_cleanup(_object: WDFOBJECT) {
    dbglog!("[pf-vd] device cleanup — releasing monitors");
    // The seat presenter first: joined here, so it cannot create a monitor after the registry
    // is drained or hit a DDI on the departing device.
    let presenter = crate::registry::lock(&SEAT_PRESENTER).take();
    drop(presenter);
    // Stop the host-liveness watchdog FIRST, and WAIT: a reap that fired mid-cleanup would race
    // this teardown over the same monitor list (the hazard `monitor.rs` documents).
    crate::watchdog::stop();
    crate::monitor::cleanup_for_device_removal();
}

/// One `IDDCX_MONITOR_MODE` (SDR) for the description mode list.
fn monitor_mode(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_MONITOR_MODE {
    iddcx::IDDCX_MONITOR_MODE {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_MODE>() as u32,
        Origin: iddcx::IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
        MonitorVideoSignalInfo: crate::monitor::display_info(width, height, refresh_rate),
    }
}

/// One `IDDCX_MONITOR_MODE2`: the SDR mode plus the per-mode wire bit-depth, which is what makes the
/// OS offer HDR10 modes on this monitor.
fn monitor_mode2(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_MONITOR_MODE2 {
    iddcx::IDDCX_MONITOR_MODE2 {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_MODE2>() as u32,
        Origin: iddcx::IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
        MonitorVideoSignalInfo: crate::monitor::display_info(width, height, refresh_rate),
        BitsPerComponent: crate::monitor::wire_bits(),
    }
}

/// The body both `EvtIddCxParseMonitorDescription` variants share: the EDID serial selects the
/// monitor's mode list, then `make` writes one entry per flattened mode.
///
/// The out struct is the same for v1 and v2 (the C header shares it), so only `M` and the in-args
/// differ. A zero `in_count` is a count-only probe: report the count and succeed. A smaller non-zero
/// buffer is `STATUS_BUFFER_TOO_SMALL`. `tag` names the caller in the log that shows which list
/// generation a re-parse served, and is `None` on the v1 path, which logs nothing.
///
/// # Safety
/// `edid` must cover the framework's description buffer, and `p_modes` must point at `in_count`
/// writable `M` entries.
unsafe fn fill_monitor_modes<M>(
    edid: &[u8],
    in_count: u32,
    p_modes: *mut M,
    out_args: &mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
    tag: Option<&str>,
    make: impl Fn(u32, u32, u32) -> M,
) -> NTSTATUS {
    let Ok(id) = pf_driver_proto::edid::get_serial(edid) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(modes) = crate::registry::find(|m| m.id == id).map(|m| m.modes()) else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    if let Some(tag) = tag
        && let Some(head) = crate::monitor::flatten(&modes).next()
    {
        dbglog!(
            "[pf-vd] {tag}(id={id}): {count} modes, head {}x{}@{}",
            head.width,
            head.height,
            head.refresh_rate
        );
    }
    out_args.MonitorModeBufferOutputCount = count;
    if in_count < count {
        return if in_count > 0 {
            STATUS_BUFFER_TOO_SMALL
        } else {
            STATUS_SUCCESS
        };
    }
    // SAFETY: `p_modes` is the caller's mode buffer, whose `in_count` capacity was checked against
    // `count` above, so >= `count` `M` entries are writable.
    let out = unsafe { core::slice::from_raw_parts_mut(p_modes, count as usize) };
    for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
        *slot = make(item.width, item.height, item.refresh_rate);
    }
    out_args.PreferredMonitorModeIdx = 0;
    STATUS_SUCCESS
}

/// SDR mode list for an EDID monitor. Registered next to [`parse_monitor_description2`]; the OS calls
/// this one only on a framework below IddCx 1.10.
pub unsafe extern "C" fn parse_monitor_description(
    p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION,
    p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: the framework supplies live in/out args for the call, an EDID buffer of `DataSize`
    // bytes, and `pMonitorModes` room for `MonitorModeBufferInputCount` IDDCX_MONITOR_MODE entries.
    unsafe {
        let in_args = &*p_in;
        fill_monitor_modes(
            core::slice::from_raw_parts(
                in_args.MonitorDescription.pData.cast::<u8>(),
                in_args.MonitorDescription.DataSize as usize,
            ),
            in_args.MonitorModeBufferInputCount,
            in_args.pMonitorModes,
            &mut *p_out,
            None,
            monitor_mode,
        )
    }
}

/// HDR mode list: the same fill, emitting `IDDCX_MONITOR_MODE2`. Mandatory under FP16, and the variant
/// the OS actually calls on IddCx 1.10.
pub unsafe extern "C" fn parse_monitor_description2(
    p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION2,
    p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: the framework supplies live in/out args for the call, an EDID buffer of `DataSize`
    // bytes, and `pMonitorModes` room for `MonitorModeBufferInputCount` IDDCX_MONITOR_MODE2 entries.
    unsafe {
        let in_args = &*p_in;
        fill_monitor_modes(
            core::slice::from_raw_parts(
                in_args.MonitorDescription.pData.cast::<u8>(),
                in_args.MonitorDescription.DataSize as usize,
            ),
            in_args.MonitorModeBufferInputCount,
            in_args.pMonitorModes,
            &mut *p_out,
            Some("parse_monitor_description2"),
            monitor_mode2,
        )
    }
}

/// Only called for EDID-less monitors; ours always carry an EDID, so this stays NOT_IMPLEMENTED.
pub unsafe extern "C" fn monitor_get_default_modes(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    _p_out: *mut iddcx::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// The body both `EvtIddCxMonitorQueryTargetModes` variants share: the monitor handle selects the mode
/// list, then `make` writes one scan-out mode per flattened entry.
///
/// The out struct is the same for v1 and v2, so only `M` and the in-args differ. Unlike the description
/// DDI there is no too-small error: the count is always reported, and the buffer is filled only when it
/// already holds `count` entries. `tag` names the caller in the log that shows whether the OS re-queries
/// after an UPDATE_MODES, and is `None` on the v1 path, which logs nothing.
///
/// # Safety
/// `p_modes` must point at `in_count` writable `M` entries.
unsafe fn fill_target_modes<M>(
    monitor: iddcx::IDDCX_MONITOR,
    in_count: u32,
    p_modes: *mut M,
    out_args: &mut iddcx::IDARG_OUT_QUERYTARGETMODES,
    tag: Option<&str>,
    make: impl Fn(u32, u32, u32) -> M,
) -> NTSTATUS {
    let Some(modes) = crate::registry::find(|m| m.object() == Some(monitor)).map(|m| m.modes())
    else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    if let Some(tag) = tag
        && let Some(head) = crate::monitor::flatten(&modes).next()
    {
        dbglog!(
            "[pf-vd] {tag}: {count} modes, head {}x{}@{} (fill={})",
            head.width,
            head.height,
            head.refresh_rate,
            in_count >= count
        );
    }
    out_args.TargetModeBufferOutputCount = count;
    if in_count >= count {
        // SAFETY: `p_modes` is the caller's mode buffer, whose `in_count` capacity was checked
        // against `count` above, so >= `count` `M` entries are writable.
        let out = unsafe { core::slice::from_raw_parts_mut(p_modes, count as usize) };
        for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
            *slot = make(item.width, item.height, item.refresh_rate);
        }
    }
    STATUS_SUCCESS
}

/// SDR target (scan-out) modes. Registered next to [`monitor_query_modes2`]; the OS calls this one only
/// on a framework below IddCx 1.10.
pub unsafe extern "C" fn monitor_query_modes(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: the framework supplies live in/out args for the call, with `pTargetModes` room for
    // `TargetModeBufferInputCount` IDDCX_TARGET_MODE entries.
    unsafe {
        let in_args = &*p_in;
        fill_target_modes(
            monitor,
            in_args.TargetModeBufferInputCount,
            in_args.pTargetModes,
            &mut *p_out,
            None,
            crate::monitor::target_mode,
        )
    }
}

/// HDR target modes: the same fill, emitting `IDDCX_TARGET_MODE2` with the per-mode wire bit-depth.
/// Mandatory under FP16, and the variant the OS actually calls on IddCx 1.10.
pub unsafe extern "C" fn monitor_query_modes2(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES2,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: the framework supplies live in/out args for the call, with `pTargetModes` room for
    // `TargetModeBufferInputCount` IDDCX_TARGET_MODE2 entries.
    unsafe {
        let in_args = &*p_in;
        fill_target_modes(
            monitor,
            in_args.TargetModeBufferInputCount,
            in_args.pTargetModes,
            &mut *p_out,
            Some("monitor_query_modes2"),
            crate::monitor::target_mode2,
        )
    }
}

/// The framework's `pPaths` array as a slice; empty when `ptr` is null.
///
/// # Safety
/// A non-null `ptr` must point at `count` initialized entries that outlive the returned slice.
unsafe fn path_slice<'a, P>(ptr: *const P, count: u32) -> &'a [P] {
    if ptr.is_null() {
        return &[];
    }
    // SAFETY: non-null, and per this function's contract `count` initialized entries.
    unsafe { core::slice::from_raw_parts(ptr, count as usize) }
}

/// Commit is a no-op for assign to drive — but the OS stamps each path ACTIVE/CHANGED here, and an
/// active→inactive flip on OUR head (while a sibling stays active) is the driver-visible form of
/// Enrico's hypothesis: the OS idles the virtual head like a physical one and the drain loop then
/// sees only E_PENDING with no unassign. Log every commit's per-path flags so a hole can be lined
/// up against a path the OS just deactivated. Low frequency (topology changes only). `Flags` is a
/// plain integer (bindgen `ModuleConsts`): `IDDCX_PATH_FLAGS_CHANGED = 0x1`, `_ACTIVE = 0x2`.
pub unsafe extern "C" fn adapter_commit_modes(
    _adapter: iddcx::IDDCX_ADAPTER,
    p_in: *const iddcx::IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    // SAFETY: the framework supplies a valid, live input-args pointer for the call.
    let in_args = unsafe { &*p_in };
    let count = in_args.PathCount;
    // SAFETY: `pPaths` points to `PathCount` valid `IDDCX_PATH` entries (framework contract).
    for (i, path) in unsafe { path_slice(in_args.pPaths, count) }
        .iter()
        .enumerate()
    {
        let bits = path.Flags;
        dbglog!(
            "[pf-vd] commit_modes: path[{i}/{count}] monitor={:?} active={} changed={} flags={bits:#x}",
            path.MonitorObject,
            bits & 0x2 != 0,
            bits & 0x1 != 0
        );
    }
    STATUS_SUCCESS
}

/// HDR (`*2`) commit over `IDDCX_PATH2`. Mandatory under FP16, and the one the OS actually calls
/// once `CAN_PROCESS_FP16` is set — so this is where the ACTIVE/CHANGED flags land in practice.
/// Same per-path logging as [`adapter_commit_modes`] (see its doc for why the flip matters).
pub unsafe extern "C" fn adapter_commit_modes2(
    _adapter: iddcx::IDDCX_ADAPTER,
    p_in: *const iddcx::IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    // SAFETY: the framework supplies a valid, live input-args pointer for the call.
    let in_args = unsafe { &*p_in };
    let count = in_args.PathCount;
    // SAFETY: `pPaths` points to `PathCount` valid `IDDCX_PATH2` entries (framework contract).
    for (i, path) in unsafe { path_slice(in_args.pPaths, count) }
        .iter()
        .enumerate()
    {
        let bits = path.Flags;
        dbglog!(
            "[pf-vd] commit_modes2: path[{i}/{count}] monitor={:?} active={} changed={} flags={bits:#x}",
            path.MonitorObject,
            bits & 0x2 != 0,
            bits & 0x1 != 0
        );
    }
    STATUS_SUCCESS
}

/// Report `HIGH_COLOR_SPACE` for HDR10 and `WIDE_COLOR_SPACE` for SDR wide colour, the FP16 SDR
/// desktop a 10-bit SDR session encodes. Mandatory under FP16.
pub unsafe extern "C" fn query_target_info(
    _adapter: iddcx::IDDCX_ADAPTER,
    _p_in: *mut iddcx::IDARG_IN_QUERYTARGET_INFO,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGET_INFO,
) -> NTSTATUS {
    // SAFETY: p_out is the framework's (uninitialised) out buffer; zero then set the one field we report.
    unsafe {
        core::ptr::write(p_out, iddcx::IDARG_OUT_QUERYTARGET_INFO::default());
        (*p_out).TargetCaps = iddcx::IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_WIDE_COLOR_SPACE
            | iddcx::IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE;
    }
    STATUS_SUCCESS
}

/// Accept the OS's default HDR10 static metadata (the host/client own the stream's final metadata).
/// Mandatory under FP16.
pub unsafe extern "C" fn set_default_hdr_metadata(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Accept (do not apply) the gamma ramp — the client display applies its own transform. MANDATORY once
/// FP16 is set, or the OS rejects the adapter at init ("Failed to get adapter").
pub unsafe extern "C" fn set_gamma_ramp(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_SET_GAMMARAMP,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// A swap-chain was assigned to the monitor: spawn the `SwapChainProcessor` that drains it (so
/// the monitor is a usable display), holding a `Weak` to the monitor it serves. Always returns
/// `STATUS_SUCCESS` — on D3D-init failure the swap-chain is deleted so the OS makes a fresh one
/// and re-assigns. Every processor this drops — the one it replaces, or one built for a monitor
/// the registry no longer has — joins its thread with no lock held.
pub unsafe extern "C" fn assign_swap_chain(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    // SAFETY: framework-provided in args, valid for the call.
    let in_args = unsafe { &*p_in };
    let swap_chain = in_args.hSwapChain;
    let render_adapter = in_args.RenderAdapterLuid;
    let new_frame_event = in_args.hNextSurfaceAvailable;

    // wdk-sys LUID → windows-crate LUID (identical { LowPart: u32, HighPart: i32 } layout). The render
    // adapter is the GPU the OS picked to render this virtual monitor; the pooled D3D device is keyed by
    // it (relevant on a hybrid iGPU+dGPU box).
    let luid = windows::Win32::Foundation::LUID {
        LowPart: render_adapter.LowPart,
        HighPart: render_adapter.HighPart,
    };
    dbglog!(
        "[pf-vd] assign_swap_chain: OS render adapter LUID = {:08x}:{:08x}",
        render_adapter.HighPart,
        render_adapter.LowPart
    );

    let entry = crate::registry::find(|m| m.object() == Some(monitor));
    // FIRST drop any existing processor on this monitor (joins its worker), with no lock held.
    if let Some(m) = &entry {
        drop(m.take_swap());
    }

    if let Some(device) = crate::direct_3d_device::pooled_device(luid) {
        // The encode session opens on the device this worker drains into.
        if let Some(m) = &entry {
            m.set_render_luid(luid);
        }
        let mut processor = crate::swap_chain_processor::SwapChainProcessor::new();
        processor.run(
            swap_chain,
            device,
            new_frame_event,
            entry.as_ref().map(Arc::downgrade).unwrap_or_default(),
        );
        match &entry {
            // Install; drop what it displaced (a race lost above) with no lock held.
            Some(m) => drop(m.set_swap(processor)),
            // No such monitor: the worker has nothing to attach to and joins right here.
            None => drop(processor),
        }
        // A mode is now committed on this path — re-declare the hardware cursor (the OS reverts
        // to a software cursor on every mode commit, which otherwise makes the cursor worker's
        // QueryHardwareCursor fail STATUS_NOT_SUPPORTED). No-op unless a cursor worker is live.
        if let Some(m) = &entry {
            m.resetup_cursor();
        }
    } else {
        // D3D init failed: delete the swap-chain so the OS generates a fresh one + retries.
        dbglog!(
            "[pf-vd] assign_swap_chain: pooled Direct3DDevice unavailable — deleting swap-chain for OS retry"
        );
        // SAFETY: `swap_chain` is the framework-provided IddCx swap-chain handle.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, swap_chain as WDFOBJECT);
        }
    }
    STATUS_SUCCESS
}

/// The monitor went inactive: take its processor out and drop it with no lock held — the drop
/// joins the worker thread, which deletes the swap-chain object before returning. The join
/// time is logged every time: the wake event should make it sub-millisecond, and a slow one
/// here is a DDI or D3D call the worker was inside when the OS unassigned it.
pub unsafe extern "C" fn unassign_swap_chain(monitor: iddcx::IDDCX_MONITOR) -> NTSTATUS {
    let had = crate::registry::find(|m| m.object() == Some(monitor)).and_then(|m| m.take_swap());
    let live = had.is_some();
    let started = std::time::Instant::now();
    drop(had);
    dbglog!(
        "[pf-vd] unassign_swap_chain — live processor: {live}, join took {} us",
        started.elapsed().as_micros()
    );
    STATUS_SUCCESS
}

/// The pf-driver-proto control plane. Returns `()` and completes the request itself (matches the C
/// `EVT_IDD_CX_DEVICE_IO_CONTROL` shape). STEP 4: dispatch the proto IOCTLs; for now just complete.
pub unsafe extern "C" fn device_io_control(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    _output_len: usize,
    _input_len: usize,
    ioctl_code: u32,
) {
    // SAFETY: `request` is the framework-provided WDFREQUEST; `control::dispatch` completes it exactly once.
    unsafe { crate::control::dispatch(request, ioctl_code) };
}

/// `EvtIddCxMonitorGetPhysicalSize` — the remote-driver-only DDI. IddCx obligates a remote-session
/// adapter to register it the way `CAN_PROCESS_FP16` obligates the `*2` set; the OS only CALLS it
/// for a remote monitor with no description, and ours always ships an EDID. The size mirrors the
/// EDID's own 16:9 block, since a zero here is invalid.
pub unsafe extern "C" fn monitor_get_physical_size(
    _monitor: iddcx::IDDCX_MONITOR,
    p_out: *mut iddcx::IDARG_OUT_MONITORGETPHYSICALSIZE,
) -> NTSTATUS {
    // SAFETY: the framework supplies a valid out-args pointer for the call.
    unsafe {
        (*p_out).PhysicalWidth = 597;
        (*p_out).PhysicalHeight = 336;
    }
    crate::STATUS_SUCCESS
}
