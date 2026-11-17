//! Safe-ish typed wrappers over the wdk-sys IddCx DDIs, dispatched through the `IddFunctions` table
//! (indexed by `_IDDFUNCENUM::<Name>TableIndex`, with `IddDriverGlobals` as the implicit first arg) —
//! the same model wdk-sys uses for WDF. Covers the whole DDI set the pf-vdisplay driver needs.
//!
//! Each DDI pins its `(_IDDFUNCENUM index, PFN_* type)` pair in exactly ONE place (the macro
//! invocation) — a wrong pairing is the only way the table dispatch can be UB, so it must never
//! be expressed twice. All wrappers but the void `IddCxAdapterSetRenderAdapter` return the raw
//! `NTSTATUS`; the caller classifies it with [`nt_success`]. The HRESULT-shaped SwapChain DDIs
//! are classified by hand at their call sites in pf-vdisplay's `swap_chain_processor`
//! (`E_PENDING` means wait).
//!
//! The `const _` items at the tail assert that the wdk-sys `iddcx` bindgen still emits and resolves
//! the whole struct + callback surface, so an allowlist regression fails CI instead of the box.
#![no_std]
#![allow(non_snake_case, clippy::missing_safety_doc)]
// The workspace lints (`[lints] workspace = true`) deny `unsafe_op_in_unsafe_fn` and undocumented
// `unsafe {}` blocks: this IddCx DDI dispatch layer is unsafe-audited, not unsafe-free.

pub use wdk_sys::iddcx;

use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

/// `NT_SUCCESS` — IddCx DDIs that return a true NTSTATUS are errors iff negative.
#[inline]
#[must_use]
pub const fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

/// Read one typed DDI from IddCxStub's runtime-populated flexible table.
///
/// The binding declares a zero-length array, so `wrapping_add` addresses the foreign tail without
/// falsely asserting it lies inside that Rust object. Runtime assertions reject negative indices
/// and a `T` whose width differs from the table's function-pointer slots.
///
/// # Safety
/// `index` and `T` name the same DDI, and IddCxStub has populated that slot.
#[inline]
unsafe fn ddi<T: Copy>(index: i32) -> T {
    assert!(index >= 0, "negative IddCx DDI index");
    assert_eq!(
        core::mem::size_of::<T>(),
        core::mem::size_of::<iddcx::PFN_IDD_CX>(),
        "IddCx DDI type has the wrong width"
    );
    let table = (&raw const iddcx::IddFunctions).cast::<iddcx::PFN_IDD_CX>();
    let slot = table.wrapping_add(index as usize);
    // SAFETY: the contract says IddCxStub populated this foreign tail slot with the matching `T`.
    unsafe { slot.cast::<T>().read() }
}

/// The IddCx driver globals (set by `IddCxStub`), passed as the implicit first arg to every DDI.
///
/// # Safety
/// Only valid once the driver is loaded by the IddCx runtime.
#[inline]
unsafe fn globals() -> iddcx::PIDD_DRIVER_GLOBALS {
    // SAFETY: we only read the pointer value of the stub-provided global.
    unsafe { (&raw const iddcx::IddDriverGlobals).read() }
}

/// Generate a typed outbound-DDI wrapper. Body is identical for every DDI (table lookup → call with the
/// globals); only the `(index, PFN_*, args)` triple varies, and it appears here exactly once.
macro_rules! iddcx_ddi {
    (
        $(#[$meta:meta])*
        $name:ident ( $( $arg:ident : $aty:ty ),* $(,)? ) @ $idx:ident as $pfn:ident
    ) => {
        $(#[$meta])*
        ///
        /// # Safety
        /// Call only after the driver is loaded by IddCx; pointers must satisfy the IddCx contract.
        #[inline]
        pub unsafe fn $name( $( $arg: $aty ),* ) -> NTSTATUS {
            // SAFETY: `$idx`/`$pfn` are the matched IddCx table index + PFN type (pinned by this macro
            // invocation), and the table is populated once the driver is loaded (this fn's contract).
            let f: iddcx::$pfn = unsafe { ddi(iddcx::_IDDFUNCENUM::$idx) };
            // SAFETY: only reads the stub-provided globals pointer; valid post-load per the contract.
            let g = unsafe { globals() };
            // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
            unsafe { (f.unwrap())(g, $( $arg ),* ) }
        }
    };
    // void-returning variant (e.g. IddCxAdapterSetRenderAdapter): the DDI's PFN returns `()`.
    (
        $(#[$meta:meta])*
        $name:ident ( $( $arg:ident : $aty:ty ),* $(,)? ) @ $idx:ident as $pfn:ident -> ()
    ) => {
        $(#[$meta])*
        ///
        /// # Safety
        /// Call only after the driver is loaded by IddCx; pointers must satisfy the IddCx contract.
        #[inline]
        pub unsafe fn $name( $( $arg: $aty ),* ) {
            // SAFETY: `$idx`/`$pfn` are the matched IddCx table index + PFN type (pinned by this macro
            // invocation), and the table is populated once the driver is loaded (this fn's contract).
            let f: iddcx::$pfn = unsafe { ddi(iddcx::_IDDFUNCENUM::$idx) };
            // SAFETY: only reads the stub-provided globals pointer; valid post-load per the contract.
            let g = unsafe { globals() };
            // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
            unsafe { (f.unwrap())(g, $( $arg ),* ) }
        }
    };
}

iddcx_ddi!(
    /// Configure a `WDFDEVICE_INIT` for IddCx (call before `WdfDeviceCreate`).
    IddCxDeviceInitConfig(device_init: PWDFDEVICE_INIT, config: *const iddcx::IDD_CX_CLIENT_CONFIG)
        @ IddCxDeviceInitConfigTableIndex as PFN_IDDCXDEVICEINITCONFIG
);
iddcx_ddi!(
    /// Finish IddCx device init (call after `WdfDeviceCreate`).
    IddCxDeviceInitialize(device: WDFDEVICE)
        @ IddCxDeviceInitializeTableIndex as PFN_IDDCXDEVICEINITIALIZE
);
iddcx_ddi!(
    /// Create the WDDM adapter asynchronously; `out.AdapterObject` is the `IDDCX_ADAPTER`.
    IddCxAdapterInitAsync(
        in_args: *const iddcx::IDARG_IN_ADAPTER_INIT,
        out_args: *mut iddcx::IDARG_OUT_ADAPTER_INIT,
    ) @ IddCxAdapterInitAsyncTableIndex as PFN_IDDCXADAPTERINITASYNC
);
iddcx_ddi!(
    /// Publish a REMOTE adapter's display configuration. The OS stores it and then reconfigures the
    /// monitors' swap chains to match; without it a remote adapter never gets one and the remoting
    /// stack discards the session's display as unusable. The `2` form is obligatory for a 1.10
    /// driver that declares `CAN_PROCESS_FP16`, exactly as for the mode and commit DDIs.
    IddCxAdapterDisplayConfigUpdate2(
        adapter: iddcx::IDDCX_ADAPTER,
        in_args: *const iddcx::IDARG_IN_ADAPTERDISPLAYCONFIGUPDATE2,
    ) @ IddCxAdapterDisplayConfigUpdate2TableIndex as PFN_IDDCXADAPTERDISPLAYCONFIGUPDATE2
);
iddcx_ddi!(
    /// Create a monitor on the adapter; `out.MonitorObject` is the `IDDCX_MONITOR`.
    IddCxMonitorCreate(
        adapter: iddcx::IDDCX_ADAPTER,
        in_args: *const iddcx::IDARG_IN_MONITORCREATE,
        out_args: *mut iddcx::IDARG_OUT_MONITORCREATE,
    ) @ IddCxMonitorCreateTableIndex as PFN_IDDCXMONITORCREATE
);
iddcx_ddi!(
    /// Announce monitor arrival; `out.OsTargetId`/`out.OsAdapterLuid` come back.
    IddCxMonitorArrival(
        monitor: iddcx::IDDCX_MONITOR,
        out_args: *mut iddcx::IDARG_OUT_MONITORARRIVAL,
    ) @ IddCxMonitorArrivalTableIndex as PFN_IDDCXMONITORARRIVAL
);
iddcx_ddi!(
    /// Depart a monitor (host disconnect / session teardown).
    IddCxMonitorDeparture(monitor: iddcx::IDDCX_MONITOR)
        @ IddCxMonitorDepartureTableIndex as PFN_IDDCXMONITORDEPARTURE
);
iddcx_ddi!(
    /// Declare hardware-cursor support for a monitor (the cursor channel): the OS stops
    /// compositing the pointer into the desktop image and signals `hNewCursorDataAvailable`
    /// on every cursor change instead (drained via [`IddCxMonitorQueryHardwareCursor3`]).
    IddCxMonitorSetupHardwareCursor(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_SETUP_HWCURSOR,
    ) @ IddCxMonitorSetupHardwareCursorTableIndex as PFN_IDDCXMONITORSETUPHARDWARECURSOR
);
iddcx_ddi!(
    /// Drain one hardware-cursor update: position/visibility always; shape bytes are copied
    /// into `in_args.pShapeBuffer` only when the OS's shape id moved past `LastShapeId`.
    IddCxMonitorQueryHardwareCursor(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_QUERY_HWCURSOR,
        out_args: *mut iddcx::IDARG_OUT_QUERY_HWCURSOR,
    ) @ IddCxMonitorQueryHardwareCursorTableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR
);
iddcx_ddi!(
    /// The v3 hardware-cursor drain — same input, richer output (adds `PositionValid` /
    /// `PositionId` / `SdrWhiteLevel`). The base [`IddCxMonitorQueryHardwareCursor`] slot answers
    /// `STATUS_NOT_SUPPORTED` to this FP16 driver; IddCx 1.10 has FP16 drivers query only through
    /// `3`, so this is the live query DDI.
    IddCxMonitorQueryHardwareCursor3(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_QUERY_HWCURSOR,
        out_args: *mut iddcx::IDARG_OUT_QUERY_HWCURSOR3,
    ) @ IddCxMonitorQueryHardwareCursor3TableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR3
);
iddcx_ddi!(
    /// Set the preferred render adapter (LUID) for the virtual adapter.
    IddCxAdapterSetRenderAdapter(
        adapter: iddcx::IDDCX_ADAPTER,
        in_args: *const iddcx::IDARG_IN_ADAPTERSETRENDERADAPTER,
    ) @ IddCxAdapterSetRenderAdapterTableIndex as PFN_IDDCXADAPTERSETRENDERADAPTER -> ()
);
iddcx_ddi!(
    /// Refresh a LIVE monitor's target-mode list (the HDR `*2` variant, IddCx 1.10 — the same API
    /// family as the `*2` mode/buffer DDIs this driver already requires): the OS re-evaluates which
    /// modes the target supports WITHOUT a monitor departure, so the host can then mode-set to a
    /// freshly-advertised mode in place (the mid-stream resize).
    IddCxMonitorUpdateModes2(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_UPDATEMODES2,
    ) @ IddCxMonitorUpdateModes2TableIndex as PFN_IDDCXMONITORUPDATEMODES2
);
iddcx_ddi!(
    /// Bind a D3D device to an assigned swap-chain. HRESULT-shaped; a failure means the OS has
    /// already unassigned it, so the caller drops the swap-chain and waits for the reassign.
    IddCxSwapChainSetDevice(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *const iddcx::IDARG_IN_SWAPCHAINSETDEVICE,
    ) @ IddCxSwapChainSetDeviceTableIndex as PFN_IDDCXSWAPCHAINSETDEVICE
);
iddcx_ddi!(
    /// Release the previous frame and acquire the next (HDR/FP16 variant). HRESULT-shaped; E_PENDING
    /// (0x8000000A) means wait on the surface-available event.
    IddCxSwapChainReleaseAndAcquireBuffer2(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *mut iddcx::IDARG_IN_RELEASEANDACQUIREBUFFER2,
        out_args: *mut iddcx::IDARG_OUT_RELEASEANDACQUIREBUFFER2,
    ) @ IddCxSwapChainReleaseAndAcquireBuffer2TableIndex as PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER2
);
iddcx_ddi!(
    /// Signal that the acquired frame has been processed. HRESULT-shaped.
    IddCxSwapChainFinishedProcessingFrame(swap_chain: iddcx::IDDCX_SWAPCHAIN)
        @ IddCxSwapChainFinishedProcessingFrameTableIndex as PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME
);
iddcx_ddi!(
    /// Raise the swap-chain's processing D3D device to realtime GPU scheduling priority — "higher
    /// than any regular application can set" (IddCx 1.9) — so buffer processing outruns ordinary
    /// GPU contention. It does NOT help against adapter-wide display servicing (modeset-class DDIs
    /// idle the hardware outright); it defends the contention case only. Best-effort at the call
    /// site: the DDI itself may decline (E_NOTIMPL on a WDDM 1.x device).
    ///
    /// Table-slot availability rests on the driver's `IddMinimumVersionRequired = 10` export
    /// (pf-vdisplay lib.rs): the loader refuses to bind a framework older than IddCx 1.10, so
    /// every slot of our compiled 1.10 surface — this 1.9 one included — is populated wherever the
    /// driver runs at all.
    IddCxSetRealtimeGPUPriority(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *const iddcx::IDARG_IN_SETREALTIMEGPUPRIORITY,
    ) @ IddCxSetRealtimeGPUPriorityTableIndex as PFN_IDDCXSETREALTIMEGPUPRIORITY
);

/// Every struct the driver constructs or reads must be `Sized` — which fails to compile if the
/// wdk-sys `iddcx` bindgen ever stops emitting one, or if a field type (DISPLAYCONFIG_*, LUID,
/// GUID, DXGI_*) falls out of the allowlist and no longer resolves.
macro_rules! assert_sized {
    ($($t:ident),+ $(,)?) => { $( const _: usize = core::mem::size_of::<iddcx::$t>(); )+ };
}

assert_sized!(
    // adapter / device init
    IDD_CX_CLIENT_CONFIG,
    IDARG_IN_ADAPTER_INIT,
    IDARG_OUT_ADAPTER_INIT,
    IDDCX_ADAPTER_CAPS,
    IDDCX_ENDPOINT_VERSION,
    // monitor create / arrival
    IDARG_IN_MONITORCREATE,
    IDARG_OUT_MONITORCREATE,
    IDARG_OUT_MONITORARRIVAL,
    IDDCX_MONITOR_INFO,
    // mode reporting — v1 + the *2 variants that embed DISPLAYCONFIG_*
    IDDCX_MONITOR_MODE,
    IDDCX_MONITOR_MODE2,
    IDDCX_TARGET_MODE,
    IDDCX_TARGET_MODE2,
    IDDCX_PATH,
    IDDCX_PATH2,
    IDARG_IN_PARSEMONITORDESCRIPTION,
    IDARG_OUT_PARSEMONITORDESCRIPTION,
    IDARG_IN_QUERYTARGETMODES,
    IDARG_OUT_QUERYTARGETMODES,
    IDARG_IN_COMMITMODES,
    // swap-chain + frame acquire (HDR *2 path)
    IDARG_IN_SETSWAPCHAIN,
    IDARG_IN_ADAPTERSETRENDERADAPTER,
    IDARG_IN_RELEASEANDACQUIREBUFFER2,
    IDARG_OUT_RELEASEANDACQUIREBUFFER2,
    IDDCX_METADATA2,
);

/// Every inbound `IDD_CX_CLIENT_CONFIG` callback type must exist and stay a nullable `extern fn`.
macro_rules! assert_pfn {
    ($($t:ident),+ $(,)?) => { $( const _: iddcx::$t = None; )+ };
}

assert_pfn!(
    PFN_IDD_CX_DEVICE_IO_CONTROL,
    PFN_IDD_CX_ADAPTER_INIT_FINISHED,
    PFN_IDD_CX_ADAPTER_COMMIT_MODES,
    PFN_IDD_CX_ADAPTER_COMMIT_MODES2,
    PFN_IDD_CX_PARSE_MONITOR_DESCRIPTION,
    PFN_IDD_CX_PARSE_MONITOR_DESCRIPTION2,
    PFN_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES,
    PFN_IDD_CX_MONITOR_QUERY_TARGET_MODES,
    PFN_IDD_CX_MONITOR_QUERY_TARGET_MODES2,
    PFN_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN,
    PFN_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN,
    PFN_IDD_CX_MONITOR_SET_GAMMA_RAMP,
    PFN_IDD_CX_MONITOR_SET_DEFAULT_HDR_METADATA,
    PFN_IDD_CX_ADAPTER_QUERY_TARGET_INFO,
);

/// The versioned struct-size machinery (`IDD_STRUCTURE_SIZE!` in C) must exist and link, so a
/// config can be sized against the live framework instead of by guessing `size_of`.
/// `IddStructures` / `IddStructureCount` are stub-provided statics.
const _: fn() = || {
    let _structs = &raw const iddcx::IddStructures;
    let _count = &raw const iddcx::IddStructureCount;
    let _higher = &raw const iddcx::IddClientVersionHigherThanFramework;
    let _i0 = iddcx::_IDDSTRUCTENUM::INDEX_IDD_CX_CLIENT_CONFIG;
    let _i1 = iddcx::_IDDSTRUCTENUM::INDEX_IDARG_IN_ADAPTER_INIT;
};

// The FP16/HDR adapter flag + high-color-space target cap gate the whole `*2` callback
// requirement. These two enums have no `_`-prefixed module (unlike `_IDDFUNCENUM`).
const _: u32 = iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16;
const _: u32 = iddcx::IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE;
