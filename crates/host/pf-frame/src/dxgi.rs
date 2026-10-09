//! DXGI capture identity and the shared D3D11 device factory.
//!
//! [`WinCaptureTarget`], [`D3d11Frame`], [`pack_luid`], and [`make_device`] live here so
//! capture (IDD-push), encode (D3D11 backends), and pf-vdisplay share one identity type
//! and one device factory without a capture↔encode↔vdisplay crate cycle.
//!
//! [`make_device`] builds a free-threaded D3D11 device on a chosen adapter and raises the
//! process GPU scheduling class (see [`elevate_process_gpu_priority`]). Recreate the
//! device after ACCESS_LOST: a device born on one desktop cannot duplicate another.
//!
//! Win32u GPU-preference, HDR converters, and DXGI self-tests stay in the capture crate.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory4,
};

#[derive(Clone)]
pub struct WinCaptureTarget {
    /// Packed DXGI adapter LUID: `(HighPart << 32) | (LowPart & 0xffff_ffff)`.
    pub adapter_luid: i64,
    /// GDI device name. Changes across a secure-desktop switch; re-resolve from [`Self::target_id`].
    pub gdi_name: String,
    /// IddCx target id. Stable across GDI-name changes; re-resolve on every recovery.
    pub target_id: u32,
    /// WUDFHost pid from the ADD reply — the process IDD-push duplicates sealed-channel
    /// handles into. `0` = unknown.
    pub wudf_pid: u32,
    /// IddCx hardware-cursor declare is adapter-wide and irrevocable until adapter reset;
    /// DWM then omits the pointer from its frames. A session without the cursor channel
    /// must self-composite or the stream has no cursor.
    pub cursor_excluded: bool,
}

/// PyroWave zero-copy share: second plane + cross-device fence.
///
/// The wavelet encoder takes two shareable textures, not planar NV12: full-res `R8_UNORM`
/// Y on [`D3d11Frame::texture`], half-res `R8G8_UNORM` CbCr here. NVIDIA's Vulkan import
/// of a single planar NV12 is unreliable at arbitrary sizes. `None` on NVENC/AMF/QSV
/// frames. The encoder mints each texture's shared handle on demand.
pub struct PyroFrameShare {
    /// Half-res `R8G8_UNORM` CbCr, created `SHARED | SHARED_NTHANDLE`.
    pub cbcr: ID3D11Texture2D,
    /// Shared D3D11/D3D12 fence NT handle. Encoder imports (duplicating) on first frame
    /// or after an encoder rebuild.
    pub fence_handle: Option<isize>,
    /// Fence value signalled after this frame's convert. Encoder Vulkan acquire waits
    /// on it so the wavelet read follows the D3D11 CSC.
    pub fence_value: u64,
    /// Capturer texture-ring generation. Encoder caches plane imports by COM address,
    /// which the allocator can recycle after a ring recreate; flush the cache when this
    /// changes.
    pub ring_gen: u32,
}

/// GPU-resident captured texture. NVENC/AMF/QSV encode in place; PyroWave imports
/// `texture` (Y) plus [`Self::pyro`] (CbCr) into its Vulkan device.
pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
    /// PyroWave CbCr + fence. `None` unless this is a PyroWave session.
    pub pyro: Option<PyroFrameShare>,
}
// SAFETY: `ID3D11Texture2D` and `ID3D11Device` are COM pointers with interlocked
// refcounting. `make_device` does not pass `D3D11_CREATE_DEVICE_SINGLETHREADED`, so
// the device is free-threaded. The value is moved, never aliased (`Send` without
// `Sync`); the single-threaded immediate context is never used concurrently.
unsafe impl Send for D3d11Frame {}

pub fn pack_luid(luid: LUID) -> i64 {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64 & 0xffff_ffff)
}

/// Fresh D3D11 device + context on `adapter` (`D3D_DRIVER_TYPE_UNKNOWN`).
///
/// Recreate on ACCESS_LOST: a device created on one desktop cannot sustain a
/// duplication on another, so a secure-desktop switch needs a device made while
/// the thread is attached to that desktop.
///
/// # Safety
/// `adapter` must stay live for the call. No lasting alias is taken; the returned
/// device/context own the new COM objects.
pub unsafe fn make_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: `adapter` is live for the call (caller contract). The two out-params are local
    // `Option`s the callee only writes, and both are checked below before use.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let device = device.context("null D3D11 device")?;
    let context = context.context("null D3D11 context")?;

    // Process-level GPU scheduling so capture/encode is not starved by a saturating game.
    // Thread priority and a 1-frame latency cap are secondary.
    elevate_process_gpu_priority();
    if let Ok(dxgi_dev) = device.cast::<IDXGIDevice>() {
        // Absolute max GPU thread priority (`0x4000_001E`); fall back to relative +7.
        // SAFETY: `dxgi_dev` is a live interface just obtained by a checked `cast`; both calls take
        // a scalar and only report failure through their return value.
        if unsafe { dxgi_dev.SetGPUThreadPriority(0x4000_001E) }.is_err()
            // SAFETY: same live interface, same scalar-in/HRESULT-out contract as the call above.
            // Own block so the `&&` short-circuit still skips the relative-priority call.
            && unsafe { dxgi_dev.SetGPUThreadPriority(7) }.is_err()
        {
            tracing::warn!("SetGPUThreadPriority failed (run as admin/SYSTEM for GPU priority)");
        }
    }
    if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
        // SAFETY: `dxgi1` is a live interface from a checked `cast`; the arg is a scalar.
        let _ = unsafe { dxgi1.SetMaximumFrameLatency(1) };
    }
    Ok((device, context))
}

/// The DXGI adapter with `luid`; `None` when the LUID is unset or names no adapter.
pub fn adapter_by_luid(luid: Option<LUID>) -> Option<IDXGIAdapter1> {
    let luid = luid?;
    // SAFETY: both calls return an owned COM reference or an error; nothing is borrowed.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        factory.EnumAdapterByLuid(luid).ok()
    }
}

/// A throwaway D3D11 device for a capability probe: on the adapter with `luid`, else the OS
/// default hardware adapter. Unlike [`make_device`] it raises no GPU priority.
pub fn probe_device(luid: Option<LUID>, flags: D3D11_CREATE_DEVICE_FLAG) -> Option<ID3D11Device> {
    let adapter = adapter_by_luid(luid);
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: `adapter` is an owned COM reference live for the call; `device` is a local
    // out-param the callee fills only on success.
    let created = unsafe {
        match &adapter {
            Some(a) => D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
            None => D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
        }
    };
    created.ok()?;
    device
}

/// `PUNKTFUNK_GPU_PRIORITY_CLASS` policy.
enum PrioMode {
    /// Skip the D3DKMT call; this is not class 0 (IDLE).
    Off,
    /// Fixed D3DKMT class: `normal`=2, `high`=4, `realtime`=5.
    Static(i32),
}

/// Resolve `PUNKTFUNK_GPU_PRIORITY_CLASS` (`off|normal|high|realtime`).
///
/// D3DKMT_SCHEDULINGPRIORITYCLASS: IDLE 0, BELOW_NORMAL 1, NORMAL 2, ABOVE_NORMAL 3,
/// HIGH 4, REALTIME 5. Default is REALTIME so capture/convert/encode preempts a
/// saturating game. Trap: REALTIME + NVIDIA + HAGS + near-full VRAM hangs NVENC —
/// set `high` then. Costing local game fps under load is by design.
fn configured_gpu_priority_mode() -> PrioMode {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => PrioMode::Off,
        Some("normal") => PrioMode::Static(2),
        Some("high") => PrioMode::Static(4),
        // `realtime`, unset, and anything unrecognized all land on REALTIME.
        _ => PrioMode::Static(5),
    }
}

/// Enable `SE_INC_BASE_PRIORITY` on this process token (best-effort).
///
/// The kernel gates HIGH/REALTIME GPU scheduling on it. SYSTEM/Administrators
/// hold it; a UAC-filtered token does not, and the warning says so.
fn enable_inc_base_priority() {
    if let Err(e) = crate::privilege::enable("SeIncreaseBasePriorityPrivilege") {
        tracing::warn!(error = %e, "SE_INC_BASE_PRIORITY not enabled (run as admin/SYSTEM for GPU priority)");
    }
}

/// `gdi32!D3DKMTSetProcessSchedulingPriorityClass(process, prio)` by name (no
/// stable windows-rs binding). Returns NTSTATUS (0 = success) or `None` if the
/// export is missing. Caller must hold `SE_INC_BASE_PRIORITY` for HIGH/REALTIME;
/// the kernel checks the caller whether the target is self or a child.
unsafe fn d3dkmt_set_scheduling_priority_class(
    process: windows::Win32::Foundation::HANDLE,
    prio: i32,
) -> Option<i32> {
    use windows::core::s;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    // SAFETY: both take static NUL-terminated literals; `LoadLibraryA` returns a module handle the
    // process keeps for its lifetime (gdi32 is never unloaded here), and `GetProcAddress` is passed
    // that live handle. Both results are checked by `?` before use.
    let gdi32 = unsafe { LoadLibraryA(s!("gdi32.dll")) }.ok()?;
    // SAFETY: `gdi32` is the live module handle the checked `LoadLibraryA` just returned, and the
    // export name is a static NUL-terminated literal; the result is checked by `?` before use.
    let p = unsafe { GetProcAddress(gdi32, s!("D3DKMTSetProcessSchedulingPriorityClass")) }?;
    type SetPrio = unsafe extern "system" fn(HANDLE, i32) -> i32;
    // SAFETY: `p` is the non-null export just resolved, and `SetPrio` is its documented signature
    // (`NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)`,
    // both arguments 4/8-byte scalars). `process` is a valid handle by this fn's own contract.
    let f: SetPrio = unsafe { std::mem::transmute(p) };
    // SAFETY: `f` is that export transmuted to its documented signature directly above; `process`
    // is a valid handle by this fn's own contract and `prio` is a plain scalar. The call returns an
    // NTSTATUS and retains nothing.
    Some(unsafe { f(process, prio) })
}

/// Raise this process's D3DKMT GPU scheduling class (once, best-effort).
fn elevate_process_gpu_priority() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use windows::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: the current-process pseudo-handle is always valid, carries every access
        // right and needs no close.
        unsafe { elevate_gpu_priority_of(GetCurrentProcess(), "self") };
    });
}

/// Raise `process`'s D3DKMT GPU scheduling class (best-effort, logged per call). `what`
/// names the process in the log.
///
/// Process class is the cross-process lever; `SetGPUThreadPriority` alone does not
/// unstarve encode under a GPU-bound game. It covers every GPU context the process owns,
/// including ones a driver creates internally (NVENC's RGB→YUV pass). Default REALTIME;
/// see [`configured_gpu_priority_mode`]. The kernel checks this caller's
/// `SE_INC_BASE_PRIORITY`, so it no-ops under a UAC-filtered token.
///
/// # Safety
/// `process` must be a live process handle carrying `PROCESS_SET_INFORMATION`.
pub unsafe fn elevate_gpu_priority_of(process: windows::Win32::Foundation::HANDLE, what: &str) {
    let prio = match configured_gpu_priority_mode() {
        PrioMode::Off => {
            tracing::info!(
                process = what,
                "GPU scheduling priority class left at default (off)"
            );
            return;
        }
        PrioMode::Static(p) => p,
    };
    enable_inc_base_priority();
    // SAFETY: `process` is live and carries the access right, per this fn's contract.
    match unsafe { d3dkmt_set_scheduling_priority_class(process, prio) } {
        Some(0) => tracing::info!(
            process = what,
            priority_class = prio,
            "GPU scheduling priority class set (2=normal 4=high 5=realtime)"
        ),
        Some(st) => tracing::warn!(
            process = what,
            status = format!("0x{st:08X}"),
            "GPU scheduling priority class not raised (the host needs admin/SYSTEM)"
        ),
        None => tracing::warn!("D3DKMTSetProcessSchedulingPriorityClass export not found"),
    }
}
