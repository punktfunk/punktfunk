//! IddCx adapter bring-up. Adapter creation is DEFERRED to the first `EvtDeviceD0Entry` (the adapter
//! object is only valid after D0), and is ASYNC: `init_adapter` builds the caps and calls
//! `IddCxAdapterInitAsync`; the adapter object arrives later via `EvtIddCxAdapterInitFinished`
//! (`adapter_init_finished` → [`set_adapter`]). FP16 caps + the obligated `*2`/gamma/hdr callbacks (in
//! `callbacks.rs`) together enable HDR.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use wdk_sys::{NTSTATUS, WDFDEVICE, iddcx};
use windows::core::w;

use crate::STATUS_SUCCESS;
use crate::worker::Sendable;

/// The IddCx adapter handle, stashed for later DDIs (e.g. `SET_RENDER_ADAPTER`).
type SendAdapter = Sendable<iddcx::IDDCX_ADAPTER>;

// A slot, NOT a OnceLock: `set_adapter` must be last-write-wins so a D0-resume re-init's fresh
// handle REPLACES the pre-power-cycle one (a OnceLock would keep the stale one for every later
// `IddCxMonitorCreate`). Poison-recovering lock idiom as in `monitor.rs` (panic = abort here,
// so poisoning is unreachable anyway).
static ADAPTER: Mutex<Option<SendAdapter>> = Mutex::new(None);

/// The WDFDEVICE the last `init_adapter` ran on (as an integer: the handle is opaque), so an ADD
/// that finds no adapter can ask for another init. `0` before the first D0 entry.
static DEVICE: AtomicUsize = AtomicUsize::new(0);

/// An `IddCxAdapterInitAsync` is in flight: its `adapter_init_finished` has not run yet, so a
/// second init would race the first.
static INIT_PENDING: AtomicBool = AtomicBool::new(false);

/// `adapter_init_finished` ran, whatever it reported.
pub fn init_settled() {
    INIT_PENDING.store(false, Ordering::Release);
}

/// Ask for the adapter init again — an ADD found no adapter because the async init failed, and
/// for a root-enumerated devnode no further D0 entry would ever retry it. `true` when an init
/// was issued; the calling ADD still fails, the host's retry lands after the completion.
pub fn retry_init() -> bool {
    if adapter().is_some() || INIT_PENDING.load(Ordering::Acquire) {
        return false;
    }
    let device = DEVICE.load(Ordering::Acquire);
    if device == 0 {
        return false;
    }
    dbglog!("[pf-vd] adapter: no adapter at ADD — re-issuing the init");
    // SAFETY: `DEVICE` is the WDFDEVICE the last D0 entry ran on. An ADD arrives on that
    // device's own queue, so the device is live; this WUDFHost hosts no other
    // (`ProcessSharingDisabled`).
    unsafe { init_adapter(device as WDFDEVICE) >= 0 }
}

/// Set once this device takes the remote-session role. The OS starts that adapter itself and then
/// expects a display on it, so the seat path presents one without waiting for a host ADD.
static SEAT_ROLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True when this device is a seat (remote-session) adapter rather than the console one.
pub fn is_seat_role() -> bool {
    SEAT_ROLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build the adapter caps (FP16/HDR-capable) and kick off the async adapter creation. Called from
/// `EvtDeviceD0Entry`, and from an ADD that found no adapter; idempotent across re-entrant D0
/// transitions.
///
/// # Safety
/// `device` must be a live `WDFDEVICE`.
pub unsafe fn init_adapter(device: WDFDEVICE) -> NTSTATUS {
    // A D0 entry that lands while the first async init is in flight must not issue a second
    // adapter; last-write-wins on `set_adapter` would leak the first. One compare-exchange is
    // the gate: an ADD's retry and the PnP thread can arrive together.
    if adapter().is_some()
        || INIT_PENDING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return STATUS_SUCCESS;
    }
    dbglog!("[pf-vd] init_adapter");
    DEVICE.store(device as usize, Ordering::Release);

    // Firmware/hardware version (telemetry): both point at one IDDCX_ENDPOINT_VERSION, a stack
    // local read synchronously by IddCxAdapterInitAsync. `.Size` is `size_of` throughout (the
    // IddCx 1.10 structs).
    let mut version = iddcx::IDDCX_ENDPOINT_VERSION {
        Size: core::mem::size_of::<iddcx::IDDCX_ENDPOINT_VERSION>() as u32,
        MajorVer: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
        MinorVer: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
        Build: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
        ..Default::default()
    };

    // Endpoint diagnostics. `pEndPointModelName` must be a non-empty string, and `w!` is what keeps
    // the three name pointers 'static — IddCx reads them after this frame. GammaSupport MUST be set:
    // a zeroed value is IDDCX_FEATURE_IMPLEMENTATION_UNINITIALIZED (0), which the framework's adapter
    // Validate rejects with INVALID_PARAMETER — set it to NONE (1) like upstream.
    let mut diag = iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
        Size: core::mem::size_of::<iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32,
        GammaSupport: iddcx::IDDCX_FEATURE_IMPLEMENTATION::IDDCX_FEATURE_IMPLEMENTATION_NONE,
        TransmissionType: iddcx::IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,
        pEndPointFriendlyName: w!("Punktfunk Virtual Display Adapter").as_ptr(),
        pEndPointManufacturerName: w!("Punktfunk").as_ptr(),
        pEndPointModelName: w!("Virtual Display").as_ptr(),
        // SAFETY: `version` is a stack local that outlives this `init_adapter` call; IddCxAdapterInitAsync
        // (below) reads through these pointers SYNCHRONOUSLY, before `version` drops — the pointer never escapes.
        pFirmwareVersion: (&raw mut version).cast(),
        pHardwareVersion: (&raw mut version).cast(),
    };

    // HDR: declare FP16 (scRGB) desktop surfaces, which marks the virtual monitor
    // advanced-color-capable (the host then sees display_hdr=true). It needs the obligated *2/HDR
    // DDIs registered in entry.rs; GammaSupport stays NONE (set above). The enum is bindgen
    // ModuleConsts: the variant is a plain-int const assignable straight to `Flags`.
    let mut caps = iddcx::IDDCX_ADAPTER_CAPS {
        Size: core::mem::size_of::<iddcx::IDDCX_ADAPTER_CAPS>() as u32,
        Flags: iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16,
        ..Default::default()
    };
    // IddCx roles are exclusive, so the role is per DEVICE and the hardware id decides it. The
    // shipped console devnode is `Root\pf_vdisplay` and structurally cannot take the seat branch
    // below (`design/windows-seat-display-tier.md`).
    // SAFETY: `device` is a live WDFDEVICE per this function's contract, which is the contract
    // `query_hardware_ids` requires.
    let hardware_ids = unsafe { pf_umdf_util::wdf::query_hardware_ids(device) };
    caps.MaxMonitorsSupported = 16;
    // `rdpidd_indirectdisplay` is the devnode the terminal-services stack creates for a session and
    // then starts itself — the one thing it will start. This driver outranks the inbox one for that
    // id, so the seat role has to cover it too.
    let seat_devnode = hardware_ids.contains("pf_vdisplay_indirectdisplay")
        || hardware_ids.contains("rdpidd_indirectdisplay");
    SEAT_ROLE.store(seat_devnode, std::sync::atomic::Ordering::Relaxed);
    if seat_devnode {
        // A remote adapter must also set USE_SMALLEST_MODE — IddCx rejects the pair
        // REMOTE_SESSION_DRIVER-without-it as STATUS_NOT_SUPPORTED. FP16 stays on: our monitor modes
        // carry HDR wire bits, and without it every mode fails validation and no monitor arrives.
        // `PFVD_SEAT_CAPS` overrides the mask while the shape is still being probed.
        let caps_override = crate::log::knob("PFVD_SEAT_CAPS").and_then(|v| v.parse::<u32>().ok());
        caps.Flags = caps_override.unwrap_or(
            iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_REMOTE_SESSION_DRIVER
                | iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE
                | iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16,
        );
        // The OS keeps every active mode inside this bandwidth budget. Our modes report no rate of
        // their own, so it only has to be non-zero — zero leaves nothing schedulable. Seat-only:
        // the console adapter shipped with 0 and stays untouched. Pixels/s at 16 × 4K144.
        caps.MaxDisplayPipelineRate = crate::log::knob("PFVD_PIPELINE_RATE")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(16 * 4096 * 2160 * 144);
        // Transmission stays WIRED_OTHER: the framework validates the enum and rejects
        // NETWORK_OTHER (9) outright, header notwithstanding. The monitor count keeps the console's
        // 16 because a monitor id has to be BELOW it, and the host numbers its first monitor 1.
        diag.TransmissionType = crate::log::knob("PFVD_SEAT_TRANSMISSION")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v as _)
            .unwrap_or(iddcx::IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER);
        caps.MaxMonitorsSupported = crate::log::knob("PFVD_SEAT_MONITORS")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(16);
        dbglog!(
            "[pf-vd] adapter: seat devnode ({hardware_ids}) caps={:#x} monitors={} transmission={} rate={}",
            caps.Flags,
            caps.MaxMonitorsSupported,
            diag.TransmissionType,
            caps.MaxDisplayPipelineRate
        );
    } else {
        dbglog!("[pf-vd] adapter: console role (hwids: {hardware_ids})");
    }
    caps.EndPointDiagnostics = diag;

    // The adapter WDF object's attributes. Execution/Synchronization must be spelled out: a zeroed
    // field is *Invalid*, not InheritFromParent. No context type — nothing reads adapter state off
    // the WDF object; the handle lives in [`ADAPTER`].
    let mut attr = wdk_sys::WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32,
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    };
    let init = iddcx::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device,
        pCaps: &raw mut caps,
        ObjectAttributes: &raw mut attr,
    };
    let mut out = iddcx::IDARG_OUT_ADAPTER_INIT::default();
    // SAFETY: `device` is live per this function's contract; `init`/`out` are valid local storage
    // IddCxAdapterInitAsync reads synchronously (the adapter object itself is delivered later via
    // adapter_init_finished). `INIT_PENDING` keeps a second init from racing this one.
    let st = unsafe { wdk_iddcx::IddCxAdapterInitAsync(&init, &mut out) };
    dbglog!("[pf-vd] IddCxAdapterInitAsync -> {st:#x}");
    if st < 0 {
        INIT_PENDING.store(false, Ordering::Release);
    }
    st
}

/// Stash the adapter object delivered by `EvtIddCxAdapterInitFinished` (`adapter()` reads it).
/// Last write wins — see [`ADAPTER`].
pub fn set_adapter(adapter: iddcx::IDDCX_ADAPTER) {
    *ADAPTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Sendable(adapter));
}

/// Forget the cached adapter. Called on a D0 re-entry from a REAL low-power state
/// (`callbacks::device_d0_entry`): the handle belongs to the pre-power-cycle incarnation, and
/// clearing is what lets `init_adapter` run again instead of short-circuiting on it.
pub fn clear_adapter() {
    *ADAPTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The created adapter handle, once `EvtIddCxAdapterInitFinished` has fired — for `create_monitor`
/// (`IddCxMonitorCreate`) and SET_RENDER_ADAPTER. `None` before adapter init completes.
pub(crate) fn adapter() -> Option<iddcx::IDDCX_ADAPTER> {
    ADAPTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|a| a.0)
}

/// The pin in force. IddCx has one render adapter per IddCx adapter, so this cannot be
/// scoped: another owner may repeat it, not move it ([`set_render_adapter`]).
static RENDER_PIN: Mutex<Option<i64>> = Mutex::new(None);

/// Honor `owner`'s `IOCTL_SET_RENDER_ADAPTER`: pin the GPU the IddCx swap-chain renders on. On a
/// hybrid iGPU+dGPU box the OS may otherwise pick the iGPU to render the virtual monitor, and the
/// encode pool then opens on an adapter whose encoder the host never selected. The pin is
/// adapter-wide and moving it flaps every live swap-chain, so a different GPU is refused with
/// `STATUS_ACCESS_DENIED` while any other host holds a monitor (the seat placeholder is
/// nobody's); the host tolerates that and streams on the GPU in force. `STATUS_NOT_FOUND`
/// before the adapter exists.
pub fn set_render_adapter(owner: u32, luid_low: u32, luid_high: i32) -> NTSTATUS {
    let Some(adapter) = adapter() else {
        return crate::STATUS_NOT_FOUND;
    };
    let packed = (i64::from(luid_high) << 32) | i64::from(luid_low);
    let pin = *crate::registry::lock(&RENDER_PIN);
    // Repeating the pinned GPU is free; moving it is refused while anyone else streams on it.
    if pin.is_some_and(|held| held != packed)
        && let Some(other) = crate::registry::find(|m| {
            m.owner != owner && m.owner != crate::monitor::SEAT_PLACEHOLDER_OWNER
        })
    {
        dbglog!(
            "[pf-vd] set_render_adapter: owner {owner} asked for {luid_high:08x}:{luid_low:08x} \
             while owner {} holds monitors on the pinned GPU — refused",
            other.owner
        );
        return crate::STATUS_ACCESS_DENIED;
    }
    *crate::registry::lock(&RENDER_PIN) = Some(packed);
    let in_args = iddcx::IDARG_IN_ADAPTERSETRENDERADAPTER {
        PreferredRenderAdapter: wdk_sys::LUID {
            LowPart: luid_low,
            HighPart: luid_high,
        },
    };
    dbglog!("[pf-vd] set_render_adapter -> {luid_high:08x}:{luid_low:08x}");
    // SAFETY: `adapter` is the stashed IddCx adapter; `in_args` is valid local storage read synchronously.
    unsafe { wdk_iddcx::IddCxAdapterSetRenderAdapter(adapter, &in_args) };
    STATUS_SUCCESS
}
